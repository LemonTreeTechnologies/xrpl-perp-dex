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
//! `HttpEpochDigestSource`/`HttpEpochSealSink`, `LibP2PProjectionSubmitter`,
//! `HttpProjectionConfirmer` — so this module is plumbing, not new logic.
#![allow(dead_code)] // wired by main.rs behind the membership-admin listen flag

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::membership_canonical::SignerEntry;
use crate::membership_coordinator::{run_membership_change, LibP2PMembershipCollector};
use crate::membership_http::{HttpEpochDigestSource, HttpEpochSealSink, HttpProjectionConfirmer};
use crate::membership_projection::{run_projection, ProjectionRequest};
use crate::membership_submit::LibP2PProjectionSubmitter;
use crate::p2p::{MembershipEpochRelay, SigningRelay};
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
    /// Local enclave admin base, e.g. `https://localhost:9089/v1` (epoch-digest).
    pub enclave_base: String,
    /// Every node's enclave admin base — seal-epoch + record-projection-confirmation
    /// are applied on each (the single `(statement, bundle)` (P), then each node's
    /// confirmation).
    pub node_admin_urls: Vec<String>,
    /// Drives `LibP2PMembershipCollector` (the β1 consent bundle).
    pub membership_epoch_tx: mpsc::Sender<MembershipEpochRelay>,
    /// The XRPL multisig signing relay (the β2 projection signatures).
    pub signing_tx: mpsc::Sender<SigningRelay>,
    /// The CURRENT on-chain signer set authorising the projection — each is
    /// (r-address, 20-byte AccountID hex). The outgoing quorum signs the
    /// SignerListSet (sync-before-spend: still on-chain through the window).
    pub current_signers: Vec<(String, String)>,
    pub current_quorum: u32,
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
    if state.node_admin_urls.is_empty() {
        bail!("no node admin URLs configured");
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

    let client = admin_http_client()?;

    // 2. β1: collect the off-chain quorum consent + seal epoch N+1 on every node.
    let digest_src = HttpEpochDigestSource::new(client.clone(), state.enclave_base.clone());
    let collector = LibP2PMembershipCollector::new(state.membership_epoch_tx.clone());
    let seal_sink = HttpEpochSealSink::new(client.clone());
    let change = run_membership_change(
        state.escrow,
        new_signers.clone(),
        req.quorum,
        &digest_src,
        &collector,
        &seal_sink,
        &state.node_admin_urls,
    )
    .await
    .context("β1 membership change (collect consent + seal per node)")?;

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
    };
    let submitter = LibP2PProjectionSubmitter::new(
        state.signing_tx.clone(),
        state.xrpl_url.clone(),
        state.current_signers.clone(),
        state.current_quorum,
    );
    let confirmer = HttpProjectionConfirmer::new(client);
    let proj = run_projection(&proj_req, &submitter, &confirmer, &state.node_admin_urls)
        .await
        .context("β2 projection (render + submit + confirm per node)")?;

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

pub fn router(state: Arc<MembershipAdminState>) -> Router {
    Router::new()
        .route("/admin/membership-change", post(handle_membership_change))
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
