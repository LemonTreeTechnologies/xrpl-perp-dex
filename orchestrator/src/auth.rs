//! XRPL signature authentication middleware.
//!
//! Users sign request body with their XRPL secp256k1 private key.
//! Server verifies signature and derives XRPL address from public key.
//!
//! Headers:
//!   X-XRPL-Address:   r-address of the signer
//!   X-XRPL-PublicKey:  compressed secp256k1 public key (hex, 66 chars)
//!   X-XRPL-Signature:  DER-encoded ECDSA signature of SHA-256(body) (hex)
//!
//! For GET requests (no body): signs the full URI path + query string.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use k256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use ripemd::Ripemd160;
use sha2::{Digest, Sha256, Sha512};
use tokio::sync::RwLock;
use tracing::warn;
use uuid::Uuid;

/// Authentication result: verified XRPL address.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub xrpl_address: String,
}

/// O-H4: signature-bound identity of a submission, stashed by the auth
/// middleware so the handler can persist it alongside the resting order
/// and the post-failover reload can re-verify before replaying the row
/// into the in-memory CLOB. The reload path calls
/// [`verify_signature_only`] — it skips the timestamp-drift check because
/// persisted rows are arbitrarily old by definition, but it re-runs the
/// full ECDSA verification and address/pubkey self-consistency check,
/// which is what binds the row to a real user.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OrderSignatureBinding {
    /// Raw JSON body the client signed (hex-encoded for PG TEXT transport).
    pub signed_body_hex: String,
    pub signature_hex: String,
    pub timestamp: String,
    pub signer_address: String,
    pub signer_pubkey_hex: String,
}

/// O-H4: re-verify a persisted signature binding without the timestamp
/// drift check. Used on validator failover when rebuilding the CLOB from
/// PG. Returns Ok iff:
///   1. The stored pubkey round-trips to the stored address.
///   2. The signature is valid over `SHA-256(body || timestamp)`
///      (either directly or via the XRPL SHA-512Half wrap).
pub fn verify_signature_only(binding: &OrderSignatureBinding) -> Result<(), String> {
    let pubkey_bytes = hex::decode(&binding.signer_pubkey_hex).map_err(|_| "invalid pubkey hex")?;
    let verifying_key =
        VerifyingKey::from_sec1_bytes(&pubkey_bytes).map_err(|_| "invalid secp256k1 public key")?;

    let sha256_hash = Sha256::digest(&pubkey_bytes);
    let ripemd_hash = Ripemd160::digest(sha256_hash);
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(&ripemd_hash);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    const XRPL_ALPHABET: &str = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alpha_bytes: &[u8; 58] = XRPL_ALPHABET
        .as_bytes()
        .try_into()
        .expect("XRPL alphabet is 58 chars");
    let alpha = bs58::Alphabet::new(alpha_bytes).expect("valid alphabet");
    let derived = bs58::encode(&payload).with_alphabet(&alpha).into_string();
    if derived != binding.signer_address {
        return Err(format!(
            "address mismatch: stored={}, derived from pubkey={}",
            binding.signer_address, derived
        ));
    }

    let body_bytes = hex::decode(&binding.signed_body_hex).map_err(|_| "invalid body hex")?;
    if body_bytes.is_empty() {
        return Err("stored signed body is empty — rebind rejected".into());
    }

    let mut hasher = Sha256::new();
    hasher.update(&body_bytes);
    hasher.update(binding.timestamp.as_bytes());
    let hash = hasher.finalize();

    let sig_bytes = hex::decode(&binding.signature_hex).map_err(|_| "invalid signature hex")?;
    let signature = Signature::from_der(&sig_bytes).map_err(|_| "invalid DER signature")?;

    if verifying_key.verify_prehash(&hash, &signature).is_ok() {
        return Ok(());
    }
    let sha512_full = Sha512::digest(hash);
    let sha512_half: [u8; 32] = sha512_full[..32].try_into().unwrap();
    verifying_key
        .verify_prehash(&sha512_half, &signature)
        .map_err(|_| "signature verification failed".to_string())?;
    Ok(())
}

