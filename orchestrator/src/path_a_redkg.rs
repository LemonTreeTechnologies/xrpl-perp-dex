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
    /// The cluster's operator XRPL addresses. EVERY route on this listener requires a signed
    /// request from one of them. Empty is not "allow all" — `verify_operator_request` refuses
    /// everything, which is the correct reading of a missing allowlist.
    pub operators: Vec<String>,
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
                warn!(%peer_pubkey, "share-export error: {:#}", e);
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
            warn!(%peer_pubkey, "share-v2 publish channel closed: {:#}", e);
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
    let operators = Arc::new(state.operators.clone());
    Router::new()
        .route("/admin/path-a/share-export", post(handle_share_export))
        .with_state(state)
        // FROST signing-round probe. Stateless (the caller names the enclave,
        // the signer set and the group key), so it is merged rather than
        // sharing AdminState.
        .merge(Router::new().route(
            "/admin/frost/round",
            post(crate::frost_round::handle_frost_round),
        ))
        // #131 Safe governance: order the owner signatures and submit the self-call.
        // Stateless like the FROST probe; configuration comes from the same env the
        // publisher reads, so there is no second place to hold the gas key.
        .merge(Router::new().route(
            "/admin/safe/exec",
            post(crate::safe_governance::handle_safe_exec),
        ))
        // What owner set does the sealed membership imply, and are we in sync? The
        // operator reads the plan here and relays it; they do not compose an operation.
        .merge(Router::new().route(
            "/admin/safe/projection",
            post(crate::safe_projection::handle_projection),
        ))
        // Independent derivation: neither request carries calldata, a hash, or an owner
        // set. Every node computes the content from its OWN enclave and its OWN read of
        // the chain — which is what keeps an opaque-hash quorum from being a blind one.
        .merge(Router::new().route(
            "/admin/safe/derive-step",
            post(crate::safe_projection::handle_derive_step),
        ))
        .merge(Router::new().route(
            "/admin/safe/attest-projection",
            post(crate::safe_projection::handle_attest),
        ))
        // APPLIED TO THE WHOLE ROUTER, not to the Safe routes, so a route ADDED here later
        // inherits the check instead of needing to remember it. Until 2026-10-08 this app was
        // `let app = router(state)` with no auth layer at all: six routes — share-export,
        // frost/round and the four Safe ones — behind loopback and nothing else. Audit ruling
        // Q1: the allowlist is a PRECONDITION to serving this surface, and loopback is a
        // mitigation rather than the control, because a co-located process or an SSRF reaches
        // loopback.
        .layer(axum::middleware::from_fn_with_state(
            operators.clone(),
            operator_only,
        ))
}

/// Refuse anything that is not a signed request from a cluster operator.
///
/// Takes the allowlist as its own state rather than reading it off `AdminState`, because two
/// of the six routes are `merge`d as stateless sub-routers and would otherwise be outside it.
async fn operator_only(
    axum::extract::State(operators): axum::extract::State<Arc<Vec<String>>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let method = request.method().as_str().to_string();
    let uri = request.uri().path().to_string();
    let headers = request.headers().clone();
    let (parts, body) = request.into_parts();
    // The body must be buffered because the signature covers it. 1 MiB is the same bound the
    // main app's auth layer uses.
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"status":"error","message":"failed to read body"})),
            )
                .into_response()
        }
    };
    if let Err((code, msg)) =
        crate::auth::verify_operator_request(&headers, &method, &body_bytes, &uri, &operators)
    {
        // Logged with the route and the reason: a refusal nobody can read is how this surface
        // came to look protected while it asked for nothing.
        warn!(route = %uri, code, reason = %msg, "admin surface REFUSED a request");
        return (
            StatusCode::from_u16(code).unwrap_or(StatusCode::FORBIDDEN),
            Json(serde_json::json!({"status":"error","message":msg})),
        )
            .into_response();
    }
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(body_bytes),
    ))
    .await
}

/// Bind a 127.0.0.1-only admin HTTP listener. Errors if `listen_addr`
/// resolves to a non-loopback socket — the admin surface is local-only
/// by construction, gated by CLI off-by-default.
pub async fn spawn_admin_listener(
    listen_addr: String,
    state: Arc<AdminState>,
) -> anyhow::Result<()> {
    // Bound through the one place that answers "how is this surface reached"
    // (admin_listen.rs). An absolute path gives a unix socket at mode 0600, where the OS
    // decides who may connect; anything else is a loopback TCP port, which every local
    // process and any SSRF can reach — and which now says so at every start.
    crate::admin_listen::serve_admin(
        "admin-listen",
        &listen_addr,
        router(state),
        "path-a re-DKG + Safe",
    )
    .await
}

#[cfg(test)]
mod admin_surface_tests {
    use super::*;

