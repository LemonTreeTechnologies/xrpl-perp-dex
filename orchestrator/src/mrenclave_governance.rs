#![allow(dead_code)] // no caller until the p2p transport + operator trigger land
//! β4 Thread B — driving the membership-governed MRENCLAVE allowlist.
//!
//! The allowlist is what turns an MRENCLAVE bump from a rip-and-replace into a
//! governed transition: it is the standing "this measurement is trusted" record
//! that `ecall_la_export_state` now requires (in ADDITION to the per-ceremony
//! delegation quorum) before it will migrate state to a new enclave.
//!
//! Note what this module deliberately does NOT do: it never computes a signing
//! hash. Operators sign through typed enclave routes that take the STRUCTURED
//! operation and re-derive the domain-separated message in-enclave, so there is
//! no canonical encoding duplicated on this side and therefore nothing that can
//! drift from the enclave's — unlike β1, where the orchestrator does encode and
//! frozen cross-language vectors are needed to pin it.
//!
//! Pure orchestration over injected traits, so the decision logic is unit-tested
//! without an enclave or a cluster.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;

use crate::membership_coordinator::NodeSealResult;

/// Operations, mirroring `TrustedMrenclaves.h`. Kept as plain constants rather
/// than an enum because the value crosses the FFI/JSON boundary verbatim.
pub const OP_ADD: u8 = 1;
pub const OP_REMOVE: u8 = 2;

/// Reproducible-build floor, mirroring `TRUSTED_MRENCLAVES_REPRO_MIN`. The
/// ENCLAVE is the authority — this copy only lets the driver fail early with a
/// clear operator message instead of after a round-trip.
pub const REPRO_MIN: usize = 2;

/// One allowlist operation, exactly as the enclave will re-derive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceOp {
    pub op: u8,
    pub mrenclave: [u8; 32],
    pub escrow: [u8; 20],
    pub proposed_epoch: u64,
    pub prev_allowlist_hash: [u8; 32],
}

/// Reads the local enclave's current allowlist head — the epoch and digest the
/// next operation must chain onto.
#[async_trait]
pub trait AllowlistStatusSource: Send + Sync {
    async fn current(&self) -> Result<(u64, [u8; 32])>;
}

/// Collects the two operator-signed bundles. Both ride the same quorum-bundle
/// wire format the β1 seal and the Path-A delegation already use.
#[async_trait]
pub trait GovernanceBundleCollector: Send + Sync {
    /// Signatures over {op, mrenclave, epoch, prev_allowlist_hash} — the
    /// cluster's authorisation of THIS operation.
    async fn collect_governance(&self, op: &GovernanceOp) -> Result<Vec<u8>>;
    /// Signatures over the measurement alone — each signer attesting it rebuilt
    /// that binary bit-identically. Returns the bundle and the number of
    /// DISTINCT signers it contains.
    async fn collect_repro(&self, mrenclave: &[u8; 32]) -> Result<(Vec<u8>, usize)>;
}

