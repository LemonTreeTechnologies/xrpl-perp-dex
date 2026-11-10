//! The account's own authorisation of a withdrawal (`docs/withdrawal-authorization-v1.md`).
//!
//! The user signs, with the key of the account being debited, a fixed 106-byte message naming
//! the escrow, the account, the amount in drops, the destination and tag, a per-user nonce and
//! the last ledger the payment may land in. The orchestrator carries the authorisation verbatim
//! to every enclave that signs the withdrawal; each enclave rebuilds the message from the Payment
//! it is about to sign and verifies the signature itself. This module builds the same message
//! (pinned to the enclave by a shared vector) and checks the wire shape early.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DOMAIN: &[u8] = b"PERP_WITHDRAW_AUTH_v1";
pub const MSG_LEN: usize = 106;

/// The authorisation as it travels: request body → enclave, p2p co-sign claim, delivery queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawAuth {
    /// 33-byte public key, hex (`02`/`03`… secp256k1, `ED`… ed25519).
    pub user_pubkey: String,
    /// Signature, hex: DER (secp256k1) or 64 bytes (ed25519), over SHA-256(message) directly or
    /// over SHA-512Half of it (the form XRPL wallets produce).
    pub user_sig: String,
    /// Decimal string of a u64 — the user's last withdrawal nonce + 1.
    pub auth_nonce: String,
    /// The latest ledger the payment may land in; the Payment's LastLedgerSequence never exceeds it.
    pub max_ledger: u32,
}

impl WithdrawAuth {
    /// Shape only (lengths, encodings, ranges). The signature is checked by the enclaves.
    pub fn validate_shape(&self) -> Result<u64> {
        let pk = hex::decode(self.user_pubkey.trim_start_matches("0x"))
            .context("user_pubkey is not hex")?;
        if pk.len() != 33 || !(pk[0] == 0x02 || pk[0] == 0x03 || pk[0] == 0xED) {
            bail!("user_pubkey must be 33 bytes: 02/03 (secp256k1) or ED (ed25519)");
        }
        let sig =
            hex::decode(self.user_sig.trim_start_matches("0x")).context("user_sig is not hex")?;
        if sig.is_empty() || sig.len() > 72 {
            bail!("user_sig must be 1..72 bytes");
        }
        let nonce: u64 = self
            .auth_nonce
            .parse()
            .context("auth_nonce must be a u64 decimal string")?;
        if nonce == 0 || self.auth_nonce.starts_with('+') {
            bail!("auth_nonce must be a positive u64 decimal string");
        }
        if self.max_ledger == 0 {
            bail!("max_ledger must be a ledger index > 0");
        }
        Ok(nonce)
    }
}

/// The canonical message (identical to the enclave's `withdraw_auth_message`).
pub fn message(
    escrow: &[u8; 20],
    user: &[u8; 20],
    amount_drops: u64,
    destination: &[u8; 20],
    tag: Option<u32>,
    nonce: u64,
    max_ledger: u32,
) -> [u8; MSG_LEN] {
    let mut m = [0u8; MSG_LEN];
    let mut o = 0;
    let mut put = |b: &[u8]| {
        m[o..o + b.len()].copy_from_slice(b);
        o += b.len();
    };
    put(DOMAIN);
    put(escrow);
    put(user);
    put(&amount_drops.to_le_bytes());
    put(destination);
    put(&[u8::from(tag.is_some())]);
    put(&tag.unwrap_or(0).to_le_bytes());
    put(&nonce.to_le_bytes());
    put(&max_ledger.to_le_bytes());
    m
}

/// What the user signs: SHA-256 of the message.
pub fn digest(msg: &[u8; MSG_LEN]) -> [u8; 32] {
    Sha256::digest(msg).into()
}

/// Exact XRP decimal string → drops, refusing more than 6 decimals (no float on money).
pub fn xrp_to_drops(s: &str) -> Result<u64> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() {
        bail!("empty amount");
    }
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("amount must be a plain decimal");
    }
    let frac_trim = frac.trim_end_matches('0');
    if frac_trim.len() > 6 {
        bail!("amount has more than 6 decimals (1 drop = 0.000001 XRP)");
    }
    let int_v: u64 = if int.is_empty() {
        0
    } else {
        int.parse().context("amount too large")?
    };
    let frac_v: u64 = if frac_trim.is_empty() {
        0
    } else {
        format!("{frac_trim:0<6}").parse()?
    };
    int_v
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac_v))
        .context("amount overflows drops")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned with the enclave's tests/test_withdraw_auth.cpp and an independent Python
    /// computation: the three builders cannot drift apart silently.
    #[test]
    fn cross_language_vector() {
        let m = message(
            &[0x11; 20],
            &[0x44; 20],
            5_000_000,
            &[0x22; 20],
            Some(7),
            42,
            123_456,
        );
        assert_eq!(m.len(), 106);
        assert_eq!(&m[..21], DOMAIN);
        assert_eq!(
            hex::encode(digest(&m)),
            "3686d0f64fe19d38036bb4f1ad9038a22300bdb2c8dcac1a99c1ec17b55fefec"
        );
    }

    #[test]
    fn no_tag_is_flag_zero_and_tag_zero() {
        let a = message(&[1; 20], &[2; 20], 1, &[3; 20], None, 1, 1);
        let b = message(&[1; 20], &[2; 20], 1, &[3; 20], Some(0), 1, 1);
        assert_ne!(a, b, "no tag and tag 0 are different authorisations");
        assert_eq!(a[89], 0);
        assert_eq!(b[89], 1);
    }

    #[test]
    fn shape_checks() {
        let ok = WithdrawAuth {
            user_pubkey: format!("02{}", "ab".repeat(32)),
            user_sig: "30".repeat(70),
            auth_nonce: "5".into(),
            max_ledger: 100,
        };
        assert_eq!(ok.validate_shape().unwrap(), 5);
        let mut b = ok.clone();
        b.user_pubkey = format!("04{}", "ab".repeat(32));
        assert!(b.validate_shape().is_err(), "unknown key prefix");
        let mut b = ok.clone();
        b.user_sig = "00".repeat(73);
        assert!(b.validate_shape().is_err(), "oversized signature");
        let mut b = ok.clone();
        b.auth_nonce = "0".into();
        assert!(b.validate_shape().is_err(), "nonce 0");
        let mut b = ok.clone();
        b.auth_nonce = "1.5".into();
        assert!(b.validate_shape().is_err(), "nonce not an integer");
        let mut b = ok;
        b.max_ledger = 0;
        assert!(b.validate_shape().is_err(), "max_ledger 0");
    }

    #[test]
    fn exact_drops_no_float() {
        assert_eq!(xrp_to_drops("5").unwrap(), 5_000_000);
        assert_eq!(xrp_to_drops("5.00000000").unwrap(), 5_000_000);
        assert_eq!(xrp_to_drops("0.000001").unwrap(), 1);
        assert_eq!(xrp_to_drops("1.1").unwrap(), 1_100_000);
        assert_eq!(
            xrp_to_drops("99999999999.999999").unwrap(),
            99_999_999_999_999_999
        );
        assert!(
            xrp_to_drops("0.0000001").is_err(),
            "a sub-drop amount is refused, not rounded"
        );
        assert!(xrp_to_drops("-1").is_err());
        assert!(xrp_to_drops("1e3").is_err());
        assert!(xrp_to_drops("").is_err());
        // the f64 path rounded this one wrong: 0.1 + 0.2 style drift
        assert_eq!(xrp_to_drops("4.35").unwrap(), 4_350_000);
    }
}
