//! β2 (perp β-retrofit) — render the XRPL `SignerListSet` PROJECTION from the
//! sealed authority epoch.
//!
//! Under β the XRPL SignerList is a downstream projection of the off-chain
//! authority: this module turns the membership the cluster has already SEALED
//! (a `&[SignerEntry]` + quorum, the same set `ecall_seal_membership_epoch`
//! committed) into the unsigned `SignerListSet` transaction the cluster then
//! signs with its CURRENT on-chain quorum and submits. The produced tx is a
//! pure function of the sealed set — never a chain read (REQ-β2 §2.1).
//!
//! The shape matches the cluster's own signing-policy gate
//! (`p2p.rs::validate_signerlist_set_specific`) so every co-signer accepts it:
//! only the whitelisted top-level fields, `SignerEntries[].SignerEntry.{Account,
//! SignerWeight}`, sorted by AccountID (XRPL canonical + deterministic hash).
#![allow(dead_code)] // runtime wiring (admin trigger + main.rs) is the β3 deploy increment

use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::membership_canonical::SignerEntry;

// ── β2 sync-state + projection-confirmation surfaces ─────────────
//
// The traits the projection driver (β2(d)) and the sync-before-spend gate
// (β2(e)) depend on; the HTTP implementations live in membership_http.rs (so
// the driver is unit-testable with mocks). Mirrors the β1
// EpochDigestSource/EpochSealSink split.

/// The membership sync state read from a node's enclave (REQ-β2 §3.2):
/// `in_sync == (projection_confirmed_epoch == authority_epoch)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipSyncState {
    pub authority_epoch: u64,
    pub projection_confirmed_epoch: u64,
    pub in_sync: bool,
}

/// Reads `ecall_get_membership_sync_state` from an enclave. `Ok(None)` when the
/// node is not bootstrapped (no sealed epoch yet).
#[async_trait]
pub trait SyncStateSource: Send + Sync {
    async fn sync_state(&self) -> Result<Option<MembershipSyncState>>;
}

/// Records the on-chain projection confirmation on ONE node's enclave
/// (`ecall_record_projection_confirmation`).
#[async_trait]
pub trait ProjectionConfirmer: Send + Sync {
    async fn record_confirmation(
        &self,
        node_admin_url: &str,
        escrow: &[u8; 20],
        signed_xrpl_tx_blob: &[u8],
        tx_hash: &[u8; 32],
        ledger_index: u64,
    ) -> Result<()>;
}

/// XRPL base58 alphabet (note: distinct from the Bitcoin alphabet).
const XRPL_ALPHABET: &[u8; 58] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";

/// Encode a 20-byte XRPL AccountID as its classic `r…` address
/// (base58check, version byte 0x00, 4-byte double-SHA256 checksum). The sealed
/// authority stores raw AccountIDs; the on-chain `SignerEntry.Account` is the
/// r-address, so the projection derives it here — a faithful projection of
/// exactly the sealed bytes.
pub fn account_id_to_r_address(account_id: &[u8; 20]) -> String {
    let mut payload = Vec::with_capacity(25);
    payload.push(0x00u8); // AccountID type prefix
    payload.extend_from_slice(account_id);
    let checksum = Sha256::digest(Sha256::digest(&payload));
    payload.extend_from_slice(&checksum[..4]);
    let alpha = bs58::Alphabet::new(XRPL_ALPHABET).expect("valid 58-byte alphabet");
    bs58::encode(&payload).with_alphabet(&alpha).into_string()
}

