//! REQ-8 PRG-2 part 4/4 — POST /admin/migrate-state endpoint.
//!
//! Closes PRG-2 (production transport layer). Operator-driven HTTP
//! admin route on a loopback-only listener that constructs a
//! production `ComposedEnclaveApi<HttpEnclaveApi, LibP2PDelegationCollector>`,
//! drives the migration ceremony state machine to completion, and
//! returns the result.
//!
//! Listener is loopback-only by construction (mirrors
//! path_a_redkg::spawn_admin_listener and signerlist_update). Off by
//! default; gated by CLI flag `--migrate-admin-listen 127.0.0.1:7095`
//! (or whatever port the operator picks).

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::p2p::PathADelegationRelay;
use crate::path_a_ceremony::{CeremonyDriver, CeremonyParams, CeremonyState};
use crate::path_a_delegation::{ComposedEnclaveApi, LibP2PDelegationCollector};
use crate::path_a_http_client::HttpEnclaveApi;

/// Shared state injected into the admin route. Constructed in main.rs
/// from the libp2p delegation-relay sender + operator-configured
/// defaults (which can be overridden per-request).
pub struct AdminState {
    /// Sender into the p2p run-loop's delegation-collection channel
    /// (wired via `P2PNode::set_path_a_delegation_channel`).
    pub path_a_delegation_tx: mpsc::Sender<PathADelegationRelay>,
    /// Default OLD enclave HTTPS base URL; request body can override.
    pub default_old_api_base: String,
    /// Default NEW enclave HTTPS base URL; request body can override.
    pub default_new_api_base: String,
}

#[derive(Debug, Deserialize)]
pub struct MigrateStateRequest {
    /// 64-char hex of the new MRENCLAVE the operator quorum agreed on.
    pub expected_mrenclave_new: String,
    /// Override default_old_api_base if present.
    #[serde(default)]
    pub old_api_base: Option<String>,
    /// Override default_new_api_base if present.
    #[serde(default)]
    pub new_api_base: Option<String>,
    /// Per-request timeout override for delegation collection (seconds).
    /// Defaults to 30s if omitted (LibP2PDelegationCollector default).
    #[serde(default)]
    pub delegation_timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MigrateStateResponse {
    pub status: &'static str,
    pub ceremony_nonce_hex: String,
    pub mrenclave_new_hex: String,
    pub manifest_hash_hex: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    status: &'static str,
    message: String,
    /// Final ceremony state when the error fired — operator log gets
    /// "Failed at ImportRequested" instead of just an opaque message.
    state: Option<&'static str>,
}

fn state_label(s: CeremonyState) -> &'static str {
    match s {
        CeremonyState::Idle => "idle",
        CeremonyState::PreparingHandshake => "preparing_handshake",
        CeremonyState::KeypairRequested => "keypair_requested",
        CeremonyState::CollectingDelegation => "collecting_delegation",
        CeremonyState::ExportRequested => "export_requested",
        CeremonyState::ImportRequested => "import_requested",
        CeremonyState::ConfirmationRequested => "confirmation_requested",
        CeremonyState::Succeeded => "succeeded",
        CeremonyState::Failed => "failed",
    }
}

async fn handle_migrate_state(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<MigrateStateRequest>,
) -> impl IntoResponse {
    let old_base = req
        .old_api_base
        .unwrap_or_else(|| state.default_old_api_base.clone());
    let new_base = req
        .new_api_base
        .unwrap_or_else(|| state.default_new_api_base.clone());

    info!(
        mrenclave_new = %req.expected_mrenclave_new,
        old_base = %old_base,
        new_base = %new_base,
        "admin: migrate-state ceremony driver invoked"
    );

    let http = match HttpEnclaveApi::new() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    status: "error",
                    message: format!("HttpEnclaveApi build failed: {e}"),
                    state: None,
                }),
            )
                .into_response();
        }
    };

    let mut delegation = LibP2PDelegationCollector::new(state.path_a_delegation_tx.clone());
    if let Some(secs) = req.delegation_timeout_secs {
        delegation = delegation.with_timeout(Duration::from_secs(secs));
    }

    let api = ComposedEnclaveApi { http, delegation };
    let mut driver = CeremonyDriver::new(api);
    let params = CeremonyParams {
        expected_mrenclave_new: req.expected_mrenclave_new,
        old_api_base: old_base,
        new_api_base: new_base,
    };

    match driver.run(&params).await {
        Ok(success) => {
            info!(
                mrenclave_new = %success.mrenclave_new_hex,
                ceremony_nonce = %success.ceremony_nonce_hex,
                "admin: ceremony succeeded — OLD retired, operator runs promotion sequence next"
            );
            (
                StatusCode::OK,
                Json(MigrateStateResponse {
                    status: "ok",
                    ceremony_nonce_hex: success.ceremony_nonce_hex,
                    mrenclave_new_hex: success.mrenclave_new_hex,
                    manifest_hash_hex: success.manifest_hash_hex,
                }),
            )
                .into_response()
        }
        Err(e) => {
            let final_state = state_label(driver.state());
            error!(state = %final_state, error = %e, "admin: ceremony failed");
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    status: "error",
                    message: format!("{e:#}"),
                    state: Some(final_state),
                }),
            )
                .into_response()
        }
    }
}

pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/admin/migrate-state", post(handle_migrate_state))
        .with_state(state)
}

/// Bind a 127.0.0.1-only admin HTTP listener. Errors if listen_addr
/// resolves to a non-loopback socket — same loopback-only discipline
/// as path_a_redkg / signerlist_update / dkg_coordinate.
pub async fn spawn_admin_listener(
    listen_addr: String,
    state: Arc<AdminState>,
) -> anyhow::Result<()> {
    let parsed: std::net::SocketAddr = listen_addr.parse().map_err(|e| {
        anyhow::anyhow!("invalid --migrate-admin-listen address {listen_addr:?}: {e}")
    })?;
    if !parsed.ip().is_loopback() {
        anyhow::bail!(
            "--migrate-admin-listen must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }

    let listener = tokio::net::TcpListener::bind(parsed)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind migrate-admin listener on {parsed}: {e}"))?;
    info!(listen = %parsed, "Path A migrate-state admin listener started");
    let app = router(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("migrate-admin listener serve error: {e}"))?;
    Ok(())
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_label_covers_all_variants() {
        // Belt-and-braces: forces a future CeremonyState variant addition
        // to update this match. If a new state ships unmapped, this test
        // still passes (compiler exhaustiveness check on state_label
        // catches that). Test just sanity-checks every existing variant
        // produces a non-empty label.
        for s in [
            CeremonyState::Idle,
            CeremonyState::PreparingHandshake,
            CeremonyState::KeypairRequested,
            CeremonyState::CollectingDelegation,
            CeremonyState::ExportRequested,
            CeremonyState::ImportRequested,
            CeremonyState::ConfirmationRequested,
            CeremonyState::Succeeded,
            CeremonyState::Failed,
        ] {
            assert!(!state_label(s).is_empty());
        }
    }

    #[tokio::test]
    async fn spawn_admin_listener_rejects_non_loopback() {
        let (tx, _rx) = mpsc::channel(1);
        let state = Arc::new(AdminState {
            path_a_delegation_tx: tx,
            default_old_api_base: "https://localhost:9088".into(),
            default_new_api_base: "https://localhost:9089".into(),
        });
        let err = spawn_admin_listener("0.0.0.0:7095".into(), state)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("loopback"));
    }

    #[test]
    fn migrate_state_request_accepts_minimal_body() {
        let body = r#"{"expected_mrenclave_new":"abc"}"#;
        let req: MigrateStateRequest = serde_json::from_str(body).unwrap();
        assert_eq!(req.expected_mrenclave_new, "abc");
        assert!(req.old_api_base.is_none());
        assert!(req.new_api_base.is_none());
        assert!(req.delegation_timeout_secs.is_none());
    }

    #[test]
    fn migrate_state_request_accepts_full_body() {
        let body = r#"{
            "expected_mrenclave_new":"deadbeef",
            "old_api_base":"https://x",
            "new_api_base":"https://y",
            "delegation_timeout_secs":120
        }"#;
        let req: MigrateStateRequest = serde_json::from_str(body).unwrap();
        assert_eq!(req.expected_mrenclave_new, "deadbeef");
        assert_eq!(req.old_api_base.as_deref(), Some("https://x"));
        assert_eq!(req.new_api_base.as_deref(), Some("https://y"));
        assert_eq!(req.delegation_timeout_secs, Some(120));
    }
}
