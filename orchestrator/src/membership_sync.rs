//! β2(e) (perp β-retrofit) — sync-before-spend + drift detection (the safety
//! core, atomic per C-β1.1-1).
//!
//! Pure decision logic over the enclave's membership sync state + the on-chain
//! SignerList, so the same rules are unit-tested deterministically and reused
//! by (a) the membership-change driver's retirement step, (b) the spend path,
//! and (c) the periodic/pre-transition drift reconciler. The live hookups
//! (`process_withdrawal`, the periodic task, the retire/config-advance step)
//! are wired with the β3 deploy increment — exactly as the β1/β2 channels are
//! left unwired pre-deploy (test-like-production).
//!
//! Three rules (REQ-β2 §3/§4, honouring RESP-β2 X-β2-1/2/3):
//!   - **retire hard-gate (X-β2-1)** — old signers may be retired ONLY when the
//!     local enclave reports `in_sync` (projection_confirmed_epoch ==
//!     authority_epoch). Mechanical, not operator-memory.
//!   - **drift detection (§4, X-β2-3)** — meaningful ONLY when `in_sync`: then
//!     the on-chain SignerList MUST equal the sealed authority set; a mismatch
//!     is DRIFT → HALT → operator reconciliation (the chain never decides). When
//!     a transition is pending (`!in_sync`), on-chain legitimately still shows
//!     the old set — that is the bounded sync window, NOT drift. Drift is
//!     detection/recovery, NOT a per-spend guarantee (X-β2-3).
//!   - **spend basis (X-β2-2)** — spends proceed in InSync AND the pending
//!     window (the on-chain set is intact throughout), but HALT on Drift. The
//!     spend signer basis must track the confirmed projection (= on-chain set),
//!     which the retire hard-gate keeps equal to the operating config.
#![allow(dead_code)] // live hookups (withdrawal.rs, periodic task) are β3 deploy wiring

use std::collections::BTreeSet;

use anyhow::{bail, Result};

use crate::membership_canonical::SignerEntry;
use crate::membership_projection::MembershipSyncState;

/// A signer set as `(account_id, weight)` members + quorum — the comparison
/// unit for drift (order-independent; weights included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerSet {
    pub members: BTreeSet<([u8; 20], u32)>,
    pub quorum: u32,
}

impl SignerSet {
    pub fn from_entries(signers: &[SignerEntry], quorum: u32) -> Self {
        SignerSet {
            members: signers.iter().map(|s| (s.account_id, s.weight)).collect(),
            quorum,
        }
    }
}

/// The reconciled membership status of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipStatus {
    /// projection_confirmed_epoch == authority_epoch AND on-chain == authority.
    InSync,
    /// authority_epoch > projection_confirmed_epoch — the bounded
    /// sync-before-spend window; on-chain still shows the old set (expected).
    TransitionPending {
        authority_epoch: u64,
        projection_confirmed_epoch: u64,
    },
    /// in_sync but on-chain SignerList != the sealed authority set, or an
    /// out-of-band change — HALT, operator reconciliation (chain never decides).
    Drift(String),
}

/// X-β2-1: old signers may be retired only when the local enclave is in sync.
/// The membership-change driver's retire/config-advance step MUST call this;
/// an early retire (before the new projection confirms) would leave the
/// on-chain quorum unmet → funds frozen.
pub fn assert_safe_to_retire(local: &MembershipSyncState) -> Result<()> {
    if !local.in_sync {
        bail!(
            "refuse to retire old signers: projection not yet confirmed \
             (authority_epoch={}, projection_confirmed_epoch={}); the on-chain \
             SignerList still requires the outgoing signers",
            local.authority_epoch,
            local.projection_confirmed_epoch
        );
    }
    Ok(())
}

/// §4 / X-β2-3: classify one node by reconciling its enclave sync state against
/// the on-chain SignerList. `authority` is the sealed authority set; `on_chain`
/// is the live SignerList (caller decodes it). Drift is only asserted when
/// `in_sync` — during a pending transition the on-chain/old-set difference is
/// the expected window, not drift.
pub fn classify_local(
    local: &MembershipSyncState,
    authority: &SignerSet,
    on_chain: &SignerSet,
) -> MembershipStatus {
    if !local.in_sync {
        return MembershipStatus::TransitionPending {
            authority_epoch: local.authority_epoch,
            projection_confirmed_epoch: local.projection_confirmed_epoch,
        };
    }
    // in_sync: the on-chain SignerList MUST equal the sealed authority set.
    if authority == on_chain {
        MembershipStatus::InSync
    } else {
        MembershipStatus::Drift(format!(
            "in_sync at epoch {} but on-chain SignerList != sealed authority set \
             (on-chain {} members / quorum {}, authority {} members / quorum {})",
            local.authority_epoch,
            on_chain.members.len(),
            on_chain.quorum,
            authority.members.len(),
            authority.quorum
        ))
    }
}

/// X-β2-2: whether a spend may proceed given the node's status. Spends proceed
/// in InSync and during the bounded pending window (the on-chain set is intact
/// throughout); they HALT on Drift. This is NOT the β-rejected variant B
/// (block-all-spends-during-every-change): the window does not block spends.
pub fn assert_spend_allowed(status: &MembershipStatus) -> Result<()> {
    match status {
        MembershipStatus::InSync | MembershipStatus::TransitionPending { .. } => Ok(()),
        MembershipStatus::Drift(why) => {
            bail!("HALT spend: membership drift detected — {why}")
        }
    }
}