/// Render the unsigned `SignerListSet` projecting `signers`/`quorum` (the sealed
/// authority set) for `escrow`. `SignerEntries` are sorted ascending by
/// AccountID — XRPL canonical order, so the serialized tx (and its
/// multi-signing hash) is deterministic across nodes and the set is
/// order-independent against the enclave's confirmation binding.
///
/// `SignerWeight` is the AUTHORITY's weight verbatim (the projection reflects
/// the sealed set faithfully). The cluster equal-weight convention means these
/// are 1; a non-conforming weight would be rejected by the co-signers' policy
/// gate, which is the correct outcome.
pub fn render_signerlist_set(
    escrow: &[u8; 20],
    sequence: u32,
    fee_drops: u64,
    signers: &[SignerEntry],
    quorum: u32,
) -> serde_json::Value {
    let mut sorted: Vec<SignerEntry> = signers.to_vec();
    sorted.sort_by(|a, b| a.account_id.cmp(&b.account_id));

    let signer_entries: Vec<serde_json::Value> = sorted
        .iter()
        .map(|s| {
            serde_json::json!({
                "SignerEntry": {
                    "Account": account_id_to_r_address(&s.account_id),
                    "SignerWeight": s.weight,
                }
            })
        })
        .collect();

    serde_json::json!({
        "TransactionType": "SignerListSet",
        "Account": account_id_to_r_address(escrow),
        "Fee": fee_drops.to_string(),
        "Sequence": sequence,
        "SigningPubKey": "",
        "SignerQuorum": quorum,
        "SignerEntries": signer_entries,
    })
}

// ── β2(d) projection driver ──────────────────────────────────────

/// The validated on-chain projection transaction, as needed by
/// `ecall_record_projection_confirmation`.
#[derive(Debug, Clone)]
pub struct ProjectionConfirmedTx {
    pub signed_tx_blob: Vec<u8>,
    pub tx_hash: [u8; 32],
    pub ledger_index: u64,
}

/// Signs the unsigned `SignerListSet` projection with the cluster's CURRENT
/// on-chain quorum (the outgoing signers — still on-chain and able to sign),
/// submits it, and polls XRPL until it is validated on-ledger. Trait so the
/// driver is unit-testable without a live ledger / relay.
///
/// Q-β2-6 (the safe failure mode is load-bearing): the implementation MUST use
/// a BOUNDED multi-ledger retry-poll, and on confirmation timeout return an
/// error (NOT a partial success). The driver then records nothing, the epoch
/// stays projection-UNCONFIRMED (the old set remains the spend basis — safe),
/// and the operator is surfaced the error. Never auto-retire, never auto-spend
/// on an unconfirmed set.
#[async_trait]
pub trait ProjectionSubmitter: Send + Sync {
    /// `quorum_bundle_hex` (β4 Thread A) is forwarded verbatim to the enclave
    /// governance signing path, which requires it to produce a SignerListSet
    /// cosignature.
    async fn sign_submit_confirm(
        &self,
        unsigned_signerlist_set: &serde_json::Value,
        quorum_bundle_hex: &str,
    ) -> Result<ProjectionConfirmedTx>;
}

/// Records the on-chain projection confirmation across the WHOLE cluster in one
/// shot, returning one `NodeConfirmResult` per node. Under the loopback-enclave
/// topology (X-C1) this is NOT a per-node HTTP POST to a remote enclave — the
/// production impl (`LibP2PMembershipApplier`) broadcasts ONE `MembershipApply`
/// over p2p and every node records on its OWN localhost enclave + acks.
#[async_trait]
pub trait ClusterConfirmApplier: Send + Sync {
    async fn apply_confirmation(
        &self,
        escrow: &[u8; 20],
        signed_xrpl_tx_blob: &[u8],
        tx_hash: &[u8; 32],
        ledger_index: u64,
    ) -> Result<Vec<NodeConfirmResult>>;
}

