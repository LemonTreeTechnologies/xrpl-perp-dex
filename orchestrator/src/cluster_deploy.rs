//! **[DEPRECATED — violates `docs/multi-operator-architecture.md` §4 by
//! requiring SSH from a coordinator host to every cluster node. Replace
//! with `node-deploy` (node-local, run by each operator on their own
//! VM) per the Phase 2.1c-E replacement plan in §11.3 of the same
//! document. Coordinated MRENCLAVE bumps are governance, not a single
//! command. This module exists only to keep testnet operations running
//! during the transition; do NOT extend it, and do NOT inform mainnet
//! design from its shape.]**
//!
//! Phase 2.1b — cluster artefact deploy + service lifecycle.
//!
//! Codifies §3-§5 of `docs/testnet-enclave-bump-procedure.md` as a Rust
//! subcommand: stop both services, backup prior artefacts with a
//! timestamp suffix, install new orchestrator + enclave + server
//! binaries, restart enclaves only (orchestrators stay DOWN until
//! Phase 2.1c provides the new signers_config + escrow).
//!
//! Reuses the topology format from `dkg_bootstrap` so a single
//! `dkg-topology.toml` covers both subcommands. Artefacts are loaded
//! from a build-output directory + a manifest file:
//!   - orchestrator/target/release/perp-dex-orchestrator (Rust binary)
//!   - EthSignerEnclave/dist-azure/{enclave.signed.so,perp-dex-server}
//!   - EthSignerEnclave/dist-azure/build-manifest.txt (sha256 + git_sha)
//!
//! Sequence per node mirrors the existing bash `deploy-azure.sh` so the
//! diff vs that script is localised: stage to /tmp first, verify SHAs,
//! THEN stop the services so the downtime window is just the swap+start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::info;

use crate::dkg_bootstrap::{DkgTopology, NodeConfig};