/// Applies the operation on every node (each against its OWN loopback enclave).
#[async_trait]
pub trait ClusterGovernApplier: Send + Sync {
    async fn apply_govern(
        &self,
        op: &GovernanceOp,
        quorum_bundle: &[u8],
        repro_bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>>;
}

#[derive(Debug, Clone)]
pub struct GovernanceOutcome {
    pub op: u8,
    pub mrenclave: [u8; 32],
    pub allowlist_epoch: u64,
    pub repro_signers: usize,
    pub node_results: Vec<NodeSealResult>,
}

impl GovernanceOutcome {
    pub fn all_applied(&self) -> bool {
        !self.node_results.is_empty() && self.node_results.iter().all(|r| r.ok)
    }
}

/// Admit or veto a measurement across the cluster.
///
/// Ordering matters: the allowlist head is read FIRST so the operation is
/// chained onto the current state, and for an admit the reproducible-build proof
/// is gathered BEFORE the cluster is touched — an admit that cannot meet the
/// floor must fail without consuming an allowlist epoch anywhere.
pub async fn run_mrenclave_governance(
    op: u8,
    mrenclave: [u8; 32],
    escrow: [u8; 20],
    status: &dyn AllowlistStatusSource,
    collector: &dyn GovernanceBundleCollector,
    applier: &dyn ClusterGovernApplier,
) -> Result<GovernanceOutcome> {
    if op != OP_ADD && op != OP_REMOVE {
        bail!("unknown allowlist op {op} (expected {OP_ADD}=add or {OP_REMOVE}=remove)");
    }

    let (current_epoch, prev_allowlist_hash) = status
        .current()
        .await
        .context("read the local enclave's allowlist head")?;

    let gov_op = GovernanceOp {
        op,
        mrenclave,
        escrow,
        proposed_epoch: current_epoch + 1,
        prev_allowlist_hash,
    };

    // L3 first, and only for an admit. A veto deliberately needs no proof:
    // moving toward LESS trust must never be harder than moving toward more.
    let (repro_bundle, repro_signers) = if op == OP_ADD {
        let (bundle, distinct) = collector
            .collect_repro(&mrenclave)
            .await
            .context("collect the reproducible-build proof")?;
        if distinct < REPRO_MIN {
            // The enclave enforces this too; failing here just gives the
            // operator the actionable message instead of a rejection code.
            return Err(anyhow!(
                "reproducible-build proof has {distinct} distinct signer(s), need at least \
                 {REPRO_MIN} — a measurement no independent operator reproduced must not be \
                 admitted (invariant 7)"
            ));
        }
        (bundle, distinct)
    } else {
        (Vec::new(), 0)
    };

    let quorum_bundle = collector
        .collect_governance(&gov_op)
        .await
        .context("collect the cluster's authorisation of this allowlist operation")?;

    let node_results = applier
        .apply_govern(&gov_op, &quorum_bundle, &repro_bundle)
        .await
        .context("apply the allowlist operation across the cluster")?;

    Ok(GovernanceOutcome {
        op,
        mrenclave,
        allowlist_epoch: gov_op.proposed_epoch,
        repro_signers,
        node_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Status(u64, [u8; 32]);
    #[async_trait]
    impl AllowlistStatusSource for Status {
        async fn current(&self) -> Result<(u64, [u8; 32])> {
            Ok((self.0, self.1))
        }
    }

    struct Collector {
        repro_distinct: usize,
        seen_gov: Mutex<Option<GovernanceOp>>,
        repro_called: Mutex<bool>,
    }
    impl Collector {
        fn new(repro_distinct: usize) -> Self {
            Self {
                repro_distinct,
                seen_gov: Mutex::new(None),
                repro_called: Mutex::new(false),
            }
        }
    }
    #[async_trait]
    impl GovernanceBundleCollector for Collector {
        async fn collect_governance(&self, op: &GovernanceOp) -> Result<Vec<u8>> {
            *self.seen_gov.lock().unwrap() = Some(op.clone());
            Ok(vec![0xAA, 0xBB])
        }
        async fn collect_repro(&self, _: &[u8; 32]) -> Result<(Vec<u8>, usize)> {
            *self.repro_called.lock().unwrap() = true;
            Ok((vec![0xCC], self.repro_distinct))
        }
    }

    /// What the applier was broadcast: (operation, governance bundle, repro bundle).
    type SeenApply = Option<(GovernanceOp, Vec<u8>, Vec<u8>)>;

    struct Applier {
        nodes: Vec<String>,
        seen: Mutex<SeenApply>,
    }
    impl Applier {
        fn new(n: usize) -> Self {
            Self {
                nodes: (1..=n).map(|i| format!("n{i}")).collect(),
                seen: Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl ClusterGovernApplier for Applier {
        async fn apply_govern(
            &self,
            op: &GovernanceOp,
            quorum_bundle: &[u8],
            repro_bundle: &[u8],
        ) -> Result<Vec<NodeSealResult>> {
            *self.seen.lock().unwrap() =
                Some((op.clone(), quorum_bundle.to_vec(), repro_bundle.to_vec()));
            Ok(self
                .nodes
                .iter()
                .map(|n| NodeSealResult {
                    node: n.clone(),
                    ok: true,
                    error: None,
                })
                .collect())
        }
    }

    /// An admit chains onto the current head and carries both bundles.
    #[tokio::test]
    async fn admit_chains_onto_head_and_carries_both_bundles() {
        let status = Status(4, [0xBB; 32]);
        let collector = Collector::new(3);
        let applier = Applier::new(3);

        let out = run_mrenclave_governance(
            OP_ADD, [0x11; 32], [0xAA; 20], &status, &collector, &applier,
        )
        .await
        .unwrap();

        assert_eq!(out.allowlist_epoch, 5, "epoch must be head + 1");
        assert_eq!(out.repro_signers, 3);
        assert!(out.all_applied());

        let (op, gov, repro) = applier.seen.lock().unwrap().clone().unwrap();
        assert_eq!(op.proposed_epoch, 5);
        assert_eq!(
            op.prev_allowlist_hash, [0xBB; 32],
            "must chain onto the head"
        );
        assert_eq!(op.op, OP_ADD);
        assert_eq!(gov, vec![0xAA, 0xBB]);
        assert_eq!(repro, vec![0xCC], "an admit must carry the repro proof");
    }

    /// The L3 floor is enforced BEFORE the cluster is touched: an admit that
    /// cannot prove independent reproduction must not consume an epoch anywhere.
    #[tokio::test]
    async fn admit_below_repro_floor_fails_before_touching_the_cluster() {
        let status = Status(0, [0u8; 32]);
        let collector = Collector::new(1); // only the builder attested
        let applier = Applier::new(3);

        let err = run_mrenclave_governance(
            OP_ADD, [0x11; 32], [0xAA; 20], &status, &collector, &applier,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("distinct signer"), "got: {err}");
        assert!(
            applier.seen.lock().unwrap().is_none(),
            "no node may be contacted when the reproduction floor is unmet"
        );
        assert!(
            collector.seen_gov.lock().unwrap().is_none(),
            "the governance bundle must not even be collected"
        );
    }

    /// A veto needs no reproducible-build proof — vetoing moves toward LESS
    /// trust and must never be harder than admitting.
    #[tokio::test]
    async fn veto_requires_no_repro_proof() {
        let status = Status(7, [0xCD; 32]);
        let collector = Collector::new(0);
        let applier = Applier::new(2);

        let out = run_mrenclave_governance(
            OP_REMOVE, [0x22; 32], [0xAA; 20], &status, &collector, &applier,
        )
        .await
        .unwrap();

        assert_eq!(out.allowlist_epoch, 8);
        assert_eq!(out.repro_signers, 0);
        assert!(
            !*collector.repro_called.lock().unwrap(),
            "a veto must not even ask for a reproduction proof"
        );
        let (_, _, repro) = applier.seen.lock().unwrap().clone().unwrap();
        assert!(repro.is_empty(), "a veto carries no repro bundle");
    }

    #[tokio::test]
    async fn unknown_op_is_refused() {
        let status = Status(1, [0u8; 32]);
        let collector = Collector::new(9);
        let applier = Applier::new(1);
        let err =
            run_mrenclave_governance(9, [0x11; 32], [0xAA; 20], &status, &collector, &applier)
                .await
                .unwrap_err();
        assert!(
            err.to_string().contains("unknown allowlist op"),
            "got: {err}"
        );
    }
}
