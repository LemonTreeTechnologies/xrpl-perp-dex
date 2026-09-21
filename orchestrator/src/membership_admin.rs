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

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::membership_apply::LibP2PMembershipApplier;
use crate::membership_canonical::SignerEntry;
use crate::membership_coordinator::{
    run_genesis_bootstrap, run_membership_change, LibP2PMembershipCollector, NodeSealResult,
};
use crate::membership_http::HttpEpochDigestSource;
use crate::membership_projection::{run_projection, NodeConfirmResult, ProjectionRequest};
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
    /// #131 AC-BASE-2″ P2-c: drives `LibP2PSpvBaselineCollector` (the SPV backing-gate
    /// ceremony — broadcast one proof, collect 2-of-N SPV cosignatures, apply).
    pub spv_baseline_tx: mpsc::Sender<crate::p2p::SpvBaselineRelay>,
    /// #131 §6: drives `LibP2PUnlPolicyCollector` (quorum fraction + freshness anchor).
    pub unl_policy_tx: mpsc::Sender<crate::p2p::UnlPolicyRelay>,
    /// #131 sweep Finding 2: drives `LibP2PUnlStatusCollector` — the read-only
    /// cross-node status query. Separate channel from the ceremony above because
    /// it proposes nothing and applies nothing.
    pub unl_status_tx: mpsc::Sender<crate::p2p::UnlStatusRelay>,
    /// #131 AC-BASE (a): the operator-capital excluded senders as 20-byte XRPL AccountID
    /// hex (decoded from OPERATOR_CAPITAL_SENDERS — the SAME config the scanner uses),
    /// committed into the baseline marker's excluded_senders_hash.
    pub operator_capital_account_ids: Vec<String>,
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
    /// WHICH nodes did not apply, and why. Empty on full success.
    pub failed_nodes: Vec<FailedNode>,
    pub message: String,
}

/// Operator-facing record of a node that did NOT apply.
///
/// Every partial-outcome message here tells the operator to "retry the failed
/// nodes". Reporting only a COUNT makes that instruction impossible to follow
/// from the response: the operator is told 2-of-3 and has to go find the third
/// by hand. `NodeSealResult`/`NodeConfirmResult` carry the node and the error
/// all the way to this boundary, where they were dropped -- the same defect as
/// echoing the request instead of the outcome, on the paths that matter most,
/// because a partial apply leaves the cluster's membership split.
#[derive(Debug, Serialize)]
pub struct FailedNode {
    pub node: String,
    pub error: Option<String>,
}

fn failed_seal_nodes(results: &[NodeSealResult]) -> Vec<FailedNode> {
    results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| FailedNode {
            node: r.node.clone(),
            error: r.error.clone(),
        })
        .collect()
}

