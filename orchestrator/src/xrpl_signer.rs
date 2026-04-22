//! XRPL transaction signing via SGX enclave.
//!
//! Rewrite of `sgx_signer.py`. The enclave generates secp256k1 keypairs and
//! signs raw 32-byte hashes. This module handles:
//!   - Public key compression (uncompressed 65B -> compressed 33B)
//!   - XRPL address derivation (SHA-256 -> RIPEMD-160 -> Base58Check)
//!   - DER signature encoding
//!   - SHA-512Half (XRPL's signing hash function)
//!
//! Architecture: hash computation always happens outside the enclave.
//! The enclave only ever signs raw 32-byte hashes.

use anyhow::{bail, Context, Result};
use ripemd::Ripemd160;
use sha2::{Digest, Sha256, Sha512};

#[allow(dead_code)]
/// Compress an uncompressed secp256k1 public key (65 bytes: 04 || x || y)
/// to compressed form (33 bytes: 02/03 || x).
pub fn compress_pubkey(uncompressed: &[u8]) -> Result<Vec<u8>> {
    if uncompressed.len() != 65 || uncompressed[0] != 0x04 {
        bail!(
            "expected uncompressed pubkey (04 + 64 bytes), got {} bytes",
            uncompressed.len()
        );
    }

    let x = &uncompressed[1..33];
    let y = &uncompressed[33..65];

    // Even y -> prefix 02, odd y -> prefix 03
    let prefix = if y[31].is_multiple_of(2) { 0x02 } else { 0x03 };

    let mut compressed = Vec::with_capacity(33);
    compressed.push(prefix);
    compressed.extend_from_slice(x);
    Ok(compressed)
}

#[allow(dead_code)]
/// Derive XRPL classic address (r...) from uncompressed public key hex.
///
/// XRPL address derivation:
///   1. Compress pubkey: 65 bytes -> 33 bytes
///   2. SHA-256(compressed) -> 32 bytes
///   3. RIPEMD-160(sha256) -> 20 bytes (account ID)
///   4. Base58Check encode with payload type prefix 0x00
pub fn pubkey_to_xrpl_address(uncompressed_hex: &str) -> Result<String> {
    let hex_clean = uncompressed_hex
        .strip_prefix("0x")
        .unwrap_or(uncompressed_hex);
    let raw = hex::decode(hex_clean).context("invalid hex in pubkey")?;
    let compressed = compress_pubkey(&raw)?;

    // SHA-256
    let sha256_hash = Sha256::digest(&compressed);

    // RIPEMD-160
    let account_id = Ripemd160::digest(sha256_hash);

    // Base58Check with XRPL alphabet and type prefix 0x00
    // XRPL uses a custom Base58 alphabet:
    //   rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz
    const XRPL_ALPHABET: &[u8; 58] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alphabet = bs58::Alphabet::new(XRPL_ALPHABET).expect("valid alphabet");

    // Payload: [0x00] + 20-byte account_id
    let mut payload = Vec::with_capacity(25);
    payload.push(0x00); // account type prefix
    payload.extend_from_slice(&account_id);

    // Checksum: first 4 bytes of SHA-256(SHA-256(payload))
    let hash1 = Sha256::digest(&payload);
    let hash2 = Sha256::digest(hash1);
    payload.extend_from_slice(&hash2[..4]);

    // Base58 encode (no additional check — we computed our own checksum)
    let encoded = bs58::encode(&payload)
        .with_alphabet(&alphabet)
        .into_string();

    Ok(encoded)
}

