// Allowed because the optional constructors (with_strict_tls,
// with_client) and the public trait impl are consumed by the
// /admin/migrate-state endpoint that lands in PRG-2 part 4/4. Tests
// in this file cover the helpers + collect_delegation_bundle stub.
#![allow(dead_code)]

//! REQ-8 PRG-2 part 2/4 — `EnclaveApi` trait impl over real HTTPS.
//!
//! Routes traffic to the C++ REST endpoints landed in PRG-2 part 1/4
//! (`/v1/path-a/*` on the enclave's port). Replaces the `MockApi` used
//! by `path_a_ceremony.rs` tests for production deployment.
//!
//! The HTTP client uses `reqwest` with rustls-tls and accepts self-
//! signed certificates by default (the enclave uses a self-signed TLS
//! cert; the operator's reverse-proxy/HSTS layer is what actually
//! protects the public-facing endpoint). For prod, callers can opt
//! into stricter cert validation via `HttpEnclaveApi::with_strict_tls`.
//!
//! `collect_delegation_bundle` is intentionally NOT implemented at
//! this commit — that's PRG-2 part 3/4 (libp2p signing-relay reuse).
//! Calling it returns an explicit error so operators driving an
//! incomplete deployment see the failure clearly rather than running
//! into mysterious quorum failures at OLD's export step.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::path_a_ceremony::{EnclaveApi, ExportResult};

/// Production `EnclaveApi` implementation. Constructed with a
/// reqwest::Client; the caller can swap in a mocked client for
/// integration tests that drive a real local HTTPS server.
pub struct HttpEnclaveApi {
    client: reqwest::Client,
}

impl HttpEnclaveApi {
    /// Default constructor: accepts self-signed TLS certs (the enclave
    /// uses one) and applies a 30 s per-request timeout. Override
    /// either via `with_client` if your topology demands stricter
    /// behaviour.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
            .context("build HttpEnclaveApi reqwest client")?;
        Ok(Self { client })
    }

    /// Strict-TLS variant: verify enclave cert against the system trust
    /// store. Use when the operator has fronted the enclave with a
    /// proper TLS cert (Let's Encrypt + reverse proxy).
    pub fn with_strict_tls() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build HttpEnclaveApi reqwest client (strict TLS)")?;
        Ok(Self { client })
    }

    /// Bring-your-own-client (integration tests, custom timeouts).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

// ── Wire-format DTOs ─────────────────────────────────────────────
//
// JSON shapes mirror the C++ PathAHandler routes from PRG-2 part 1/4.
// Byte buffers are hex-encoded (lowercase, no 0x prefix). Names match
// the C++ side verbatim so a handler-level type mismatch surfaces as
// a deserialise error rather than a silent field skip.

#[derive(Deserialize)]
struct ResponseEnvelope {
    status: String,
    #[serde(default)]
    code: Option<i32>,
    #[serde(default)]
    message: Option<String>,
}

fn check_status(env: &ResponseEnvelope, route: &str) -> Result<()> {
    if env.status == "ok" {
        return Ok(());
    }
    let code = env.code.unwrap_or(0);
    let msg = env.message.as_deref().unwrap_or("(no message)");
    Err(anyhow!(
        "enclave {route} returned status={} code={code} message={msg}",
        env.status
    ))
}

#[derive(Deserialize)]
struct TargetInfoResponse {
    #[serde(flatten)]
    envelope: ResponseEnvelope,
    #[serde(default)]
    target_info_hex: Option<String>,
}

#[derive(Serialize)]
struct GenerateKeypairRequest<'a> {
    expected_mrenclave_old_hex: &'a str,
    ceremony_nonce_hex: &'a str,
    target_info_of_old_hex: &'a str,
}

#[derive(Deserialize)]
struct GenerateKeypairResponse {
    #[serde(flatten)]
    envelope: ResponseEnvelope,
    #[serde(default)]
    peer_pk_compressed_hex: Option<String>,
    #[serde(default)]
    la_report_of_new_hex: Option<String>,
}

#[derive(Serialize)]
struct ExportStateRequest<'a> {
    target_info_of_new_hex: &'a str,
    la_report_of_new_hex: &'a str,
    expected_mrenclave_new_hex: &'a str,
    ceremony_nonce_hex: &'a str,
    peer_pk_compressed_hex: &'a str,
    delegation_bundle_hex: &'a str,
}

