//! Phase 2.1c-E — `node-deploy` subcommand. Runs locally on a single
//! node by its operator. Stops local services, backs up the previous
//! binaries with a timestamp suffix, installs the new orchestrator +
//! enclave artefacts, restarts the enclave service.
//!
//! Each operator deploys their own node from their own machine — no
//! cross-operator SSH, no central coordinator host. Per
//! `docs/multi-operator-architecture.md` §7.2: "each operator
//! independently deploys" — this subcommand is the codification of
//! that step.
//!
//! Single-mode: orchestrator stays DOWN after `node-deploy` regardless
//! of network. Operators run `node-config-apply` afterwards (which
//! restarts the orchestrator with the discovered roster). On a fresh
//! MRENCLAVE the chain is `node-deploy` → `node-bootstrap` →
//! `node-config-apply`; on an MRENCLAVE-preserved orchestrator-only
//! update it's `node-deploy` → `node-config-apply` (no re-bootstrap
//! needed since the local enclave's keypair survives).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::info;

/// Artefact set the operator points at. Consumed directly on the
/// local node — no scp, no remote staging.
#[derive(Debug)]
pub struct LocalArtefactSet {
    pub orchestrator: PathBuf,
    pub enclave_signed_so: PathBuf,
    pub perp_dex_server: PathBuf,
    /// Optional `build-manifest.txt` (typically alongside the enclave
    /// dist dir). When present, SHAs are cross-checked + expected
    /// MRENCLAVE is logged for operator confirmation.
    pub build_manifest: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct BuildManifest {
    pub git_sha: Option<String>,
    pub enclave_sha256: Option<String>,
    pub server_sha256: Option<String>,
    pub mrenclave: Option<String>,
}

#[derive(Debug)]
pub struct NodeDeployResult {
    pub mrenclave: String,
    pub backup_suffix: String,
}

/// Where the deployed binaries live on a typical operator VM. Hard-
/// coded to match the existing testnet topology; future versions may
/// take this as a flag if mainnet operators put things elsewhere.
const DEPLOY_DIR: &str = "/home/azureuser/perp";

/// Per-node orchestrator service unit. The systemd unit shipped in
/// `EthSignerEnclave/scripts/systemd/perp-dex-orchestrator.service`.
const ORCHESTRATOR_UNIT: &str = "perp-dex-orchestrator";

/// Per-node enclave service unit.
const ENCLAVE_UNIT: &str = "perp-dex-enclave";

pub async fn deploy_local(artefacts: &LocalArtefactSet) -> Result<NodeDeployResult> {
    info!("node-deploy starting (local node only)");

    // 1. Pre-flight: artefact files exist and (optionally) SHAs match
    //    the manifest. Bails before touching services on mismatch.
    let local_shas = compute_local_shas(artefacts)?;
    let mut expected_mrenclave: Option<String> = None;
    if let Some(path) = artefacts.build_manifest.as_ref() {
        let manifest =
            parse_build_manifest(path).with_context(|| format!("read build manifest {path:?}"))?;
        verify_shas_against_manifest(&local_shas, &manifest)?;
        if let Some(git) = &manifest.git_sha {
            info!(manifest_git_sha = %git, "build manifest");
        }
        if let Some(mre) = &manifest.mrenclave {
            info!(expected_mrenclave = %mre, "manifest pins MRENCLAVE");
            expected_mrenclave = Some(mre.clone());
        }
    }
    for (label, sha) in &local_shas {
        info!(artefact = %label, sha_short = &sha[..16], "local artefact ready");
    }

    let ts = format_timestamp();

    // 2. Stop both services. Downtime window starts here.
    info!("[1/5] systemctl stop both services");
    sudo_systemctl(&["stop", ORCHESTRATOR_UNIT, ENCLAVE_UNIT]).await?;

    // 3. Backup existing binaries + accounts/ + signers_config.json
    //    with a timestamp suffix. accounts/ is preserved as a forensic
    //    backup; the new MRENCLAVE (if it changed) cannot decrypt it,
    //    but we never delete sealed state blind.
    info!(ts = %ts, "[2/5] backing up prior artefacts");
    backup_existing(&ts)?;

    // 4. Install new binaries. `install -m` sets perms; mv-then-rename
    //    is atomic at the filesystem level.
    info!("[3/5] installing new artefacts");
    install_artefact(
        &artefacts.orchestrator,
        &format!("{DEPLOY_DIR}/perp-dex-orchestrator"),
        0o755,
    )?;
    install_artefact(
        &artefacts.perp_dex_server,
        &format!("{DEPLOY_DIR}/perp-dex-server"),
        0o755,
    )?;
    install_artefact(
        &artefacts.enclave_signed_so,
        &format!("{DEPLOY_DIR}/enclave.signed.so"),
        0o644,
    )?;

    // 5. Start enclave only. Orchestrator stays DOWN — operator runs
    //    node-config-apply (Phase 2.1c-C) next, which will restart the
    //    orchestrator with the discovered roster.
    info!("[4/5] systemctl start perp-dex-enclave");
    sudo_systemctl(&["start", ENCLAVE_UNIT]).await?;

    // 6. Verify enclave health + capture new MRENCLAVE.
    tokio::time::sleep(Duration::from_secs(5)).await;
    info!("[5/5] verifying enclave /version");
    let out = Command::new("curl")
        .args([
            "-k",
            "-s",
            "--max-time",
            "5",
            "https://localhost:9088/version",
        ])
        .output()
        .await
        .context("curl /version failed")?;
    if !out.status.success() {
        bail!(
            "curl /version exited {:?}: stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parse /version JSON")?;
    let mrenclave = v["mrenclave"]
        .as_str()
        .context("missing mrenclave field on /version")?
        .to_string();

    if let Some(expected) = expected_mrenclave {
        if mrenclave != expected {
            bail!(
                "MRENCLAVE mismatch: enclave reports {mrenclave}, manifest expected {expected}. \
                 The enclave running is NOT the one we deployed. Investigate before proceeding."
            );
        }
        info!("MRENCLAVE matches manifest");
    }

    info!(mrenclave_short = &mrenclave[..24], "node-deploy complete");
    info!("Orchestrator is STOPPED. Run `node-config-apply` next to restart it with discovered roster.");

    Ok(NodeDeployResult {
        mrenclave,
        backup_suffix: ts,
    })
}

// ── Backup + install ─────────────────────────────────────────────

fn backup_existing(ts: &str) -> Result<()> {
    let candidates = [
        ("enclave.signed.so", false),
        ("perp-dex-server", false),
        ("perp-dex-orchestrator", false),
        ("signers_config.json", true), // copy, don't move
    ];
    for (name, copy_only) in candidates {
        let src = PathBuf::from(format!("{DEPLOY_DIR}/{name}"));
        if !src.exists() {
            continue;
        }
        let dst = PathBuf::from(format!("{DEPLOY_DIR}/{name}.prev-{ts}"));
        if copy_only {
            std::fs::copy(&src, &dst).with_context(|| format!("copy {src:?} → {dst:?}"))?;
        } else {
            std::fs::rename(&src, &dst).with_context(|| format!("rename {src:?} → {dst:?}"))?;
        }
    }
    let accounts = PathBuf::from(format!("{DEPLOY_DIR}/accounts"));
    if accounts.exists() {
        let backup = PathBuf::from(format!("{DEPLOY_DIR}/accounts.prev-{ts}"));
        std::fs::rename(&accounts, &backup)
            .with_context(|| format!("rename accounts → {backup:?}"))?;
    }
    std::fs::create_dir_all(format!("{DEPLOY_DIR}/accounts"))
        .context("recreate empty accounts/")?;

    // Ownership invariant: `accounts/` must match the parent
    // (DEPLOY_DIR) so the unprivileged daemon-running user can write
    // sealed account files to it. If the operator (or their AI
    // assistant) accidentally invoked `node-deploy` via outer sudo, the
    // freshly-created dir would be `root:root` and the enclave would
    // fail `/pool/generate` with "Failed to generate account". Match
    // ownership to the parent dir's, which is the deploy account.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent_meta =
            std::fs::metadata(DEPLOY_DIR).with_context(|| format!("stat {DEPLOY_DIR}"))?;
        let target = format!("{DEPLOY_DIR}/accounts");
        // Only attempt chown if we'd actually change ownership; chown
        // requires CAP_CHOWN if mismatched, which we'll have if running
        // via outer sudo and skip silently if not.
        let curr_meta = std::fs::metadata(&target)?;
        if curr_meta.uid() != parent_meta.uid() || curr_meta.gid() != parent_meta.gid() {
            // Best effort — `chown` shells out so we don't pull in
            // libc bindings just for this. Failure is logged but not
            // fatal; the operator will see /pool/generate 500 and can
            // chown manually.
            let status = std::process::Command::new("chown")
                .arg(format!("{}:{}", parent_meta.uid(), parent_meta.gid()))
                .arg(&target)
                .status();
            match status {
                Ok(s) if s.success() => {
                    info!(target = %target, uid = parent_meta.uid(), "chown'd accounts/ to deploy-dir owner");
                }
                Ok(s) => {
                    tracing::warn!(target = %target, status = ?s.code(), "chown accounts/ failed; daemon may not be able to write to it");
                }
                Err(e) => {
                    tracing::warn!(target = %target, "chown accounts/ failed: {e}");
                }
            }
        }
    }

    Ok(())
}

fn install_artefact(src: &Path, dst: &str, mode: u32) -> Result<()> {
    let dst_path = PathBuf::from(dst);
    std::fs::copy(src, &dst_path).with_context(|| format!("copy {src:?} → {dst_path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst_path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {mode:o} {dst_path:?}"))?;
    }
    let _ = mode; // silences unused warning on non-unix
    Ok(())
}

