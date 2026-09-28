//! The quorum-bundle wire format — ONE producer, because there were four.
//!
//! WHY THIS MODULE EXISTS. Four separate functions in this crate emitted these bytes:
//! `membership_coordinator::build_quorum_bundle`, `mrenclave_governance::build_quorum_bundle`,
//! `reserves_baseline::build_quorum_bundle` and `path_a_delegation::build_delegation_bundle`.
//! They agreed — byte for byte, verified before this consolidation — but only because each was
//! copied carefully. Nothing maintained the agreement, and there is exactly ONE consumer:
//! `seal_verify_quorum_bundle_with_set` inside the enclave.
//!
//! That shape has already cost this project once, in this same week: the SPV attested-prefix
//! framing had three producers against one consumer's parser, and TWO of the three never framed
//! the section at all — each tested against its own idea of the format, all green, the defect
//! live. Counting the producers is the check; a format with more producers than one is a format
//! waiting to disagree. A fifth producer was about to be added by the β18 upgrade corpus, which
//! is what prompted this.
//!
//! THE FORMAT, as the enclave's parser reads it:
//!
//! ```text
//!   uint32_t version      = 1        (little-endian)
//!   uint32_t entry_count             (little-endian)
//!   for each entry:
//!     uint8_t  pk_compressed[33]
//!     uint8_t  sig_len               (8..=72 — DER-encoded ECDSA)
//!     uint8_t  sig[sig_len]
//! ```
//!
//! The enclave DEDUPS by public key and sums weights, so a caller need not deduplicate for
//! correctness — but a bundle with duplicates is harder to read in operator logs, so callers
//! that can dedup cheaply still should.

/// Serialise `(pk_compressed, der_sig)` pairs into the bundle the enclave verifies.
///
/// No validation here on purpose: length and signature validity are the enclave's job, and a
/// producer that silently dropped a malformed entry would hide a caller's bug rather than let
/// the verifier report it. `sig_len` is written as a single byte, which is the format's own
/// limit — a signature longer than 255 bytes cannot be represented and is not reachable for
/// DER-encoded ECDSA (max 72).
pub fn build(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (pk, sig) in entries {
        out.extend_from_slice(pk);
        out.push(sig.len() as u8);
        out.extend_from_slice(sig);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_enclave_parser() {
        let pk_a = vec![0x02u8; 33];
        let pk_b = vec![0x03u8; 33];
        let sig_a = vec![0xAAu8; 70];
        let sig_b = vec![0xBBu8; 8]; // the documented lower bound
        let b = build(&[(pk_a.clone(), sig_a.clone()), (pk_b.clone(), sig_b.clone())]);

        assert_eq!(&b[0..4], &1u32.to_le_bytes(), "version");
        assert_eq!(&b[4..8], &2u32.to_le_bytes(), "entry_count");
        assert_eq!(&b[8..41], &pk_a[..], "first pk");
        assert_eq!(b[41], 70, "first sig_len");
        assert_eq!(&b[42..112], &sig_a[..], "first sig");
        assert_eq!(&b[112..145], &pk_b[..], "second pk");
        assert_eq!(b[145], 8, "second sig_len");
        assert_eq!(&b[146..154], &sig_b[..], "second sig");
        assert_eq!(b.len(), 154, "no trailing bytes: the parser rejects a tail");
    }

    #[test]
    fn zero_entries_is_a_well_formed_header() {
        // The enclave refuses this on threshold, not on parse — so it must still be
        // structurally valid, or the refusal an operator sees names the wrong problem.
        let b = build(&[]);
        assert_eq!(b, [1u32.to_le_bytes(), 0u32.to_le_bytes()].concat());
    }

    #[test]
    fn entries_are_emitted_in_the_order_given() {
        // Not cosmetic: the enclave dedups by pk keeping the FIRST occurrence, so order
        // decides which duplicate survives.
        let b = build(&[(vec![0x02; 33], vec![1; 8]), (vec![0x03; 33], vec![2; 8])]);
        assert_eq!(b[8], 0x02);
        assert_eq!(b[8 + 33 + 1 + 8], 0x03);
    }
}
