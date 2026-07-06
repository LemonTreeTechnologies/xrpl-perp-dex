//! β3.2b — the real `ProjectionSubmitter`: sign the XRPL `SignerListSet`
//! projection with the CURRENT on-chain quorum over the libp2p relay, submit it,
//! and poll (bounded, multi-ledger) until validated. This is the production
//! impl of the trait `run_projection` (β2d) was mock-tested against.
//!
//! The projection is authorised by the **current on-chain signer set** (the
//! outgoing operators — still on-chain through the sync-before-spend window).
//! Reuses the same relay-collection + submit + poll the audited
//! `signerlist_update::drive` uses; the only β2 difference is that the unsigned
//! tx is RENDERED FROM the sealed authority (membership_projection), not computed
//! from a chain read.
#![allow(dead_code)] // constructed by the β3.2b membership-change admin trigger

use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::membership_projection::{ProjectionConfirmedTx, ProjectionSubmitter};
use crate::p2p::{SigningMessage, SigningRelay};
use crate::signerlist_update::{poll_validated_tx_bounded, submit_multisigned};

const PER_SIGNER_TIMEOUT: Duration = Duration::from_secs(30);

/// Produces a confirmed projection tx by collecting current-quorum signatures
/// over the relay, submitting, and polling for on-ledger validation.
pub struct LibP2PProjectionSubmitter {
    signing_tx: mpsc::Sender<SigningRelay>,
    xrpl_url: String,
    /// The CURRENT on-chain signers that authorise the projection — each is
    /// (xrpl r-address, 20-byte AccountID hex). `quorum`-many must sign.
    current_signers: Vec<(String, String)>,
    quorum: u32,
    /// Bounded multi-ledger poll (Q-β2-6 / X-β3.2-4): a SignerListSet may take
    /// several ledgers; on timeout the caller leaves the epoch UNCONFIRMED.
    poll_attempts: u32,
    poll_interval_ms: u64,
}

impl LibP2PProjectionSubmitter {
    pub fn new(
        signing_tx: mpsc::Sender<SigningRelay>,
        xrpl_url: String,
        current_signers: Vec<(String, String)>,
        quorum: u32,
    ) -> Self {
        Self {
            signing_tx,
            xrpl_url,
            current_signers,
            quorum,
            // ~40 s default: testnet ledgers close ~every 4 s, so this spans
            // ~10 ledgers — generous for a SignerListSet to validate.
            poll_attempts: 20,
            poll_interval_ms: 2000,
        }
    }

    pub fn with_poll(mut self, attempts: u32, interval_ms: u64) -> Self {
        self.poll_attempts = attempts;
        self.poll_interval_ms = interval_ms;
        self
    }

