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

/// REQ-8 Path A side-by-side deploy: where the NEW enclave lives,
/// alongside the still-running OLD on port 9088 + DEPLOY_DIR.
const DEPLOY_DIR_NEW: &str = "/home/azureuser/perp-next";

/// systemd unit for the NEW enclave during a side-by-side migration.
/// Operator pre-stages /etc/systemd/system/perp-dex-enclave-next.service
/// before invoking `node-deploy --side-by-side` (sample in
/// docs/path-a-runbook ships with commit 11).
const ENCLAVE_UNIT_NEW: &str = "perp-dex-enclave-next";

/// Port the NEW enclave listens on for the duration of the migration.
/// Per REQ-7 §3.3 step 2: "old enclave on port 9088 [...] new enclave
/// on port 9089". After ceremony completion the operator's promotion
/// step (commit 11) stops the OLD service and reroutes external
/// traffic; the NEW enclave can either keep 9089 or rebind to 9088
/// depending on the operator's TLS-cert + reverse-proxy topology.
const ENCLAVE_PORT_NEW: u16 = 9089;

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

// ── REQ-8 Path A: side-by-side deploy ────────────────────────────

#[derive(Debug)]
pub struct SideBySideDeployResult {
    pub mrenclave_new: String,
    pub deploy_dir: PathBuf,
    pub unit: String,
    pub port: u16,
}

/// Phase REQ-8 commit 10: install + start the NEW enclave alongside
/// the still-running OLD without touching OLD. Refuses to proceed if
/// the OLD service is not active (no state to migrate from), if the
/// NEW systemd unit is missing (operator pre-stages it), if the NEW
/// deploy dir is occupied (half-completed prior attempt), or if the
/// NEW port is already listening.
///
/// On success: NEW enclave is alive on port 9089 with a fresh
/// MRENCLAVE; OLD enclave continues serving traffic on 9088
/// untouched. Operator next invokes the ceremony driver
/// (commit 11: POST /admin/migrate-state on OLD's orchestrator).
pub async fn deploy_local_side_by_side(
    artefacts: &LocalArtefactSet,
) -> Result<SideBySideDeployResult> {
    info!("node-deploy --side-by-side starting (NEW enclave alongside OLD)");

    // 1. Pre-flight checks specific to side-by-side deploy. None of
    //    these mutate filesystem or services — fail fast before any
    //    side effect.
    preflight_old_running().await?;
    preflight_new_unit_exists()?;
    preflight_new_dir_clean()?;
    preflight_new_port_free().await?;

    // 2. SHA + manifest cross-check (same logic as deploy_local).
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
            info!(expected_mrenclave_new = %mre, "manifest pins MRENCLAVE_new");
            expected_mrenclave = Some(mre.clone());
        }
    }

    // 3. Prepare NEW deploy dir + accounts/ subdir with parent-
    //    matching ownership. Mirrors the OLD-side discipline from
    //    backup_existing (post-mortem fix where root-owned accounts/
    //    blocked the daemon-running user from writing sealed files).
    info!("[1/4] preparing NEW deploy dir {}", DEPLOY_DIR_NEW);
    std::fs::create_dir_all(DEPLOY_DIR_NEW).with_context(|| format!("mkdir {DEPLOY_DIR_NEW}"))?;
    let accounts_dir = format!("{DEPLOY_DIR_NEW}/accounts");
    std::fs::create_dir_all(&accounts_dir).with_context(|| format!("mkdir {accounts_dir}"))?;
    chown_to_parent(&accounts_dir);

    // 4. Install artefacts.
    info!("[2/4] installing NEW artefacts to {}", DEPLOY_DIR_NEW);
    install_artefact(
        &artefacts.orchestrator,
        &format!("{DEPLOY_DIR_NEW}/perp-dex-orchestrator"),
        0o755,
    )?;
    install_artefact(
        &artefacts.perp_dex_server,
        &format!("{DEPLOY_DIR_NEW}/perp-dex-server"),
        0o755,
    )?;
    install_artefact(
        &artefacts.enclave_signed_so,
        &format!("{DEPLOY_DIR_NEW}/enclave.signed.so"),
        0o644,
    )?;

    // 5. Start NEW enclave service. OLD is NOT touched.
    info!("[3/4] systemctl start {ENCLAVE_UNIT_NEW}");
    sudo_systemctl(&["start", ENCLAVE_UNIT_NEW]).await?;

    // 6. Verify NEW health on port 9089. Loop briefly because enclave
    //    init takes a moment; budget = 30 s total.
    info!(
        "[4/4] verifying NEW enclave /version on port {}",
        ENCLAVE_PORT_NEW
    );
    let mrenclave_new = curl_version_with_retry(ENCLAVE_PORT_NEW, Duration::from_secs(30)).await?;

    if let Some(expected) = expected_mrenclave {
        if mrenclave_new != expected {
            bail!(
                "MRENCLAVE mismatch on NEW enclave: reports {mrenclave_new}, manifest expected {expected}. \
                 Stop {ENCLAVE_UNIT_NEW}, investigate which enclave is actually running, before proceeding to ceremony."
            );
        }
        info!("MRENCLAVE_new matches manifest");
    }

    info!(
        mrenclave_new_short = &mrenclave_new[..24],
        "NEW enclave alive alongside OLD; ready for migration ceremony driver"
    );
    info!(
        "OLD enclave (port 9088) UNTOUCHED. Run `POST /admin/migrate-state` on OLD's orchestrator next \
         (commit 11) to drive the ceremony."
    );

    Ok(SideBySideDeployResult {
        mrenclave_new,
        deploy_dir: PathBuf::from(DEPLOY_DIR_NEW),
        unit: ENCLAVE_UNIT_NEW.to_string(),
        port: ENCLAVE_PORT_NEW,
    })
}

