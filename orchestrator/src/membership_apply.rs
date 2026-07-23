//! β3.2b — the cluster apply-broadcast: the production `ClusterSealApplier` +
//! `ClusterConfirmApplier`.
//!
//! Under the loopback-enclave topology (X-C1: the enclave admin API is NEVER
//! network-exposed — only the orchestrator's api/p2p ports are), a sealed epoch
//! (β1) or a projection confirmation (β2) CANNOT be applied by POSTing to a
//! remote node's enclave. So the driving node broadcasts ONE `MembershipApply`
//! over the p2p signing topic; every node (including the driver itself) applies
//! it to its OWN localhost enclave and publishes an ack. This realises the
//! cluster-wide (P) single-successor: ONE bundle, ONE broadcast, applied
//! identically everywhere — no per-node independent collection (which would
//! risk split-brain).
//!
//! The local apply is performed by the audited `HttpEpochSealSink` /
//! `HttpProjectionConfirmer` adapters pointed at localhost (inside the p2p
//! run-loop's `handle_membership_apply`); this module is only the broadcast +
//! ack-collection half.
#![allow(dead_code)] // constructed by the β3.2b membership-change admin trigger

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use crate::membership_coordinator::{
    ClusterGenesisApplier, ClusterSealApplier, MembershipEpochStatement, NodeSealResult,
};
use crate::membership_projection::{ClusterConfirmApplier, NodeConfirmResult};
use crate::mrenclave_governance::{ClusterGovernApplier, GovernanceOp};
use crate::p2p::{
    MembershipApplyPayload, MembershipApplyRelay, MembershipSignerWire, SigningMessage,
};

/// One node's ack of an apply broadcast, deduped by the node's signer address.
struct NodeApplyAck {
    node: String,
    ok: bool,
    error: Option<String>,
}

/// Broadcasts a `MembershipApply` (seal or confirm) and collects per-node acks.
/// Implements BOTH driver applier traits — both operations are "broadcast one
/// payload, gather acks", differing only in the payload + result type.
pub struct LibP2PMembershipApplier {
    apply_tx: mpsc::Sender<MembershipApplyRelay>,
    /// Distinct node acks expected (the cluster roster size). A shortfall means
    /// at least one node did not apply within the window → reported as a failed
    /// aggregate entry so the outcome is NOT `all_sealed()`/`all_recorded()` and
    /// the operator retries (the enclave's monotonic/idempotent guards make a
    /// retry safe).
    expected_nodes: usize,
    timeout: Duration,
}

