//! AC-BASE: orchestrator side of the one-time custody-baseline ceremony.
//!
//! Recomputes the enclave's issuer/account-PINNED baseline message hash (so the
//! quorum bundle it assembles verifies against `seal_verify_quorum_bundle_with_set`),
//! recovers each node's compressed secp256k1 pubkey from its recoverable signature
//! (no extra ecall needed — the 65-byte [r||s||v] is recoverable), and encodes the
//! wire bundle the enclave consumes. The message hash MUST match
//! `compute_perp_reserves_baseline_message_hash` byte-for-byte — a golden vector
//! cross-checks it against the C++ (test `baseline_hash_matches_enclave_golden`).
//!
//! NOTE: the ceremony driver that consumes these building blocks (escrow query →
//! per-node sign → recover → bundle → apply) lands in the next commit; until then
//! the helpers are dead-code-allowed.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Domain prefix — must equal `kReservesBaselineDomain` in perp_reserves_baseline.cpp.
const BASELINE_DOMAIN: &[u8] = b"PERP_RESERVES_BASELINE_v1"; // 25 bytes

/// SHA-256 over the exact preimage the enclave hashes:
///   domain || le_u32(shard) || le_u64(L) || escrow_account[20] || rlusd_issuer[20]
///   || le_u64(rlusd) || le_u64(xrp)
pub fn baseline_message_hash(
    shard_id: u32,
    ledger_index: u64,
    escrow_account: &[u8; 20],
    rlusd_issuer: &[u8; 20],
    escrow_rlusd: i64,
    escrow_xrp: i64,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(BASELINE_DOMAIN);
    h.update(shard_id.to_le_bytes());
    h.update(ledger_index.to_le_bytes());
    h.update(escrow_account);
    h.update(rlusd_issuer);
    h.update((escrow_rlusd as u64).to_le_bytes());
    h.update((escrow_xrp as u64).to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// From a node's recoverable signature `(r, s, v)` over `msg_hash`, recover the
/// compressed pubkey (33) + DER-encode `(r, s)`. The enclave normalises S (low-S)
/// before returning, so the DER is canonical and `seal_verify` accepts it.
pub fn recover_pubkey_and_der(
    r_hex: &str,
    s_hex: &str,
    v: u8,
    msg_hash: &[u8; 32],
) -> Result<(Vec<u8>, Vec<u8>)> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    let r = hex::decode(r_hex.trim_start_matches("0x")).context("decode r")?;
    let s = hex::decode(s_hex.trim_start_matches("0x")).context("decode s")?;
    if r.len() != 32 || s.len() != 32 {
        bail!("r/s must be 32 bytes each (got {}/{})", r.len(), s.len());
    }
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(&r);
    rs[32..].copy_from_slice(&s);
    let sig = Signature::from_slice(&rs).context("parse ecdsa r||s")?;
    let rec = if v >= 27 { v - 27 } else { v };
    let rec_id = RecoveryId::from_byte(rec).context("recovery id out of range")?;
    let vk = VerifyingKey::recover_from_prehash(msg_hash, &sig, rec_id)
        .context("pubkey recovery failed (wrong figure/hash or bad v?)")?;
    let pk = vk.to_encoded_point(true).as_bytes().to_vec(); // 33-byte compressed
    let der = sig.to_der().as_bytes().to_vec();
    Ok((pk, der))
}

/// Wire format `seal_verify_quorum_bundle_with_set` consumes:
///   u32 version=1 || u32 count || { pk[33] || u8 sig_len || sig[sig_len] }…
/// (matches `mrenclave_governance::build_quorum_bundle`). Entries must be distinct.
pub fn build_quorum_bundle(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
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

/// Decode a 20-byte hex (0x-optional) into a fixed array.
pub fn hex20(s: &str) -> Result<[u8; 20]> {
    let v = hex::decode(s.trim_start_matches("0x")).context("hex20 decode")?;
    if v.len() != 20 {
        bail!("expected 20 bytes, got {}", v.len());
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&v);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector cross-check vs the C++ enclave hash. Inputs match
    /// tests/test_perp_reserves_baseline.cpp (acct=0x10+i, issuer=0xA0+i, shard=0,
    /// L=84000000, rlusd=123456789012, xrp=55550000). The expected hex is emitted
    /// by that C++ test (printed golden) — if the two encodings drift, the recovered
    /// pubkeys differ and the live quorum would silently fail; this catches it.
    #[test]
    fn baseline_hash_matches_enclave_golden() {
        let mut acct = [0u8; 20];
        let mut issuer = [0u8; 20];
        for i in 0..20 {
            acct[i] = 0x10 + i as u8;
            issuer[i] = 0xA0 + i as u8;
        }
        let h = baseline_message_hash(0, 84_000_000, &acct, &issuer, 123_456_789_012, 55_550_000);
        // GOLDEN emitted by tests/test_perp_reserves_baseline.cpp (C++ / BearSSL SHA-256).
        let expected = "be64d2ff057f196728156d39b4b5df4701446ff4dd9237d56856df35551a85f6";
        assert_eq!(
            hex::encode(h),
            expected,
            "Rust baseline hash must match the C++ enclave hash"
        );
    }

    #[test]
    fn bundle_wire_format() {
        let e = vec![(vec![0x02u8; 33], vec![0xAAu8; 70])];
        let b = build_quorum_bundle(&e);
        assert_eq!(&b[0..4], &1u32.to_le_bytes()); // version
        assert_eq!(&b[4..8], &1u32.to_le_bytes()); // count
        assert_eq!(b[8..41], [0x02u8; 33]); // pk
        assert_eq!(b[41], 70); // sig_len
        assert_eq!(b.len(), 8 + 33 + 1 + 70);
    }
}