// ── Session token store ──────────────────────────────────────────

const SESSION_TTL: Duration = Duration::from_secs(30 * 60); // 30 minutes

struct Session {
    address: String,
    expires: Instant,
}

/// In-memory session store for Bearer token auth.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new session, returning the token.
    pub async fn create(&self, address: String) -> String {
        let token = Uuid::new_v4().to_string();
        let session = Session {
            address,
            expires: Instant::now() + SESSION_TTL,
        };
        let mut map = self.sessions.write().await;
        // Lazy cleanup: remove expired sessions
        map.retain(|_, s| s.expires > Instant::now());
        map.insert(token.clone(), session);
        token
    }

    /// Look up a session by token. Returns the address if valid.
    pub async fn get(&self, token: &str) -> Option<String> {
        let map = self.sessions.read().await;
        match map.get(token) {
            Some(s) if s.expires > Instant::now() => Some(s.address.clone()),
            _ => None,
        }
    }
}

/// Global session store — initialized lazily on first access via
/// `get_or_init`. O-M1: we deliberately do NOT expose an eager
/// `init_session_store()` setter. `OnceLock::set()` silently fails if
/// another thread already initialized through `get_or_init`, which
/// meant the previous eager/lazy pair could race-land two different
/// stores (one eager, one lazy) depending on call order. Relying on a
/// single `get_or_init` path eliminates the race by construction.
static SESSION_STORE: std::sync::OnceLock<Arc<SessionStore>> = std::sync::OnceLock::new();

pub fn session_store() -> &'static Arc<SessionStore> {
    SESSION_STORE.get_or_init(|| Arc::new(SessionStore::new()))
}