/// Artefact set we expect on the local filesystem.
#[derive(Debug)]
pub struct ArtefactSet {
    /// Path to the freshly-built orchestrator binary.
    pub orchestrator: PathBuf,
    /// Path to the freshly-built `enclave.signed.so` (Docker artefact).
    pub enclave_signed_so: PathBuf,
    /// Path to the freshly-built `perp-dex-server` (Docker artefact).
    pub perp_dex_server: PathBuf,
    /// Optional path to `build-manifest.txt`. If present, SHAs are
    /// cross-checked against the manifest before deploy. The manifest
    /// also surfaces the expected MRENCLAVE in logs.
    pub build_manifest: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct BuildManifest {
    pub git_sha: Option<String>,
    pub enclave_sha256: Option<String>,
    pub server_sha256: Option<String>,
    pub mrenclave: Option<String>,
}

/// Per-node deployment outcome — used by the operator to confirm each
/// VM came up on the expected MRENCLAVE.
#[derive(Debug)]
pub struct NodeDeployResult {
    pub label: String,
    pub mrenclave: String,
}

/// Entry point. Stops services, swaps binaries, starts enclaves only.
/// Orchestrators remain stopped — Phase 2.1c (operator-setup + escrow)
/// is the natural next step before they come back up.
pub async fn deploy(
    topology: &DkgTopology,
    artefacts: &ArtefactSet,
) -> Result<Vec<NodeDeployResult>> {
    info!(
        n = topology.nodes.len(),
        bastion = ?topology.bastion,
        "cluster-deploy starting"
    );

    // 1. Pre-flight: artefact files exist, SHAs match manifest if provided.
    let local_shas = compute_local_shas(artefacts)?;
    if let Some(path) = artefacts.build_manifest.as_ref() {
        let manifest =
            parse_build_manifest(path).with_context(|| format!("read build manifest {path:?}"))?;
        verify_shas_against_manifest(&local_shas, &manifest)?;
        if let Some(git) = &manifest.git_sha {
            info!(manifest_git_sha = %git, "build manifest");
        }
    }
    for (label, sha) in &local_shas {
        info!(artefact = %label, sha_short = &sha[..16], "local artefact ready");
    }

    let ts = format_timestamp();
    let mut results = Vec::with_capacity(topology.nodes.len());

    for node in &topology.nodes {
        let result = deploy_to_node(topology, artefacts, &local_shas, node, &ts)
            .await
            .with_context(|| format!("deploy to {}", node.label))?;
        results.push(result);
    }

    info!(
        nodes = results.len(),
        "cluster-deploy complete; orchestrators are STOPPED — run Phase 2.1c (operator-setup + escrow) next"
    );
    Ok(results)
}

async fn deploy_to_node(
    topology: &DkgTopology,
    artefacts: &ArtefactSet,
    local_shas: &HashMap<&'static str, String>,
    node: &NodeConfig,
    ts: &str,
) -> Result<NodeDeployResult> {
    info!(label = %node.label, ssh = %node.ssh, "deploying to node");
    let bastion = topology.bastion.as_deref();

    // [1/7] Stage artefacts to /tmp on the remote (no service impact yet).
    info!(label = %node.label, "[1/7] staging artefacts");
    scp_to(
        node,
        bastion,
        &artefacts.orchestrator,
        "/tmp/perp-dex-orchestrator.new",
    )
    .await?;
    scp_to(
        node,
        bastion,
        &artefacts.enclave_signed_so,
        "/tmp/enclave.signed.so.new",
    )
    .await?;
    scp_to(
        node,
        bastion,
        &artefacts.perp_dex_server,
        "/tmp/perp-dex-server.new",
    )
    .await?;

    // [2/7] Verify staged SHAs match what we sent.
    info!(label = %node.label, "[2/7] verifying staged SHAs");
    let cmd = r#"sha256sum /tmp/perp-dex-orchestrator.new /tmp/enclave.signed.so.new /tmp/perp-dex-server.new"#;
    let out = run_ssh(node, bastion, cmd).await?;
    let stdout = String::from_utf8_lossy(&out);
    let mut remote_shas: HashMap<&str, String> = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(sha), Some(path)) = (parts.next(), parts.next()) {
            let key = match path {
                "/tmp/perp-dex-orchestrator.new" => "orchestrator",
                "/tmp/enclave.signed.so.new" => "enclave_signed_so",
                "/tmp/perp-dex-server.new" => "perp_dex_server",
                _ => continue,
            };
            remote_shas.insert(key, sha.to_string());
        }
    }
    for (key, expected) in local_shas {
        let got = remote_shas
            .get(*key)
            .with_context(|| format!("remote sha missing for {key} on {}", node.label))?;
        if got != expected {
            bail!(
                "{}: SHA mismatch for {key}: local {expected}, remote {got}",
                node.label
            );
        }
    }

    // [3/7] Stop both services. Downtime window starts here.
    info!(label = %node.label, "[3/7] systemctl stop both services");
    run_ssh(
        node,
        bastion,
        "sudo systemctl stop perp-dex-orchestrator perp-dex-enclave",
    )
    .await?;

    // [4/7] Backup prior artefacts + accounts/ + signers_config.json.
    // accounts/ is preserved as forensic evidence even though new MRENCLAVE
    // (if it changed) cannot decrypt it; we never delete sealed state blind.
    info!(label = %node.label, ts = %ts, "[4/7] backing up prior artefacts");
    let backup_cmd = format!(
        r#"cd /home/azureuser/perp && \
[ -f enclave.signed.so ] && mv enclave.signed.so enclave.signed.so.prev-{ts} || true && \
[ -f perp-dex-server ] && mv perp-dex-server perp-dex-server.prev-{ts} || true && \
[ -f perp-dex-orchestrator ] && mv perp-dex-orchestrator perp-dex-orchestrator.prev-{ts} || true && \
[ -f signers_config.json ] && cp -a signers_config.json signers_config.json.prev-{ts} || true && \
[ -d accounts ] && mv accounts accounts.prev-{ts} || true && \
mkdir accounts"#
    );
    run_ssh(node, bastion, &backup_cmd).await?;

    // [5/7] Install new artefacts (atomic mv from staging).
    info!(label = %node.label, "[5/7] installing new artefacts");
    let install_cmd = r#"cd /home/azureuser/perp && \
install -m 0755 /tmp/perp-dex-orchestrator.new ./perp-dex-orchestrator && \
install -m 0755 /tmp/perp-dex-server.new ./perp-dex-server && \
install -m 0644 /tmp/enclave.signed.so.new ./enclave.signed.so && \
rm -f /tmp/perp-dex-orchestrator.new /tmp/perp-dex-server.new /tmp/enclave.signed.so.new"#;
    run_ssh(node, bastion, install_cmd).await?;

    // [6/7] Start enclave only. Orchestrator stays DOWN until 2.1c.
    info!(label = %node.label, "[6/7] systemctl start perp-dex-enclave");
    run_ssh(node, bastion, "sudo systemctl start perp-dex-enclave").await?;

    // [7/7] Verify enclave health + capture new MRENCLAVE.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let v = run_ssh(
        node,
        bastion,
        "curl -k -s --max-time 5 https://localhost:9088/version",
    )
    .await?;
    let v: serde_json::Value = serde_json::from_slice(&v)
        .with_context(|| format!("parse /version JSON on {}", node.label))?;
    let mrenclave = v["mrenclave"]
        .as_str()
        .with_context(|| format!("missing mrenclave field on {}", node.label))?
        .to_string();
    info!(label = %node.label, mrenclave_short = &mrenclave[..24], "[7/7] enclave running");

    Ok(NodeDeployResult {
        label: node.label.clone(),
        mrenclave,
    })
}