/// REQ-β4.2 Part C — reset the NEW enclave to empty after a dry-run rehearsal,
/// so the subsequent REAL ceremony starts clean. A dry-run's import populated
/// `{DEPLOY_DIR_NEW}/accounts` and loaded that state into NEW's memory; stop
/// NEW, clear the scratch sealed files, restart so it boots empty — the same
/// empty NEW the side-by-side deploy produced. OLD is NEVER touched (it stayed
/// byte-for-byte inert through the dry-run — the export skipped the durable
/// nonce record + pending-migration arming). Only the NEW side is reset.
pub async fn reset_new_side_after_dry_run() -> Result<()> {
    let accounts_dir = format!("{DEPLOY_DIR_NEW}/accounts");
    info!("dry-run cleanup: stopping {ENCLAVE_UNIT_NEW} to reset NEW to empty state");
    sudo_systemctl(&["stop", ENCLAVE_UNIT_NEW]).await?;

    // The accounts/ subdir holds ONLY sealed migration state (config.json,
    // perp.pem etc. live one level up in perp-next/). Remove every file in it —
    // the rehearsal's resealed import — leaving an empty dir for the real run.
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(&accounts_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                std::fs::remove_file(&p)
                    .with_context(|| format!("remove dry-run scratch file {p:?}"))?;
                removed += 1;
            }
        }
    }
    info!(
        removed_files = removed,
        "dry-run cleanup: cleared NEW scratch accounts; restarting {ENCLAVE_UNIT_NEW}"
    );
    sudo_systemctl(&["start", ENCLAVE_UNIT_NEW]).await?;
    Ok(())
}

// ── Pre-flight helpers ────────────────────────────────────────────

async fn preflight_old_running() -> Result<()> {
    let out = Command::new("systemctl")
        .args(["is-active", ENCLAVE_UNIT])
        .output()
        .await
        .context("spawn systemctl is-active")?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state != "active" {
        bail!(
            "OLD enclave service {ENCLAVE_UNIT} is not active (state={state}). \
             Path A side-by-side deploy requires a live OLD enclave to migrate state from. \
             If you are bootstrapping a fresh node, use plain `node-deploy` (without --side-by-side) instead."
        );
    }
    Ok(())
}

fn preflight_new_unit_exists() -> Result<()> {
    let unit_path = format!("/etc/systemd/system/{ENCLAVE_UNIT_NEW}.service");
    if !Path::new(&unit_path).exists() {
        bail!(
            "NEW enclave systemd unit not found at {unit_path}. \
             Operator must pre-stage this unit before --side-by-side deploy. \
             Sample unit ships in docs/path-a-runbook (commit 11). \
             Refusing to deploy artefacts without a way to start them."
        );
    }
    Ok(())
}

/// Files that are the node's OWN provisioned identity, not migration state: the NEW
/// enclave's `config.json` (its port / accounts dir / cert path) and its per-node TLS
/// `perp.pem`. They are NOT installed by this deploy, and `perp-dex-server` refuses to
/// start without config.json — so demanding a strictly-empty directory forced the
/// operator to delete them, watch the deploy fail its own [4/4] health check, then
/// restore and restart by hand. Two migrations hit that dance. Tolerate exactly these
/// two; everything else (stale binaries, a populated accounts/, prior scratch state)
/// still fails closed, which is the property the check exists for.
const NEW_DIR_PRESERVED: [&str; 2] = ["config.json", "perp.pem"];

fn preflight_new_dir_clean() -> Result<()> {
    dir_holds_only_node_identity(Path::new(DEPLOY_DIR_NEW))
}

/// The real predicate, taking the path so it is testable. The previous tests inlined a
/// copy of it "to avoid the const-path coupling" and therefore asserted nothing about the
/// function that actually runs before a migration.
fn dir_holds_only_node_identity(p: &Path) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    let mut unexpected: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(p).with_context(|| format!("read_dir {}", p.display()))? {
        let name = entry
            .with_context(|| format!("read_dir entry in {}", p.display()))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if !NEW_DIR_PRESERVED.contains(&name.as_str()) {
            unexpected.push(name);
        }
    }
    if !unexpected.is_empty() {
        unexpected.sort();
        bail!(
            "NEW deploy dir {} holds state that is not this node's provisioned identity: {}. \
             Either a prior --side-by-side attempt left state behind, or the path is in \
             unexpected use. Operator must inspect + remove those entries (keeping config.json \
             and perp.pem) before retrying.",
            p.display(),
            unexpected.join(", ")
        );
    }
    Ok(())
}