/// Extract and verify XRPL signature from request headers.
///
/// `method` is the HTTP method (uppercase). It is used to disambiguate
/// two cases that both present with an empty body at this level:
///   * `GET` — the client signs the URI path (+ query) as the body.
///   * `POST`/`DELETE` with empty body (e.g. `/v1/auth/login`) — the
///     client signs the domain-separated canonical to defeat the
///     proof-of-life / foreign-oracle attack flagged in
///     SECURITY-REAUDIT-4 O-H1.
pub fn verify_request(
    headers: &HeaderMap,
    method: &str,
    body_bytes: &[u8],
    uri_path: &str,
) -> Result<AuthenticatedUser, String> {
    // Extract headers
    let address = headers
        .get("x-xrpl-address")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-XRPL-Address header")?;

    let pubkey_hex = headers
        .get("x-xrpl-publickey")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-XRPL-PublicKey header")?;

    let sig_hex = headers
        .get("x-xrpl-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-XRPL-Signature header")?;

    // Replay protection: timestamp header REQUIRED
    let timestamp_str = headers
        .get("x-xrpl-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing X-XRPL-Timestamp header (required for replay protection)")?;

    {
        let ts: u64 = timestamp_str
            .parse()
            .map_err(|_| "invalid X-XRPL-Timestamp")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let drift = now.abs_diff(ts);
        if drift > 60 {
            return Err(format!(
                "request expired: timestamp drift {drift}s (max 60s)"
            ));
        }
    }

    // Validate address format
    if !address.starts_with('r') || address.len() < 25 || address.len() > 35 {
        return Err("invalid XRPL address format".into());
    }

    // Validate pubkey format (33 bytes compressed = 66 hex chars)
    if pubkey_hex.len() != 66 {
        return Err("invalid public key length (expected 66 hex chars)".into());
    }

    // Decode public key
    let pubkey_bytes = hex::decode(pubkey_hex).map_err(|_| "invalid public key hex")?;
    let verifying_key =
        VerifyingKey::from_sec1_bytes(&pubkey_bytes).map_err(|_| "invalid secp256k1 public key")?;

    // Verify pubkey → XRPL address derivation
    // XRPL: SHA-256(compressed_pubkey) → RIPEMD-160 → Base58Check with prefix 0x00
    let sha256_hash = Sha256::digest(&pubkey_bytes);
    let ripemd_hash = Ripemd160::digest(sha256_hash);

    // Base58Check: [0x00] + ripemd_hash + checksum
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(&ripemd_hash);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);

    // XRPL uses its own Base58 alphabet
    const XRPL_ALPHABET: &str = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alpha_bytes: &[u8; 58] = XRPL_ALPHABET
        .as_bytes()
        .try_into()
        .expect("XRPL alphabet is 58 chars");
    let alpha = bs58::Alphabet::new(alpha_bytes).expect("valid alphabet");
    let derived_address = bs58::encode(&payload).with_alphabet(&alpha).into_string();

    if derived_address != address {
        return Err(format!(
            "address mismatch: header={address}, derived from pubkey={derived_address}"
        ));
    }

    // Compute canonical hash to verify against.
    //
    //   GET              → SHA-256(uri_path || timestamp)
    //   non-empty body   → SHA-256(body || timestamp)
    //   empty-body POST  → SHA-256("xperp/v1/login|" || uri_path || "|" || timestamp)
    //
    // The empty-body POST path is domain-separated to defeat the
    // "foreign oracle" attack flagged in SECURITY-REAUDIT-4 O-H1: any
    // external signer that produced `SHA-256(timestamp)` (proof-of-life,
    // telemetry, unrelated XRPL contexts) previously doubled as a valid
    // login credential. The domain prefix pins the signature to this
    // server's login flow.
    let is_get = method.eq_ignore_ascii_case("GET");
    let hash = {
        let mut hasher = Sha256::new();
        if is_get {
            hasher.update(uri_path.as_bytes());
        } else if body_bytes.is_empty() {
            hasher.update(b"xperp/v1/login|");
            hasher.update(uri_path.as_bytes());
            hasher.update(b"|");
        } else {
            hasher.update(body_bytes);
        }
        hasher.update(timestamp_str.as_bytes());
        hasher.finalize()
    };

    // Decode and verify signature
    let sig_bytes = hex::decode(sig_hex).map_err(|_| "invalid signature hex")?;
    let signature = Signature::from_der(&sig_bytes).map_err(|_| "invalid DER signature")?;

    // Verify ECDSA signature over pre-hashed data.
    // Mode 1 (default): client signs the canonical hash directly (Python,
    //   Node.js, raw secp256k1).
    // Mode 2 (XRPL wallets): Crossmark/GemWallet use ripple-keypairs which
    //   applies SHA-512Half(message) before ECDSA. The client passes the
    //   hex of the canonical hash as message; the wallet internally signs
    //   SHA512(hex_bytes)[0..32].
    let verified = {
        // Mode 1: direct canonical hash.
        if verifying_key.verify_prehash(&hash, &signature).is_ok() {
            true
        } else {
            // Mode 2: SHA-512Half wrap.
            let sha512_full = Sha512::digest(hash);
            let sha512_half: [u8; 32] = sha512_full[..32].try_into().unwrap();
            verifying_key
                .verify_prehash(&sha512_half, &signature)
                .is_ok()
        }
    };

    if !verified {
        tracing::debug!(
            hash_hex = %hex::encode(hash),
            sig_hex = %sig_hex,
            pubkey_hex = %pubkey_hex,
            "signature verification failed (tried both direct SHA-256 and XRPL SHA-512Half)"
        );
        return Err("signature verification failed".into());
    }

    Ok(AuthenticatedUser {
        xrpl_address: address.to_string(),
    })
}

