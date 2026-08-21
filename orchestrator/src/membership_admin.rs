//! β3.2b — the membership-change admin trigger.
//!
//! The single operator-facing entrypoint that drives the whole β flow end to
//! end on one POST: collect the off-chain quorum's consent for a new signer set
//! and seal it on every node (β1 `run_membership_change`), then produce, submit
//! and confirm the XRPL `SignerListSet` projection of that sealed epoch (β2
//! `run_projection`). Loopback-only admin listener (mirrors the
//! `signerlist_update` admin).
//!
//! It composes the already-audited pieces — `LibP2PMembershipCollector`,
//! `HttpEpochDigestSource`, `LibP2PMembershipApplier` (the p2p apply-broadcast),
//! `LibP2PProjectionSubmitter` — so this module is plumbing, not new logic.
//!
//! Topology note (X-C1): the enclave admin API is loopback-only, so the seal +
//! confirmation are NOT POSTed to remote node enclaves. They ride the p2p
//! apply-broadcast (`LibP2PMembershipApplier`): the driving node broadcasts ONE
//! apply and every node applies it to its OWN localhost enclave + acks. Only the
//! LOCAL epoch-digest read is a direct (loopback) HTTP GET.
#![allow(dead_code)] // wired by main.rs behind the membership-admin listen flag

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::membership_apply::LibP2PMembershipApplier;
use crate::membership_canonical::SignerEntry;
use crate::membership_coordinator::{
    run_genesis_bootstrap, run_membership_change, LibP2PMembershipCollector,
};
use crate::membership_http::HttpEpochDigestSource;
use crate::membership_projection::{run_projection, ProjectionRequest};
use crate::membership_submit::LibP2PProjectionSubmitter;
use crate::p2p::{MembershipApplyRelay, MembershipEpochRelay, MembershipSignerWire, SigningRelay};
use crate::signerlist_update::fetch_account_sequence;
use crate::xrpl_signer::decode_xrpl_address;

/// SignerListSet projection fee floor (matches the policy validator).
const FEE_DROPS: u64 = 12_000;

pub struct MembershipAdminState {
    pub xrpl_url: String,
    /// Escrow as raw 20-byte AccountID (the authority record's key).
    pub escrow: [u8; 20],
    /// Escrow r-address (for the XRPL sequence fetch + the projection Account).
    pub escrow_r_address: String,
    /// Local enclave admin base, e.g. `https://localhost:9089` (epoch-digest GET
    /// is the only direct enclave call — loopback).
    pub enclave_base: String,
    /// Cluster roster size — the number of nodes expected to ack each apply
    /// broadcast (seal, then confirmation). A shortfall is reported so the
    /// operator retries; it is NOT a list of reachable URLs (the enclave admin
    /// API is loopback-only, X-C1 — apply rides the p2p broadcast, not HTTP).
    pub cluster_size: usize,
    /// Drives `LibP2PMembershipCollector` (the β1 consent bundle).
    pub membership_epoch_tx: mpsc::Sender<MembershipEpochRelay>,
    /// Drives `LibP2PMembershipApplier` (the β1 seal + β2 confirm apply-broadcast).
    pub membership_apply_tx: mpsc::Sender<MembershipApplyRelay>,
    /// The XRPL multisig signing relay (the β2 projection signatures).
    pub signing_tx: mpsc::Sender<SigningRelay>,
    /// The CURRENT on-chain signer set authorising the projection — each is
    /// (r-address, 20-byte AccountID hex). The outgoing quorum signs the
    /// SignerListSet (sync-before-spend: still on-chain through the window).
    pub current_signers: Vec<(String, String)>,
    pub current_quorum: u32,
    /// β4 Thread B: drives `LibP2PGovernanceBundleCollector` (the governance +
    /// reproducible-build bundles for a trusted-MRENCLAVE allowlist op).
    pub mrenclave_governance_tx: mpsc::Sender<crate::p2p::MrenclaveGovernanceRelay>,
    /// #131 AC-BASE: drives `LibP2PReservesBaselineCollector` (the one-time
    /// custody-baseline 2-of-3 ceremony).
    pub reserves_baseline_tx: mpsc::Sender<crate::p2p::ReservesBaselineRelay>,
}

