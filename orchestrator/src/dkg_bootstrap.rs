//! Phase 2.1a — DKG bootstrap subcommand.
//!
//! Drives the full §9 ceremony from the testnet enclave-bump procedure:
//!   1. Pre-DKG attestation round (group_id sentinel = 32 zero bytes).
//!   2. Round 1 — VSS commitment per node.
//!   3. Round 1.5 — pairwise share export-v2 (ECDH-over-DCAP, AAD-bound).
//!   4. Round 2 — pairwise share import-v2 + VSS verify.
//!   5. Finalize — emit group_pubkey, cross-check byte-identical across N.
//!
//! The driver shells out to `ssh` + `scp` because Azure's enclave port 9088
//! is firewalled to localhost. Per-node bodies (DCAP quotes are ~9.5 KB)
//! travel as files via `scp` to dodge the SSH-shell-quoting truncation
//! foot-gun documented in `docs/testnet-enclave-bump-procedure §App A`.
//!
//! No dry-run / rollback / parallelism in this first cut — Phase 2.1d
//! ties it together with the rest of the bump cycle and adds those.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{info, warn};

const GROUP_ID_ZEROS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// `dkg-topology.toml` shape. Operator builds this once per cluster.
#[derive(Debug, Deserialize)]
pub struct DkgTopology {
    /// FROST threshold (must be 2..=nodes.len()).
    pub threshold: u32,
    /// SSH ProxyJump host. None = direct connect to each node.
    #[serde(default)]
    pub bastion: Option<String>,
    /// Cluster nodes, one per FROST participant.
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NodeConfig {
    /// 0-indexed FROST participant id (0..n-1).
    pub pid: u32,
    /// Human label, e.g. "node-1".
    pub label: String,
    /// `user@host` SSH target for the Azure VM.
    pub ssh: String,
    /// Loopback enclave URL on that VM, e.g. `https://localhost:9088/v1`.
    pub enclave_url: String,
}

/// Result of a successful bootstrap.
#[derive(Debug)]
pub struct BootstrapResult {
    /// 32-byte BIP340 x-only group public key (hex, no `0x` prefix).
    pub group_pubkey: String,
    /// Per-node group_pubkey (must all match). Recorded for forensics if
    /// a future check ever finds a divergence.
    pub per_node: Vec<(String, String)>,
}

/// Entry point — parses topology + drives the 7 stages.
pub async fn run(topology_path: &PathBuf) -> Result<BootstrapResult> {
    let raw = std::fs::read_to_string(topology_path)
        .with_context(|| format!("read topology {topology_path:?}"))?;
    let topo: DkgTopology = toml::from_str(&raw).context("parse topology TOML")?;
    validate_topology(&topo)?;

    info!(
        n = topo.nodes.len(),
        threshold = topo.threshold,
        bastion = ?topo.bastion,
        "DKG bootstrap starting"
    );

    let pubkeys = collect_ecdh_pubkeys(&topo).await?;
    let attestations = collect_attestations(&topo, &pubkeys).await?;
    cross_verify(&topo, &pubkeys, &attestations).await?;
    let vss = round1_generate(&topo).await?;
    let envelopes = round1_export(&topo, &pubkeys).await?;
    round2_import(&topo, &envelopes, &vss, &pubkeys).await?;
    let result = finalize_all(&topo).await?;

    info!(group_pubkey = %result.group_pubkey, "DKG bootstrap successful");
    Ok(result)
}

fn validate_topology(topo: &DkgTopology) -> Result<()> {
    let n = topo.nodes.len() as u32;
    if n < 2 {
        bail!("need at least 2 nodes; got {n}");
    }
    let mut pids: Vec<u32> = topo.nodes.iter().map(|n| n.pid).collect();
    pids.sort();
    for (i, pid) in pids.iter().enumerate() {
        if *pid != i as u32 {
            bail!("nodes must have pid 0..{} (contiguous, distinct); got {pids:?}", n - 1);
        }
    }
    if topo.threshold < 2 || topo.threshold > n {
        bail!("threshold {} not in [2, {n}]", topo.threshold);
    }
    Ok(())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

// ── SSH helpers ───────────────────────────────────────────────────

async fn run_ssh(target: &str, bastion: Option<&str>, remote_cmd: &str) -> Result<Vec<u8>> {
    let mut ssh = Command::new("ssh");
    ssh.arg("-o").arg("StrictHostKeyChecking=no");
    if let Some(b) = bastion {
        ssh.arg("-o").arg(format!("ProxyJump={b}"));
    }
    ssh.arg(target).arg(remote_cmd);
    let output = ssh.output().await.context("spawn ssh")?;
    if !output.status.success() {
        bail!(
            "ssh {target} failed (status {:?}): stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn ssh_get_json(node: &NodeConfig, bastion: Option<&str>, path_after_base: &str) -> Result<serde_json::Value> {
    let url = format!("{}{}", node.enclave_url, path_after_base);
    let cmd = format!("curl -k -s {url}");
    let out = run_ssh(&node.ssh, bastion, &cmd).await?;
    parse_json(&out, &url)
}

async fn ssh_post_json(node: &NodeConfig, bastion: Option<&str>, path_after_base: &str, body: &str) -> Result<serde_json::Value> {
    let body_uuid = uuid::Uuid::new_v4().simple().to_string();
    let local_tmp = std::env::temp_dir().join(format!("dkg-{body_uuid}.json"));
    let remote_path = format!("/tmp/dkg-{body_uuid}.json");

    std::fs::write(&local_tmp, body).context("write body to local tempfile")?;

    let scp_target = format!("{}:{remote_path}", node.ssh);
    let mut scp = Command::new("scp");
    scp.arg("-o").arg("StrictHostKeyChecking=no").arg("-q");
    if let Some(b) = bastion {
        scp.arg("-o").arg(format!("ProxyJump={b}"));
    }
    scp.arg(&local_tmp).arg(&scp_target);
    let scp_status = scp.status().await.context("spawn scp")?;
    let _ = std::fs::remove_file(&local_tmp);
    if !scp_status.success() {
        bail!("scp to {} failed (status {:?})", node.ssh, scp_status.code());
    }

    let url = format!("{}{}", node.enclave_url, path_after_base);
    let cmd = format!(
        r#"curl -k -s -X POST -H 'Content-Type: application/json' --data-binary @{remote_path} {url}; rm -f {remote_path}"#
    );
    let out = run_ssh(&node.ssh, bastion, &cmd).await?;
    parse_json(&out, &url)
}

fn parse_json(bytes: &[u8], url: &str) -> Result<serde_json::Value> {
    serde_json::from_slice(bytes).with_context(|| {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
        format!("parse JSON from {url}: body[..200]={snippet:?}")
    })
}

// ── Stage 1 — collect ECDH pubkeys ───────────────────────────────

async fn collect_ecdh_pubkeys(topo: &DkgTopology) -> Result<Vec<String>> {
    info!("Stage 1/7: collecting ECDH pubkeys");
    let bastion = topo.bastion.as_deref();
    let mut pks = vec![String::new(); topo.nodes.len()];
    for n in &topo.nodes {
        let json = ssh_get_json(n, bastion, "/pool/ecdh/pubkey").await
            .with_context(|| format!("ecdh/pubkey on {}", n.label))?;
        let pk = json["pubkey"].as_str()
            .with_context(|| format!("missing pubkey field on {}", n.label))?;
        let pk = strip_0x(pk).to_string();
        info!(pid = n.pid, label = %n.label, pk_short = %&pk[..16], "ECDH pubkey");
        pks[n.pid as usize] = pk;
    }
    Ok(pks)
}

// ── Stage 2 — report_data + DCAP quote per node ──────────────────

/// Each node's DCAP quote hex (no `0x` prefix), ~9.5 KB. Indexed by pid.
type AttestationQuotes = Vec<String>;

async fn collect_attestations(topo: &DkgTopology, _pubkeys: &[String]) -> Result<AttestationQuotes> {
    info!("Stage 2/7: collecting DCAP report_data + quote per node");
    let bastion = topo.bastion.as_deref();
    let mut out: AttestationQuotes = vec![String::new(); topo.nodes.len()];

    for n in &topo.nodes {
        let body = serde_json::json!({
            "shard_id": 0,
            "group_id": GROUP_ID_ZEROS,
        }).to_string();
        let rd_json = ssh_post_json(n, bastion, "/pool/ecdh/report-data", &body).await
            .with_context(|| format!("ecdh/report-data on {}", n.label))?;
        let rd = strip_0x(rd_json["report_data"].as_str()
            .with_context(|| format!("missing report_data field on {}", n.label))?
        ).to_string();

        let body = serde_json::json!({ "user_data": rd }).to_string();
        let q_json = ssh_post_json(n, bastion, "/pool/attestation-quote", &body).await
            .with_context(|| format!("attestation-quote on {}", n.label))?;
        let quote = strip_0x(q_json["quote_hex"].as_str()
            .with_context(|| format!("missing quote_hex field on {}", n.label))?
        ).to_lowercase();

        info!(
            pid = n.pid,
            label = %n.label,
            rd_len = rd.len(),
            quote_len = quote.len(),
            "attestation collected"
        );
        out[n.pid as usize] = quote;
    }
    Ok(out)
}

// ── Stage 3 — cross-verify quotes (each peer verifies the other N-1) ─

async fn cross_verify(
    topo: &DkgTopology,
    pubkeys: &[String],
    quotes: &AttestationQuotes,
) -> Result<()> {
    info!("Stage 3/7: cross-verifying DCAP quotes (group_id = zeros sentinel)");
    let bastion = topo.bastion.as_deref();
    let now = now_ts();

    for tgt in &topo.nodes {
        for src in &topo.nodes {
            if src.pid == tgt.pid {
                continue;
            }
            let body = serde_json::json!({
                "quote": quotes[src.pid as usize],
                "peer_pubkey": pubkeys[src.pid as usize],
                "shard_id": 0,
                "group_id": GROUP_ID_ZEROS,
                "now_ts": now,
            }).to_string();

            let resp = ssh_post_json(tgt, bastion, "/pool/attest/verify-peer-quote", &body).await
                .with_context(|| format!(
                    "verify-peer-quote on {} for {}",
                    tgt.label, src.label
                ))?;
            let status = resp["status"].as_str().unwrap_or("?");
            if status != "success" {
                bail!(
                    "{} → {} verify rejected: {resp}",
                    tgt.label, src.label
                );
            }
            info!(tgt = %tgt.label, src = %src.label, "verify-peer-quote OK");
        }
    }
    Ok(())
}

// ── Stage 4 — Round 1 (VSS commitment per node) ──────────────────

#[derive(Debug)]
struct Round1Output {
    /// `vss_commitment` from /pool/dkg/round1-generate, hex no `0x`.
    vss_commitment: String,
}

async fn round1_generate(topo: &DkgTopology) -> Result<Vec<Round1Output>> {
    info!("Stage 4/7: DKG round 1 — VSS commitment generation");
    let bastion = topo.bastion.as_deref();
    let n = topo.nodes.len() as u32;
    let mut out: Vec<Round1Output> = Vec::with_capacity(topo.nodes.len());
    out.resize_with(topo.nodes.len(), || Round1Output { vss_commitment: String::new() });

    for node in &topo.nodes {
        let body = serde_json::json!({
            "my_participant_id": node.pid,
            "threshold": topo.threshold,
            "n_participants": n,
        }).to_string();
        let resp = ssh_post_json(node, bastion, "/pool/dkg/round1-generate", &body).await
            .with_context(|| format!("round1-generate on {}", node.label))?;
        let vss = strip_0x(resp["vss_commitment"].as_str()
            .with_context(|| format!("missing vss_commitment on {}", node.label))?
        ).to_string();
        info!(pid = node.pid, label = %node.label, vss_len = vss.len(), "round1 done");
        out[node.pid as usize] = Round1Output { vss_commitment: vss };
    }
    Ok(out)
}

// ── Stage 5 — Round 1.5 (pairwise share export-v2) ───────────────

/// envelopes[src][tgt] = ShareEnvelope from src to tgt (None when src==tgt).
type EnvelopeMatrix = Vec<Vec<Option<ShareEnvelope>>>;

#[derive(Debug, Clone)]
struct ShareEnvelope {
    /// JSON object as returned by the enclave: {ceremony_nonce, iv, ct, tag, sender_pubkey}.
    json: serde_json::Value,
}

async fn round1_export(topo: &DkgTopology, _pubkeys: &[String]) -> Result<EnvelopeMatrix> {
    info!("Stage 5/7: DKG round 1.5 — share export-v2 (ECDH-over-DCAP)");
    let bastion = topo.bastion.as_deref();
    let n = topo.nodes.len();
    let mut envs: EnvelopeMatrix = (0..n).map(|_| (0..n).map(|_| None).collect()).collect();

    let now = now_ts();
    for src in &topo.nodes {
        for tgt in &topo.nodes {
            if src.pid == tgt.pid {
                continue;
            }
            let body = serde_json::json!({
                "target_participant_id": tgt.pid,
                "peer_pubkey": _pubkeys[tgt.pid as usize],
                "shard_id": 0,
                "group_id": GROUP_ID_ZEROS,
                "now_ts": now,
            }).to_string();
            let resp = ssh_post_json(src, bastion, "/pool/dkg/round1-export-share-v2", &body).await
                .with_context(|| format!("round1-export-share-v2 on {} (target {})", src.label, tgt.label))?;
            let env = resp.get("envelope").cloned()
                .with_context(|| format!("missing envelope in response on {}", src.label))?;
            envs[src.pid as usize][tgt.pid as usize] = Some(ShareEnvelope { json: env });
            info!(src = %src.label, tgt = %tgt.label, "share exported");
        }
    }
    Ok(envs)
}

// ── Stage 6 — Round 2 (pairwise share import-v2 + VSS verify) ────

async fn round2_import(
    topo: &DkgTopology,
    envelopes: &EnvelopeMatrix,
    vss: &[Round1Output],
    pubkeys: &[String],
) -> Result<()> {
    info!("Stage 6/7: DKG round 2 — share import-v2 + VSS verify");
    let bastion = topo.bastion.as_deref();
    let now = now_ts();

    for tgt in &topo.nodes {
        for src in &topo.nodes {
            if src.pid == tgt.pid {
                continue;
            }
            let env = envelopes[src.pid as usize][tgt.pid as usize].as_ref()
                .with_context(|| format!("missing envelope src={} tgt={}", src.label, tgt.label))?;
            let body = serde_json::json!({
                "from_participant_id": src.pid,
                "sender_pubkey": pubkeys[src.pid as usize],
                "shard_id": 0,
                "group_id": GROUP_ID_ZEROS,
                "now_ts": now,
                "envelope": env.json,
                "vss_commitment": vss[src.pid as usize].vss_commitment,
            }).to_string();
            let resp = ssh_post_json(tgt, bastion, "/pool/dkg/round2-import-share-v2", &body).await
                .with_context(|| format!("round2-import-share-v2 on {} (from {})", tgt.label, src.label))?;
            let status = resp["status"].as_str().unwrap_or("?");
            if status != "success" {
                bail!(
                    "{} import from {} rejected: {}",
                    tgt.label, src.label, resp
                );
            }
            info!(tgt = %tgt.label, src = %src.label, "share imported + verified");
        }
    }
    Ok(())
}

// ── Stage 7 — finalize per node, cross-check group_pubkey ────────

async fn finalize_all(topo: &DkgTopology) -> Result<BootstrapResult> {
    info!("Stage 7/7: finalize per node + cross-check group_pubkey");
    let bastion = topo.bastion.as_deref();
    let mut per_node: Vec<(String, String)> = Vec::with_capacity(topo.nodes.len());

    for node in &topo.nodes {
        // Empty body — finalize takes no args.
        let resp = ssh_post_json(node, bastion, "/pool/dkg/finalize", "{}").await
            .with_context(|| format!("finalize on {}", node.label))?;
        let group_pk = strip_0x(resp["group_pubkey"].as_str()
            .with_context(|| format!("missing group_pubkey on {}", node.label))?
        ).to_string();
        info!(pid = node.pid, label = %node.label, group_pk_short = %&group_pk[..16], "finalize done");
        per_node.push((node.label.clone(), group_pk));
    }

    // Cross-check: all entries must match byte-identical.
    let canonical = per_node[0].1.clone();
    for (label, pk) in &per_node {
        if pk != &canonical {
            warn!(label = %label, pk_first8 = %&pk[..16], canonical_first8 = %&canonical[..16],
                "DIVERGENT group_pubkey");
            bail!(
                "DKG transcript inconsistent: {} produced {}, expected {} (matches {}). Abort.",
                label, pk, canonical, per_node[0].0
            );
        }
    }

    Ok(BootstrapResult { group_pubkey: canonical, per_node })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_parses_three_node_example() {
        let toml_text = r#"
threshold = 2
bastion = "andrey@94.130.18.162"

[[nodes]]
pid = 0
label = "node-1"
ssh = "azureuser@20.71.184.176"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 1
label = "node-2"
ssh = "azureuser@20.224.243.60"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 2
label = "node-3"
ssh = "azureuser@52.236.130.102"
enclave_url = "https://localhost:9088/v1"
"#;
        let topo: DkgTopology = toml::from_str(toml_text).unwrap();
        assert_eq!(topo.threshold, 2);
        assert_eq!(topo.bastion.as_deref(), Some("andrey@94.130.18.162"));
        assert_eq!(topo.nodes.len(), 3);
        assert_eq!(topo.nodes[2].label, "node-3");
        validate_topology(&topo).unwrap();
    }

    #[test]
    fn topology_rejects_non_contiguous_pids() {
        let toml_text = r#"
threshold = 2

[[nodes]]
pid = 0
label = "n0"
ssh = "u@h0"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 2
label = "n2"
ssh = "u@h2"
enclave_url = "https://localhost:9088/v1"
"#;
        let topo: DkgTopology = toml::from_str(toml_text).unwrap();
        let err = validate_topology(&topo).unwrap_err();
        assert!(err.to_string().contains("contiguous"), "got: {err}");
    }

    #[test]
    fn topology_rejects_threshold_below_2() {
        let toml_text = r#"
threshold = 1

[[nodes]]
pid = 0
label = "n0"
ssh = "u@h0"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 1
label = "n1"
ssh = "u@h1"
enclave_url = "https://localhost:9088/v1"
"#;
        let topo: DkgTopology = toml::from_str(toml_text).unwrap();
        let err = validate_topology(&topo).unwrap_err();
        assert!(err.to_string().contains("threshold"), "got: {err}");
    }

    #[test]
    fn topology_rejects_threshold_above_n() {
        let toml_text = r#"
threshold = 4

[[nodes]]
pid = 0
label = "n0"
ssh = "u@h0"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 1
label = "n1"
ssh = "u@h1"
enclave_url = "https://localhost:9088/v1"

[[nodes]]
pid = 2
label = "n2"
ssh = "u@h2"
enclave_url = "https://localhost:9088/v1"
"#;
        let topo: DkgTopology = toml::from_str(toml_text).unwrap();
        let err = validate_topology(&topo).unwrap_err();
        assert!(err.to_string().contains("threshold"), "got: {err}");
    }

    #[test]
    fn strip_0x_handles_lowercase_uppercase_no_prefix() {
        assert_eq!(strip_0x("0x030002"), "030002");
        assert_eq!(strip_0x("0X030002"), "0X030002"); // case-sensitive — only strict "0x" lowercase prefix is dropped
        assert_eq!(strip_0x("030002"), "030002");
        assert_eq!(strip_0x(""), "");
    }
}
