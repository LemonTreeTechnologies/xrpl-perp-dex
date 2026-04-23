//! Path A re-DKG share-v2 export driver + local admin HTTP listener.
//!
//! Invoked manually during a re-DKG ceremony. The operator curls the
//! admin endpoint once per round, passing the local `signer_id`, the
//! `group_id`, and the list of target peer ECDH pubkeys. For each
//! target we call `POST /v1/pool/frost/share-export-v2` on the local
//! enclave, then publish the sealed envelope over the
//! `perp-dex/path-a/share-v2` gossipsub topic for the recipient to
//! import via `POST /v1/pool/frost/share-import-v2`.
//!
//! The listener binds to `127.0.0.1` only and is gated by
//! `--admin-listen` (defaults to off). It is not reachable from the
//! public API surface; peer attestation + AEAD bind security to the
//! enclave pair.
//!
//! Preconditions (enforced by the enclave, not here): each target must
//! already have a verified peer quote in the local attest cache, and we
//! must be in the sender's attest cache on the recipient side. Both are
//! handled by the periodic announcer + inbound verifier wired in 6a.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::p2p::ShareEnvelopeV2Message;
use crate::pool_path_a_client::PoolPathAClient;
use crate::shard_router::PathAGroup;

/// Shared state handed to the admin route.
pub struct AdminState {
    pub client: PoolPathAClient,
    pub share_v2_pub_tx: mpsc::Sender<ShareEnvelopeV2Message>,
    pub groups: Vec<PathAGroup>,
}

#[derive(Debug, Deserialize)]
pub struct ShareExportRequest {
    pub shard_id: u32,
    pub group_id: String,
    pub signer_id: u32,
    /// 33-byte compressed ECDH pubkeys (hex, no `0x`), one per recipient.
    pub targets: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ShareExportResponse {
    pub published: usize,
    pub refused: usize,
    pub errored: usize,
    pub errors: Vec<String>,
}

/// Export one v2 share per target and publish each envelope on the
/// share-v2 gossipsub topic. Returns per-target outcome counts.
///
/// A target is "refused" if the enclave returns 403 (peer not in attest
/// cache); "errored" on transport / parse / channel failures. We keep
/// going on failure so a partial export is still useful.
pub async fn export_shares(
    client: &PoolPathAClient,
    pub_tx: &mpsc::Sender<ShareEnvelopeV2Message>,
    shard_id: u32,
    group_id_hex: &str,
    signer_id: u32,
    targets: &[String],
) -> ShareExportResponse {
    let mut published = 0usize;
    let mut refused = 0usize;
    let mut errored = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for peer_pubkey in targets {
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let envelope = match client
            .frost_share_export_v2(signer_id, peer_pubkey, shard_id, group_id_hex, now_ts)
            .await
        {
            Ok(Some(env)) => env,
            Ok(None) => {
                warn!(%peer_pubkey, "share-export refused (peer not in attest cache)");
                refused += 1;
                continue;
            }
            Err(e) => {
                warn!(%peer_pubkey, "share-export error: {}", e);
                errored += 1;
                errors.push(format!("{peer_pubkey}: {e}"));
                continue;
            }
        };

        let msg = ShareEnvelopeV2Message::Deliver {
            recipient_pubkey: peer_pubkey.to_lowercase(),
            shard_id,
            group_id: group_id_hex.to_lowercase(),
            signer_id,
            envelope,
        };
        if let Err(e) = pub_tx.send(msg).await {
            warn!(%peer_pubkey, "share-v2 publish channel closed: {}", e);
            errored += 1;
            errors.push(format!("{peer_pubkey}: publish channel closed"));
            continue;
        }

        info!(%peer_pubkey, shard_id, signer_id, "queued share-v2 delivery");
        published += 1;
    }

    ShareExportResponse {
        published,
        refused,
        errored,
        errors,
    }
}

async fn handle_share_export(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ShareExportRequest>,
) -> Result<Json<ShareExportResponse>, (StatusCode, String)> {
    let gid = req.group_id.trim_start_matches("0x").to_lowercase();

    let group = state
        .groups
        .iter()
        .find(|g| g.shard_id == req.shard_id && g.group_id_hex == gid)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!(
                    "no Path A group configured for shard_id={} group_id={}",
                    req.shard_id, gid
                ),
            )
        })?;

    info!(
        shard_id = req.shard_id,
        group_id = %gid,
        signer_id = req.signer_id,
        target_count = req.targets.len(),
        enclave_url = %group.enclave_url,
        "admin: share-v2 export driver invoked"
    );

    let resp = export_shares(
        &state.client,
        &state.share_v2_pub_tx,
        req.shard_id,
        &gid,
        req.signer_id,
        &req.targets,
    )
    .await;

    Ok(Json(resp))
}

pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/admin/path-a/share-export", post(handle_share_export))
        .with_state(state)
}

/// Bind a 127.0.0.1-only admin HTTP listener. Errors if `listen_addr`
/// resolves to a non-loopback socket — the admin surface is local-only
/// by construction, gated by CLI off-by-default.
pub async fn spawn_admin_listener(
    listen_addr: String,
    state: Arc<AdminState>,
) -> anyhow::Result<()> {
    let parsed: std::net::SocketAddr = listen_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --admin-listen address {listen_addr:?}: {e}"))?;
    if !parsed.ip().is_loopback() {
        anyhow::bail!(
            "--admin-listen must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }

    let listener = tokio::net::TcpListener::bind(parsed)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind admin listener on {parsed}: {e}"))?;
    info!(listen = %parsed, "Path A admin listener started");
    let app = router(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("admin listener serve error: {e}"))?;
    Ok(())
}