#[derive(Debug, Deserialize)]
pub struct MembershipChangeRequest {
    /// The full proposed new signer set (r-addresses). Equal-weight (1 each),
    /// matching the cluster convention + the projection policy gate.
    pub new_signers: Vec<String>,
    /// The new SignerQuorum.
    pub quorum: u32,
}

#[derive(Debug, Serialize)]
pub struct MembershipChangeResponse {
    pub status: String,
    pub proposed_epoch: u64,
    pub message_hash_hex: String,
    pub sealed_nodes: usize,
    pub projection_tx_hash_hex: Option<String>,
    pub projection_ledger_index: Option<u64>,
    pub confirmed_nodes: usize,
    pub message: String,
}

/// Build one reqwest client that accepts the enclave's self-signed TLS (the
/// admin routes are loopback/self-signed; the reverse proxy fronts public TLS).
fn admin_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(30))
        .build()
        .context("build membership-admin http client")
}

async fn drive_change(
    state: &MembershipAdminState,
    req: MembershipChangeRequest,
) -> Result<MembershipChangeResponse> {
    if state.cluster_size == 0 {
        bail!("cluster_size is zero — no nodes to apply the membership change to");
    }
    // 1. Decode the proposed r-addresses → the sealed-form signer set.
    let mut new_signers: Vec<SignerEntry> = Vec::with_capacity(req.new_signers.len());
    for addr in &req.new_signers {
        let account_id =
            decode_xrpl_address(addr).with_context(|| format!("invalid r-address {addr}"))?;
        new_signers.push(SignerEntry {
            account_id,
            weight: 1,
        });
    }

    // The only direct enclave call is the LOCAL epoch-digest GET (loopback);
    // seal + confirm ride the p2p apply-broadcast.
    let client = admin_http_client()?;

    // 2. β1: collect the off-chain quorum consent, then apply the SAME sealed
    //    `(statement, bundle)` to every node via the p2p apply-broadcast (the
    //    enclave admin API is loopback-only, X-C1 — each node seals locally).
    let digest_src = HttpEpochDigestSource::new(client, state.enclave_base.clone());
    let collector = LibP2PMembershipCollector::new(state.membership_epoch_tx.clone());
    // D-2 (REQ-β3.2c): hand the applier this node's OUTGOING (M-1) set — its own
    // current on-chain signer set — so the `Seal` payload carries it and every
    // node retains the complete `(authority, attesting, bundle)` tuple for later
    // serving a joining newcomer. Equal-weight (1 each), the cluster convention.
    let attesting_signers: Vec<MembershipSignerWire> = state
        .current_signers
        .iter()
        .map(|(_r_addr, account_id_hex)| MembershipSignerWire {
            account_id_hex: account_id_hex.clone(),
            weight: 1,
        })
        .collect();
    let applier =
        LibP2PMembershipApplier::new(state.membership_apply_tx.clone(), state.cluster_size)
            .with_attesting(attesting_signers, state.current_quorum);
    let change = run_membership_change(
        state.escrow,
        new_signers.clone(),
        req.quorum,
        &digest_src,
        &collector,
        &applier,
    )
    .await
    .context("β1 membership change (collect consent + seal across the cluster)")?;

    let sealed_nodes = change.node_results.iter().filter(|r| r.ok).count();
    if !change.all_sealed() {
        return Ok(MembershipChangeResponse {
            status: "partial_seal".into(),
            proposed_epoch: change.proposed_epoch,
            message_hash_hex: hex::encode(change.message_hash),
            sealed_nodes,
            projection_tx_hash_hex: None,
            projection_ledger_index: None,
            confirmed_nodes: 0,
            message: "epoch sealed on a subset of nodes; projection NOT attempted — \
                      retry the failed nodes (idempotent) before projecting"
                .into(),
        });
    }

    // 3. β2: produce + submit + confirm the XRPL SignerListSet projection.
    let sequence = fetch_account_sequence(&state.xrpl_url, &state.escrow_r_address)
        .await
        .context("fetch escrow sequence for the projection")?;
    let proj_req = ProjectionRequest {
        escrow: state.escrow,
        sequence,
        fee_drops: FEE_DROPS,
        signers: new_signers,
        quorum: req.quorum,
        // β4 Thread A (AC-β4-A1): forward the SAME β1 bundle that just authorised
        // this epoch — each signer's enclave requires it to cosign the projection.
        quorum_bundle_hex: change.quorum_bundle_hex.clone(),
    };
    let submitter = LibP2PProjectionSubmitter::new(
        state.signing_tx.clone(),
        state.xrpl_url.clone(),
        state.current_signers.clone(),
        state.current_quorum,
    );
    let proj = run_projection(&proj_req, &submitter, &applier)
        .await
        .context("β2 projection (render + submit + confirm across the cluster)")?;

    let confirmed_nodes = proj.node_results.iter().filter(|r| r.ok).count();
    let status = if proj.all_recorded() {
        "ok"
    } else {
        "projection_partial_record"
    };
    Ok(MembershipChangeResponse {
        status: status.into(),
        proposed_epoch: change.proposed_epoch,
        message_hash_hex: hex::encode(change.message_hash),
        sealed_nodes,
        projection_tx_hash_hex: Some(hex::encode(proj.tx_hash)),
        projection_ledger_index: Some(proj.ledger_index),
        confirmed_nodes,
        message: if proj.all_recorded() {
            "membership changed + projected + confirmed on all nodes".into()
        } else {
            "projection landed but confirmation not recorded on all nodes — \
             retry the failed nodes (ERR_ALREADY_CONFIRMED is idempotent)"
                .into()
        },
    })
}