#[derive(Deserialize)]
struct ExportStateResponse {
    #[serde(flatten)]
    envelope: ResponseEnvelope,
    #[serde(default)]
    ciphertext_hex: Option<String>,
    #[serde(default)]
    ephemeral_pk_hex: Option<String>,
    #[serde(default)]
    tag_hex: Option<String>,
    #[serde(default)]
    la_report_old_hex: Option<String>,
}

#[derive(Serialize)]
struct ImportStateRequest<'a> {
    target_info_of_old_hex: &'a str,
    la_report_of_old_hex: &'a str,
    ceremony_nonce_hex: &'a str,
    ciphertext_hex: &'a str,
    ephemeral_pk_hex: &'a str,
    tag_hex: &'a str,
}

#[derive(Deserialize)]
struct ImportStateResponse {
    #[serde(flatten)]
    envelope: ResponseEnvelope,
    #[serde(default)]
    completion_la_report_hex: Option<String>,
}

#[derive(Serialize)]
struct VerifyConfirmationRequest<'a> {
    completion_la_report_hex: &'a str,
    expected_blob_hash_hex: &'a str,
    expected_ceremony_nonce_hex: &'a str,
    expected_manifest_hash_hex: &'a str,
}

// ── Helpers ─────────────────────────────────────────────────────

fn require_field<'a>(field: &'a Option<String>, name: &str, route: &str) -> Result<&'a str> {
    field
        .as_deref()
        .ok_or_else(|| anyhow!("enclave {route} response missing {name}"))
}

fn decode_hex_field(field: &str, name: &str, route: &str) -> Result<Vec<u8>> {
    hex::decode(field).map_err(|e| anyhow!("enclave {route} {name} not valid hex: {e}"))
}

fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

#[async_trait]
impl EnclaveApi for HttpEnclaveApi {
    async fn get_target_info(&self, base: &str) -> Result<Vec<u8>> {
        let url = format!("{base}/v1/path-a/target-info");
        let resp: TargetInfoResponse = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            // Empty body — the route accepts and ignores any body.
            .body("")
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .json()
            .await
            .with_context(|| format!("decode JSON from {url}"))?;
        check_status(&resp.envelope, "target-info")?;
        let h = require_field(&resp.target_info_hex, "target_info_hex", "target-info")?;
        let b = decode_hex_field(h, "target_info_hex", "target-info")?;
        if b.len() != 512 {
            return Err(anyhow!("target-info expected 512 bytes; got {}", b.len()));
        }
        Ok(b)
    }

    async fn generate_keypair(
        &self,
        new_base: &str,
        expected_mrenclave_old: &[u8; 32],
        ceremony_nonce: &[u8; 32],
        target_info_of_old: &[u8; 512],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let url = format!("{new_base}/v1/path-a/generate-keypair");
        let mre_hex = hex_lower(expected_mrenclave_old);
        let nonce_hex = hex_lower(ceremony_nonce);
        let ti_hex = hex_lower(target_info_of_old);
        let body = GenerateKeypairRequest {
            expected_mrenclave_old_hex: &mre_hex,
            ceremony_nonce_hex: &nonce_hex,
            target_info_of_old_hex: &ti_hex,
        };
        let resp: GenerateKeypairResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .json()
            .await
            .with_context(|| format!("decode JSON from {url}"))?;
        check_status(&resp.envelope, "generate-keypair")?;
        let pk = decode_hex_field(
            require_field(
                &resp.peer_pk_compressed_hex,
                "peer_pk_compressed_hex",
                "generate-keypair",
            )?,
            "peer_pk_compressed_hex",
            "generate-keypair",
        )?;
        let la = decode_hex_field(
            require_field(
                &resp.la_report_of_new_hex,
                "la_report_of_new_hex",
                "generate-keypair",
            )?,
            "la_report_of_new_hex",
            "generate-keypair",
        )?;
        if pk.len() != 33 {
            return Err(anyhow!(
                "peer_pk_compressed_hex expected 33 bytes; got {}",
                pk.len()
            ));
        }
        if la.len() != 432 {
            return Err(anyhow!(
                "la_report_of_new_hex expected 432 bytes; got {}",
                la.len()
            ));
        }
        Ok((pk, la))
    }

