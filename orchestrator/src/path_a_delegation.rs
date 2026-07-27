// Allowed because the consumer of LibP2PDelegationCollector — the
// /admin/migrate-state endpoint composing it into a ProductionEnclaveApi
// — lands in PRG-2 part 4/4. Tests in this file cover the wire-format
// builder and the timeout path; cargo dead-code analysis can't see the
// production wiring until that endpoint exists.
#![allow(dead_code)]

//! REQ-8 PRG-2 part 3/4 — Path A delegation bundle collector via libp2p.
//!
//! Closes the `EnclaveApi::collect_delegation_bundle` gap that
//! `HttpEnclaveApi` (PRG-2 part 2/4) explicitly stubbed. Reuses the
//! existing libp2p signing-relay infrastructure extended in
//! `p2p.rs` with the `PathADelegationRequest` / `PathADelegationRelay`
//! types.
//!
//! Flow:
//!   1. Ceremony driver calls `LibP2PDelegationCollector::collect(mre, nonce)`.
//!   2. Collector pushes a `PathADelegationRelay` down the
//!      mpsc to the p2p run-loop.
//!   3. p2p publishes `SigningMessage::PathADelegationRequest` on the
//!      signing topic. Each peer's local enclave signs locally
//!      (X-C1 hardened: receiver re-derives the canonical hash from
//!      bytes-on-wire) and replies with `SigningMessage::Response`.
//!   4. Responses flow back via the per-relay mpsc; collector
//!      accumulates them until quorum or timeout.
//!   5. Collector packages valid responses into the wire-format
//!      delegation_bundle bytes per REQ-7 amendment 2026-05-07 (b).
//!
//! Validation posture: the collector does NOT verify signatures or
//! check signer membership in the SealedSignerList — both are the
//! enclave's responsibility (`verify_delegation_bundle` in path_a.cpp).
//! Collector only enforces structural sanity (DER signature length
//! bounds, 33-byte pubkey) so a malformed gossipsub message can't
//! corrupt the bundle wire format.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::p2p::{PathADelegationRelay, SigningMessage};
use crate::path_a_ceremony::EnclaveApi;

/// Trait so the ceremony driver can be parameterised with a mock
/// collector for unit testing — same posture as `EnclaveApi`.
#[async_trait]
pub trait DelegationCollector: Send + Sync {
    async fn collect(&self, mrenclave_new: &[u8; 32], ceremony_nonce: &[u8; 32])
        -> Result<Vec<u8>>;
}

pub struct LibP2PDelegationCollector {
    relay_tx: mpsc::Sender<PathADelegationRelay>,
    timeout: Duration,
}

