//! HTTP client for Path A pool endpoints (`/v1/pool/ecdh/*`,
//! `/v1/pool/attest/*`, `/v1/pool/frost/share-*-v2`).
//!
//! Paired with the enclave-side handlers added in phases 5c.1–5c.3:
//! ECDH identity, peer DCAP attestation, and v2 (ECDH+AES-GCM) FROST share
//! transport.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Client for the Path A pool REST API at `/v1/pool/*`.
pub struct PoolPathAClient {
    base_url: String,
    client: reqwest::Client,
}

/// Serialized v2 share envelope as returned by
/// `POST /v1/pool/frost/share-export-v2` and accepted by
/// `POST /v1/pool/frost/share-import-v2`. All hex, no `0x` prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareEnvelopeV2 {
    pub ceremony_nonce: String,
    pub iv: String,
    pub ct: String,
    pub tag: String,
    pub sender_pubkey: String,
    pub keygen_cache: String,
    pub threshold: u32,
    pub n_participants: u32,
}

#[allow(dead_code)]
impl PoolPathAClient {
    pub fn new(base_url: &str) -> Result<Self> {
        crate::http_helpers::ensure_loopback_url(base_url)
            .context("PoolPathAClient requires a loopback enclave URL (O-L4)")?;
        let client = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))
            .context("failed to build reqwest client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    // ── ECDH identity ───────────────────────────────────────────

    /// `GET /v1/pool/ecdh/pubkey` → 33-byte compressed secp256k1 pubkey (hex).
    pub async fn ecdh_pubkey(&self) -> Result<String> {
        let v = self.get("/pool/ecdh/pubkey").await?;
        Ok(v["pubkey"]
            .as_str()
            .context("pool/ecdh/pubkey: missing pubkey field")?
            .trim_start_matches("0x")
            .to_string())
    }

    /// `POST /v1/pool/ecdh/rotate` → new 33-byte pubkey (hex).
    #[allow(dead_code)]
    pub async fn ecdh_rotate(&self) -> Result<String> {
        let v = self
            .post("/pool/ecdh/rotate", serde_json::json!({}))
            .await?;
        Ok(v["pubkey"]
            .as_str()
            .context("pool/ecdh/rotate: missing pubkey field")?
            .trim_start_matches("0x")
            .to_string())
    }

    /// `POST /v1/pool/ecdh/report-data` → 64-byte DCAP report_data (hex).
    pub async fn ecdh_report_data(&self, shard_id: u32, group_id_hex: &str) -> Result<String> {
        let v = self
            .post(
                "/pool/ecdh/report-data",
                serde_json::json!({
                    "shard_id": shard_id,
                    "group_id": group_id_hex,
                }),
            )
            .await?;
        Ok(v["report_data"]
            .as_str()
            .context("pool/ecdh/report-data: missing report_data field")?
            .trim_start_matches("0x")
            .to_string())
    }

    // ── DCAP quote (bound to user-supplied report_data) ─────────

    /// `POST /v1/pool/attestation-quote` with `{user_data}` — returns the
    /// full DCAP quote bytes (lowercase hex, no `0x`) with `user_data`
    /// placed in the 64-byte `report_data` field. For Path A, callers pass
    /// the 64-byte output of `ecdh_report_data(shard_id, group_id)` as
    /// `user_data_hex`; the receiver's `verify_peer_quote` recomputes the
    /// same `report_data` formula and binds.
    pub async fn attestation_quote(&self, user_data_hex: &str) -> Result<String> {
        let v = self
            .post(
                "/pool/attestation-quote",
                serde_json::json!({ "user_data": user_data_hex }),
            )
            .await?;
        Ok(v["quote_hex"]
            .as_str()
            .context("pool/attestation-quote: missing quote_hex field")?
            .trim_start_matches("0x")
            .to_lowercase())
    }

    // ── Peer attestation ────────────────────────────────────────

    /// `POST /v1/pool/attest/verify-peer-quote` — verifies `quote` binds to
    /// `peer_pubkey`+`shard_id`+`group_id`, writes to the enclave's attest
    /// cache. Returns the verified peer MRENCLAVE on success.
    /// Returns `Ok(None)` on 403 refusal.
    /// `expiration_check_date` defaults to `now_ts`, `qve_isvsvn_threshold` to
    /// `0` server-side — not exposed here to keep the call site readable.
    pub async fn attest_verify_peer_quote(
        &self,
        quote_hex: &str,
        peer_pubkey_hex: &str,
        shard_id: u32,
        group_id_hex: &str,
        now_ts: u64,
    ) -> Result<Option<String>> {
        let body = serde_json::json!({
            "quote": quote_hex,
            "peer_pubkey": peer_pubkey_hex,
            "shard_id": shard_id,
            "group_id": group_id_hex,
            "now_ts": now_ts,
        });
        let url = format!("{}{}", self.base_url, "/pool/attest/verify-peer-quote");
        let resp = self.client.post(&url).json(&body).send().await?;
        if resp.status().as_u16() == 403 {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v["verified_mrenclave"]
            .as_str()
            .map(|s| s.trim_start_matches("0x").to_string()))
    }

    /// `POST /v1/pool/attest/peer-lookup` — read-only cache probe.
    /// Returns `Ok(None)` on 404 (miss/expired).
    pub async fn attest_peer_lookup(
        &self,
        peer_pubkey_hex: &str,
        shard_id: u32,
        group_id_hex: &str,
        now_ts: u64,
    ) -> Result<Option<String>> {
        let body = serde_json::json!({
            "peer_pubkey": peer_pubkey_hex,
            "shard_id": shard_id,
            "group_id": group_id_hex,
            "now_ts": now_ts,
        });
        let url = format!("{}{}", self.base_url, "/pool/attest/peer-lookup");
        let resp = self.client.post(&url).json(&body).send().await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v["verified_mrenclave"]
            .as_str()
            .map(|s| s.trim_start_matches("0x").to_string()))
    }

    // ── v2 FROST share transport ────────────────────────────────

    /// `POST /v1/pool/frost/share-export-v2` — peer MUST already be in attest
    /// cache (call `attest_verify_peer_quote` first). Returns sealed envelope.
    /// Returns `Ok(None)` on 403 refusal.
    pub async fn frost_share_export_v2(
        &self,
        signer_id: u32,
        peer_pubkey_hex: &str,
        shard_id: u32,
        group_id_hex: &str,
        now_ts: u64,
    ) -> Result<Option<ShareEnvelopeV2>> {
        let body = serde_json::json!({
            "signer_id": signer_id,
            "peer_pubkey": peer_pubkey_hex,
            "shard_id": shard_id,
            "group_id": group_id_hex,
            "now_ts": now_ts,
        });
        let url = format!("{}{}", self.base_url, "/pool/frost/share-export-v2");
        let resp = self.client.post(&url).json(&body).send().await?;
        if resp.status().as_u16() == 403 {
            return Ok(None);
        }
        let v: Value = resp.error_for_status()?.json().await?;
        let env = v["envelope"].clone();
        let parsed: ShareEnvelopeV2 =
            serde_json::from_value(env).context("failed to parse share envelope")?;
        Ok(Some(parsed))
    }

    /// `POST /v1/pool/frost/share-import-v2` — sender MUST already be in
    /// attest cache. Returns `false` on 403 refusal, `true` on success.
    pub async fn frost_share_import_v2(
        &self,
        envelope: &ShareEnvelopeV2,
        shard_id: u32,
        group_id_hex: &str,
        signer_id: u32,
        now_ts: u64,
    ) -> Result<bool> {
        // The enclave's import-v2 REST handler expects threshold,
        // n_participants, and sender_pubkey at the **top level** (it does
        // not look inside `envelope` for them). Lift them out so the
        // body matches the handler contract — see
        // server/api/v1/pool_handler.cpp::handleFrostShareImportV2.
        let body = serde_json::json!({
            "envelope": {
                "ceremony_nonce": envelope.ceremony_nonce,
                "iv": envelope.iv,
                "ct": envelope.ct,
                "tag": envelope.tag,
                "keygen_cache": envelope.keygen_cache,
            },
            "shard_id": shard_id,
            "group_id": group_id_hex,
            "signer_id": signer_id,
            "now_ts": now_ts,
            "threshold": envelope.threshold,
            "n_participants": envelope.n_participants,
            "sender_pubkey": envelope.sender_pubkey,
        });
        let url = format!("{}{}", self.base_url, "/pool/frost/share-import-v2");
        let resp = self.client.post(&url).json(&body).send().await?;
        if resp.status().as_u16() == 403 {
            return Ok(false);
        }
        resp.error_for_status()?;
        Ok(true)
    }

    // ── Internal ────────────────────────────────────────────────

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp: Value = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp: Value = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    //! Wire-shape unit tests for `PoolPathAClient`. Spins up an axum
    //! mock that mirrors the enclave-server's REST handlers (same
    //! method, path, required body fields, response shape) and
    //! exercises each client method against it. This is the
    //! same-language Rust mock referenced by SECURITY-REAUDIT-4-FIXPLAN
    //! Appendix C §1.2 — it kills the cross-language drift gap that
    //! produced APP-WIRE-1 (orchestrator's `frost_share_import_v2`
    //! body did not match the enclave handler's required fields).
    //!
    //! The mock asserts on body shape inline: any future client-side
    //! drift breaks compilation or fails an assertion at test time.
    use super::*;
    use axum::{
        extract::{Json as AxumJson, Query},
        routing::{get, post},
        Router,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::net::SocketAddr;

    async fn spawn_mock() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/pool/ecdh/pubkey", get(mock_ecdh_pubkey))
            .route("/v1/pool/ecdh/report-data", post(mock_ecdh_report_data))
            .route("/v1/pool/attestation-quote", post(mock_attestation_quote))
            .route(
                "/v1/pool/attest/verify-peer-quote",
                post(mock_attest_verify_peer_quote),
            )
            .route("/v1/pool/attest/peer-lookup", post(mock_attest_peer_lookup))
            .route(
                "/v1/pool/frost/share-export-v2",
                post(mock_frost_share_export_v2),
            )
            .route(
                "/v1/pool/frost/share-import-v2",
                post(mock_frost_share_import_v2),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, handle)
    }

    async fn mock_ecdh_pubkey() -> AxumJson<Value> {
        // 33 bytes compressed secp256k1 = 66 hex chars (without 0x).
        // Construct as "02" prefix + 32 bytes of payload.
        AxumJson(json!({
            "status": "success",
            "pubkey": "0x02".to_string() + &"aa".repeat(32),
        }))
    }

    async fn mock_ecdh_report_data(AxumJson(body): AxumJson<Value>) -> AxumJson<Value> {
        // Wire contract: shard_id (uint), group_id (32-byte hex string).
        assert!(body.get("shard_id").is_some());
        assert!(body.get("group_id").and_then(|v| v.as_str()).is_some());
        AxumJson(json!({
            "status": "success",
            "report_data": "0x".to_string() + &"11".repeat(64),
        }))
    }

    async fn mock_attestation_quote(AxumJson(body): AxumJson<Value>) -> AxumJson<Value> {
        // Wire contract: user_data is hex string.
        assert!(body.get("user_data").and_then(|v| v.as_str()).is_some());
        AxumJson(json!({
            "status": "success",
            "quote_hex": "0x".to_string() + &"22".repeat(64),
        }))
    }

    async fn mock_attest_verify_peer_quote(AxumJson(body): AxumJson<Value>) -> AxumJson<Value> {
        // Wire contract: every required field present.
        for f in ["quote", "peer_pubkey", "shard_id", "group_id", "now_ts"] {
            assert!(body.get(f).is_some(), "missing required field: {f}");
        }
        AxumJson(json!({
            "status": "success",
            "verified_mrenclave": "0x".to_string() + &"33".repeat(32),
        }))
    }

    async fn mock_attest_peer_lookup(AxumJson(body): AxumJson<Value>) -> AxumJson<Value> {
        for f in ["peer_pubkey", "shard_id", "group_id", "now_ts"] {
            assert!(body.get(f).is_some(), "missing required field: {f}");
        }
        AxumJson(json!({
            "status": "success",
            "verified_mrenclave": "0x".to_string() + &"44".repeat(32),
        }))
    }

    async fn mock_frost_share_export_v2(AxumJson(body): AxumJson<Value>) -> AxumJson<Value> {
        for f in ["signer_id", "peer_pubkey", "shard_id", "group_id", "now_ts"] {
            assert!(body.get(f).is_some(), "missing required field: {f}");
        }
        AxumJson(json!({
            "status": "success",
            "envelope": {
                "ceremony_nonce": "0x".to_string() + &"55".repeat(32),
                "iv":             "0x".to_string() + &"66".repeat(12),
                "ct":             "0x".to_string() + &"77".repeat(32),
                "tag":            "0x".to_string() + &"88".repeat(16),
                "sender_pubkey":  "0x".to_string() + &"99".repeat(33),
                "keygen_cache":   "0x".to_string() + &"aa".repeat(101),
                "threshold":      2,
                "n_participants": 3,
            }
        }))
    }

    /// APP-WIRE-1 lock: assert the body is in the *new* (lifted)
    /// shape — threshold / n_participants / sender_pubkey at the
    /// top level, not inside `envelope`. The enclave handler reads
    /// them only at the top level; if a future change buries them
    /// back inside `envelope` this test fails immediately.
    async fn mock_frost_share_import_v2(
        Query(_q): Query<HashMap<String, String>>,
        AxumJson(body): AxumJson<Value>,
    ) -> AxumJson<Value> {
        for f in [
            "envelope",
            "shard_id",
            "group_id",
            "signer_id",
            "now_ts",
            "threshold",
            "n_participants",
            "sender_pubkey",
        ] {
            assert!(
                body.get(f).is_some(),
                "missing required top-level field: {f} (APP-WIRE-1 regression?)"
            );
        }
        // Envelope must NOT carry the lifted fields at top level
        // (that's where the enclave handler refuses to look). The
        // shape we ship is "envelope = { ceremony_nonce, iv, ct,
        // tag, keygen_cache }" — five fields, no more.
        let env = body.get("envelope").unwrap();
        for f in ["ceremony_nonce", "iv", "ct", "tag", "keygen_cache"] {
            assert!(env.get(f).is_some(), "envelope missing nested field: {f}");
        }
        AxumJson(json!({"status": "success"}))
    }

    #[tokio::test]
    async fn ecdh_pubkey_strips_0x_prefix() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let pk = c.ecdh_pubkey().await.unwrap();
        assert!(!pk.starts_with("0x"));
        assert_eq!(pk.len(), 66, "33 bytes hex"); // 02 + 32 zero bytes
    }

    #[tokio::test]
    async fn ecdh_report_data_round_trip() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let rd = c.ecdh_report_data(0, &"a".repeat(64)).await.unwrap();
        assert_eq!(rd.len(), 128, "64 bytes hex");
    }

    #[tokio::test]
    async fn attest_verify_peer_quote_round_trip() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let mre = c
            .attest_verify_peer_quote("0xdeadbeef", "0x02aa", 0, &"b".repeat(64), 1234567890)
            .await
            .unwrap();
        assert!(mre.is_some());
        assert_eq!(mre.unwrap().len(), 64); // 32 bytes hex
    }

    #[tokio::test]
    async fn attest_peer_lookup_round_trip() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let mre = c
            .attest_peer_lookup("0x02aa", 0, &"c".repeat(64), 1234567890)
            .await
            .unwrap();
        assert!(mre.is_some());
    }

    #[tokio::test]
    async fn frost_share_export_v2_round_trip() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let env = c
            .frost_share_export_v2(1, "0x02aa", 0, &"d".repeat(64), 1234567890)
            .await
            .unwrap();
        let env = env.expect("export-v2 must return envelope on success");
        assert_eq!(env.threshold, 2);
        assert_eq!(env.n_participants, 3);
    }

    /// APP-WIRE-1 regression test: the import-v2 body must carry
    /// threshold / n_participants / sender_pubkey at top level. The
    /// mock asserts this; if the client reverts to nesting them, the
    /// mock's assert! fires and the test fails.
    #[tokio::test]
    async fn frost_share_import_v2_wire_shape_locked() {
        let (addr, _h) = spawn_mock().await;
        let c = PoolPathAClient::new(&format!("http://{addr}/v1")).unwrap();
        let envelope = ShareEnvelopeV2 {
            ceremony_nonce: "0x".to_string() + &"55".repeat(32),
            iv: "0x".to_string() + &"66".repeat(12),
            ct: "0x".to_string() + &"77".repeat(32),
            tag: "0x".to_string() + &"88".repeat(16),
            sender_pubkey: "0x".to_string() + &"99".repeat(33),
            keygen_cache: "0x".to_string() + &"aa".repeat(101),
            threshold: 2,
            n_participants: 3,
        };
        let ok = c
            .frost_share_import_v2(&envelope, 0, &"e".repeat(64), 1, 1234567890)
            .await
            .unwrap();
        assert!(ok, "import-v2 should succeed under matching wire");
    }
}