fn failed_confirm_nodes(results: &[NodeConfirmResult]) -> Vec<FailedNode> {
    results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| FailedNode {
            node: r.node.clone(),
            error: r.error.clone(),
        })
        .collect()
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
            failed_nodes: failed_seal_nodes(&change.node_results),
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
        failed_nodes: failed_confirm_nodes(&proj.node_results),
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
        failed_nodes: failed_seal_nodes(&outcome.node_results),
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
    /// The measurement the cluster ACTUALLY governed, echoed from the outcome rather than
    /// from the request. Reporting the request back confirms what was asked for, not what
    /// happened — an operator who mistyped a measurement would read an identical response.
    /// That is the same shape that let a 1-of-1 Safe look correct for a month.
    pub mrenclave: String,
    pub allowlist_epoch: u64,
    pub repro_signers: usize,
    pub applied_nodes: usize,
    /// WHICH nodes did not apply, and why. Empty on full success.
    pub failed_nodes: Vec<FailedNode>,
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
        mrenclave: format!("0x{}", hex::encode(outcome.mrenclave)),
        allowlist_epoch: outcome.allowlist_epoch,
        repro_signers: outcome.repro_signers,
        applied_nodes: applied,
        failed_nodes: failed_seal_nodes(&outcome.node_results),
        // `outcome.op`, not `req.op`: the message must describe what the cluster did.
        message: if outcome.all_applied() {
            format!(
                "op {} on 0x{} applied on all {} nodes",
                outcome.op,
                hex::encode(outcome.mrenclave),
                applied
            )
        } else {
            format!(
                "op {} on 0x{} applied on {applied}/{} nodes; retry (enclave ops are idempotent)",
                outcome.op,
                hex::encode(outcome.mrenclave),
                state.cluster_size
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

// ── #131 AC-BASE — baseline ceremony roster ──
//
// The pre-SPV `ReservesBaselineRequest`/`ReservesBaselineResponse` pair lived here
// with no route, no handler and no constructor: the custody baseline is driven by
// `SpvBaselineRequest` below. Removed rather than kept, because their doc comments
// described the C-Q1.1 pre-flight as if this were the path that runs it. It is not
// — `reserves_baseline::{enforce_distinct_endpoints, assert_distinct_sources}` run
// it on the SPV ceremony, which is why deleting this loses no check.

#[derive(Debug, Deserialize)]
pub struct BaselineNodeReq {
    /// 33-byte compressed secp256k1 baseline pubkey, lowercase hex.
    pub pubkey: String,
    /// This node's XRPL endpoint.
    pub endpoint: String,
}

// ── #131 AC-BASE-2″ P2-c — SPV backing-gate ceremony trigger ──

#[derive(Debug, Deserialize)]
pub struct SpvBaselineRequest {
    /// Patched-node JSON-RPC endpoint that produces the SHAMap proof (UNTRUSTED — the
    /// enclave verifies the proof against the validator-signed account_hash).
    pub proof_http_url: String,
    /// Patched-node ws endpoint the leader subscribes to for validations.
    pub proof_ws_url: String,
    /// Escrow classic r-address the proof is built for (the SPV target). The enclave
    /// pins its OWN escrow from the sealed SignerList, so a wrong value here just fails
    /// the SPV inclusion check — it is not a trust surface.
    pub escrow: String,
    /// Minimum full validations to collect for ONE converging ledger before building.
    pub validations_quorum: usize,
    /// Cosignature quorum required (2 on the 3-node cluster).
    pub cosign_quorum: usize,
    /// ws collect window (seconds); default 45.
    #[serde(default)]
    pub collect_secs: Option<u64>,
    /// The ceremony roster — each cosigning node's baseline signing pubkey + an identifier
    /// endpoint (mapped back for the ≥quorum distinct-cosigner assertion).
    pub nodes: Vec<BaselineNodeReq>,
}

#[derive(Debug, Serialize)]
pub struct SpvBaselineResponse {
    pub status: String,
    pub message: String,
    pub enclave: serde_json::Value,
}

async fn drive_reserves_spv_baseline(
    state: &MembershipAdminState,
    req: SpvBaselineRequest,
) -> Result<SpvBaselineResponse> {
    use crate::reserves_baseline::{
        run_reserves_spv_baseline_ceremony, BaselineNode, LibP2PSpvBaselineCollector,
    };
    if req.cosign_quorum == 0 {
        bail!("cosign_quorum must be >= 1");
    }
    if req.validations_quorum == 0 {
        bail!("validations_quorum must be >= 1");
    }
    let roster: Vec<BaselineNode> = req
        .nodes
        .iter()
        .map(|n| BaselineNode {
            compressed_pubkey_hex: n.pubkey.trim_start_matches("0x").to_lowercase(),
            xrpl_endpoint: n.endpoint.clone(),
        })
        .collect();
    let fetch_cfg = crate::spv_proof::SpvFetchConfig {
        http_url: req.proof_http_url.clone(),
        ws_url: req.proof_ws_url.clone(),
        escrow_account: req.escrow.clone(),
        quorum: req.validations_quorum,
        collect_secs: req.collect_secs.unwrap_or(45),
    };
    let collector = LibP2PSpvBaselineCollector::new(state.spv_baseline_tx.clone());
    let enclave_v1 = format!("{}/v1", state.enclave_base.trim_end_matches('/'));
    let host_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let res = run_reserves_spv_baseline_ceremony(
        &collector,
        &enclave_v1,
        &fetch_cfg,
        &roster,
        req.cosign_quorum,
        host_ts,
        &state.operator_capital_account_ids,
    )
    .await?;
    Ok(SpvBaselineResponse {
        status: "ok".into(),
        message: "SPV backing baseline applied — custody seeded from an SPV-proven escrow balance"
            .into(),
        enclave: res,
    })
}

async fn handle_reserves_spv_baseline(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<SpvBaselineRequest>,
) -> impl IntoResponse {
    info!(
        escrow = %req.escrow,
        cosign_quorum = req.cosign_quorum,
        nodes = req.nodes.len(),
        "#131 AC-BASE-2\u{2033} P2-c SPV baseline ceremony requested"
    );
    match drive_reserves_spv_baseline(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            warn!(error = %e, "#131 SPV baseline ceremony failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

// ── #131 AC-BASE-2″ §6 — refresh the enclave's DERIVED validator set ──
// Permissionless in the enclave (a forged manifest cannot pass ed25519 against the measured
// master anchor), so this trigger carries no bundle and no signatures: it just moves public
// bytes from the validator list into the enclave, which then verifies them itself.

#[derive(Debug, Deserialize)]
pub struct UnlRefreshRequest {
    /// Validator-list URL. UNTRUSTED transport: we neither verify its publisher signature
    /// nor need several publishers (C-UNL-4 is moot under the measured anchor). A hostile
    /// one can only withhold, which fails closed.
    pub vl_url: String,
}

#[derive(Debug, Serialize)]
pub struct UnlRefreshResponse {
    pub status: String,
    pub submitted: usize,
    /// Entries that actually changed the derived state. 0 is a normal outcome — it means
    /// no validator rotated since the last refresh, and the enclave sealed nothing.
    pub changed: i64,
    pub message: String,
}

async fn handle_unl_refresh(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<UnlRefreshRequest>,
) -> impl IntoResponse {
    info!(vl_url = %req.vl_url, "#131 §6 validator-manifest refresh requested");
    match crate::validator_manifests::refresh_validator_set(&req.vl_url, &state.enclave_base).await
    {
        Ok((submitted, changed)) => (
            StatusCode::OK,
            Json(UnlRefreshResponse {
                status: "ok".into(),
                submitted,
                changed,
                message: format!(
                    "{submitted} manifest(s) submitted; {changed} changed the enclave-derived \
                     validator set (each verified in-enclave against the measured master anchor)"
                ),
            }),
        )
            .into_response(),
        Err(e) => {
            warn!(error = %e, "#131 §6 validator-manifest refresh failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UnlPolicyRequest {
    /// The freshness anchor to pin. Each cosigner refuses unless this is at or below ITS
    /// OWN validated ledger and within the accept window (Q-UNL-4) — a future ledger is
    /// always refused.
    pub pinned_ledger_seq: u64,
    /// SPV quorum fraction; must be >= 80% (enforced in-enclave, checked here early too).
    pub quorum_num: u32,
    pub quorum_den: u32,
    /// Endorsements required (2 on the 3-node cluster).
    pub cosign_quorum: usize,
}

#[derive(Debug, Serialize)]
pub struct UnlPolicyResponse {
    pub status: String,
    pub message: String,
    pub enclave: serde_json::Value,
}

/// #131 sweep Finding 2 — read-only: ask every node what §6 record it holds and
/// report whether they agree.
///
/// Deliberately NOT a gate. Q-FORK-3 ruled the existing split accept-and-document,
/// and halting a cluster we have decided to run forked would be the wrong trade.
/// What was missing is that the accepted anomaly lived only in a ruling document:
/// an operator looking at the cluster could not see it, so the next person to hit
/// it would rediscover it by investigation — which is exactly how the 1-of-1 Safe
/// was found.
async fn handle_unl_cluster_status(
    State(state): State<Arc<MembershipAdminState>>,
) -> impl IntoResponse {
    info!("#131 unl-status: asking every node which §6 record it holds");
    let collector = crate::unl_policy::LibP2PUnlStatusCollector::new(
        state.unl_status_tx.clone(),
        state.cluster_size,
    );
    match collector.collect().await {
        Ok(status) => {
            if !status.in_sync {
                warn!(
                    groups = status.groups.len(),
                    unreadable = status.unreadable,
                    "#131 unl-status: cluster does NOT agree on its UNL record"
                );
            }
            let expected = state.cluster_size;
            let message = if status.responded < expected {
                format!(
                    "{}/{} nodes answered — a node that did not answer is neither agreement \
                     nor disagreement, and is absent from the groups below",
                    status.responded, expected
                )
            } else if status.in_sync {
                format!("all {expected} nodes hold the same UNL record")
            } else {
                format!(
                    "cluster is SPLIT across {} distinct UNL records ({} node(s) could not \
                     read their own) — an indicator, not a gate: see Q-FORK-3",
                    status.groups.len(),
                    status.unreadable
                )
            };
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "ok",
                    "expected_nodes": expected,
                    "cluster": status,
                    "message": message,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": "error", "message": format!("{e:#}")})),
        )
            .into_response(),
    }
}

async fn handle_unl_policy(
    State(state): State<Arc<MembershipAdminState>>,
    Json(req): Json<UnlPolicyRequest>,
) -> impl IntoResponse {
    info!(
        pinned_ledger_seq = req.pinned_ledger_seq,
        quorum = format!("{}/{}", req.quorum_num, req.quorum_den),
        "#131 §6 UNL policy ceremony requested"
    );
    let collector = crate::unl_policy::LibP2PUnlPolicyCollector::new(state.unl_policy_tx.clone());
    // #131 §6: the sealed policy record must land on EVERY node, so the apply rides the
    // same audited broadcast the membership seal uses. `cluster_size` is what "every node"
    // means here — a shortfall is reported, not hidden.
    let applier =
        LibP2PMembershipApplier::new(state.membership_apply_tx.clone(), state.cluster_size);
    match crate::unl_policy::run_unl_policy_ceremony(
        &collector,
        &state.enclave_base,
        &hex::encode(state.escrow),
        req.pinned_ledger_seq,
        req.quorum_num,
        req.quorum_den,
        req.cosign_quorum,
        &applier,
    )
    .await
    {
        Ok(res) => (
            StatusCode::OK,
            Json(UnlPolicyResponse {
                status: "ok".into(),
                message: "UNL policy updated (quorum fraction + freshness anchor)".into(),
                enclave: res,
            }),
        )
            .into_response(),
        Err(e) => {
            warn!(error = %e, "#131 §6 UNL policy ceremony failed");
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
        .route(
            "/admin/reserves-spv-baseline",
            post(handle_reserves_spv_baseline),
        )
        .route("/admin/unl-refresh", post(handle_unl_refresh))
        .route("/admin/unl-policy", post(handle_unl_policy))
        .route("/admin/unl-cluster-status", get(handle_unl_cluster_status))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(node: &str, ok: bool, err: Option<&str>) -> NodeSealResult {
        NodeSealResult {
            node: node.into(),
            ok,
            error: err.map(Into::into),
        }
    }

    /// The partial-outcome messages tell the operator to "retry the failed nodes".
    /// This is the assertion that the response can actually answer *which*.
    #[test]
    fn a_partial_apply_names_the_node_that_failed_and_why() {
        let results = [
            seal("node-1", true, None),
            seal("node-2", false, Some("ERR_SEAL_REFUSED")),
            seal("node-3", true, None),
        ];
        let failed = failed_seal_nodes(&results);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].node, "node-2");
        assert_eq!(failed[0].error.as_deref(), Some("ERR_SEAL_REFUSED"));
    }

    #[test]
    fn a_full_success_names_nobody() {
        let results = [seal("node-1", true, None), seal("node-2", true, None)];
        assert!(failed_seal_nodes(&results).is_empty());
    }

    /// A node can fail without reporting a reason; it must still be NAMED, because
    /// the name is the part the operator needs to retry.
    #[test]
    fn a_failure_with_no_error_string_is_still_named() {
        let failed = failed_confirm_nodes(&[NodeConfirmResult {
            node: "node-3".into(),
            ok: false,
            error: None,
        }]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].node, "node-3");
        assert!(failed[0].error.is_none());
    }

    /// The defect was at the serialization boundary, not in the helper: the fields
    /// were populated all the way here and dropped on the way out. Pin the wire shape.
    #[test]
    fn the_failed_nodes_reach_the_operators_json() {
        let resp = MrenclaveGovernResponse {
            status: "partial_apply".into(),
            mrenclave: "0xab".into(),
            allowlist_epoch: 4,
            repro_signers: 2,
            applied_nodes: 2,
            failed_nodes: failed_seal_nodes(&[seal("node-2", false, Some("boom"))]),
            message: "retry the failed nodes".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["failed_nodes"][0]["node"], "node-2");
        assert_eq!(v["failed_nodes"][0]["error"], "boom");
    }
}