async fn handle_membership_change(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<MembershipChangeRequest>,
) -> impl IntoResponse {
    info!(
        new_size = req.new_signers.len(),
        quorum = req.quorum,
        "β membership-change requested"
    );
    match drive_change(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            warn!(error = %e, "β membership-change failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

// ── β4 Thread A — genesis bootstrap trigger (seal the founding epoch 1) ──────
//
// β3.2 live-wiring for the genesis case (#122): `drive_change` above is
// transitions-only (it reads the CURRENT epoch digest and requires an already
// sealed epoch). A fresh cluster has no epoch 0 to transition from, so genesis
// takes the self-authorising path `run_genesis_bootstrap` →
// `ecall_bootstrap_from_quorum_attestation`: the founding members attest their
// own founding epoch (attesting set == authority set). Composes the SAME
// already-audited pieces (`LibP2PMembershipCollector`, `LibP2PMembershipApplier`
// via `apply_genesis`) — plumbing, not new logic. No XRPL projection: the
// initial `SignerListSet` is already on-chain (escrow-init, master-signed); this
// only seals epoch 1 as each enclave's version=1 baseline so the cluster can
// cosign thereafter.
async fn drive_genesis(
    state: &MembershipAdminState,
    req: MembershipChangeRequest,
) -> Result<MembershipChangeResponse> {
    if state.cluster_size == 0 {
        bail!("cluster_size is zero — no nodes to bootstrap");
    }
    // Decode the founding r-addresses → the sealed-form signer set (equal weight).
    let mut genesis_signers: Vec<SignerEntry> = Vec::with_capacity(req.new_signers.len());
    for addr in &req.new_signers {
        let account_id =
            decode_xrpl_address(addr).with_context(|| format!("invalid r-address {addr}"))?;
        genesis_signers.push(SignerEntry {
            account_id,
            weight: 1,
        });
    }

    // Founding-quorum consent (collector) → seal epoch 1 on every node via the
    // p2p apply-broadcast (applier.apply_genesis). Same X-C1 topology as a
    // transition: each node seals its OWN loopback enclave and acks.
    let collector = LibP2PMembershipCollector::new(state.membership_epoch_tx.clone());
    let applier =
        LibP2PMembershipApplier::new(state.membership_apply_tx.clone(), state.cluster_size);
    let outcome = run_genesis_bootstrap(
        state.escrow,
        genesis_signers,
        req.quorum,
        &collector,
        &applier,
    )
    .await
    .context("β4 genesis bootstrap (collect founding consent + seal epoch 1 across the cluster)")?;

    let sealed_nodes = outcome.node_results.iter().filter(|r| r.ok).count();
    Ok(MembershipChangeResponse {
        status: if outcome.all_sealed() {
            "ok".into()
        } else {
            "partial_seal".into()
        },
        proposed_epoch: outcome.proposed_epoch,
        message_hash_hex: hex::encode(outcome.message_hash),
        sealed_nodes,
        // Genesis performs NO projection: the SignerListSet is already on-chain.
        projection_tx_hash_hex: None,
        projection_ledger_index: None,
        confirmed_nodes: 0,
        message: if outcome.all_sealed() {
            format!(
                "genesis epoch {} sealed on all {sealed_nodes} nodes; SignerListSet already \
                 on-chain (escrow-init) — cluster can now cosign",
                outcome.proposed_epoch
            )
        } else {
            "genesis sealed on a subset of nodes — retry the failed nodes \
             (bootstrap is idempotent under the (P) single-successor guard)"
                .into()
        },
    })
}

async fn handle_membership_genesis(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<MembershipChangeRequest>,
) -> impl IntoResponse {
    info!(
        size = req.new_signers.len(),
        quorum = req.quorum,
        "β4 genesis bootstrap requested"
    );
    match drive_genesis(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            warn!(error = %e, "β4 genesis bootstrap failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

// ── β4 Thread B — trusted-MRENCLAVE allowlist governance trigger ────────────

#[derive(Debug, Deserialize)]
pub struct MrenclaveGovernRequest {
    /// "add" or "remove".
    pub op: String,
    /// 64-char hex of the 32-byte target measurement.
    pub mrenclave: String,
}

#[derive(Debug, Serialize)]
pub struct MrenclaveGovernResponse {
    pub status: String,
    pub allowlist_epoch: u64,
    pub repro_signers: usize,
    pub applied_nodes: usize,
    pub message: String,
}

async fn drive_govern(
    state: &MembershipAdminState,
    req: MrenclaveGovernRequest,
) -> Result<MrenclaveGovernResponse> {
    use crate::mrenclave_governance::{
        run_mrenclave_governance, LibP2PGovernanceBundleCollector, OP_ADD, OP_REMOVE,
    };

    let op = match req.op.as_str() {
        "add" => OP_ADD,
        "remove" => OP_REMOVE,
        other => bail!("unknown op {other:?} (expected \"add\" or \"remove\")"),
    };
    let mrenclave_v = hex::decode(&req.mrenclave).context("mrenclave not hex")?;
    if mrenclave_v.len() != 32 {
        bail!("mrenclave must be 32 bytes, got {}", mrenclave_v.len());
    }
    let mut mrenclave = [0u8; 32];
    mrenclave.copy_from_slice(&mrenclave_v);

    // The allowlist head is read from the LOCAL enclave; the collector gathers
    // the operator quorum's signatures over the p2p relay; the applier broadcasts
    // the ONE resulting operation to every node's loopback enclave.
    let client = admin_http_client()?;
    let status_src =
        crate::membership_http::HttpAllowlistStatusSource::new(client, state.enclave_base.clone());
    let collector = LibP2PGovernanceBundleCollector::new(state.mrenclave_governance_tx.clone());
    let applier =
        LibP2PMembershipApplier::new(state.membership_apply_tx.clone(), state.cluster_size);

    let outcome = run_mrenclave_governance(
        op,
        mrenclave,
        state.escrow,
        &status_src,
        &collector,
        &applier,
    )
    .await?;

    let applied = outcome.node_results.iter().filter(|r| r.ok).count();
    Ok(MrenclaveGovernResponse {
        status: if outcome.all_applied() {
            "ok"
        } else {
            "partial_apply"
        }
        .into(),
        allowlist_epoch: outcome.allowlist_epoch,
        repro_signers: outcome.repro_signers,
        applied_nodes: applied,
        message: if outcome.all_applied() {
            format!("{} applied on all {} nodes", req.op, applied)
        } else {
            format!(
                "{} applied on {applied}/{} nodes; retry (enclave ops are idempotent)",
                req.op, state.cluster_size
            )
        },
    })
}

async fn handle_govern(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<MrenclaveGovernRequest>,
) -> impl IntoResponse {
    info!(op = %req.op, mrenclave = %req.mrenclave, "β4 mrenclave-governance requested");
    match drive_govern(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            warn!(error = %e, "β4 mrenclave-governance failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

// ── #131 AC-BASE — one-time custody-baseline ceremony trigger ──

#[derive(Debug, Deserialize)]
pub struct ReservesBaselineRequest {
    /// RLUSD issuer classic r-address (pinned in the baseline hash).
    pub rlusd_issuer: String,
    /// Quorum required (2 on the 3-node cluster).
    pub quorum: usize,
    /// The ceremony roster: each participating node's baseline signing pubkey +
    /// its OWN (distinct) XRPL endpoint. C-Q1.1 pre-flight refuses unless the
    /// endpoints are pairwise-distinct; the driver maps accepted bundle pubkeys
    /// back to these endpoints to assert >= quorum distinct observation sources.
    pub nodes: Vec<BaselineNodeReq>,
}

#[derive(Debug, Deserialize)]
pub struct BaselineNodeReq {
    /// 33-byte compressed secp256k1 baseline pubkey, lowercase hex.
    pub pubkey: String,
    /// This node's XRPL endpoint.
    pub endpoint: String,
}

#[derive(Debug, Serialize)]
pub struct ReservesBaselineResponse {
    pub status: String,
    pub message: String,
    pub enclave: serde_json::Value,
}

async fn drive_reserves_baseline(
    state: &MembershipAdminState,
    req: ReservesBaselineRequest,
) -> Result<ReservesBaselineResponse> {
    use crate::reserves_baseline::{
        run_reserves_baseline_ceremony, BaselineNode, LibP2PReservesBaselineCollector,
    };
    if req.quorum == 0 {
        bail!("quorum must be >= 1");
    }
    let roster: Vec<BaselineNode> = req
        .nodes
        .iter()
        .map(|n| BaselineNode {
            compressed_pubkey_hex: n.pubkey.trim_start_matches("0x").to_lowercase(),
            xrpl_endpoint: n.endpoint.clone(),
        })
        .collect();
    let collector = LibP2PReservesBaselineCollector::new(state.reserves_baseline_tx.clone());
    // The apply targets the LOCAL sequencer enclave (loopback). `enclave_base` had
    // `/v1` stripped for the membership admin GETs; re-add it for the PerpClient base.
    let enclave_v1 = format!("{}/v1", state.enclave_base.trim_end_matches('/'));
    let host_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let res = run_reserves_baseline_ceremony(
        &collector,
        &enclave_v1,
        &state.xrpl_url,
        &state.escrow_r_address,
        &req.rlusd_issuer,
        &roster,
        req.quorum,
        host_ts,
    )
    .await?;
    Ok(ReservesBaselineResponse {
        status: "ok".into(),
        message: format!(
            "baseline applied from {} independent operator observation(s) at the pinned ledger",
            req.nodes.len()
        ),
        enclave: res,
    })
}

async fn handle_reserves_baseline(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<ReservesBaselineRequest>,
) -> impl IntoResponse {
    info!(
        issuer = %req.rlusd_issuer,
        quorum = req.quorum,
        nodes = req.nodes.len(),
        "#131 AC-BASE reserves-baseline ceremony requested"
    );
    match drive_reserves_baseline(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            warn!(error = %e, "#131 reserves-baseline ceremony failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

pub fn router(state: Arc<MembershipAdminState>) -> Router {
    Router::new()
        .route("/admin/membership-change", post(handle_membership_change))
        .route("/admin/membership-genesis", post(handle_membership_genesis))
        .route("/admin/mrenclave-govern", post(handle_govern))
        .route("/admin/reserves-baseline", post(handle_reserves_baseline))
        .with_state(state)
}

pub async fn spawn_admin_listener(
    listen_addr: String,
    state: Arc<MembershipAdminState>,
) -> Result<()> {
    let parsed: std::net::SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid --membership-admin-listen address {listen_addr:?}"))?;
    if !parsed.ip().is_loopback() {
        bail!(
            "--membership-admin-listen must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }
    let listener = tokio::net::TcpListener::bind(parsed)
        .await
        .with_context(|| format!("membership-admin bind on {parsed} failed"))?;
    info!(listen = %parsed, "β membership-change admin listener started");
    axum::serve(listener, router(state))
        .await
        .context("membership-admin serve error")?;
    Ok(())
}