    /// Sign a request the way the ADMIN canonical does:
    /// SHA-256("xperp/v1/admin|" ‖ method ‖ "|" ‖ uri_path ‖ "|" ‖ body ‖ "|" ‖ timestamp).
    ///
    /// Written out here rather than shared from auth's test module, and that is SAFE in this
    /// direction: the real verifier is what accepts or rejects it, so a wrong replication
    /// makes the test FAIL, never pass. A fixture can only co-delude when both sides are mine.
    ///
    /// IT USED TO SIGN THE PUBLIC CANONICAL — SHA-256(body ‖ timestamp) — and that is why this
    /// test was red. The signature was real and the key was an allowlisted operator's; what the
    /// surface rejected was the DOMAIN. Which is the layer doing exactly its job: an admin route
    /// does not accept a public-canonical signature, because that canonical binds neither the
    /// method nor the route. So the red test was the feature, and the fixture was the bug —
    /// worth keeping in mind before "fixing" a refusal by loosening the thing refusing.
    fn operator_headers(
        method: &str,
        uri_path: &str,
        body: &[u8],
    ) -> (reqwest::header::HeaderMap, String) {
        use k256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
        use sha2::{Digest, Sha256};

        let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
        let pubkey = sk.verifying_key().to_sec1_bytes();
        let pubkey_hex = hex::encode(&pubkey);
        let address = crate::auth::pubkey_to_xrpl_address(&pubkey);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let mut h = Sha256::new();
        h.update(b"xperp/v1/admin|");
        h.update(method.as_bytes());
        h.update(b"|");
        h.update(uri_path.as_bytes());
        h.update(b"|");
        h.update(body);
        h.update(b"|");
        h.update(ts.as_bytes());
        let (sig, _): (Signature, _) = sk.sign_prehash(&h.finalize()).unwrap();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig.to_der().as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());
        (headers, address)
    }

    /// Serve the real admin router on an ephemeral loopback port and return its base URL.
    async fn serve(operators: Vec<String>) -> String {
        let (tx, _rx) = mpsc::channel(1);
        let state = Arc::new(AdminState {
            client: PoolPathAClient::new("https://localhost:9088/v1").unwrap(),
            share_v2_pub_tx: tx,
            groups: vec![],
            operators,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        format!("http://{addr}")
    }

    /// THE LAYER IS WIRED, not merely written.
    ///
    /// The primitive has its own unit tests in auth.rs; this drives the REAL router over REAL
    /// HTTP, because probing a predicate is not probing its wiring — the same distinction the
    /// audit drew on the clock guard, where I had checked a truth table and never that the
    /// refusal actually skipped the spawn.
    ///
    /// `/admin/safe/projection` is used as the probe route; the assertion is about the LAYER,
    /// so a legitimate operator need only get past it (the handler then fails on its own, with
    /// some other status, because no enclave is listening in a unit test).
    #[tokio::test]
    async fn the_admin_surface_refuses_unsigned_and_stranger_but_admits_an_operator() {
        let body = br#"{"probe":true}"#;
        let (headers, operator_addr) = operator_headers("POST", "/admin/safe/projection", body);

        // (1) operator configured, request UNSIGNED -> 401
        let base = serve(vec![operator_addr.clone()]).await;
        let c = reqwest::Client::new();
        let unsigned = c
            .post(format!("{base}/admin/safe/projection"))
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(unsigned.status().as_u16(), 401, "unsigned must be 401");

        // (2) SIGNED, but the signer is not on the allowlist -> 403
        let base2 = serve(vec!["rSomeoneElseEntirely".to_string()]).await;
        let stranger = c
            .post(format!("{base2}/admin/safe/projection"))
            .headers(headers.clone())
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(stranger.status().as_u16(), 403, "a stranger must be 403");

        // (3) SIGNED by an operator -> the layer must NOT be what stops it
        let ok = c
            .post(format!("{base}/admin/safe/projection"))
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        let s = ok.status().as_u16();
        assert!(
            s != 401 && s != 403,
            "an operator's signed request must get past the layer; got {s}"
        );
    }

    /// EVERY route on the listener is covered, not just the Safe ones — share-export and the
    /// FROST round driver are on the same app, and they were the two that worried me most when
    /// I found the listener had no auth layer at all.
    #[tokio::test]
    async fn the_layer_covers_share_export_and_the_frost_round_too() {
        let base = serve(vec!["rOperatorOne".to_string()]).await;
        let c = reqwest::Client::new();
        for route in [
            "/admin/path-a/share-export",
            "/admin/frost/round",
            "/admin/safe/exec",
            "/admin/safe/derive-step",
            "/admin/safe/attest-projection",
        ] {
            let r = c
                .post(format!("{base}{route}"))
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(
                r.status().as_u16(),
                401,
                "{route} answered {} to an unsigned request",
                r.status()
            );
        }
    }

    /// An EMPTY allowlist serves nothing — end to end, not only in the primitive.
    #[tokio::test]
    async fn an_empty_allowlist_serves_nothing_over_http() {
        let body = br#"{"probe":true}"#;
        let (headers, _) = operator_headers("POST", "/admin/safe/projection", body);
        let base = serve(vec![]).await;
        let r = reqwest::Client::new()
            .post(format!("{base}/admin/safe/projection"))
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
    }
}