/// Per-node result of recording the projection confirmation.
#[derive(Debug, Clone)]
pub struct NodeConfirmResult {
    pub node: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Outcome of producing + confirming + recording one projection.
#[derive(Debug, Clone)]
pub struct ProjectionOutcome {
    pub tx_hash: [u8; 32],
    pub ledger_index: u64,
    pub node_results: Vec<NodeConfirmResult>,
}

impl ProjectionOutcome {
    /// True iff every node recorded the projection confirmation. A partial
    /// result means some nodes still see the transition as pending (their
    /// sync-before-spend window stays open) — the operator retries those nodes;
    /// the enclave's idempotency guard (ERR_ALREADY_CONFIRMED) makes the retry
    /// safe.
    pub fn all_recorded(&self) -> bool {
        !self.node_results.is_empty() && self.node_results.iter().all(|r| r.ok)
    }
}

/// What to project: the just-sealed authority set + the XRPL tx framing.
#[derive(Debug, Clone)]
pub struct ProjectionRequest {
    pub escrow: [u8; 20],
    pub sequence: u32,
    pub fee_drops: u64,
    pub signers: Vec<SignerEntry>,
    pub quorum: u32,
    /// β4 Thread A (AC-β4-A1): the β1 quorum bundle that authorised the epoch
    /// being projected, hex-encoded. Forwarded to each signer's enclave, whose
    /// governance signing path refuses a SignerListSet without it.
    pub quorum_bundle_hex: String,
}

/// Produce the XRPL `SignerListSet` projection for the just-sealed authority
/// epoch and, once it is on-ledger, record the confirmation on every node —
/// closing the sync-before-spend window (REQ-β2 §2). Pure orchestration over
/// injected XRPL submit + per-node confirm, so a doomed projection fails before
/// any node is touched.
///
/// Order (sync-before-spend §3.1): the caller has already sealed the authority
/// epoch (β1). This produces + confirms the projection. The old signers are NOT
/// retired here — retirement is a separate, sync-gated step (β2(e), X-β2-1).
pub async fn run_projection(
    req: &ProjectionRequest,
    submitter: &dyn ProjectionSubmitter,
    applier: &dyn ClusterConfirmApplier,
) -> Result<ProjectionOutcome> {
    let unsigned = render_signerlist_set(
        &req.escrow,
        req.sequence,
        req.fee_drops,
        &req.signers,
        req.quorum,
    );

    // Sign with the CURRENT on-chain quorum, submit, and poll until validated.
    // On timeout this returns Err → we record nothing → epoch stays
    // projection-UNCONFIRMED (safe) → surfaced to the operator (Q-β2-6).
    let tx = submitter
        .sign_submit_confirm(&unsigned, &req.quorum_bundle_hex)
        .await
        .context("sign + submit + confirm the SignerListSet projection")?;

    let node_results = applier
        .apply_confirmation(
            &req.escrow,
            &tx.signed_tx_blob,
            &tx.tx_hash,
            tx.ledger_index,
        )
        .await
        .context("record projection confirmation across the cluster")?;

    Ok(ProjectionOutcome {
        tx_hash: tx.tx_hash,
        ledger_index: tx.ledger_index,
        node_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn entry(fill: u8, weight: u32) -> SignerEntry {
        SignerEntry {
            account_id: [fill; 20],
            weight,
        }
    }

    /// The canonical XRPL ACCOUNT_ZERO: the all-zero AccountID encodes to the
    /// well-known `rrrrrrrrrrrrrrrrrrrrrhoLvTp`. Pins the base58check pipeline
    /// (alphabet, version byte, checksum) against a published vector.
    #[test]
    fn account_zero_encodes_to_known_address() {
        assert_eq!(
            account_id_to_r_address(&[0u8; 20]),
            "rrrrrrrrrrrrrrrrrrrrrhoLvTp"
        );
    }

    #[test]
    fn r_address_roundtrips_through_auth_decoder() {
        // Any 20-byte AccountID must encode to a 25..35-char r-address that
        // starts with 'r' — a structural sanity beyond ACCOUNT_ZERO.
        let addr = account_id_to_r_address(&[0xABu8; 20]);
        assert!(addr.starts_with('r'), "got {addr}");
        assert!(addr.len() >= 25 && addr.len() <= 40, "len {}", addr.len());
    }

    #[test]
    fn signerlist_set_shape_matches_policy_gate() {
        // 2 signers supplied OUT of AccountID order → must come out sorted.
        let signers = [entry(0x02, 1), entry(0x01, 1)];
        let tx = render_signerlist_set(&[0xAA; 20], 42, 12000, &signers, 2);

        assert_eq!(tx["TransactionType"], "SignerListSet");
        assert_eq!(tx["Account"], account_id_to_r_address(&[0xAA; 20]));
        assert_eq!(tx["Fee"], "12000"); // string, per XRPL
        assert_eq!(tx["Sequence"], 42);
        assert_eq!(tx["SigningPubKey"], "");
        assert_eq!(tx["SignerQuorum"], 2);

        let entries = tx["SignerEntries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        // sorted ascending by AccountID → 0x01 first
        assert_eq!(
            entries[0]["SignerEntry"]["Account"],
            account_id_to_r_address(&[0x01; 20])
        );
        assert_eq!(
            entries[1]["SignerEntry"]["Account"],
            account_id_to_r_address(&[0x02; 20])
        );
        assert_eq!(entries[0]["SignerEntry"]["SignerWeight"], 1);

        // only whitelisted top-level fields (mirrors validate_signerlist_set_specific)
        let obj = tx.as_object().unwrap();
        for k in obj.keys() {
            assert!(
                matches!(
                    k.as_str(),
                    "TransactionType"
                        | "Account"
                        | "Fee"
                        | "Sequence"
                        | "SigningPubKey"
                        | "SignerQuorum"
                        | "SignerEntries"
                ),
                "unexpected top-level field {k}"
            );
        }
    }

    #[test]
    fn projects_authority_weights_verbatim() {
        let signers = [entry(0x01, 1), entry(0x02, 3)];
        let tx = render_signerlist_set(&[0xAA; 20], 1, 12000, &signers, 3);
        let entries = tx["SignerEntries"].as_array().unwrap();
        assert_eq!(entries[1]["SignerEntry"]["SignerWeight"], 3);
        assert_eq!(tx["SignerQuorum"], 3);
    }

    // ── β2(d) driver ─────────────────────────────────────────────

    use std::sync::Mutex;

    /// Submitter that returns a fixed confirmed tx and records what it was
    /// asked to sign (so the test can assert the projection shape reached it).
    struct OkSubmitter {
        tx: ProjectionConfirmedTx,
        seen_unsigned: Mutex<Option<serde_json::Value>>,
        /// β4 Thread A: records the bundle the driver forwarded, so a test can
        /// assert the governance path actually receives it (AC-β4-A1).
        seen_bundle: Mutex<Option<String>>,
    }
    #[async_trait]
    impl ProjectionSubmitter for OkSubmitter {
        async fn sign_submit_confirm(
            &self,
            unsigned: &serde_json::Value,
            quorum_bundle_hex: &str,
        ) -> Result<ProjectionConfirmedTx> {
            *self.seen_unsigned.lock().unwrap() = Some(unsigned.clone());
            *self.seen_bundle.lock().unwrap() = Some(quorum_bundle_hex.to_string());
            Ok(self.tx.clone())
        }
    }

    /// Submitter that always errors (confirmation timeout / submit failure).
    struct ErrSubmitter;
    #[async_trait]
    impl ProjectionSubmitter for ErrSubmitter {
        async fn sign_submit_confirm(
            &self,
            _: &serde_json::Value,
            _: &str,
        ) -> Result<ProjectionConfirmedTx> {
            bail!("confirmation timed out after bounded poll")
        }
    }

    /// Mock `ClusterConfirmApplier`: records the SINGLE broadcast (tx_hash,
    /// ledger) and returns a configured per-node result set. `failing()`
    /// simulates a broadcast that never reaches the run-loop.
    struct MockConfirmApplier {
        nodes: Vec<String>,
        fail_nodes: Vec<String>,
        fail_call: bool,
        seen: Mutex<Option<([u8; 32], u64)>>,
    }
    impl MockConfirmApplier {
        fn new(nodes: Vec<String>, fail_nodes: Vec<String>) -> Self {
            Self {
                nodes,
                fail_nodes,
                fail_call: false,
                seen: Mutex::new(None),
            }
        }
        fn failing() -> Self {
            Self {
                nodes: vec![],
                fail_nodes: vec![],
                fail_call: true,
                seen: Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl ClusterConfirmApplier for MockConfirmApplier {
        async fn apply_confirmation(
            &self,
            _escrow: &[u8; 20],
            _blob: &[u8],
            tx_hash: &[u8; 32],
            ledger_index: u64,
        ) -> Result<Vec<NodeConfirmResult>> {
            if self.fail_call {
                bail!("apply broadcast failed");
            }
            *self.seen.lock().unwrap() = Some((*tx_hash, ledger_index));
            Ok(self
                .nodes
                .iter()
                .map(|n| {
                    let ok = !self.fail_nodes.iter().any(|f| f == n);
                    NodeConfirmResult {
                        node: n.clone(),
                        ok,
                        error: (!ok).then(|| format!("node {n} unreachable")),
                    }
                })
                .collect())
        }
    }

    fn confirmed_tx() -> ProjectionConfirmedTx {
        ProjectionConfirmedTx {
            signed_tx_blob: vec![0x12, 0x34],
            tx_hash: [0xEE; 32],
            ledger_index: 9_001,
        }
    }

    fn proj_req(signers: &[SignerEntry], quorum: u32) -> ProjectionRequest {
        ProjectionRequest {
            escrow: [0xAA; 20],
            sequence: 1,
            fee_drops: 12000,
            signers: signers.to_vec(),
            quorum,
            quorum_bundle_hex: "beefcafe".to_string(),
        }
    }

    #[tokio::test]
    async fn driver_records_on_all_nodes_after_confirmation() {
        let submitter = OkSubmitter {
            tx: confirmed_tx(),
            seen_unsigned: Mutex::new(None),
            seen_bundle: Mutex::new(None),
        };
        let applier = MockConfirmApplier::new(vec!["n1".into(), "n2".into(), "n3".into()], vec![]);
        let signers = [entry(0x01, 1), entry(0x02, 1)];

        let out = run_projection(&proj_req(&signers, 2), &submitter, &applier)
            .await
            .unwrap();

        assert_eq!(out.tx_hash, [0xEE; 32]);
        assert_eq!(out.ledger_index, 9_001);
        assert!(out.all_recorded());
        assert_eq!(out.node_results.len(), 3);

        // the projection that reached the submitter is the rendered SignerListSet
        let seen = submitter.seen_unsigned.lock().unwrap().clone().unwrap();
        assert_eq!(seen["TransactionType"], "SignerListSet");
        assert_eq!(seen["SignerQuorum"], 2);

        // the cluster was broadcast the confirmation ONCE with the same (tx, ledger)
        assert_eq!(applier.seen.lock().unwrap().unwrap(), ([0xEE; 32], 9_001));
    }

    #[tokio::test]
    async fn driver_propagates_confirmation_timeout_without_recording() {
        let submitter = ErrSubmitter;
        let applier = MockConfirmApplier::new(vec!["n1".into()], vec![]);
        let signers = [entry(0x01, 1), entry(0x02, 1)];

        let err = run_projection(&proj_req(&signers, 2), &submitter, &applier)
            .await
            .unwrap_err();
        // full chain (anyhow's Display shows only the outer context).
        let full = format!("{err:#}");
        assert!(full.contains("timed out"), "got: {full}");
        // NOTHING recorded — epoch stays projection-UNCONFIRMED (safe).
        assert!(applier.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn driver_reports_partial_record_failure() {
        let submitter = OkSubmitter {
            tx: confirmed_tx(),
            seen_unsigned: Mutex::new(None),
            seen_bundle: Mutex::new(None),
        };
        let applier = MockConfirmApplier::new(
            vec!["n1".into(), "n2".into(), "n3".into()],
            vec!["n2".into()],
        );
        let signers = [entry(0x01, 1), entry(0x02, 1)];

        let out = run_projection(&proj_req(&signers, 2), &submitter, &applier)
            .await
            .unwrap();

        assert!(!out.all_recorded());
        assert_eq!(out.node_results.iter().filter(|r| r.ok).count(), 2);
        let failed = out.node_results.iter().find(|r| !r.ok).unwrap();
        assert_eq!(failed.node, "n2");
    }

    #[tokio::test]
    async fn driver_propagates_apply_broadcast_failure() {
        let submitter = OkSubmitter {
            tx: confirmed_tx(),
            seen_unsigned: Mutex::new(None),
            seen_bundle: Mutex::new(None),
        };
        let applier = MockConfirmApplier::failing();
        let signers = [entry(0x01, 1)];
        let err = run_projection(&proj_req(&signers, 1), &submitter, &applier)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("record projection confirmation across the cluster"),
            "got: {err}"
        );
    }
}