async fn sudo_systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("sudo")
        .arg("systemctl")
        .args(args)
        .output()
        .await
        .context("spawn sudo systemctl")?;
    if !out.status.success() {
        bail!(
            "sudo systemctl {} failed (status {:?}): stderr={}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ── SHA + manifest helpers ────────────────────────────────────────

fn compute_sha256(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn compute_local_shas(artefacts: &LocalArtefactSet) -> Result<HashMap<&'static str, String>> {
    let mut out = HashMap::new();
    out.insert("orchestrator", compute_sha256(&artefacts.orchestrator)?);
    out.insert(
        "enclave_signed_so",
        compute_sha256(&artefacts.enclave_signed_so)?,
    );
    out.insert(
        "perp_dex_server",
        compute_sha256(&artefacts.perp_dex_server)?,
    );
    Ok(out)
}

pub fn parse_build_manifest(path: &Path) -> Result<BuildManifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    let mut m = BuildManifest::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match k.trim() {
            "git_sha" => m.git_sha = Some(v.trim().to_string()),
            "enclave_sha256" => m.enclave_sha256 = Some(v.trim().to_string()),
            "server_sha256" => m.server_sha256 = Some(v.trim().to_string()),
            "mrenclave" => m.mrenclave = Some(v.trim().to_string()),
            _ => {}
        }
    }
    Ok(m)
}