#[allow(dead_code)]
/// DER-encode an ECDSA signature (r, s) for XRPL's TxnSignature field.
///
/// DER format:
///   30 <total_len>
///     02 <r_len> <r_bytes>
///     02 <s_len> <s_bytes>
///
/// Both r and s are big-endian unsigned integers.
/// If the high bit is set, a 0x00 byte is prepended (ASN.1 signed integer).
/// Decode an XRPL classic r-address into a 20-byte AccountID.
///
/// Base58Check with XRPL alphabet: decode → strip 1-byte version prefix
/// and 4-byte checksum → 20 bytes remain.
pub fn decode_xrpl_address(addr: &str) -> Result<[u8; 20]> {
    const XRPL_ALPHABET: &[u8; 58] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alphabet = bs58::Alphabet::new(XRPL_ALPHABET).expect("valid alphabet");
    let decoded = bs58::decode(addr)
        .with_alphabet(&alphabet)
        .into_vec()
        .context("invalid XRPL base58 address")?;
    if decoded.len() != 25 {
        bail!(
            "decoded XRPL address is {} bytes, expected 25",
            decoded.len()
        );
    }
    let payload = &decoded[..21];
    let checksum = &decoded[21..25];
    let hash1 = Sha256::digest(payload);
    let hash2 = Sha256::digest(hash1);
    if &hash2[..4] != checksum {
        bail!("XRPL address checksum mismatch");
    }
    let mut account_id = [0u8; 20];
    account_id.copy_from_slice(&decoded[1..21]);
    Ok(account_id)
}

pub fn der_encode_signature(r: &[u8], s: &[u8]) -> Vec<u8> {
    fn encode_integer(bytes: &[u8]) -> Vec<u8> {
        // Strip leading zeros but keep at least one byte
        let stripped = match bytes.iter().position(|&b| b != 0) {
            Some(pos) => &bytes[pos..],
            None => &[0u8],
        };

        // Prepend 0x00 if high bit is set
        let mut tlv = Vec::new();
        tlv.push(0x02); // INTEGER tag
        if stripped[0] & 0x80 != 0 {
            tlv.push((stripped.len() + 1) as u8);
            tlv.push(0x00);
        } else {
            tlv.push(stripped.len() as u8);
        }
        tlv.extend_from_slice(stripped);
        tlv
    }

    let r_tlv = encode_integer(r);
    let s_tlv = encode_integer(s);

    let mut der = Vec::new();
    der.push(0x30); // SEQUENCE tag
    der.push((r_tlv.len() + s_tlv.len()) as u8);
    der.extend_from_slice(&r_tlv);
    der.extend_from_slice(&s_tlv);
    der
}

#[allow(dead_code)]
/// SHA-512Half: first 32 bytes of SHA-512.
/// This is XRPL's signing hash function.
pub fn sha512_half(data: &[u8]) -> [u8; 32] {
    let full = Sha512::digest(data);
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_pubkey() {
        // Known test vector: uncompressed with even y
        let uncompressed = hex::decode(
            "04\
             79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798\
             483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8",
        )
        .unwrap();
        let compressed = compress_pubkey(&uncompressed).unwrap();
        assert_eq!(compressed.len(), 33);
        assert_eq!(compressed[0], 0x02); // even y
    }

    #[test]
    fn test_der_encode_signature() {
        let r = vec![0x01, 0x02, 0x03];
        let s = vec![0x04, 0x05, 0x06];
        let der = der_encode_signature(&r, &s);
        assert_eq!(der[0], 0x30); // SEQUENCE
        assert_eq!(der[2], 0x02); // INTEGER (r)
        assert_eq!(der[2 + 2 + 3], 0x02); // INTEGER (s)
    }

    #[test]
    fn test_sha512_half() {
        let data = b"test";
        let hash = sha512_half(data);
        assert_eq!(hash.len(), 32);
        // First 32 bytes of SHA-512("test")
        let full = Sha512::digest(data);
        assert_eq!(&hash[..], &full[..32]);
    }

    #[test]
    fn test_pubkey_to_xrpl_address() {
        // The address derivation should produce a string starting with 'r'
        let uncompressed_hex = "04\
            79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798\
            483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8";
        let addr = pubkey_to_xrpl_address(uncompressed_hex).unwrap();
        assert!(
            addr.starts_with('r'),
            "XRPL address should start with 'r': {addr}"
        );
    }
}