/// Derive XRPL r-address from compressed secp256k1 public key bytes.
/// Used in tests and utilities.
#[cfg(test)]
pub fn pubkey_to_xrpl_address(pubkey_bytes: &[u8]) -> String {
    let sha256_hash = Sha256::digest(pubkey_bytes);
    let ripemd_hash = Ripemd160::digest(sha256_hash);
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(&ripemd_hash);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    const XRPL_ALPHABET: &str = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alpha_bytes: &[u8; 58] = XRPL_ALPHABET.as_bytes().try_into().unwrap();
    let alpha = bs58::Alphabet::new(alpha_bytes).unwrap();
    bs58::encode(&payload).with_alphabet(&alpha).into_string()
}

/// Axum middleware: verify auth headers on mutating endpoints.
/// GET requests to public market data are exempt.
pub async fn auth_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().path().to_string();

    // Public endpoints — no auth required
    if uri == "/v1/health"
        || uri == "/v1/system/status"
        || uri == "/v1/openapi.json"
        || uri == "/v1/pool/status"
        || uri == "/v1/auth/login"
        || uri.starts_with("/v1/attestation/")
        || uri.starts_with("/v1/perp/liquidations/")
        || (method == "GET" && (uri == "/v1/markets" || uri.starts_with("/v1/markets/")))
    {
        return next.run(request).await;
    }

    let headers = request.headers().clone();
    let uri_string = request.uri().to_string();

    // Check for Bearer token first (session-based auth)
    if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            if let Some(address) = session_store().get(token).await {
                let user = AuthenticatedUser {
                    xrpl_address: address,
                };

                let (mut parts, body) = request.into_parts();
                let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"status": "error", "message": "failed to read body"})),
                        )
                            .into_response();
                    }
                };

                // Verify user_id matches token's address (same checks as signature auth)
                if !body_bytes.is_empty() {
                    if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    {
                        if let Some(body_user_id) =
                            body_json.get("user_id").and_then(|v| v.as_str())
                        {
                            if body_user_id != user.xrpl_address {
                                return (
                                    StatusCode::FORBIDDEN,
                                    Json(serde_json::json!({
                                        "status": "error",
                                        "message": format!(
                                            "user_id '{}' does not match session address '{}'",
                                            body_user_id, user.xrpl_address
                                        )
                                    })),
                                )
                                    .into_response();
                            }
                        }
                    }
                }
                if method == "GET" {
                    if let Some(query) = parts.uri.query() {
                        for pair in query.split('&') {
                            if let Some(val) = pair.strip_prefix("user_id=") {
                                if val != user.xrpl_address {
                                    return (
                                        StatusCode::FORBIDDEN,
                                        Json(serde_json::json!({
                                            "status": "error",
                                            "message": "user_id does not match session address"
                                        })),
                                    )
                                        .into_response();
                                }
                            }
                        }
                    }
                }

                parts.extensions.insert(user);
                let request = Request::from_parts(parts, Body::from(body_bytes));
                return next.run(request).await;
            } else {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"status": "error", "message": "session expired or invalid token"})),
                )
                    .into_response();
            }
        }
    }

    // For requests with body, we need to read it for signature verification
    let (mut parts, body) = request.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"status": "error", "message": "failed to read body"})),
            )
                .into_response();
        }
    };

    match verify_request(&headers, method.as_str(), &body_bytes, &uri_string) {
        Ok(user) => {
            // O-H4: build a binding we can persist with resting orders so
            // the row can be re-verified on failover reload.
            let binding: Option<OrderSignatureBinding> =
                if !body_bytes.is_empty() && method != "GET" {
                    let sig_hex = headers
                        .get("x-xrpl-signature")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let ts = headers
                        .get("x-xrpl-timestamp")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let pk = headers
                        .get("x-xrpl-publickey")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    Some(OrderSignatureBinding {
                        signed_body_hex: hex::encode(&body_bytes),
                        signature_hex: sig_hex,
                        timestamp: ts,
                        signer_address: user.xrpl_address.clone(),
                        signer_pubkey_hex: pk,
                    })
                } else {
                    None
                };
            // For POST/DELETE with body: verify user_id matches authenticated address
            if !body_bytes.is_empty() {
                match serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                    Ok(body_json) => {
                        if let Some(body_user_id) =
                            body_json.get("user_id").and_then(|v| v.as_str())
                        {
                            if body_user_id != user.xrpl_address {
                                return (
                                    StatusCode::FORBIDDEN,
                                    Json(serde_json::json!({
                                        "status": "error",
                                        "message": format!(
                                            "user_id '{}' does not match authenticated address '{}'",
                                            body_user_id, user.xrpl_address
                                        )
                                    })),
                                )
                                    .into_response();
                            }
                        }
                    }
                    Err(_) => {
                        // Reject non-JSON bodies on authenticated endpoints
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"status": "error", "message": "request body must be valid JSON"})),
                        )
                            .into_response();
                    }
                }
            }

            // For GET with user_id query param: verify matches
            if method == "GET" {
                if let Some(query) = parts.uri.query() {
                    for pair in query.split('&') {
                        if let Some(val) = pair.strip_prefix("user_id=") {
                            if val != user.xrpl_address {
                                return (
                                    StatusCode::FORBIDDEN,
                                    Json(serde_json::json!({
                                        "status": "error",
                                        "message": "user_id does not match authenticated address"
                                    })),
                                )
                                    .into_response();
                            }
                        }
                    }
                }
            }

            // Inject authenticated user into request extensions
            parts.extensions.insert(user);
            if let Some(b) = binding {
                parts.extensions.insert(b);
            }
            let request = Request::from_parts(parts, Body::from(body_bytes));
            next.run(request).await
        }
        Err(msg) => {
            warn!(uri = %uri_string, "auth failed: {}", msg);
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"status": "error", "message": msg})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
    use k256::elliptic_curve::rand_core::OsRng;

    /// Helper: generate a test keypair and derive XRPL address.
    fn test_keypair() -> (SigningKey, VerifyingKey, String, String) {
        let sk = SigningKey::random(&mut OsRng);
        let vk = *sk.verifying_key();
        let pubkey_bytes = vk.to_sec1_bytes();
        let pubkey_hex = hex::encode(&pubkey_bytes);
        let address = pubkey_to_xrpl_address(&pubkey_bytes);
        (sk, vk, pubkey_hex, address)
    }

    fn current_ts() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    /// Helper: sign body + timestamp and build auth headers.
    fn sign_body(sk: &SigningKey, pubkey_hex: &str, address: &str, body: &[u8]) -> HeaderMap {
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(body);
        hasher.update(ts.as_bytes());
        let hash = hasher.finalize();
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();
        let sig_der = sig.to_der();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig_der.as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());
        headers
    }

    /// Helper: sign URI path + timestamp (for GET requests).
    fn sign_uri(sk: &SigningKey, pubkey_hex: &str, address: &str, uri: &str) -> HeaderMap {
        sign_body(sk, pubkey_hex, address, uri.as_bytes())
    }

    /// Helper: sign the domain-separated empty-body POST canonical
    /// hash — `SHA-256("xperp/v1/login|" || uri_path || "|" || ts)`.
    /// Mirrors the server-side canonical introduced by O-H1.
    fn sign_empty_body_post(
        sk: &SigningKey,
        pubkey_hex: &str,
        address: &str,
        uri_path: &str,
    ) -> HeaderMap {
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(b"xperp/v1/login|");
        hasher.update(uri_path.as_bytes());
        hasher.update(b"|");
        hasher.update(ts.as_bytes());
        let hash = hasher.finalize();
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();
        let sig_der = sig.to_der();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig_der.as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());
        headers
    }

    #[test]
    fn valid_post_signature_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"{\"user_id\":\"test\",\"side\":\"buy\"}";
        let headers = sign_body(&sk, &pubkey_hex, &address, body);
        let result = verify_request(&headers, "POST", body, "/v1/orders");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().xrpl_address, address);
    }

    #[test]
    fn valid_get_signature_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/orders?user_id=rTest123";
        // GET: empty body, signs URI
        let headers = sign_uri(&sk, &pubkey_hex, &address, uri);
        let result = verify_request(&headers, "GET", &[], uri);
        assert!(result.is_ok());
    }

    #[test]
    fn missing_address_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-publickey", "aa".repeat(33).parse().unwrap());
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-Address header");
    }

    #[test]
    fn missing_pubkey_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-xrpl-address",
            "rTest12345678901234567890".parse().unwrap(),
        );
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-PublicKey header");
    }

    #[test]
    fn missing_signature_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-xrpl-address",
            "rTest12345678901234567890".parse().unwrap(),
        );
        headers.insert("x-xrpl-publickey", "aa".repeat(33).parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-Signature header");
    }

    #[test]
    fn invalid_address_format_rejected() {
        let (sk, _, pubkey_hex, _) = test_keypair();
        let body = b"test";
        let headers = sign_body(&sk, &pubkey_hex, "xNotAnAddress", body);
        let result = verify_request(&headers, "POST", body, "/");
        assert_eq!(result.unwrap_err(), "invalid XRPL address format");
    }

    #[test]
    fn address_too_short_rejected() {
        let (sk, _, pubkey_hex, _) = test_keypair();
        let headers = sign_body(&sk, &pubkey_hex, "rShort", b"test");
        let result = verify_request(&headers, "POST", b"test", "/");
        assert_eq!(result.unwrap_err(), "invalid XRPL address format");
    }

    #[test]
    fn wrong_pubkey_length_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-xrpl-address",
            "rTest12345678901234567890".parse().unwrap(),
        );
        headers.insert("x-xrpl-publickey", "aabb".parse().unwrap()); // too short
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        headers.insert("x-xrpl-timestamp", current_ts().parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(
            result.unwrap_err(),
            "invalid public key length (expected 66 hex chars)"
        );
    }

    #[test]
    fn address_mismatch_rejected() {
        let (sk, _, pubkey_hex, _address) = test_keypair();
        let body = b"test";
        // Use a different (but valid-format) address
        let fake_address = "rFakeAddress1234567890123";
        let headers = sign_body(&sk, &pubkey_hex, fake_address, body);
        let result = verify_request(&headers, "POST", body, "/");
        assert!(result.unwrap_err().starts_with("address mismatch"));
    }

    #[test]
    fn wrong_signature_rejected() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"correct body";
        let headers = sign_body(&sk, &pubkey_hex, &address, b"different body");
        // Verify with correct body but signature was for different body
        let result = verify_request(&headers, "POST", body, "/");
        assert_eq!(result.unwrap_err(), "signature verification failed");
    }

    #[test]
    fn invalid_signature_hex_rejected() {
        let (_, _, pubkey_hex, address) = test_keypair();
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert("x-xrpl-signature", "not_hex!!".parse().unwrap());
        headers.insert("x-xrpl-timestamp", current_ts().parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(result.unwrap_err(), "invalid signature hex");
    }

    #[test]
    fn invalid_der_signature_rejected() {
        let (_, _, pubkey_hex, address) = test_keypair();
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        headers.insert("x-xrpl-timestamp", current_ts().parse().unwrap());
        let result = verify_request(&headers, "POST", b"body", "/");
        assert_eq!(result.unwrap_err(), "invalid DER signature");
    }

    #[test]
    fn missing_timestamp_rejected() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"test";
        let hash = Sha256::digest(body);
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig.to_der().as_bytes()).parse().unwrap(),
        );
        // NO timestamp header
        let result = verify_request(&headers, "POST", body, "/");
        assert!(result.unwrap_err().contains("X-XRPL-Timestamp"));
    }

    #[test]
    fn xrpl_address_derivation_deterministic() {
        let (_, _, pubkey_hex, address) = test_keypair();
        let pubkey_bytes = hex::decode(&pubkey_hex).unwrap();
        let derived = pubkey_to_xrpl_address(&pubkey_bytes);
        assert_eq!(derived, address);
        // Derive again — should be identical
        assert_eq!(pubkey_to_xrpl_address(&pubkey_bytes), address);
    }

    #[test]
    fn xrpl_address_starts_with_r() {
        let (_, _, _pubkey_hex, address) = test_keypair();
        assert!(address.starts_with('r'));
        assert!(address.len() >= 25 && address.len() <= 35);
    }

    /// Helper: sign body with XRPL wallet style (SHA-512Half wrapping).
    /// Mimics what Crossmark/GemWallet do: SHA512(SHA256(body+ts))[0..32] → ECDSA sign.
    fn sign_body_xrpl_wallet(
        sk: &SigningKey,
        pubkey_hex: &str,
        address: &str,
        body: &[u8],
    ) -> HeaderMap {
        use sha2::Sha512;
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(body);
        hasher.update(ts.as_bytes());
        let sha256_hash = hasher.finalize();
        // XRPL wallet applies SHA-512Half on top
        let sha512_full = Sha512::digest(sha256_hash);
        let sha512_half: [u8; 32] = sha512_full[..32].try_into().unwrap();
        let (sig, _): (Signature, _) = sk.sign_prehash(&sha512_half).unwrap();
        let sig_der = sig.to_der();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig_der.as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());
        headers
    }

    #[test]
    fn xrpl_wallet_sha512half_signature_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"{\"user_id\":\"test\",\"side\":\"buy\"}";
        let headers = sign_body_xrpl_wallet(&sk, &pubkey_hex, &address, body);
        let result = verify_request(&headers, "POST", body, "/v1/orders");
        assert!(result.is_ok(), "SHA-512Half wallet signature should pass");
        assert_eq!(result.unwrap().xrpl_address, address);
    }

    #[test]
    fn xrpl_wallet_get_signature_passes() {
        use sha2::Sha512;
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/orders?user_id=rTest123";
        let ts = current_ts();
        // GET: sign URI path with SHA-512Half wrapping
        let mut hasher = Sha256::new();
        hasher.update(uri.as_bytes());
        hasher.update(ts.as_bytes());
        let sha256_hash = hasher.finalize();
        let sha512_full = Sha512::digest(sha256_hash);
        let sha512_half: [u8; 32] = sha512_full[..32].try_into().unwrap();
        let (sig, _): (Signature, _) = sk.sign_prehash(&sha512_half).unwrap();
        let sig_der = sig.to_der();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig_der.as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());
        let result = verify_request(&headers, "GET", &[], uri);
        assert!(result.is_ok(), "SHA-512Half GET signature should pass");
    }

    #[test]
    fn different_keys_different_addresses() {
        let (_, _, _, addr1) = test_keypair();
        let (_, _, _, addr2) = test_keypair();
        assert_ne!(addr1, addr2);
    }

    // ── O-H1 regression tests (SECURITY-REAUDIT-4) ──────────────────

    #[test]
    fn login_domain_separated_hash_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/auth/login";
        let headers = sign_empty_body_post(&sk, &pubkey_hex, &address, uri);
        let result = verify_request(&headers, "POST", &[], uri);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.unwrap().xrpl_address, address);
    }

    /// The audit-flagged vulnerable path: any signature produced for an
    /// unrelated purpose over just a unix timestamp (e.g. a
    /// proof-of-life probe) must NOT authenticate a login. Before the
    /// O-H1 fix, `SHA-256(timestamp)` was an accepted alternate hash.
    #[test]
    fn login_timestamp_only_hash_rejected() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/auth/login";
        let ts = current_ts();
        // Produce a signature over SHA-256(timestamp) — what the old
        // alt_hash branch accepted.
        let mut hasher = Sha256::new();
        hasher.update(ts.as_bytes());
        let hash = hasher.finalize();
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig.to_der().as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());

        let result = verify_request(&headers, "POST", &[], uri);
        assert_eq!(result.unwrap_err(), "signature verification failed");
    }

    /// The pre-audit non-domain-separated empty-body canonical
    /// `SHA-256(uri || ts)` must also fail — a signature produced
    /// against it was not an oracle risk, but accepting it after the
    /// fix would keep a second verify path open and mask client bugs.
    #[test]
    fn login_non_domain_separated_hash_rejected() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/auth/login";
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(uri.as_bytes());
        hasher.update(ts.as_bytes());
        let hash = hasher.finalize();
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig.to_der().as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());

        let result = verify_request(&headers, "POST", &[], uri);
        assert_eq!(result.unwrap_err(), "signature verification failed");
    }

    /// SHA-512Half wrapping (Crossmark / GemWallet) must still compose
    /// with the domain-separated empty-body canonical.
    #[test]
    fn login_sha512half_domain_separated_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/auth/login";
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(b"xperp/v1/login|");
        hasher.update(uri.as_bytes());
        hasher.update(b"|");
        hasher.update(ts.as_bytes());
        let canonical = hasher.finalize();
        let sha512_full = Sha512::digest(canonical);
        let sha512_half: [u8; 32] = sha512_full[..32].try_into().unwrap();
        let (sig, _): (Signature, _) = sk.sign_prehash(&sha512_half).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert(
            "x-xrpl-signature",
            hex::encode(sig.to_der().as_bytes()).parse().unwrap(),
        );
        headers.insert("x-xrpl-timestamp", ts.parse().unwrap());

        let result = verify_request(&headers, "POST", &[], uri);
        assert!(result.is_ok(), "{result:?}");
    }

    // ── O-H4 regression tests ──────────────────────────────────────

    fn make_binding(
        sk: &SigningKey,
        pubkey_hex: &str,
        address: &str,
        body: &[u8],
    ) -> OrderSignatureBinding {
        let ts = current_ts();
        let mut hasher = Sha256::new();
        hasher.update(body);
        hasher.update(ts.as_bytes());
        let hash = hasher.finalize();
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();
        OrderSignatureBinding {
            signed_body_hex: hex::encode(body),
            signature_hex: hex::encode(sig.to_der().as_bytes()),
            timestamp: ts,
            signer_address: address.to_string(),
            signer_pubkey_hex: pubkey_hex.to_string(),
        }
    }

    #[test]
    fn verify_signature_only_accepts_valid_binding() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = br#"{"user_id":"rEXAMPLE","side":"long","size":"1"}"#;
        let binding = make_binding(&sk, &pubkey_hex, &address, body);
        assert!(verify_signature_only(&binding).is_ok());
    }

    #[test]
    fn verify_signature_only_rejects_tampered_body() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = br#"{"user_id":"rEXAMPLE","side":"long","size":"1"}"#;
        let mut binding = make_binding(&sk, &pubkey_hex, &address, body);
        // Flip one byte in the signed body — hash changes, signature invalid.
        binding.signed_body_hex =
            hex::encode(br#"{"user_id":"rEXAMPLE","side":"long","size":"9"}"#);
        let r = verify_signature_only(&binding);
        assert!(r.is_err(), "tampered body must be rejected: {r:?}");
    }

    #[test]
    fn verify_signature_only_rejects_swapped_address() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let (_, _, _, other_address) = test_keypair();
        let body = br#"{"user_id":"rEXAMPLE"}"#;
        let mut binding = make_binding(&sk, &pubkey_hex, &address, body);
        // Attacker stored a different address alongside the valid pubkey.
        binding.signer_address = other_address;
        let r = verify_signature_only(&binding);
        assert!(r.is_err(), "address mismatch must be rejected: {r:?}");
    }

    #[test]
    fn verify_signature_only_rejects_empty_body() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let binding = make_binding(&sk, &pubkey_hex, &address, b"");
        let r = verify_signature_only(&binding);
        assert!(r.is_err(), "empty body must be rejected: {r:?}");
    }
}
