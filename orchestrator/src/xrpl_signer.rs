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

use crate::enclave_client::{EnclaveClient, EnclaveAccount};

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
    let prefix = if y[31] % 2 == 0 { 0x02 } else { 0x03 };

    let mut compressed = Vec::with_capacity(33);
    compressed.push(prefix);
    compressed.extend_from_slice(x);
    Ok(compressed)
}

/// Derive XRPL classic address (r...) from uncompressed public key hex.
///
/// XRPL address derivation:
///   1. Compress pubkey: 65 bytes -> 33 bytes
///   2. SHA-256(compressed) -> 32 bytes
///   3. RIPEMD-160(sha256) -> 20 bytes (account ID)
///   4. Base58Check encode with payload type prefix 0x00
pub fn pubkey_to_xrpl_address(uncompressed_hex: &str) -> Result<String> {
    let hex_clean = uncompressed_hex.strip_prefix("0x").unwrap_or(uncompressed_hex);
    let raw = hex::decode(hex_clean).context("invalid hex in pubkey")?;
    let compressed = compress_pubkey(&raw)?;

    // SHA-256
    let sha256_hash = Sha256::digest(&compressed);

    // RIPEMD-160
    let account_id = Ripemd160::digest(sha256_hash);

    // Base58Check with XRPL alphabet and type prefix 0x00
    // XRPL uses a custom Base58 alphabet:
    //   rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz
    const XRPL_ALPHABET: &[u8; 58] =
        b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
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

/// DER-encode an ECDSA signature (r, s) for XRPL's TxnSignature field.
///
/// DER format:
///   30 <total_len>
///     02 <r_len> <r_bytes>
///     02 <s_len> <s_bytes>
///
/// Both r and s are big-endian unsigned integers.
/// If the high bit is set, a 0x00 byte is prepended (ASN.1 signed integer).
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

/// SHA-512Half: first 32 bytes of SHA-512.
/// This is XRPL's signing hash function.
pub fn sha512_half(data: &[u8]) -> [u8; 32] {
    let full = Sha512::digest(data);
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    result
}

/// Signs XRPL transactions using the SGX enclave.
///
/// Holds enclave account metadata (address, pubkey, session_key).
/// Replaces xrpl.wallet.Wallet for transaction signing.
pub struct XrplSigner {
    pub enclave: EnclaveClient,
    /// Ethereum-format enclave address ("0x...")
    pub eth_address: String,
    /// Uncompressed 65-byte public key hex ("0x...")
    pub pubkey_uncompressed: String,
    /// Session key for signing requests ("0x...")
    pub session_key: String,
    /// Compressed public key, uppercase hex (for SigningPubKey)
    pub compressed_pubkey_hex: String,
    /// XRPL classic address ("r...")
    pub xrpl_address: String,
}

impl XrplSigner {
    /// Create a signer from an enclave client and generated account.
    pub fn new(enclave: EnclaveClient, account: &EnclaveAccount) -> Result<Self> {
        let hex_clean = account
            .public_key
            .strip_prefix("0x")
            .unwrap_or(&account.public_key);
        let raw = hex::decode(hex_clean).context("invalid pubkey hex")?;
        let compressed = compress_pubkey(&raw)?;
        let compressed_hex = hex::encode_upper(&compressed);
        let xrpl_address = pubkey_to_xrpl_address(&account.public_key)?;

        Ok(Self {
            enclave,
            eth_address: account.address.clone(),
            pubkey_uncompressed: account.public_key.clone(),
            session_key: account.session_key.clone(),
            compressed_pubkey_hex: compressed_hex,
            xrpl_address,
        })
    }

    /// Sign an XRPL transaction (represented as JSON).
    ///
    /// This is a simplified stub — full implementation requires XRPL binary
    /// codec serialization (`encode_for_signing`). For now it:
    ///   1. Sets SigningPubKey
    ///   2. Serializes the JSON canonically
    ///   3. Computes SHA-512Half
    ///   4. Sends hash to enclave
    ///   5. DER-encodes the signature
    ///   6. Injects TxnSignature
    ///
    /// TODO: integrate proper XRPL binary codec for production use.
    pub async fn sign_xrpl_tx(
        &self,
        tx_json: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut tx = tx_json.clone();

        // Set SigningPubKey (compressed, uppercase hex)
        tx["SigningPubKey"] = serde_json::Value::String(self.compressed_pubkey_hex.clone());

        // For a full implementation, we would use XRPL binary codec to serialize.
        // For now, serialize the JSON canonically as a placeholder.
        // Production code should use encode_for_signing() equivalent.
        let serialized = serde_json::to_vec(&tx)?;

        // SHA-512Half -> 32-byte signing hash
        let signing_hash = sha512_half(&serialized);

        // Send hash to enclave for ECDSA signing
        let hash_hex = format!("0x{}", hex::encode(signing_hash));
        let sig = self
            .enclave
            .sign_hash(&self.eth_address, &self.session_key, &hash_hex)
            .await?;

        // DER-encode (r, s)
        let r_bytes = hex::decode(&sig.r).context("invalid r hex")?;
        let s_bytes = hex::decode(&sig.s).context("invalid s hex")?;
        let der_sig = der_encode_signature(&r_bytes, &s_bytes);

        // Inject signature
        tx["TxnSignature"] = serde_json::Value::String(hex::encode_upper(&der_sig));

        Ok(tx)
    }

    /// Get the XRPL classic address.
    pub fn address(&self) -> &str {
        &self.xrpl_address
    }

    /// Get the compressed public key hex for XRPL.
    pub fn signing_pubkey(&self) -> &str {
        &self.compressed_pubkey_hex
    }

    /// Serialize account data for storage (no private keys — only enclave references).
    pub fn to_account_data(&self) -> serde_json::Value {
        serde_json::json!({
            "enclave_address": self.eth_address,
            "public_key": self.pubkey_uncompressed,
            "session_key": self.session_key,
            "compressed_pubkey": self.compressed_pubkey_hex,
            "xrpl_address": self.xrpl_address,
        })
    }
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
             483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8"
        ).unwrap();
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
        assert!(addr.starts_with('r'), "XRPL address should start with 'r': {}", addr);
    }
}