    async fn collect_delegation_bundle(
        &self,
        _mrenclave_new: &[u8; 32],
        _ceremony_nonce: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // PRG-2 part 3/4: libp2p signing-relay reuse for delegation
        // collection is the dedicated next sub-item. Until then, the
        // HTTP client surfaces an explicit error so an operator
        // attempting to drive a full ceremony with this incomplete
        // build sees the gap clearly rather than running into a
        // mysterious quorum failure at OLD's export step.
        Err(anyhow!(
            "collect_delegation_bundle not implemented in HttpEnclaveApi yet — \
             PRG-2 part 3/4 (libp2p signing-relay reuse) is the next sub-item. \
             For development against a single-operator testnet, supply a \
             pre-computed delegation_bundle externally via an alternative \
             EnclaveApi impl that wraps HttpEnclaveApi and overrides this \
             one method."
        ))
    }

    async fn export_state(
        &self,
        old_base: &str,
        target_info_of_new: &[u8; 512],
        la_report_of_new: &[u8; 432],
        expected_mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
        peer_pk_compressed: &[u8; 33],
        delegation_bundle: &[u8],
    ) -> Result<ExportResult> {
        let url = format!("{old_base}/v1/path-a/export-state");
        let ti_new_hex = hex_lower(target_info_of_new);
        let la_report_new_hex = hex_lower(la_report_of_new);
        let mre_new_hex = hex_lower(expected_mrenclave_new);
        let nonce_hex = hex_lower(ceremony_nonce);
        let peer_pk_hex = hex_lower(peer_pk_compressed);
        let delegation_hex = hex_lower(delegation_bundle);
        let body = ExportStateRequest {
            target_info_of_new_hex: &ti_new_hex,
            la_report_of_new_hex: &la_report_new_hex,
            expected_mrenclave_new_hex: &mre_new_hex,
            ceremony_nonce_hex: &nonce_hex,
            peer_pk_compressed_hex: &peer_pk_hex,
            delegation_bundle_hex: &delegation_hex,
        };
        let resp: ExportStateResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .json()
            .await
            .with_context(|| format!("decode JSON from {url}"))?;
        check_status(&resp.envelope, "export-state")?;

        let ciphertext = decode_hex_field(
            require_field(&resp.ciphertext_hex, "ciphertext_hex", "export-state")?,
            "ciphertext_hex",
            "export-state",
        )?;
        let ephemeral_pk_vec = decode_hex_field(
            require_field(&resp.ephemeral_pk_hex, "ephemeral_pk_hex", "export-state")?,
            "ephemeral_pk_hex",
            "export-state",
        )?;
        let tag_vec = decode_hex_field(
            require_field(&resp.tag_hex, "tag_hex", "export-state")?,
            "tag_hex",
            "export-state",
        )?;
        let la_report_old_vec = decode_hex_field(
            require_field(&resp.la_report_old_hex, "la_report_old_hex", "export-state")?,
            "la_report_old_hex",
            "export-state",
        )?;

        if ephemeral_pk_vec.len() != 33 {
            return Err(anyhow!(
                "ephemeral_pk expected 33 bytes; got {}",
                ephemeral_pk_vec.len()
            ));
        }
        if tag_vec.len() != 16 {
            return Err(anyhow!("tag expected 16 bytes; got {}", tag_vec.len()));
        }
        if la_report_old_vec.len() != 432 {
            return Err(anyhow!(
                "la_report_old expected 432 bytes; got {}",
                la_report_old_vec.len()
            ));
        }

        let mut ephemeral_pk = [0u8; 33];
        ephemeral_pk.copy_from_slice(&ephemeral_pk_vec);
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&tag_vec);
        let mut la_report_old = [0u8; 432];
        la_report_old.copy_from_slice(&la_report_old_vec);

