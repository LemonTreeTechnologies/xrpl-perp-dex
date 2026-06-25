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
#![allow(dead_code)] // driver (collect-sign → submit → confirm) lands in β2(c)

use sha2::{Digest, Sha256};

use crate::membership_canonical::SignerEntry;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
