//! HTTP client for the SGX enclave REST API.
//!
//! Rewrite of `sgx_client.py`. Talks to the enclave at `https://localhost:8085/v1`.
//!
//! Endpoints:
//!   POST /v1/pool/generate  - create new secp256k1 account
//!   POST /v1/pool/sign      - sign 32-byte hash with ECDSA
//!   GET  /v1/pool/status    - pool info

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Account generated inside the SGX enclave.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnclaveAccount {
    /// Ethereum-format address, "0x..." (42 chars)
    pub address: String,
    /// Uncompressed 65-byte public key, "0x..." (132 chars)
    pub public_key: String,
    /// Session key for signing requests, "0x..." (66 chars)
    pub session_key: String,
}

/// ECDSA signature returned by the enclave.
#[derive(Debug, Clone)]
pub struct EnclaveSignature {
    /// r component, 64-char hex (no 0x prefix)
    pub r: String,
    /// s component, 64-char hex (no 0x prefix)
    pub s: String,
    /// Recovery id: 27 or 28
    pub v: u8,
}

/// HTTP client for the SGX enclave API.
pub struct EnclaveClient {
    base_url: String,
    client: reqwest::Client,
}

impl EnclaveClient {
    /// Create a new client. TLS verification is disabled (self-signed enclave cert).
    pub fn new(base_url: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("failed to build reqwest client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Generate a new secp256k1 account inside the enclave.
    pub async fn generate_account(&self) -> Result<EnclaveAccount> {
        let url = format!("{}/pool/generate", self.base_url);
        let resp: serde_json::Value = self
            .client
            .post(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.get("status").and_then(|s| s.as_str()) != Some("success") {
            bail!("SGX generate failed: {}", resp);
        }

        Ok(EnclaveAccount {
            address: resp["address"]
                .as_str()
                .context("missing address")?
                .to_string(),
            public_key: resp["public_key"]
                .as_str()
                .context("missing public_key")?
                .to_string(),
            session_key: resp["session_key"]
                .as_str()
                .context("missing session_key")?
                .to_string(),
        })
    }

    /// Sign a 32-byte hash using ECDSA secp256k1.
    ///
    /// - `account_address`: "0x..." enclave account address
    /// - `session_key`: "0x..." session key
    /// - `hash_hex`: "0x..." 32-byte hash to sign (66 chars)
    pub async fn sign_hash(
        &self,
        account_address: &str,
        session_key: &str,
        hash_hex: &str,
    ) -> Result<EnclaveSignature> {
        let url = format!("{}/pool/sign", self.base_url);
        let payload = serde_json::json!({
            "from": account_address,
            "hash": hash_hex,
            "session_key": session_key,
        });

        let resp: serde_json::Value = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.get("status").and_then(|s| s.as_str()) != Some("success") {
            bail!("SGX sign failed: {}", resp);
        }

        let sig = &resp["signature"];
        Ok(EnclaveSignature {
            r: sig["r"]
                .as_str()
                .context("missing signature.r")?
                .to_string(),
            s: sig["s"]
                .as_str()
                .context("missing signature.s")?
                .to_string(),
            v: sig["v"]
                .as_u64()
                .context("missing signature.v")? as u8,
        })
    }

    /// Get enclave pool status.
    pub async fn pool_status(&self) -> Result<serde_json::Value> {
        let url = format!("{}/pool/status", self.base_url);
        let resp: serde_json::Value = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    /// Check if the enclave API is reachable.
    pub async fn is_available(&self) -> bool {
        // Strip /v1 suffix to hit /version
        let version_url = match self.base_url.rsplit_once("/v1") {
            Some((prefix, _)) => format!("{}/version", prefix),
            None => format!("{}/version", self.base_url),
        };

        self.client
            .get(&version_url)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}