    /// Collect signatures over the unsigned SignerListSet via the relay and
    /// assemble the multisigned tx (Signers[] sorted by AccountID). Split out so
    /// the collection/assembly is unit-tested with a mock relay (the submit +
    /// poll are XRPL IO).
    ///
    /// X-cosig-5 (RESP-β3.1-cosig, Option A logged interim): we collect from
    /// ALL current signers, not just quorum-many, so EVERY node's key is on the
    /// projection envelope. This is required because the β3.1 confirmation cosig
    /// check (`ecall_record_projection_confirmation` step 6(e)) currently demands
    /// the node's OWN key among the cosigners — so a node whose key is absent
    /// can never confirm and is stranded in the pending window. Collecting all N
    /// converges the live cluster today.
    ///
    /// KNOWN LIMITATION (removed once Option B — outgoing-quorum cosig in the
    /// enclave — ships): a node OFFLINE during the projection never gets its key
    /// on this tx, so it cannot confirm that epoch even after it returns; and
    /// the on-chain tx is N-of-N rather than M-of-N. This is convergence-only
    /// (no spend-safety impact) but must not be silently relied upon.
    async fn collect_and_assemble(
        &self,
        unsigned: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut collected: Vec<serde_json::Value> = Vec::new();
        // X-cosig-5 interim: collect ALL signers (no early break at quorum).
        for (xrpl_address, account_id_hex) in &self.current_signers {
            let request_id = format!("beta2-proj-{:016x}", rand::random::<u64>());
            let (resp_tx, resp_rx) = oneshot::channel();
            if self
                .signing_tx
                .send(SigningRelay {
                    request_id,
                    unsigned_tx: unsigned.clone(),
                    signer_account_id_hex: account_id_hex.clone(),
                    signer_xrpl_address: xrpl_address.clone(),
                    response_tx: resp_tx,
                })
                .await
                .is_err()
            {
                bail!("signing relay channel closed — orchestrator shutting down?");
            }
            match tokio::time::timeout(PER_SIGNER_TIMEOUT, resp_rx).await {
                Ok(Ok(SigningMessage::Response {
                    der_signature: Some(der),
                    compressed_pubkey: Some(pk),
                    error: None,
                    ..
                })) => {
                    info!(signer = %xrpl_address, "collected projection signature");
                    collected.push(serde_json::json!({
                        "Signer": {
                            "Account": xrpl_address,
                            "SigningPubKey": pk,
                            "TxnSignature": der,
                        }
                    }));
                }
                Ok(Ok(SigningMessage::Response { error: Some(e), .. })) => {
                    warn!(signer = %xrpl_address, error = %e, "projection signer rejected");
                }
                Ok(Ok(_)) => warn!(signer = %xrpl_address, "malformed projection signing response"),
                Ok(Err(_)) => warn!(signer = %xrpl_address, "projection signing channel dropped"),
                Err(_) => warn!(signer = %xrpl_address, "projection signing relay timeout"),
            }
        }

        if collected.len() < self.quorum as usize {
            bail!(
                "collected {} of {} projection signatures",
                collected.len(),
                self.quorum
            );
        }

        // X-cosig-5 interim: N-of-N (all available) rather than M-of-N. Logged
        // so the known limitation is visible in operator logs, not silent.
        warn!(
            collected = collected.len(),
            total = self.current_signers.len(),
            quorum = self.quorum,
            "X-cosig-5 interim: projection signed by ALL available signers (N-of-N) \
             so every node can confirm; an offline-during-projection node still \
             cannot confirm that epoch — remove once Option B (enclave outgoing-\
             quorum cosig) ships"
        );

        // XRPL canonical: Signers[] sorted ascending by AccountID.
        collected.sort_by(|a, b| {
            let aa = crate::xrpl_signer::decode_xrpl_address(
                a.pointer("/Signer/Account")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .unwrap_or([0xff; 20]);
            let bb = crate::xrpl_signer::decode_xrpl_address(
                b.pointer("/Signer/Account")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .unwrap_or([0xff; 20]);
            aa.cmp(&bb)
        });

        let mut full_tx = unsigned.clone();
        full_tx["Signers"] = serde_json::Value::Array(collected);
        Ok(full_tx)
    }
}

#[async_trait]
impl ProjectionSubmitter for LibP2PProjectionSubmitter {
    async fn sign_submit_confirm(
        &self,
        unsigned_signerlist_set: &serde_json::Value,
    ) -> Result<ProjectionConfirmedTx> {
        let full_tx = self.collect_and_assemble(unsigned_signerlist_set).await?;

        let tx_hash_hex = submit_multisigned(&self.xrpl_url, &full_tx)
            .await
            .context("submit the SignerListSet projection")?;

        // Bounded multi-ledger poll; on timeout this errors and the driver
        // records nothing (epoch stays projection-UNCONFIRMED — safe, Q-β2-6).
        let (tx_blob_hex, ledger_index) = poll_validated_tx_bounded(
            &self.xrpl_url,
            &tx_hash_hex,
            self.poll_attempts,
            self.poll_interval_ms,
        )
        .await
        .context("poll the SignerListSet projection to on-ledger validation")?;

        let signed_tx_blob = hex::decode(&tx_blob_hex).context("validated tx blob not hex")?;
        let tx_hash_bytes = hex::decode(&tx_hash_hex).context("tx_hash not hex")?;
        if tx_hash_bytes.len() != 32 {
            bail!("tx_hash is {} bytes, want 32", tx_hash_bytes.len());
        }
        let mut tx_hash = [0u8; 32];
        tx_hash.copy_from_slice(&tx_hash_bytes);

        Ok(ProjectionConfirmedTx {
            signed_tx_blob,
            tx_hash,
            ledger_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_min_signerlist() -> serde_json::Value {
        serde_json::json!({
            "TransactionType": "SignerListSet",
            "Account": "rEscrowXXXXXXXXXXXXXXXXXXXXXXXX",
            "SignerQuorum": 2,
            "SignerEntries": [],
        })
    }

    /// A mock relay that replies with a valid signature for every request.
    /// Drives `collect_and_assemble` without XRPL IO.
    #[tokio::test]
    async fn collect_and_assemble_quorum_met_sorts_signers() {
        use crate::membership_projection::account_id_to_r_address;
        let (tx, mut rx) = mpsc::channel::<SigningRelay>(8);
        // Valid r-addresses generated from AccountIDs (round-trip through
        // decode_xrpl_address). Supplied 0x05 BEFORE 0x01 so the assembly must
        // SORT 0x01 first.
        let signers = vec![
            (account_id_to_r_address(&[0x05; 20]), "05".repeat(20)),
            (account_id_to_r_address(&[0x01; 20]), "01".repeat(20)),
        ];
        let submitter =
            LibP2PProjectionSubmitter::new(tx, "http://xrpl".into(), signers.clone(), 2);

        // responder: reply to each request with a fixed valid sig + pk.
        let responder = tokio::spawn(async move {
            while let Some(relay) = rx.recv().await {
                let _ = relay.response_tx.send(SigningMessage::Response {
                    request_id: relay.request_id,
                    signer_xrpl_address: relay.signer_xrpl_address.clone(),
                    der_signature: Some("30".to_string() + &"45".repeat(35)),
                    compressed_pubkey: Some("02".to_string() + &"ab".repeat(32)),
                    error: None,
                });
            }
        });

        let full = submitter
            .collect_and_assemble(&render_min_signerlist())
            .await
            .unwrap();
        let arr = full["Signers"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // sorted ascending by AccountID → 0x01 first despite being supplied second.
        let a0 =
            crate::xrpl_signer::decode_xrpl_address(arr[0]["Signer"]["Account"].as_str().unwrap())
                .unwrap();
        assert_eq!(a0, [0x01; 20], "Signers not sorted by AccountID");
        responder.abort();
    }

    /// Quorum not met → error, nothing assembled.
    #[tokio::test]
    async fn collect_and_assemble_quorum_not_met_errors() {
        let (tx, mut rx) = mpsc::channel::<SigningRelay>(8);
        let signers = vec![(
            crate::membership_projection::account_id_to_r_address(&[0x01; 20]),
            "01".repeat(20),
        )];
        let submitter = LibP2PProjectionSubmitter::new(tx, "http://xrpl".into(), signers, 2);

        // responder: reject every request (error) → zero collected.
        let responder = tokio::spawn(async move {
            while let Some(relay) = rx.recv().await {
                let _ = relay.response_tx.send(SigningMessage::Response {
                    request_id: relay.request_id,
                    signer_xrpl_address: relay.signer_xrpl_address.clone(),
                    der_signature: None,
                    compressed_pubkey: None,
                    error: Some("nope".into()),
                });
            }
        });

        let err = submitter
            .collect_and_assemble(&render_min_signerlist())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("of 2 projection signatures"), "got: {err}");
        responder.abort();
    }

    /// X-cosig-5 interim: with quorum 2 but 3 signers, collect ALL 3 (not just
    /// quorum-many) so every node's key lands on the projection envelope and
    /// every node can pass the β3.1 own-key cosig check.
    #[tokio::test]
    async fn collect_and_assemble_gathers_all_n_not_just_quorum() {
        use crate::membership_projection::account_id_to_r_address;
        let (tx, mut rx) = mpsc::channel::<SigningRelay>(8);
        let signers = vec![
            (account_id_to_r_address(&[0x01; 20]), "01".repeat(20)),
            (account_id_to_r_address(&[0x02; 20]), "02".repeat(20)),
            (account_id_to_r_address(&[0x03; 20]), "03".repeat(20)),
        ];
        // quorum 2, but 3 signers available → interim must collect all 3.
        let submitter = LibP2PProjectionSubmitter::new(tx, "http://xrpl".into(), signers, 2);

        let responder = tokio::spawn(async move {
            while let Some(relay) = rx.recv().await {
                let _ = relay.response_tx.send(SigningMessage::Response {
                    request_id: relay.request_id,
                    signer_xrpl_address: relay.signer_xrpl_address.clone(),
                    der_signature: Some("30".to_string() + &"45".repeat(35)),
                    compressed_pubkey: Some("02".to_string() + &"ab".repeat(32)),
                    error: None,
                });
            }
        });

        let full = submitter
            .collect_and_assemble(&render_min_signerlist())
            .await
            .unwrap();
        let arr = full["Signers"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            3,
            "interim must collect ALL N signers, not quorum-many"
        );
        responder.abort();
    }
}
