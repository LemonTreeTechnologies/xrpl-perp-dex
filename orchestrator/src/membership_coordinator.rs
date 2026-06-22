//! β1 (perp β-retrofit) single-bundle membership-epoch coordination —
//! orchestrator side. This module owns the ceremony **decision logic**: given
//! the current sealed epoch (read from the enclave) and a requested new signer
//! set, it produces the single `MembershipEpochStatement` the off-chain quorum
//! is asked to authorise.
//!
//! The (P) single-successor enforcer (REQ-β1.1 §1): exactly ONE statement is
//! prepared, ONE quorum bundle is collected over its `message_hash` via the
//! libp2p signing-relay (the same relay as Path-A delegation + withdrawals),
//! and the SAME `(statement, bundle)` is broadcast to every node, each of which
//! calls `ecall_seal_membership_epoch`. The per-node monotonic-epoch guard then
//! rejects any second/different epoch N+1 (the in-enclave half of (P)).
//!
//! Scope of THIS module: the pure, deterministic statement preparation +
//! validation (mirrors the enclave's pre-seal sanity so a doomed ceremony is
//! rejected before any signatures are collected). The libp2p relay collection,
//! the broadcast, and the per-node `ecall_seal_membership_epoch` HTTP call are
//! wired separately (they reuse the delegation-bundle relay + a new enclave
//! admin route). Deploy/cutover is β3.

#![allow(dead_code)] // ceremony wiring (relay + enclave call) lands next increment

use crate::membership_canonical::{compute_membership_message_hash, compute_set_hash, SignerEntry};

const MAX_SIGNERS: usize = 32; // XRPL SignerList limit (mirrors the enclave)

/// The fully-resolved membership transition presented to the quorum. Every
/// field except `message_hash` is an input to `ecall_seal_membership_epoch`;
/// `message_hash` is what the quorum signs and what the bundle is collected
/// over (the enclave recomputes it from the other fields and must agree).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipEpochStatement {
    pub escrow: [u8; 20],
    pub proposed_epoch: u64,
    pub prev_epoch_hash: [u8; 32],
    pub new_signers: Vec<SignerEntry>,
    pub new_quorum: u32,
    pub new_set_hash: [u8; 32],
    pub message_hash: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrepareError {
    EmptySet,
    TooManySigners,
    ZeroWeight,
    QuorumOutOfBounds,
}

/// Prepare the single membership-transition statement. `proposed_epoch =
/// current_epoch + 1`; `prev_epoch_hash = current_epoch_digest` (the hash-chain
/// link the enclave will re-derive and check against its CURRENT sealed epoch).
/// Pure + deterministic — the basis of the (P) single statement. Validation
/// mirrors the enclave's pre-seal quorum sanity so a doomed ceremony never
/// collects signatures.
pub fn prepare_statement(
    escrow: [u8; 20],
    current_epoch: u64,
    current_epoch_digest: [u8; 32],
    new_signers: Vec<SignerEntry>,
    new_quorum: u32,
) -> Result<MembershipEpochStatement, PrepareError> {
    if new_signers.is_empty() {
        return Err(PrepareError::EmptySet);
    }
    if new_signers.len() > MAX_SIGNERS {
        return Err(PrepareError::TooManySigners);
    }
    let mut weight_sum: u64 = 0;
    for s in &new_signers {
        if s.weight == 0 {
            return Err(PrepareError::ZeroWeight);
        }
        weight_sum += s.weight as u64;
    }
    if new_quorum == 0 || (new_quorum as u64) > weight_sum {
        return Err(PrepareError::QuorumOutOfBounds);
    }

    let proposed_epoch = current_epoch + 1;
    let prev_epoch_hash = current_epoch_digest;
    let new_set_hash = compute_set_hash(&new_signers, new_quorum);
    let message_hash =
        compute_membership_message_hash(&escrow, proposed_epoch, &prev_epoch_hash, &new_set_hash);

    Ok(MembershipEpochStatement {
        escrow,
        proposed_epoch,
        prev_epoch_hash,
        new_signers,
        new_quorum,
        new_set_hash,
        message_hash,
    })
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
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The statement's message_hash MUST equal the cross-language golden vector:
    /// transition current_epoch 4 → 5 adopting {A,B} quorum 2, escrow 0xAA*20,
    /// current digest = 0xBB*32. Ties the coordinator to the frozen contract.
    #[test]
    fn prepare_statement_matches_golden_message_hash() {
        let st = prepare_statement(
            [0xAA; 20],
            4,
            [0xBB; 32],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
        )
        .expect("valid");
        assert_eq!(st.proposed_epoch, 5);
        assert_eq!(st.prev_epoch_hash, [0xBB; 32]);
        assert_eq!(
            hex(&st.new_set_hash),
            "2321fc70f09b9e269683cb04b42704db324d67c37ba09e1bb5a7be277761cf5b"
        );
        assert_eq!(
            hex(&st.message_hash),
            "01ad9ce518f2e5dd4b970fd03746322621311acf1820d6d3f45d5b22f3c2f8f2"
        );
    }

    #[test]
    fn prepare_statement_rejects_bad_input() {
        let esc = [0u8; 20];
        let dig = [0u8; 32];
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![], 1),
            Err(PrepareError::EmptySet)
        );
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 0)], 1),
            Err(PrepareError::ZeroWeight)
        );
        // quorum 0 and quorum > weight sum (1+2=3)
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 1), entry(2, 2)], 0),
            Err(PrepareError::QuorumOutOfBounds)
        );
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 1), entry(2, 2)], 4),
            Err(PrepareError::QuorumOutOfBounds)
        );
        let too_many: Vec<SignerEntry> = (0..33).map(|i| entry(i as u8, 1)).collect();
        assert_eq!(
            prepare_statement(esc, 1, dig, too_many, 1),
            Err(PrepareError::TooManySigners)
        );
    }
}
