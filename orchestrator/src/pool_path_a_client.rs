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
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()
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
        let body = serde_json::json!({
            "envelope": envelope,
            "shard_id": shard_id,
            "group_id": group_id_hex,
            "signer_id": signer_id,
            "now_ts": now_ts,
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
