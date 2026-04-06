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
use sha2::{Digest, Sha256};
use tracing::warn;

/// Authentication result: verified XRPL address.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub xrpl_address: String,
}

/// Extract and verify XRPL signature from request headers.
pub fn verify_request(
    headers: &HeaderMap,
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

    // Validate address format
    if !address.starts_with('r') || address.len() < 25 || address.len() > 35 {
        return Err("invalid XRPL address format".into());
    }

    // Validate pubkey format (33 bytes compressed = 66 hex chars)
    if pubkey_hex.len() != 66 {
        return Err("invalid public key length (expected 66 hex chars)".into());
    }

    // Decode public key
    let pubkey_bytes = hex::decode(pubkey_hex)
        .map_err(|_| "invalid public key hex")?;
    let verifying_key = VerifyingKey::from_sec1_bytes(&pubkey_bytes)
        .map_err(|_| "invalid secp256k1 public key")?;

    // Verify pubkey → XRPL address derivation
    // XRPL: SHA-256(compressed_pubkey) → RIPEMD-160 → Base58Check with prefix 0x00
    let sha256_hash = Sha256::digest(&pubkey_bytes);
    let ripemd_hash = Ripemd160::digest(&sha256_hash);

    // Base58Check: [0x00] + ripemd_hash + checksum
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(&ripemd_hash);
    let checksum = Sha256::digest(&Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);

    // XRPL uses its own Base58 alphabet
    const XRPL_ALPHABET: &str = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alpha_bytes: &[u8; 58] = XRPL_ALPHABET.as_bytes().try_into()
        .expect("XRPL alphabet is 58 chars");
    let alpha = bs58::Alphabet::new(alpha_bytes)
        .expect("valid alphabet");
    let derived_address = bs58::encode(&payload).with_alphabet(&alpha).into_string();

    if derived_address != address {
        return Err(format!(
            "address mismatch: header={}, derived from pubkey={}",
            address, derived_address
        ));
    }

    // Compute hash of body (or URI for GET)
    let hash = if body_bytes.is_empty() {
        Sha256::digest(uri_path.as_bytes())
    } else {
        Sha256::digest(body_bytes)
    };

    // Decode and verify signature
    let sig_bytes = hex::decode(sig_hex)
        .map_err(|_| "invalid signature hex")?;
    let signature = Signature::from_der(&sig_bytes)
        .map_err(|_| "invalid DER signature")?;

    // Verify ECDSA signature over pre-hashed data (SHA-256 already computed)
    if let Err(e) = verifying_key.verify_prehash(&hash, &signature) {
        tracing::debug!(
            hash_hex = %hex::encode(&hash),
            sig_hex = %sig_hex,
            pubkey_hex = %pubkey_hex,
            err = %e,
            "signature verification details"
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
    let ripemd_hash = Ripemd160::digest(&sha256_hash);
    let mut payload = vec![0x00u8];
    payload.extend_from_slice(&ripemd_hash);
    let checksum = Sha256::digest(&Sha256::digest(&payload));
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
    if uri == "/v1/openapi.json"
        || uri == "/v1/pool/status"
        || uri.starts_with("/v1/attestation/")
        || uri.starts_with("/v1/perp/liquidations/")
        || (method == "GET" && uri.starts_with("/v1/markets/"))
    {
        return next.run(request).await;
    }

    let headers = request.headers().clone();
    let uri_string = request.uri().to_string();

    // For requests with body, we need to read it for signature verification
    let (parts, body) = request.into_parts();
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

    match verify_request(&headers, &body_bytes, &uri_string) {
        Ok(user) => {
            // For POST with body: verify user_id in body matches authenticated address
            if !body_bytes.is_empty() {
                if let Ok(body_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                    if let Some(body_user_id) = body_json.get("user_id").and_then(|v| v.as_str()) {
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

            // Reconstruct request with body
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

    /// Helper: sign body and build auth headers.
    fn sign_body(sk: &SigningKey, pubkey_hex: &str, address: &str, body: &[u8]) -> HeaderMap {
        let hash = Sha256::digest(body);
        let (sig, _): (Signature, _) = sk.sign_prehash(&hash).unwrap();
        let sig_der = sig.to_der();

        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert("x-xrpl-signature", hex::encode(sig_der.as_bytes()).parse().unwrap());
        headers
    }

    /// Helper: sign URI path (for GET requests).
    fn sign_uri(sk: &SigningKey, pubkey_hex: &str, address: &str, uri: &str) -> HeaderMap {
        sign_body(sk, pubkey_hex, address, uri.as_bytes())
    }

    #[test]
    fn valid_post_signature_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"{\"user_id\":\"test\",\"side\":\"buy\"}";
        let headers = sign_body(&sk, &pubkey_hex, &address, body);
        let result = verify_request(&headers, body, "/v1/orders");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().xrpl_address, address);
    }

    #[test]
    fn valid_get_signature_passes() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let uri = "/v1/orders?user_id=rTest123";
        // GET: empty body, signs URI
        let headers = sign_uri(&sk, &pubkey_hex, &address, uri);
        let result = verify_request(&headers, &[], uri);
        assert!(result.is_ok());
    }

    #[test]
    fn missing_address_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-publickey", "aa".repeat(33).parse().unwrap());
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-Address header");
    }

    #[test]
    fn missing_pubkey_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", "rTest12345678901234567890".parse().unwrap());
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-PublicKey header");
    }

    #[test]
    fn missing_signature_header_fails() {
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", "rTest12345678901234567890".parse().unwrap());
        headers.insert("x-xrpl-publickey", "aa".repeat(33).parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "missing X-XRPL-Signature header");
    }

    #[test]
    fn invalid_address_format_rejected() {
        let (sk, _, pubkey_hex, _) = test_keypair();
        let body = b"test";
        let headers = sign_body(&sk, &pubkey_hex, "xNotAnAddress", body);
        let result = verify_request(&headers, body, "/");
        assert_eq!(result.unwrap_err(), "invalid XRPL address format");
    }

    #[test]
    fn address_too_short_rejected() {
        let (sk, _, pubkey_hex, _) = test_keypair();
        let headers = sign_body(&sk, &pubkey_hex, "rShort", b"test");
        let result = verify_request(&headers, b"test", "/");
        assert_eq!(result.unwrap_err(), "invalid XRPL address format");
    }

    #[test]
    fn wrong_pubkey_length_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", "rTest12345678901234567890".parse().unwrap());
        headers.insert("x-xrpl-publickey", "aabb".parse().unwrap()); // too short
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "invalid public key length (expected 66 hex chars)");
    }

    #[test]
    fn address_mismatch_rejected() {
        let (sk, _, pubkey_hex, _address) = test_keypair();
        let body = b"test";
        // Use a different (but valid-format) address
        let fake_address = "rFakeAddress1234567890123";
        let headers = sign_body(&sk, &pubkey_hex, fake_address, body);
        let result = verify_request(&headers, body, "/");
        assert!(result.unwrap_err().starts_with("address mismatch"));
    }

    #[test]
    fn wrong_signature_rejected() {
        let (sk, _, pubkey_hex, address) = test_keypair();
        let body = b"correct body";
        let headers = sign_body(&sk, &pubkey_hex, &address, b"different body");
        // Verify with correct body but signature was for different body
        let result = verify_request(&headers, body, "/");
        assert_eq!(result.unwrap_err(), "signature verification failed");
    }

    #[test]
    fn invalid_signature_hex_rejected() {
        let (_, _, pubkey_hex, address) = test_keypair();
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert("x-xrpl-signature", "not_hex!!".parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "invalid signature hex");
    }

    #[test]
    fn invalid_der_signature_rejected() {
        let (_, _, pubkey_hex, address) = test_keypair();
        let mut headers = HeaderMap::new();
        headers.insert("x-xrpl-address", address.parse().unwrap());
        headers.insert("x-xrpl-publickey", pubkey_hex.parse().unwrap());
        headers.insert("x-xrpl-signature", "deadbeef".parse().unwrap());
        let result = verify_request(&headers, b"body", "/");
        assert_eq!(result.unwrap_err(), "invalid DER signature");
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
        let (_, _, pubkey_hex, address) = test_keypair();
        assert!(address.starts_with('r'));
        assert!(address.len() >= 25 && address.len() <= 35);
    }

    #[test]
    fn different_keys_different_addresses() {
        let (_, _, _, addr1) = test_keypair();
        let (_, _, _, addr2) = test_keypair();
        assert_ne!(addr1, addr2);
    }
}