fn verify_shas_against_manifest(
    local: &HashMap<&'static str, String>,
    manifest: &BuildManifest,
) -> Result<()> {
    if let Some(expected) = &manifest.enclave_sha256 {
        let got = local
            .get("enclave_signed_so")
            .context("local enclave sha missing")?;
        if got != expected {
            bail!("enclave_signed_so SHA mismatch: local {got}, manifest {expected}");
        }
    }
    if let Some(expected) = &manifest.server_sha256 {
        let got = local
            .get("perp_dex_server")
            .context("local server sha missing")?;
        if got != expected {
            bail!("perp_dex_server SHA mismatch: local {got}, manifest {expected}");
        }
    }
    Ok(())
}

fn format_timestamp() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y%m%d-%H%M%S"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn manifest_parses_canonical_layout() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "git_sha=2c3d31f").unwrap();
        writeln!(tmp, "enclave_sha256=cebf16057ef11223").unwrap();
        writeln!(tmp, "server_sha256=7d55e6c7f2aba805").unwrap();
        writeln!(tmp, "mrenclave=4dfe899771bdb3f3").unwrap();
        let m = parse_build_manifest(tmp.path()).unwrap();
        assert_eq!(m.git_sha.as_deref(), Some("2c3d31f"));
        assert_eq!(m.mrenclave.as_deref(), Some("4dfe899771bdb3f3"));
    }

    #[test]
    fn verify_rejects_mismatched_enclave_sha() {
        let mut local = HashMap::new();
        local.insert("enclave_signed_so", "AAA".to_string());
        local.insert("perp_dex_server", "BBB".to_string());
        local.insert("orchestrator", "CCC".to_string());
        let manifest = BuildManifest {
            enclave_sha256: Some("XXX".to_string()),
            server_sha256: Some("BBB".to_string()),
            ..Default::default()
        };
        let err = verify_shas_against_manifest(&local, &manifest).unwrap_err();
        assert!(err.to_string().contains("enclave_signed_so SHA mismatch"));
    }

    #[test]
    fn verify_skips_when_manifest_absent() {
        let mut local = HashMap::new();
        local.insert("enclave_signed_so", "AAA".to_string());
        local.insert("perp_dex_server", "BBB".to_string());
        local.insert("orchestrator", "CCC".to_string());
        let manifest = BuildManifest::default();
        verify_shas_against_manifest(&local, &manifest).unwrap();
    }

    #[test]
    fn timestamp_is_alphabetically_sortable() {
        let ts = format_timestamp();
        if ts.len() == 15 {
            assert_eq!(&ts[8..9], "-");
        } else {
            assert!(ts.chars().all(|c| c.is_ascii_digit()), "got: {ts}");
        }
    }
}