impl LibP2PDelegationCollector {
    /// Default 30-second collection window. Migration ceremonies are
    /// operator-driven (out-of-band coordination already happened), so
    /// the wall-clock between «driver publishes request» and «every
    /// awake operator's enclave has signed» is dominated by libp2p
    /// gossipsub propagation (sub-second on a healthy mesh) plus the
    /// ecall round-trip (~100 ms). 30 s is generous; exposes
    /// stragglers without blocking forever on an offline operator.
    pub fn new(relay_tx: mpsc::Sender<PathADelegationRelay>) -> Self {
        Self {
            relay_tx,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

#[async_trait]
impl DelegationCollector for LibP2PDelegationCollector {
    async fn collect(
        &self,
        mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let request_id = format!("pa-delegation-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = mpsc::channel(32);

        let relay = PathADelegationRelay {
            request_id: request_id.clone(),
            mrenclave_new: *mrenclave_new,
            ceremony_nonce: *ceremony_nonce,
            responses_tx,
        };
        self.relay_tx
            .send(relay)
            .await
            .context("send PathADelegationRelay to p2p run-loop")?;

        let mut entries: Vec<DelegationEntry> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => break, // sender dropped
                Err(_) => break,   // total timeout
            };
            if let Some(entry) = parse_delegation_response(&resp) {
                // Dedup by pk — the local-self-sign path sends a
                // response immediately + the same operator's gossipsub
                // round-trip might land too. Counting both as separate
                // entries inflates the apparent quorum on the
                // collector side; the enclave's verifier dedups too,
                // but a clean bundle is friendlier to operator logs.
                if !entries.iter().any(|e| e.pk == entry.pk) {
                    entries.push(entry);
                }
            }
        }

        if entries.is_empty() {
            return Err(anyhow!(
                "collected zero delegation responses within {:?}; \
                 check operator quorum is online + gossipsub mesh is healthy",
                self.timeout
            ));
        }

        Ok(build_delegation_bundle(&entries))
    }
}

/// Composed `EnclaveApi` impl: forwards every method except
/// `collect_delegation_bundle` to a wrapped HTTP client; routes
/// `collect_delegation_bundle` through a `DelegationCollector`. Lets
/// PRG-2 part 4/4's admin endpoint construct a single dyn-trait
/// client that production driver code uses.
pub struct ComposedEnclaveApi<H, C> {
    pub http: H,
    pub delegation: C,
}

#[async_trait]
impl<H, C> EnclaveApi for ComposedEnclaveApi<H, C>
where
    H: EnclaveApi + Send + Sync,
    C: DelegationCollector + Send + Sync,
{
    async fn get_target_info(&self, base: &str) -> Result<Vec<u8>> {
        self.http.get_target_info(base).await
    }
    async fn generate_keypair(
        &self,
        new_base: &str,
        expected_mrenclave_old: &[u8; 32],
        ceremony_nonce: &[u8; 32],
        target_info_of_old: &[u8; 512],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        self.http
            .generate_keypair(
                new_base,
                expected_mrenclave_old,
                ceremony_nonce,
                target_info_of_old,
            )
            .await
    }
    async fn collect_delegation_bundle(
        &self,
        mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
    ) -> Result<Vec<u8>> {
        self.delegation.collect(mrenclave_new, ceremony_nonce).await
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
        dry_run: bool,
    ) -> Result<crate::path_a_ceremony::ExportResult> {
        self.http
            .export_state(
                old_base,
                target_info_of_new,
                la_report_of_new,
                expected_mrenclave_new,
                ceremony_nonce,
                peer_pk_compressed,
                delegation_bundle,
                dry_run,
            )
            .await
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
        self.http
            .import_state(
                new_base,
                target_info_of_old,
                la_report_of_old,
                ceremony_nonce,
                ciphertext,
                ephemeral_pk,
                tag,
            )
            .await
    }
    async fn verify_import_confirmation(
        &self,
        old_base: &str,
        completion_la_report: &[u8; 432],
        expected_blob_hash: &[u8; 32],
        expected_ceremony_nonce: &[u8; 32],
        expected_manifest_hash: &[u8; 32],
    ) -> Result<()> {
        self.http
            .verify_import_confirmation(
                old_base,
                completion_la_report,
                expected_blob_hash,
                expected_ceremony_nonce,
                expected_manifest_hash,
            )
            .await
    }
}

// ── helpers ─────────────────────────────────────────────────────

#[derive(Debug)]
struct DelegationEntry {
    pk: Vec<u8>,  // 33 bytes compressed
    sig: Vec<u8>, // ECDSA-DER, 8..72 bytes
}

fn parse_delegation_response(msg: &SigningMessage) -> Option<DelegationEntry> {
    let SigningMessage::Response {
        der_signature,
        compressed_pubkey,
        error,
        ..
    } = msg
    else {
        return None;
    };
    if error.is_some() {
        return None;
    }
    let sig_hex = der_signature.as_ref()?;
    let pk_hex = compressed_pubkey.as_ref()?;
    let sig = hex::decode(sig_hex).ok()?;
    let pk = hex::decode(pk_hex).ok()?;
    if pk.len() != 33 {
        return None;
    }
    if sig.len() < 8 || sig.len() > 72 {
        return None;
    }
    Some(DelegationEntry { pk, sig })
}

/// Build wire-format delegation_bundle bytes per REQ-7 amendment
/// 2026-05-07 (b):
///   uint32_t version          = 1     (LE)
///   uint32_t entry_count            (LE)
///   for each entry:
///     uint8_t pk_compressed[33]
///     uint8_t sig_len               (8..72)
///     uint8_t sig[sig_len]
fn build_delegation_bundle(entries: &[DelegationEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        out.extend_from_slice(&entry.pk);
        out.push(entry.sig.len() as u8);
        out.extend_from_slice(&entry.sig);
    }
    out
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_response(sig_hex: &str, pk_hex: &str) -> SigningMessage {
        SigningMessage::Response {
            request_id: "pa-delegation-test".into(),
            signer_xrpl_address: "rTest".into(),
            der_signature: Some(sig_hex.to_string()),
            compressed_pubkey: Some(pk_hex.to_string()),
            error: None,
        }
    }

    #[test]
    fn parse_response_accepts_valid() {
        // Valid 33-byte pk + 70-byte sig (within 8..72 DER bound).
        let pk = "02".to_string() + &"AB".repeat(32);
        let sig = "30".to_string() + &"45".repeat(69);
        let resp = fake_response(&sig, &pk);
        let entry = parse_delegation_response(&resp).unwrap();
        assert_eq!(entry.pk.len(), 33);
        assert_eq!(entry.sig.len(), 70);
    }

    #[test]
    fn parse_response_rejects_short_sig() {
        let pk = "02".to_string() + &"AB".repeat(32);
        let sig = "3045".to_string(); // 2 bytes = too short
        let resp = fake_response(&sig, &pk);
        assert!(parse_delegation_response(&resp).is_none());
    }

    #[test]
    fn parse_response_rejects_wrong_pk_size() {
        let pk = "02".to_string() + &"AB".repeat(31); // 32 bytes, not 33
        let sig = "30".to_string() + &"45".repeat(35);
        let resp = fake_response(&sig, &pk);
        assert!(parse_delegation_response(&resp).is_none());
    }

    #[test]
    fn parse_response_rejects_error_response() {
        let resp = SigningMessage::Response {
            request_id: "pa-delegation-test".into(),
            signer_xrpl_address: "rTest".into(),
            der_signature: None,
            compressed_pubkey: None,
            error: Some("session key invalid".into()),
        };
        assert!(parse_delegation_response(&resp).is_none());
    }

    #[test]
    fn build_bundle_layout_matches_spec() {
        let pk1 = vec![0x02; 33];
        let sig1 = vec![0x30, 0x45, 0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03];
        let pk2 = vec![0x03; 33];
        let sig2 = vec![0x30; 70];
        let entries = vec![
            DelegationEntry {
                pk: pk1.clone(),
                sig: sig1.clone(),
            },
            DelegationEntry {
                pk: pk2.clone(),
                sig: sig2.clone(),
            },
        ];
        let bundle = build_delegation_bundle(&entries);

        // version(4) + entry_count(4) + entry1(33+1+8) + entry2(33+1+70)
        let expected_len = 4 + 4 + (33 + 1 + 8) + (33 + 1 + 70);
        assert_eq!(bundle.len(), expected_len);

        // version = 1 LE
        assert_eq!(&bundle[..4], &1u32.to_le_bytes());
        // entry_count = 2 LE
        assert_eq!(&bundle[4..8], &2u32.to_le_bytes());

        // entry 1: pk[33] || sig_len(1)=8 || sig[8]
        assert_eq!(&bundle[8..41], &pk1[..]);
        assert_eq!(bundle[41], 8);
        assert_eq!(&bundle[42..50], &sig1[..]);

        // entry 2: pk[33] || sig_len(1)=70 || sig[70]
        assert_eq!(&bundle[50..83], &pk2[..]);
        assert_eq!(bundle[83], 70);
        assert_eq!(&bundle[84..154], &sig2[..]);
    }

    #[test]
    fn build_bundle_zero_entries_is_well_formed() {
        let bundle = build_delegation_bundle(&[]);
        assert_eq!(bundle.len(), 8); // version(4) + entry_count(4) = 0
        assert_eq!(&bundle[..4], &1u32.to_le_bytes());
        assert_eq!(&bundle[4..8], &0u32.to_le_bytes());
    }

    #[tokio::test]
    async fn libp2p_collector_times_out_on_zero_responses() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PDelegationCollector::new(relay_tx).with_timeout(Duration::from_millis(100));
        let mre = [0u8; 32];
        let nonce = [0u8; 32];

        // Spawn a task to drain the relay channel without sending
        // any responses — simulates an offline gossipsub mesh.
        let drain = tokio::spawn(async move {
            let _relay = relay_rx.recv().await;
            // Hold the relay so its responses_tx isn't dropped; the
            // collector should still time out on the wall clock.
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let result = collector.collect(&mre, &nonce).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("zero delegation responses"));
        drain.abort();
    }

    #[tokio::test]
    async fn libp2p_collector_packages_one_response() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PDelegationCollector::new(relay_tx).with_timeout(Duration::from_millis(500));
        let mre = [0u8; 32];
        let nonce = [0u8; 32];

        let inject = tokio::spawn(async move {
            let relay = relay_rx.recv().await.unwrap();
            // Send one valid response.
            let pk = "02".to_string() + &"AB".repeat(32);
            let sig = "30".to_string() + &"45".repeat(69);
            let resp = SigningMessage::Response {
                request_id: relay.request_id.clone(),
                signer_xrpl_address: "rTest".into(),
                der_signature: Some(sig),
                compressed_pubkey: Some(pk),
                error: None,
            };
            relay.responses_tx.send(resp).await.unwrap();
            // Hold the sender alive until collector times out.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let bundle = collector.collect(&mre, &nonce).await.unwrap();
        // version(4) + count(4) + (pk[33] + sig_len[1] + sig[70]) = 112
        assert_eq!(bundle.len(), 4 + 4 + 33 + 1 + 70);
        assert_eq!(&bundle[4..8], &1u32.to_le_bytes());
        inject.abort();
    }

    #[tokio::test]
    async fn libp2p_collector_dedups_responses_by_pk() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PDelegationCollector::new(relay_tx).with_timeout(Duration::from_millis(500));
        let mre = [0u8; 32];
        let nonce = [0u8; 32];

        let inject = tokio::spawn(async move {
            let relay = relay_rx.recv().await.unwrap();
            // Send the SAME response twice — local-self-sign + gossipsub
            // round-trip duplication scenario.
            let pk = "02".to_string() + &"AB".repeat(32);
            let sig = "30".to_string() + &"45".repeat(69);
            for _ in 0..2 {
                let resp = SigningMessage::Response {
                    request_id: relay.request_id.clone(),
                    signer_xrpl_address: "rTest".into(),
                    der_signature: Some(sig.clone()),
                    compressed_pubkey: Some(pk.clone()),
                    error: None,
                };
                relay.responses_tx.send(resp).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let bundle = collector.collect(&mre, &nonce).await.unwrap();
        // entry_count must be 1, not 2.
        assert_eq!(&bundle[4..8], &1u32.to_le_bytes());
        inject.abort();
    }
}