        Ok(ExportResult {
            ciphertext,
            ephemeral_pk,
            tag,
            la_report_old,
        })
    }

    async fn import_state(
        &self,
        new_base: &str,
        target_info_of_old: &[u8; 512],
        la_report_of_old: &[u8; 432],
        ceremony_nonce: &[u8; 32],
        ciphertext: &[u8],
        ephemeral_pk: &[u8; 33],
        tag: &[u8; 16],
    ) -> Result<Vec<u8>> {
        let url = format!("{new_base}/v1/path-a/import-state");
        let ti_old_hex = hex_lower(target_info_of_old);
        let la_report_old_hex = hex_lower(la_report_of_old);
        let nonce_hex = hex_lower(ceremony_nonce);
        let ct_hex = hex_lower(ciphertext);
        let pk_hex = hex_lower(ephemeral_pk);
        let tag_hex = hex_lower(tag);
        let body = ImportStateRequest {
            target_info_of_old_hex: &ti_old_hex,
            la_report_of_old_hex: &la_report_old_hex,
            ceremony_nonce_hex: &nonce_hex,
            ciphertext_hex: &ct_hex,
            ephemeral_pk_hex: &pk_hex,
            tag_hex: &tag_hex,
        };
        let resp: ImportStateResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .json()
            .await
            .with_context(|| format!("decode JSON from {url}"))?;
        check_status(&resp.envelope, "import-state")?;
        let cr = decode_hex_field(
            require_field(
                &resp.completion_la_report_hex,
                "completion_la_report_hex",
                "import-state",
            )?,
            "completion_la_report_hex",
            "import-state",
        )?;
        if cr.len() != 432 {
            return Err(anyhow!(
                "completion_la_report expected 432 bytes; got {}",
                cr.len()
            ));
        }
        Ok(cr)
    }

    async fn verify_import_confirmation(
        &self,
        old_base: &str,
        completion_la_report: &[u8; 432],
        expected_blob_hash: &[u8; 32],
        expected_ceremony_nonce: &[u8; 32],
        expected_manifest_hash: &[u8; 32],
    ) -> Result<()> {
        let url = format!("{old_base}/v1/path-a/verify-confirmation");
        let report_hex = hex_lower(completion_la_report);
        let blob_hex = hex_lower(expected_blob_hash);
        let nonce_hex = hex_lower(expected_ceremony_nonce);
        let manif_hex = hex_lower(expected_manifest_hash);
        let body = VerifyConfirmationRequest {
            completion_la_report_hex: &report_hex,
            expected_blob_hash_hex: &blob_hex,
            expected_ceremony_nonce_hex: &nonce_hex,
            expected_manifest_hash_hex: &manif_hex,
        };
        let resp: ResponseEnvelope = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .json()
            .await
            .with_context(|| format!("decode JSON from {url}"))?;
        check_status(&resp, "verify-confirmation")
    }
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_ok_passes() {
        let env = ResponseEnvelope {
            status: "ok".to_string(),
            code: None,
            message: None,
        };
        check_status(&env, "test-route").unwrap();
    }

    #[test]
    fn check_status_error_includes_code_and_message() {
        let env = ResponseEnvelope {
            status: "error".to_string(),
            code: Some(-150),
            message: Some("la_get_target_info failed".to_string()),
        };
        let err = check_status(&env, "target-info").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("-150"), "missing code in error: {msg}");
        assert!(
            msg.contains("la_get_target_info failed"),
            "missing message: {msg}"
        );
        assert!(msg.contains("target-info"), "missing route: {msg}");
    }

    #[test]
    fn require_field_surfaces_missing_name() {
        let none: Option<String> = None;
        let err = require_field(&none, "ciphertext_hex", "export-state").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ciphertext_hex"), "missing field name: {msg}");
        assert!(msg.contains("export-state"), "missing route: {msg}");
    }

    #[test]
    fn hex_lower_round_trips() {
        let bytes = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF, 0x42];
        let s = hex_lower(&bytes);
        assert_eq!(s, "deadbeef00ff42");
        let decoded = decode_hex_field(&s, "test", "test").unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn decode_hex_field_rejects_invalid() {
        let err = decode_hex_field("not-hex!!", "test", "test").unwrap_err();
        assert!(err.to_string().contains("not valid hex"));
    }

    #[tokio::test]
    async fn collect_delegation_bundle_returns_clear_not_implemented() {
        // The HttpEnclaveApi explicitly stubs collect_delegation_bundle
        // until PRG-2 part 3/4 lands. An operator running an incomplete
        // ceremony driver gets a clear error rather than mysterious
        // quorum failures downstream.
        let api = HttpEnclaveApi::new().unwrap();
        let mre = [0u8; 32];
        let nonce = [0u8; 32];
        let err = api
            .collect_delegation_bundle(&mre, &nonce)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("PRG-2 part 3/4"), "missing PRG ref in: {msg}");
        assert!(
            msg.contains("collect_delegation_bundle not implemented"),
            "missing not-implemented marker: {msg}"
        );
    }
}