/// §4 cross-node: every node must agree on the same `(authority_epoch,
/// projection_confirmed_epoch)`. A node not bootstrapped (`None`) or holding a
/// different pair is a split → HALT → operator reconciliation. Returns the
/// agreed state on success.
pub fn reconcile_cluster(states: &[Option<MembershipSyncState>]) -> Result<MembershipSyncState> {
    let mut agreed: Option<MembershipSyncState> = None;
    for (i, st) in states.iter().enumerate() {
        let Some(s) = st else {
            bail!("HALT: node {i} reports no sealed membership epoch (not bootstrapped) — cluster split");
        };
        match agreed {
            None => agreed = Some(*s),
            Some(a) => {
                if a.authority_epoch != s.authority_epoch
                    || a.projection_confirmed_epoch != s.projection_confirmed_epoch
                {
                    bail!(
                        "HALT: cross-node membership disagreement — node 0 at \
                         (authority={}, confirmed={}), node {i} at \
                         (authority={}, confirmed={}); operator reconciliation \
                         required (chain never decides the winner)",
                        a.authority_epoch,
                        a.projection_confirmed_epoch,
                        s.authority_epoch,
                        s.projection_confirmed_epoch
                    );
                }
            }
        }
    }
    agreed.ok_or_else(|| anyhow::anyhow!("reconcile_cluster: empty node set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fill: u8, weight: u32) -> SignerEntry {
        SignerEntry {
            account_id: [fill; 20],
            weight,
        }
    }

    fn sync(authority: u64, confirmed: u64) -> MembershipSyncState {
        MembershipSyncState {
            authority_epoch: authority,
            projection_confirmed_epoch: confirmed,
            in_sync: authority == confirmed,
        }
    }

    fn set(signers: &[SignerEntry], quorum: u32) -> SignerSet {
        SignerSet::from_entries(signers, quorum)
    }

    // ── X-β2-1 retire hard-gate ──────────────────────────────────

    #[test]
    fn retire_allowed_only_when_in_sync() {
        assert!(assert_safe_to_retire(&sync(5, 5)).is_ok());
        let err = assert_safe_to_retire(&sync(6, 5)).unwrap_err().to_string();
        assert!(err.contains("refuse to retire"), "got: {err}");
        assert!(err.contains("authority_epoch=6"), "got: {err}");
    }

    // ── §4 drift classification ──────────────────────────────────

    #[test]
    fn in_sync_matching_chain_is_in_sync() {
        let signers = [entry(1, 1), entry(2, 1)];
        let st = classify_local(&sync(5, 5), &set(&signers, 2), &set(&signers, 2));
        assert_eq!(st, MembershipStatus::InSync);
    }

    #[test]
    fn pending_window_is_not_drift() {
        // authority ahead of confirmed → window; on-chain still the OLD set,
        // which legitimately differs from the (new) authority set. NOT drift.
        let authority = [entry(1, 1), entry(3, 1)]; // new set
        let on_chain = [entry(1, 1), entry(2, 1)]; // old set still on-chain
        let st = classify_local(&sync(6, 5), &set(&authority, 2), &set(&on_chain, 2));
        assert_eq!(
            st,
            MembershipStatus::TransitionPending {
                authority_epoch: 6,
                projection_confirmed_epoch: 5,
            }
        );
    }

    #[test]
    fn in_sync_but_chain_differs_is_drift() {
        let authority = [entry(1, 1), entry(2, 1)];
        let on_chain = [entry(1, 1), entry(9, 1)]; // out-of-band change
        let st = classify_local(&sync(5, 5), &set(&authority, 2), &set(&on_chain, 2));
        assert!(matches!(st, MembershipStatus::Drift(_)));
    }

    #[test]
    fn in_sync_but_quorum_differs_is_drift() {
        let signers = [entry(1, 1), entry(2, 1)];
        let st = classify_local(&sync(5, 5), &set(&signers, 2), &set(&signers, 1));
        assert!(matches!(st, MembershipStatus::Drift(_)));
    }

    // ── X-β2-2 spend gate ────────────────────────────────────────

    #[test]
    fn spends_proceed_in_sync_and_window_halt_on_drift() {
        assert!(assert_spend_allowed(&MembershipStatus::InSync).is_ok());
        assert!(assert_spend_allowed(&MembershipStatus::TransitionPending {
            authority_epoch: 6,
            projection_confirmed_epoch: 5
        })
        .is_ok());
        let err = assert_spend_allowed(&MembershipStatus::Drift("x".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("HALT spend"), "got: {err}");
    }

    // ── §4 cross-node reconciliation ─────────────────────────────

    #[test]
    fn cluster_in_agreement_reconciles() {
        let states = [Some(sync(6, 5)), Some(sync(6, 5)), Some(sync(6, 5))];
        let agreed = reconcile_cluster(&states).unwrap();
        assert_eq!(agreed.authority_epoch, 6);
        assert_eq!(agreed.projection_confirmed_epoch, 5);
    }

    #[test]
    fn cluster_disagreement_halts() {
        let states = [Some(sync(6, 5)), Some(sync(6, 6)), Some(sync(6, 5))];
        let err = reconcile_cluster(&states).unwrap_err().to_string();
        assert!(
            err.contains("cross-node membership disagreement"),
            "got: {err}"
        );
    }

    #[test]
    fn cluster_unbootstrapped_node_halts() {
        let states = [Some(sync(6, 6)), None];
        let err = reconcile_cluster(&states).unwrap_err().to_string();
        assert!(err.contains("not bootstrapped"), "got: {err}");
    }
}