async fn preflight_new_port_free() -> Result<()> {
    use tokio::net::TcpListener;
    let bind_addr = format!("127.0.0.1:{ENCLAVE_PORT_NEW}");
    match TcpListener::bind(&bind_addr).await {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(e) => bail!(
            "port {ENCLAVE_PORT_NEW} is not available for NEW enclave (bind {bind_addr} failed: {e}). \
             Stop the conflicting process before --side-by-side deploy."
        ),
    }
}

// ── Helpers shared with side-by-side path ─────────────────────────

/// Match a path's owner to its parent's owner via shell `chown`.
/// Best-effort: failure is logged, not fatal — the operator will see
/// a permission error later and can chown manually. Same posture as
/// the OLD-path chown in backup_existing.
fn chown_to_parent(target: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = match Path::new(target).parent() {
            Some(p) => p,
            None => return,
        };
        let parent_meta = match std::fs::metadata(parent) {
            Ok(m) => m,
            Err(_) => return,
        };
        let curr_meta = match std::fs::metadata(target) {
            Ok(m) => m,
            Err(_) => return,
        };
        if curr_meta.uid() == parent_meta.uid() && curr_meta.gid() == parent_meta.gid() {
            return;
        }
        let status = std::process::Command::new("chown")
            .arg(format!("{}:{}", parent_meta.uid(), parent_meta.gid()))
            .arg(target)
            .status();
        match status {
            Ok(s) if s.success() => {
                info!(target = %target, uid = parent_meta.uid(), "chown'd to parent dir owner");
            }
            Ok(s) => {
                tracing::warn!(target = %target, status = ?s.code(), "chown to parent failed");
            }
            Err(e) => {
                tracing::warn!(target = %target, "chown to parent failed: {e}");
            }
        }
    }
    #[cfg(not(unix))]
    let _ = target;
}

/// curl /version on the given port; retry until either MRENCLAVE
/// returns or budget elapses. NEW enclave's first start can take a
/// few seconds (libsgx + DCAP init).
async fn curl_version_with_retry(port: u16, budget: Duration) -> Result<String> {
    let url = format!("https://localhost:{port}/version");
    let start = SystemTime::now();
    let mut last_err: Option<String> = None;
    loop {
        let elapsed = SystemTime::now().duration_since(start).unwrap_or_default();
        if elapsed >= budget {
            bail!(
                "curl /version on port {port} failed after {:?}. last error: {}",
                budget,
                last_err.unwrap_or_else(|| "(none)".into())
            );
        }
        let out = Command::new("curl")
            .args(["-k", "-s", "--max-time", "5", &url])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    Ok(v) => {
                        if let Some(mre) = v["mrenclave"].as_str() {
                            return Ok(mre.to_string());
                        }
                        last_err = Some("mrenclave field missing on /version response".into());
                    }
                    Err(e) => last_err = Some(format!("parse /version JSON: {e}")),
                }
            }
            Ok(o) => {
                last_err = Some(format!(
                    "curl status {:?} stderr={}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
            }
            Err(e) => last_err = Some(format!("spawn curl: {e}")),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
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
    fn side_by_side_constants_distinct_from_old() {
        // Belt-and-braces: defend against a future refactor accidentally
        // collapsing OLD and NEW deploy targets into the same path or
        // service unit. If anyone makes them equal, this test screams.
        assert_ne!(DEPLOY_DIR, DEPLOY_DIR_NEW);
        assert_ne!(ENCLAVE_UNIT, ENCLAVE_UNIT_NEW);
        assert_ne!(ENCLAVE_PORT_NEW, 9088);
    }

    // These exercise the REAL predicate. They used to inline a copy of it and assert
    // things like `!p.exists()`, which passed no matter what the deploy actually did.

    #[test]
    fn new_dir_ok_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        dir_holds_only_node_identity(&tmp.path().join("does-not-exist")).unwrap();
    }

    #[test]
    fn new_dir_ok_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        dir_holds_only_node_identity(tmp.path()).unwrap();
    }

    #[test]
    fn new_dir_ok_with_only_node_identity() {
        // The node's own provisioned files must NOT block the deploy: perp-dex-server
        // refuses to start without config.json, so requiring their removal made the
        // deploy fail its own health check.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("perp.pem"), b"x").unwrap();
        dir_holds_only_node_identity(tmp.path()).unwrap();
    }

    #[test]
    fn new_dir_rejects_leftover_state() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), b"{}").unwrap();
        std::fs::create_dir(tmp.path().join("accounts")).unwrap();
        std::fs::write(tmp.path().join("enclave.signed.so"), b"x").unwrap();
        let err = dir_holds_only_node_identity(tmp.path())
            .unwrap_err()
            .to_string();
        // Names what to remove, and does NOT name the files to keep.
        assert!(err.contains("accounts"), "{err}");
        assert!(err.contains("enclave.signed.so"), "{err}");
        assert!(
            !err.contains("holds state that is not this node's provisioned identity: config.json"),
            "{err}"
        );
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
