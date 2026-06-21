//! β1 (perp β-retrofit) canonical encodings for the off-chain membership-epoch
//! authority — the Rust (orchestrator) side of the cross-language contract.
//!
//! These MUST be byte-identical to the enclave's
//! `EthSignerEnclave/Enclave/membership_canonical.{h,cpp}` so that the message
//! the orchestrator asks operators to sign (and the bundle it broadcasts) is
//! exactly the message `ecall_seal_membership_epoch` reconstructs and verifies
//! in-enclave. A drift here is a silent liveness break or a bypass — hence the
//! frozen golden vectors below mirror the enclave host test
//! (`tests/test_xrpl_signerlist.cpp`) one-for-one (RESP-β1.1 O-β1.1-3 / Q-2).
//!
//! Canonical encodings (integers little-endian, fixed-length framing):
//!   set_hash     = SHA-256( u32(count) || u32(quorum) ||
//!                           {account_id[20] || u32(weight)} ASC by account_id )
//!   epoch_digest = SHA-256( "PERP_EPOCH_DIGEST_v1" || escrow[20] ||
//!                           u64(epoch) || prev_epoch_hash[32] || set_hash[32] )
//!   message_hash = SHA-256( "PERP_MEMBERSHIP_EPOCH_v1" || escrow[20] ||
//!                           u64(epoch) || prev_epoch_hash[32] || new_set_hash[32] )
#![allow(dead_code)] // consumed by the β1 single-bundle coordination (next increment)

use sha2::{Digest, Sha256};

/// A signer entry: XRPL AccountID (RIPEMD160(SHA256(pubkey))) + weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignerEntry {
    pub account_id: [u8; 20],
    pub weight: u32,
}

/// Distinct constant domain prefixes — hashed FIRST, never operator-supplied →
/// disjoint preimage spaces vs the Path-A delegation domain and any XRPL tx
/// prefix (REQ-β1.1 §2 / O-β1-3).
const MEMBERSHIP_EPOCH_DOMAIN: &[u8] = b"PERP_MEMBERSHIP_EPOCH_v1"; // 24 bytes
const EPOCH_DIGEST_DOMAIN: &[u8] = b"PERP_EPOCH_DIGEST_v1"; // 20 bytes

/// Canonical set hash. The input order is irrelevant — a copy is sorted
/// ascending by `account_id` (mirrors the XRPL `Signers[]` AccountID sort).
pub fn compute_set_hash(signers: &[SignerEntry], quorum: u32) -> [u8; 32] {
    let mut sorted: Vec<SignerEntry> = signers.to_vec();
    sorted.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    let mut h = Sha256::new();
    h.update((sorted.len() as u32).to_le_bytes());
    h.update(quorum.to_le_bytes());
    for s in &sorted {
        h.update(s.account_id);
        h.update(s.weight.to_le_bytes());
    }
    h.finalize().into()
}

/// Hash-chain link: the authority digest of an epoch. EXCLUDES the projection
/// metadata (`last_updated_*`) — the authority record is chain-independent (Q-3).
pub fn compute_epoch_authority_digest(
    escrow: &[u8; 20],
    epoch: u64,
    prev_epoch_hash: &[u8; 32],
    signers: &[SignerEntry],
    quorum: u32,
) -> [u8; 32] {
    let set_hash = compute_set_hash(signers, quorum);
    let mut h = Sha256::new();
    h.update(EPOCH_DIGEST_DOMAIN);
    h.update(escrow);
    h.update(epoch.to_le_bytes());
    h.update(prev_epoch_hash);
    h.update(set_hash);
    h.finalize().into()
}

/// The domain-separated message the off-chain quorum signs to authorise a
/// transition to `proposed_epoch` adopting the set whose hash is `new_set_hash`.
pub fn compute_membership_message_hash(
    escrow: &[u8; 20],
    proposed_epoch: u64,
    prev_epoch_hash: &[u8; 32],
    new_set_hash: &[u8; 32],
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(MEMBERSHIP_EPOCH_DOMAIN);
    h.update(escrow);
    h.update(proposed_epoch.to_le_bytes());
    h.update(prev_epoch_hash);
    h.update(new_set_hash);
    h.finalize().into()
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

    // FROZEN cross-language golden vectors — these MUST equal the enclave host
    // test (EthSignerEnclave/tests/test_xrpl_signerlist.cpp). Fixture: 2 signers
    // A(0x01*20, w1) B(0x02*20, w2), quorum 2; escrow 0xAA*20, epoch 5, prev 0xBB*32.
    #[test]
    fn beta1_set_hash_frozen() {
        let s = [entry(0x01, 1), entry(0x02, 2)];
        assert_eq!(
            hex(&compute_set_hash(&s, 2)),
            "2321fc70f09b9e269683cb04b42704db324d67c37ba09e1bb5a7be277761cf5b"
        );
        // sort-invariance: reversed input → identical hash.
        let r = [entry(0x02, 2), entry(0x01, 1)];
        assert_eq!(compute_set_hash(&s, 2), compute_set_hash(&r, 2));
    }

    #[test]
    fn beta1_message_hash_frozen() {
        let escrow = [0xAAu8; 20];
        let prev = [0xBBu8; 32];
        let set_hash = compute_set_hash(&[entry(0x01, 1), entry(0x02, 2)], 2);
        assert_eq!(
            hex(&compute_membership_message_hash(
                &escrow, 5, &prev, &set_hash
            )),
            "01ad9ce518f2e5dd4b970fd03746322621311acf1820d6d3f45d5b22f3c2f8f2"
        );
    }

    #[test]
    fn beta1_epoch_digest_frozen() {
        let escrow = [0xAAu8; 20];
        let prev = [0xBBu8; 32];
        let s = [entry(0x01, 1), entry(0x02, 2)];
        assert_eq!(
            hex(&compute_epoch_authority_digest(&escrow, 5, &prev, &s, 2)),
            "f5ff6db6d2b6ea958b50a6ac73d5cadf5081e04ae2274925034d6c1021c3b943"
        );
    }
}