impl LibP2PMembershipApplier {
    /// 45 s default window — gossipsub propagation + one ecall per node, with
    /// headroom over the β1 collector's 30 s so a slow node still lands.
    pub fn new(apply_tx: mpsc::Sender<MembershipApplyRelay>, expected_nodes: usize) -> Self {
        Self {
            apply_tx,
            expected_nodes: expected_nodes.max(1),
            timeout: Duration::from_secs(45),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Send one apply relay, then collect per-node acks until `expected_nodes`
    /// distinct successes land or the window closes. Dedups by node address (the
    /// local self-apply and that node's gossipsub round-trip can both arrive).
    async fn broadcast_and_collect(
        &self,
        payload: MembershipApplyPayload,
    ) -> Result<Vec<NodeApplyAck>> {
        let request_id = format!("beta-apply-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = mpsc::channel(32);

        self.apply_tx
            .send(MembershipApplyRelay {
                request_id,
                payload,
                responses_tx,
            })
            .await
            .context("send MembershipApplyRelay to p2p run-loop")?;

        let mut acks: Vec<NodeApplyAck> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            if acks.iter().filter(|a| a.ok).count() >= self.expected_nodes {
                break; // every node acked OK — done early
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => break, // all senders dropped
                Err(_) => break,   // window closed
            };
            if let SigningMessage::Response {
                signer_xrpl_address,
                error,
                ..
            } = resp
            {
                let node = if signer_xrpl_address.is_empty() {
                    "<unknown>".to_string()
                } else {
                    signer_xrpl_address
                };
                if acks.iter().any(|a| a.node == node) {
                    continue; // dedup: same node already accounted for
                }
                let ok = error.is_none();
                if ok {
                    info!(node = %node, "β apply: node applied");
                } else {
                    warn!(node = %node, err = ?error, "β apply: node rejected");
                }
                acks.push(NodeApplyAck { node, ok, error });
            }
        }
        Ok(acks)
    }

    /// If fewer than `expected_nodes` acked OK, append one explicit failure entry
    /// so the driver's `all_*` check is correctly false. (We can't name the
    /// missing nodes — they never acked — so the shortfall is one aggregate row.)
    fn shortfall_error(&self, ok_count: usize) -> Option<String> {
        (ok_count < self.expected_nodes).then(|| {
            format!(
                "only {ok_count} of {} nodes acked within {:?}; retry the change \
                 (the enclave monotonic/idempotent guards make a retry safe)",
                self.expected_nodes, self.timeout
            )
        })
    }
}

#[async_trait]
impl ClusterSealApplier for LibP2PMembershipApplier {
    async fn apply_seal(
        &self,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>> {
        let new_signers: Vec<MembershipSignerWire> = statement
            .new_signers
            .iter()
            .map(|s| MembershipSignerWire {
                account_id_hex: hex::encode(s.account_id),
                weight: s.weight,
            })
            .collect();
        let payload = MembershipApplyPayload::Seal {
            escrow_hex: hex::encode(statement.escrow),
            proposed_epoch: statement.proposed_epoch,
            prev_epoch_hash_hex: hex::encode(statement.prev_epoch_hash),
            new_signers,
            new_quorum: statement.new_quorum,
            quorum_bundle_hex: hex::encode(bundle),
        };
        let acks = self.broadcast_and_collect(payload).await?;
        let ok_count = acks.iter().filter(|a| a.ok).count();
        let mut out: Vec<NodeSealResult> = acks
            .into_iter()
            .map(|a| NodeSealResult {
                node: a.node,
                ok: a.ok,
                error: a.error,
            })
            .collect();
        if let Some(msg) = self.shortfall_error(ok_count) {
            out.push(NodeSealResult {
                node: "<cluster>".into(),
                ok: false,
                error: Some(msg),
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl ClusterGenesisApplier for LibP2PMembershipApplier {
    /// β4 Thread A genesis: one broadcast, every node bootstraps its OWN enclave
    /// from the founding attestation and acks — the same (P) single-successor
    /// shape as `apply_seal`, so genesis cannot fork the cluster either.
    async fn apply_genesis(
        &self,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>> {
        let signers: Vec<MembershipSignerWire> = statement
            .new_signers
            .iter()
            .map(|s| MembershipSignerWire {
                account_id_hex: hex::encode(s.account_id),
                weight: s.weight,
            })
            .collect();
        let payload = MembershipApplyPayload::Bootstrap {
            escrow_hex: hex::encode(statement.escrow),
            epoch: statement.proposed_epoch,
            prev_epoch_hash_hex: hex::encode(statement.prev_epoch_hash),
            signers,
            quorum: statement.new_quorum,
            quorum_bundle_hex: hex::encode(bundle),
        };
        let acks = self.broadcast_and_collect(payload).await?;
        let ok_count = acks.iter().filter(|a| a.ok).count();
        let mut out: Vec<NodeSealResult> = acks
            .into_iter()
            .map(|a| NodeSealResult {
                node: a.node,
                ok: a.ok,
                error: a.error,
            })
            .collect();
        if let Some(msg) = self.shortfall_error(ok_count) {
            out.push(NodeSealResult {
                node: "<cluster>".into(),
                ok: false,
                error: Some(msg),
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl ClusterGovernApplier for LibP2PMembershipApplier {
    /// β4 Thread B: one broadcast, every node applies the SAME allowlist
    /// operation to its OWN loopback enclave and acks — the enclave admin API is
    /// loopback-only (X-C1), so this cannot be a per-node HTTP POST to remote
    /// enclaves; it rides the apply-broadcast like seal/genesis/confirm.
    async fn apply_govern(
        &self,
        op: &GovernanceOp,
        quorum_bundle: &[u8],
        repro_bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>> {
        let payload = MembershipApplyPayload::GovernMrenclave {
            op_code: op.op,
            mrenclave_hex: hex::encode(op.mrenclave),
            escrow_hex: hex::encode(op.escrow),
            proposed_epoch: op.proposed_epoch,
            prev_allowlist_hash_hex: hex::encode(op.prev_allowlist_hash),
            quorum_bundle_hex: hex::encode(quorum_bundle),
            repro_bundle_hex: hex::encode(repro_bundle),
        };
        let acks = self.broadcast_and_collect(payload).await?;
        let ok_count = acks.iter().filter(|a| a.ok).count();
        let mut out: Vec<NodeSealResult> = acks
            .into_iter()
            .map(|a| NodeSealResult {
                node: a.node,
                ok: a.ok,
                error: a.error,
            })
            .collect();
        if let Some(msg) = self.shortfall_error(ok_count) {
            out.push(NodeSealResult {
                node: "<cluster>".into(),
                ok: false,
                error: Some(msg),
            });
        }
        Ok(out)
    }
}

#[async_trait]
impl ClusterConfirmApplier for LibP2PMembershipApplier {
    async fn apply_confirmation(
        &self,
        escrow: &[u8; 20],
        signed_xrpl_tx_blob: &[u8],
        tx_hash: &[u8; 32],
        ledger_index: u64,
    ) -> Result<Vec<NodeConfirmResult>> {
        let payload = MembershipApplyPayload::Confirm {
            escrow_hex: hex::encode(escrow),
            signed_xrpl_tx_blob_hex: hex::encode(signed_xrpl_tx_blob),
            tx_hash_hex: hex::encode(tx_hash),
            ledger_index,
        };
        let acks = self.broadcast_and_collect(payload).await?;
        let ok_count = acks.iter().filter(|a| a.ok).count();
        let mut out: Vec<NodeConfirmResult> = acks
            .into_iter()
            .map(|a| NodeConfirmResult {
                node: a.node,
                ok: a.ok,
                error: a.error,
            })
            .collect();
        if let Some(msg) = self.shortfall_error(ok_count) {
            out.push(NodeConfirmResult {
                node: "<cluster>".into(),
                ok: false,
                error: Some(msg),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(addr: &str, err: Option<&str>) -> SigningMessage {
        SigningMessage::Response {
            request_id: "beta-apply-x".into(),
            signer_xrpl_address: addr.into(),
            der_signature: None,
            compressed_pubkey: None,
            error: err.map(|e| e.to_string()),
        }
    }

    /// All 3 nodes ack OK → 3 results, all ok, no shortfall entry.
    #[tokio::test]
    async fn apply_seal_all_nodes_ack() {
        let (tx, mut rx) = mpsc::channel::<MembershipApplyRelay>(4);
        let responder = tokio::spawn(async move {
            let relay = rx.recv().await.unwrap();
            for a in ["rAaa", "rBbb", "rCcc"] {
                let _ = relay.responses_tx.send(ack(a, None)).await;
            }
        });
        let applier = LibP2PMembershipApplier::new(tx, 3).with_timeout(Duration::from_secs(2));
        let st = crate::membership_coordinator::prepare_statement(
            [0xAA; 20],
            4,
            [0xBB; 32],
            vec![crate::membership_canonical::SignerEntry {
                account_id: [0x01; 20],
                weight: 1,
            }],
            1,
        )
        .unwrap();
        let out = applier.apply_seal(&st, &[0xDE, 0xAD]).await.unwrap();
        responder.await.unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|r| r.ok));
    }

    /// Only 2 of 3 ack → a shortfall failure entry is appended (not all_sealed).
    #[tokio::test]
    async fn apply_seal_shortfall_adds_failure() {
        let (tx, mut rx) = mpsc::channel::<MembershipApplyRelay>(4);
        let responder = tokio::spawn(async move {
            let relay = rx.recv().await.unwrap();
            for a in ["rAaa", "rBbb"] {
                let _ = relay.responses_tx.send(ack(a, None)).await;
            }
            // third node never acks; let the window close.
        });
        let applier = LibP2PMembershipApplier::new(tx, 3).with_timeout(Duration::from_millis(300));
        let out = applier
            .apply_confirmation(&[0xAA; 20], &[0x01], &[0xCC; 32], 42)
            .await
            .unwrap();
        responder.await.unwrap();
        // 2 OK acks + 1 aggregate shortfall failure.
        assert_eq!(out.iter().filter(|r| r.ok).count(), 2);
        let fail = out.iter().find(|r| !r.ok).unwrap();
        assert_eq!(fail.node, "<cluster>");
        assert!(fail.error.as_ref().unwrap().contains("only 2 of 3"));
    }

    /// A node that rejects (error) is recorded as a failed node.
    #[tokio::test]
    async fn apply_records_node_rejection() {
        let (tx, mut rx) = mpsc::channel::<MembershipApplyRelay>(4);
        let responder = tokio::spawn(async move {
            let relay = rx.recv().await.unwrap();
            let _ = relay.responses_tx.send(ack("rAaa", None)).await;
            let _ = relay
                .responses_tx
                .send(ack("rBbb", Some("ERR_EPOCH_MISMATCH")))
                .await;
            let _ = relay.responses_tx.send(ack("rCcc", None)).await;
        });
        let applier = LibP2PMembershipApplier::new(tx, 3).with_timeout(Duration::from_millis(500));
        let st = crate::membership_coordinator::prepare_statement(
            [0xAA; 20],
            1,
            [0u8; 32],
            vec![crate::membership_canonical::SignerEntry {
                account_id: [0x01; 20],
                weight: 1,
            }],
            1,
        )
        .unwrap();
        let out = applier.apply_seal(&st, &[0x00]).await.unwrap();
        responder.await.unwrap();
        // rBbb rejected → it is a failed node, plus the shortfall (only 2 OK).
        let rejected = out.iter().find(|r| r.node == "rBbb").unwrap();
        assert!(!rejected.ok);
        assert_eq!(rejected.error.as_deref(), Some("ERR_EPOCH_MISMATCH"));
        assert_eq!(out.iter().filter(|r| r.ok).count(), 2);
    }
}