// ── SHA + manifest helpers ───────────────────────────────────────

fn compute_sha256(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn compute_local_shas(artefacts: &ArtefactSet) -> Result<HashMap<&'static str, String>> {
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
    // Orchestrator binary isn't in the enclave manifest — that one is
    // produced by `cargo build`, not the docker-build pipeline. We rely
    // on the remote-staging SHA-vs-local check to catch transmission
    // errors there.
    Ok(())
}

// ── SSH/SCP helpers ──────────────────────────────────────────────

async fn run_ssh(node: &NodeConfig, bastion: Option<&str>, remote_cmd: &str) -> Result<Vec<u8>> {
    let mut ssh = Command::new("ssh");
    ssh.arg("-o").arg("StrictHostKeyChecking=no");
    if let Some(b) = bastion {
        ssh.arg("-o").arg(format!("ProxyJump={b}"));
    }
    ssh.arg(&node.ssh).arg(remote_cmd);
    let output = ssh.output().await.context("spawn ssh")?;
    if !output.status.success() {
        bail!(
            "ssh {} failed (status {:?}): stderr={}",
            node.ssh,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn scp_to(
    node: &NodeConfig,
    bastion: Option<&str>,
    local: &Path,
    remote: &str,
) -> Result<()> {
    let mut scp = Command::new("scp");
    scp.arg("-o").arg("StrictHostKeyChecking=no").arg("-q");
    if let Some(b) = bastion {
        scp.arg("-o").arg(format!("ProxyJump={b}"));
    }
    scp.arg(local).arg(format!("{}:{remote}", node.ssh));
    let status = scp.status().await.context("spawn scp")?;
    if !status.success() {
        bail!(
            "scp {local:?} -> {}:{remote} failed (status {:?})",
            node.ssh,
            status.code()
        );
    }
    Ok(())
}

fn format_timestamp() -> String {
    // Convention from the manual procedure (deploy-azure.sh) is
    // YYYYMMDD-HHMMSS UTC. Shell out to coreutils `date` since we're
    // already shelling out to ssh/scp anyway and `chrono`/`time` aren't
    // in the orchestrator's dep tree.
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y%m%d-%H%M%S"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        // Fallback: raw seconds-since-epoch. Still sortable, just less
        // human-friendly when grepping prev-* backup directories.
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
        writeln!(tmp, "build_date=2026-04-26T22:11:03Z").unwrap();
        writeln!(tmp, "image=perp-dex-azure:2c3d31f").unwrap();
        writeln!(
            tmp,
            "enclave_sha256=cebf16057ef11223d5cfbc46b6635ef54aaa4553d6cf04530bfd508fbd52ad5d"
        )
        .unwrap();
        writeln!(
            tmp,
            "server_sha256=7d55e6c7f2aba8056481998739cb309b8651aac35c00a5b25f605e5c4e99a8cb"
        )
        .unwrap();
        writeln!(
            tmp,
            "mrenclave=4dfe899771bdb3f3097714013d054c08c7dd6e28f2acd17948f8a08f328c011b"
        )
        .unwrap();
        let m = parse_build_manifest(tmp.path()).unwrap();
        assert_eq!(m.git_sha.as_deref(), Some("2c3d31f"));
        assert_eq!(
            m.enclave_sha256.as_deref(),
            Some("cebf16057ef11223d5cfbc46b6635ef54aaa4553d6cf04530bfd508fbd52ad5d")
        );
        assert_eq!(
            m.server_sha256.as_deref(),
            Some("7d55e6c7f2aba8056481998739cb309b8651aac35c00a5b25f605e5c4e99a8cb")
        );
        assert_eq!(
            m.mrenclave.as_deref(),
            Some("4dfe899771bdb3f3097714013d054c08c7dd6e28f2acd17948f8a08f328c011b")
        );
    }

    #[test]
    fn manifest_handles_blank_and_comment_lines() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "# header comment").unwrap();
        writeln!(tmp).unwrap();
        writeln!(tmp, "git_sha=abc").unwrap();
        writeln!(tmp, "  # indented comment").unwrap();
        let m = parse_build_manifest(tmp.path()).unwrap();
        assert_eq!(m.git_sha.as_deref(), Some("abc"));
        assert!(m.enclave_sha256.is_none());
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
        // Either YYYYMMDD-HHMMSS (15 chars, with `-` at index 8) — when
        // coreutils `date` is reachable — or a unix-seconds fallback
        // (variable-length but still sortable lexicographically because
        // post-1970 timestamps are 10+ digits and grow monotonically).
        if ts.len() == 15 {
            assert_eq!(&ts[8..9], "-");
        } else {
            // Fallback path: just digits.
            assert!(ts.chars().all(|c| c.is_ascii_digit()), "got: {ts}");
        }
    }
}
