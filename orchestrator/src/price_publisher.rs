//! Signed price-quote producer in the enclave's exact `XPQ1` wire format.
//!
//! This module is the SIGNER + ASSEMBLER half of the signed-price path. A
//! (separate, later) per-node driver POSTs the assembled blob to the enclave's
//! `/v1/perp/price/update-signed` route; the AUTHORITATIVE verifier is the
//! enclave's `price_quotes.cpp`. The Rust round-trip test in this file is a
//! SMOKE test only — it proves this producer agrees with ITSELF, not that it
//! agrees with the enclave. The seam between producer and consumer is tested
//! against the enclave, not here.
//!
//! WHY THESE PRIMITIVES. The per-quote signature is XRPL's signer-signature
//! scheme exactly as the orchestrator already signs XRPL transactions:
//!   * SHA-512Half of a domain-prefixed preimage — reusing
//!     `crate::xrpl_signer::sha512_half` (the ONE named SHA-512Half in the
//!     crate), and
//!   * secp256k1 ECDSA, DER-encoded, LOW-S normalised — reusing the identical
//!     `k256` `sign_prehash` + `Signature::to_der()` path that
//!     `cli_tools`'s `XrplKeypair::sign_hash` and `reserves_baseline` use for
//!     real on-chain XRPL signatures. `k256`'s `sign_prehash` returns a low-S
//!     signature by default; the `low_s_is_default` test pins that invariant so
//!     a future `k256` bump cannot silently emit high-S (which the enclave,
//!     like the XRPL network, rejects as non-canonical).
//!
//! No new signing crate and no second SHA-512 implementation are introduced.

#![allow(dead_code)] // SIGNER+ASSEMBLER library; the per-node driver that calls it lands next.

use anyhow::{bail, Context, Result};
use k256::ecdsa::signature::hazmat::PrehashSigner;
use k256::ecdsa::{Signature, SigningKey};

/// Domain prefix for a price-quote signature preimage: ASCII `PRC\0`.
/// `0x50524300u32.to_be_bytes() == [0x50, 0x52, 0x43, 0x00]`.
const PRICE_QUOTE_PREFIX: u32 = 0x5052_4300;

/// Ripple epoch offset (seconds between the Unix epoch and 2000-01-01T00:00:00Z).
/// `publish_time` fields in this module are ALREADY in ripple-epoch seconds; this
/// constant is exported only so a caller converting a Unix time has one source.
pub const RIPPLE_EPOCH_OFFSET_SECS: u64 = 946_684_800;

/// Maximum quotes per `XPQ1` blob (the `count` field is one byte and the enclave caps at 16).
pub const MAX_QUOTES: usize = 16;

/// Maximum DER-encoded secp256k1 signature length (the `sig_len` field is one byte; a
/// DER ECDSA signature over secp256k1 is at most 72 bytes).
pub const MAX_SIG_LEN: usize = 72;

/// The 28-byte signature preimage for one price quote.
///
/// Layout (all integers big-endian):
/// ```text
///   be32(0x50524300)   // 'PRC\0'
///   be32(network_id)   // u32
///   be32(market_id)    // u32
///   be64(price_fp8)    // i64, two's complement (price * 1e8)
///   be64(publish_time) // u64, RIPPLE epoch seconds
/// ```
pub fn price_quote_preimage(
    network_id: u32,
    market_id: u32,
    price_fp8: i64,
    publish_time: u64,
) -> [u8; 28] {
    let mut p = [0u8; 28];
    p[0..4].copy_from_slice(&PRICE_QUOTE_PREFIX.to_be_bytes());
    p[4..8].copy_from_slice(&network_id.to_be_bytes());
    p[8..12].copy_from_slice(&market_id.to_be_bytes());
    // i64::to_be_bytes is two's-complement big-endian — no cast, no wrap.
    p[12..20].copy_from_slice(&price_fp8.to_be_bytes());
    p[20..28].copy_from_slice(&publish_time.to_be_bytes());
    p
}

/// The 32-byte signature digest for one price quote: XRPL SHA-512Half of the preimage.
pub fn price_quote_digest(
    network_id: u32,
    market_id: u32,
    price_fp8: i64,
    publish_time: u64,
) -> [u8; 32] {
    let preimage = price_quote_preimage(network_id, market_id, price_fp8, publish_time);
    crate::xrpl_signer::sha512_half(&preimage)
}

/// Sign one price quote with `secret_key`.
///
/// Returns the signer's 33-byte compressed secp256k1 public key and the
/// DER-encoded, LOW-S-normalised ECDSA signature over the quote's SHA-512Half
/// digest — byte-for-byte the scheme the enclave's `price_quotes.cpp` verifies.
pub fn sign_quote(
    secret_key: &SigningKey,
    network_id: u32,
    market_id: u32,
    price_fp8: i64,
    publish_time: u64,
) -> Result<([u8; 33], Vec<u8>)> {
    let digest = price_quote_digest(network_id, market_id, price_fp8, publish_time);

    // Identical to cli_tools::XrplKeypair::sign_hash — k256 normalises to low-S.
    let (sig, _): (Signature, _) = secret_key
        .sign_prehash(&digest)
        .map_err(|e| anyhow::anyhow!("secp256k1 price-quote signing failed: {e}"))?;
    let der = sig.to_der().as_bytes().to_vec();

    let compressed_point = secret_key.verifying_key().to_encoded_point(true);
    let compressed: [u8; 33] = compressed_point
        .as_bytes()
        .try_into()
        .context("compressed secp256k1 pubkey was not 33 bytes")?;

    Ok((compressed, der))
}

/// One quote's wire fields: `(compressed_pubkey, price_fp8, publish_time, der_sig)`.
pub type QuoteEntry = ([u8; 33], i64, u64, Vec<u8>);

/// Assemble the `XPQ1` blob the enclave consumes.
///
/// ```text
///   "XPQ1"               // 4 ascii bytes
///   0x01                 // version
///   count                // 1 byte, number of quotes (<= 16)
///   for each quote:
///     pubkey[33]         // compressed secp256k1 public key
///     be64(price_fp8)    // i64 big-endian
///     be64(publish_time) // u64 big-endian
///     sig_len            // 1 byte (<= 72)
///     sig[sig_len]       // DER secp256k1 low-S signature
///   // no trailing bytes
/// ```
///
/// Returns an error (never a truncated byte) when `count > 16` or any
/// `sig_len > 72`, since both counters are serialised as a single byte and a
/// truncating `as u8` would silently corrupt the blob. The byte layout of a
/// valid blob is unaffected by this guard.
pub fn assemble_xpq1(quotes: &[QuoteEntry]) -> Result<Vec<u8>> {
    if quotes.len() > MAX_QUOTES {
        bail!(
            "too many quotes for one XPQ1 blob: {} (max {MAX_QUOTES})",
            quotes.len()
        );
    }
    // Checked, not cast: len <= MAX_QUOTES (16) so this cannot fail, but the
    // conversion carries the proof instead of a bare `as u8`.
    let count = u8::try_from(quotes.len()).expect("len <= MAX_QUOTES fits in u8");

    let mut out = Vec::new();
    out.extend_from_slice(b"XPQ1");
    out.push(0x01u8); // version
    out.push(count);

    for (idx, (pubkey, price_fp8, publish_time, sig)) in quotes.iter().enumerate() {
        if sig.len() > MAX_SIG_LEN {
            bail!(
                "quote {idx}: DER signature is {} bytes (max {MAX_SIG_LEN})",
                sig.len()
            );
        }
        let sig_len = u8::try_from(sig.len()).expect("sig.len() <= MAX_SIG_LEN fits in u8");

        out.extend_from_slice(pubkey); // 33 bytes
        out.extend_from_slice(&price_fp8.to_be_bytes()); // i64 be
        out.extend_from_slice(&publish_time.to_be_bytes()); // u64 be
        out.push(sig_len);
        out.extend_from_slice(sig);
    }
    Ok(out)
}

/// Generate `n` DISTINCT secp256k1 test keypairs.
///
/// Random keys are distinct with overwhelming probability; this still checks and
/// regenerates on the astronomically unlikely collision so callers get the
/// guarantee by construction rather than by assumption. These are DISPOSABLE
/// mechanism-proof keys — never custody material.
pub fn gen_distinct_keypairs(n: usize) -> Vec<SigningKey> {
    use rand::rngs::OsRng;
    let mut keys: Vec<SigningKey> = Vec::with_capacity(n);
    let mut seen: std::collections::BTreeSet<[u8; 33]> = std::collections::BTreeSet::new();
    while keys.len() < n {
        let sk = SigningKey::random(&mut OsRng);
        let pk: [u8; 33] = sk
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .try_into()
            .expect("compressed secp256k1 pubkey is 33 bytes");
        if seen.insert(pk) {
            keys.push(sk);
        }
    }
    keys
}

/// Compressed 33-byte public key of a signing key, as hex.
pub fn compressed_pubkey_hex(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
}

/// One publisher keypair entry in the `PRICE_PUBLISHER_KEYS` JSON file.
#[derive(serde::Deserialize)]
struct PublisherKeyEntry {
    secret_hex: String,
    /// Optional: when present it is cross-checked against the key we derive, so a
    /// hand-edited file fails here with a clear message rather than earning a -93
    /// signature rejection from the enclave.
    #[serde(default)]
    pubkey_compressed_hex: Option<String>,
}

/// The `PRICE_PUBLISHER_KEYS` JSON file: `{"keys":[{"secret_hex":...}, ...]}`.
#[derive(serde::Deserialize)]
struct PublisherKeyFile {
    keys: Vec<PublisherKeyEntry>,
}

/// Minimum signers in the mechanism-proof key file. A shorter file would only
/// ever earn NOT_ENOUGH (`-7`) from the enclave's threshold, so it is refused
/// here with the reason instead of a late opaque code.
const MIN_TEST_PUBLISHERS: usize = 4;

/// Sign one mechanism-proof quote bundle with EVERY key in the `PRICE_PUBLISHER_KEYS`
/// JSON file and assemble the enclave's `XPQ1` blob.
///
/// MECHANISM-PROOF ONLY: in production the N quotes come from INDEPENDENT publisher
/// services; here one node signs with all N test keys, which proves the enclave
/// verify+median+threshold mechanism but NOT independence (that is a 1-of-1 costume).
///
/// Loads `>= MIN_TEST_PUBLISHERS` DISPOSABLE secp256k1 keypairs from `keys_path`
/// (shape `{"keys":[{"secret_hex":...,"pubkey_compressed_hex":...}, ...]}`), signs
/// ONE quote per key over the SAME `(network_id, market_id, price_fp8,
/// publish_time)`, and returns the assembled blob ready for `update_price_signed`.
/// These are NEVER custody keys.
pub fn test_sign_local_quotes(
    keys_path: &str,
    network_id: u32,
    market_id: u32,
    price_fp8: i64,
    publish_time: u64,
) -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string(keys_path)
        .with_context(|| format!("reading PRICE_PUBLISHER_KEYS file {keys_path}"))?;
    let file: PublisherKeyFile = serde_json::from_str(&raw)
        .with_context(|| format!("parsing publisher keys JSON {keys_path}"))?;

    if file.keys.len() < MIN_TEST_PUBLISHERS {
        bail!(
            "PRICE_PUBLISHER_KEYS {keys_path} has {} keys; the mechanism proof needs at least \
             {MIN_TEST_PUBLISHERS}",
            file.keys.len()
        );
    }

    let mut quotes: Vec<QuoteEntry> = Vec::with_capacity(file.keys.len());
    for (idx, entry) in file.keys.iter().enumerate() {
        let secret_bytes = hex::decode(entry.secret_hex.trim())
            .with_context(|| format!("key {idx}: secret_hex is not hex"))?;
        let sk = SigningKey::from_slice(&secret_bytes)
            .map_err(|e| anyhow::anyhow!("key {idx}: not a valid secp256k1 secret key: {e}"))?;

        let (pubkey, der) = sign_quote(&sk, network_id, market_id, price_fp8, publish_time)
            .with_context(|| format!("key {idx}: signing the quote failed"))?;

        if let Some(expected_hex) = entry.pubkey_compressed_hex.as_deref() {
            let got_hex = hex::encode(pubkey);
            if !expected_hex.trim().eq_ignore_ascii_case(&got_hex) {
                bail!(
                    "key {idx}: pubkey_compressed_hex {expected_hex} does not match the key's \
                     derived pubkey {got_hex}"
                );
            }
        }

        quotes.push((pubkey, price_fp8, publish_time, der));
    }

    assemble_xpq1(&quotes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use k256::ecdsa::VerifyingKey;

    const NETWORK_ID: u32 = 1; // XRPL testnet id, for the mechanism proof
    const MARKET_ID: u32 = 7;
    const PUBLISH_TIME: u64 = 780_000_000; // arbitrary ripple-epoch seconds

    /// SMOKE round-trip: this is NOT the authoritative verifier (the enclave's
    /// price_quotes.cpp is). It only proves this producer agrees with itself.
    fn verify_one(pubkey: &[u8; 33], price_fp8: i64, publish_time: u64, der_sig: &[u8]) -> bool {
        let vk = match VerifyingKey::from_sec1_bytes(pubkey) {
            Ok(vk) => vk,
            Err(_) => return false,
        };
        let sig = match Signature::from_der(der_sig) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let digest = price_quote_digest(NETWORK_ID, MARKET_ID, price_fp8, publish_time);
        vk.verify_prehash(&digest, &sig).is_ok()
    }

    #[test]
    fn preimage_is_the_28_byte_prc_layout() {
        let p = price_quote_preimage(NETWORK_ID, MARKET_ID, -120_000_000, PUBLISH_TIME);
        assert_eq!(p.len(), 28);
        assert_eq!(&p[0..4], &[0x50, 0x52, 0x43, 0x00], "PRC\\0 prefix");
        assert_eq!(&p[4..8], &NETWORK_ID.to_be_bytes());
        assert_eq!(&p[8..12], &MARKET_ID.to_be_bytes());
        // negative price_fp8 is two's-complement big-endian
        assert_eq!(&p[12..20], &(-120_000_000i64).to_be_bytes());
        assert_eq!(&p[20..28], &PUBLISH_TIME.to_be_bytes());
    }

    #[test]
    fn low_s_is_default() {
        // Pins the invariant that k256's sign_prehash emits a LOW-S signature.
        // normalize_s() returns Some only when the sig WAS high-S; None == already low-S.
        let sk = &gen_distinct_keypairs(1)[0];
        let (_pk, der) = sign_quote(sk, NETWORK_ID, MARKET_ID, 120_000_000, PUBLISH_TIME).unwrap();
        let sig = Signature::from_der(&der).unwrap();
        assert!(
            sig.normalize_s().is_none(),
            "k256 produced a high-S signature; the enclave/XRPL reject non-canonical S"
        );
    }

    #[test]
    fn xpq1_roundtrip_four_distinct_signers_all_verify_then_one_corrupt_fails() {
        // 4 DISTINCT keypairs, same network/market/time, prices 1.20..1.23 fp8.
        let keys = gen_distinct_keypairs(4);
        let distinct: std::collections::BTreeSet<_> =
            keys.iter().map(compressed_pubkey_hex).collect();
        assert_eq!(distinct.len(), 4, "the 4 signers must be distinct");

        let prices_fp8: [i64; 4] = [120_000_000, 121_000_000, 122_000_000, 123_000_000];

        let mut quotes: Vec<QuoteEntry> = Vec::new();
        for (sk, &price) in keys.iter().zip(prices_fp8.iter()) {
            let (pk, der) = sign_quote(sk, NETWORK_ID, MARKET_ID, price, PUBLISH_TIME).unwrap();
            assert!(der.len() <= MAX_SIG_LEN);
            quotes.push((pk, price, PUBLISH_TIME, der));
        }

        let blob = assemble_xpq1(&quotes).unwrap();

        // --- parse the blob back (mirror of the enclave's reader) ---
        assert_eq!(&blob[0..4], b"XPQ1", "magic");
        assert_eq!(blob[4], 0x01, "version");
        let count = blob[5] as usize;
        assert_eq!(count, 4, "count");

        let mut off = 6usize;
        let mut parsed: Vec<([u8; 33], i64, u64, Vec<u8>)> = Vec::new();
        for _ in 0..count {
            let mut pk = [0u8; 33];
            pk.copy_from_slice(&blob[off..off + 33]);
            off += 33;
            let price = i64::from_be_bytes(blob[off..off + 8].try_into().unwrap());
            off += 8;
            let ptime = u64::from_be_bytes(blob[off..off + 8].try_into().unwrap());
            off += 8;
            let sig_len = blob[off] as usize;
            off += 1;
            let sig = blob[off..off + sig_len].to_vec();
            off += sig_len;
            parsed.push((pk, price, ptime, sig));
        }
        assert_eq!(off, blob.len(), "no trailing bytes");

        // round-trip fidelity of the carried fields
        for (i, (pk, price, ptime, _sig)) in parsed.iter().enumerate() {
            assert_eq!(pk, &quotes[i].0, "pubkey {i}");
            assert_eq!(*price, prices_fp8[i], "price {i}");
            assert_eq!(*ptime, PUBLISH_TIME, "publish_time {i}");
        }

        // ALL 4 must verify against their carried pubkey.
        for (i, (pk, price, ptime, sig)) in parsed.iter().enumerate() {
            assert!(
                verify_one(pk, *price, *ptime, sig),
                "quote {i} should verify"
            );
        }

        // --- flip ONE byte of ONE signature; that quote fails, others still pass ---
        let victim = 2usize;
        let mut corrupt = parsed.clone();
        // Flip the last signature byte: keeps DER structurally valid (same length),
        // so it parses but fails verification — the verify-level failure path.
        let last = corrupt[victim].3.len() - 1;
        corrupt[victim].3[last] ^= 0x01;

        for (i, (pk, price, ptime, sig)) in corrupt.iter().enumerate() {
            let ok = verify_one(pk, *price, *ptime, sig);
            if i == victim {
                assert!(!ok, "corrupted quote {i} must fail verification");
            } else {
                assert!(ok, "untouched quote {i} must still verify");
            }
        }
    }

    #[test]
    fn assemble_rejects_overflowing_counts() {
        // count > 16 is refused rather than truncated by `as u8`.
        let (pk, der) = sign_quote(
            &gen_distinct_keypairs(1)[0],
            NETWORK_ID,
            MARKET_ID,
            1,
            PUBLISH_TIME,
        )
        .unwrap();
        let too_many: Vec<QuoteEntry> = (0..17)
            .map(|_| (pk, 1i64, PUBLISH_TIME, der.clone()))
            .collect();
        assert!(
            assemble_xpq1(&too_many).is_err(),
            "17 quotes must be rejected"
        );

        // sig_len > 72 is refused.
        let one_bad: Vec<QuoteEntry> = vec![(pk, 1i64, PUBLISH_TIME, vec![0u8; 73])];
        assert!(
            assemble_xpq1(&one_bad).is_err(),
            "73-byte sig must be rejected"
        );
    }

    #[test]
    fn test_sign_local_quotes_loads_file_and_signs_all_keys() {
        // Uses the REAL file shape (secret_hex + pubkey_compressed_hex), so this
        // exercises the exact loader the per-node driver calls — not a stripped
        // fixture that could pass tautologically.
        let keys = gen_distinct_keypairs(4);
        let entries: Vec<serde_json::Value> = keys
            .iter()
            .map(|sk| {
                serde_json::json!({
                    "secret_hex": hex::encode(sk.to_bytes()),
                    "pubkey_compressed_hex": compressed_pubkey_hex(sk),
                })
            })
            .collect();
        let doc = serde_json::json!({ "keys": entries });

        let dir = std::env::temp_dir();
        let path = dir.join(format!("xpq1_test_keys_{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();

        let price_fp8 = 123_000_000i64;
        let publish_time = 780_000_000u64;
        let blob = test_sign_local_quotes(
            path.to_str().unwrap(),
            NETWORK_ID,
            MARKET_ID,
            price_fp8,
            publish_time,
        )
        .expect("sign + assemble the bundle");

        // Parse it back: four quotes, each signed by its carried key over the SAME
        // price/time — the exact bundle the enclave's median consumer reads.
        assert_eq!(&blob[0..4], b"XPQ1", "magic");
        assert_eq!(blob[5] as usize, 4, "four quotes");
        let mut off = 6usize;
        for i in 0..4 {
            let mut pk = [0u8; 33];
            pk.copy_from_slice(&blob[off..off + 33]);
            off += 33;
            let price = i64::from_be_bytes(blob[off..off + 8].try_into().unwrap());
            off += 8;
            let ptime = u64::from_be_bytes(blob[off..off + 8].try_into().unwrap());
            off += 8;
            let sig_len = blob[off] as usize;
            off += 1;
            let sig = blob[off..off + sig_len].to_vec();
            off += sig_len;
            assert_eq!(price, price_fp8, "quote {i} price");
            assert_eq!(ptime, publish_time, "quote {i} publish_time");
            assert!(verify_one(&pk, price, ptime, &sig), "quote {i} must verify");
        }
        assert_eq!(off, blob.len(), "no trailing bytes");
        let _ = std::fs::remove_file(&path);

        // Fewer than the mechanism-proof floor is refused with a reason, not a late
        // opaque enclave code.
        let short = serde_json::json!({
            "keys": [ { "secret_hex": hex::encode(keys[0].to_bytes()) } ]
        });
        let short_path = dir.join(format!("xpq1_short_{}.json", std::process::id()));
        std::fs::write(&short_path, serde_json::to_string(&short).unwrap()).unwrap();
        assert!(
            test_sign_local_quotes(short_path.to_str().unwrap(), NETWORK_ID, MARKET_ID, 1, 1)
                .is_err(),
            "a 1-key file is below the 4-signer mechanism-proof floor"
        );
        let _ = std::fs::remove_file(&short_path);

        // A declared pubkey that does not match the secret is caught HERE, before
        // the enclave would reject the signature with -93.
        let bad_entries: Vec<serde_json::Value> = keys
            .iter()
            .enumerate()
            .map(|(i, sk)| {
                // Key 0 lies about its pubkey (claims key 1's compressed point).
                let pub_hex = if i == 0 {
                    compressed_pubkey_hex(&keys[1])
                } else {
                    compressed_pubkey_hex(sk)
                };
                serde_json::json!({
                    "secret_hex": hex::encode(sk.to_bytes()),
                    "pubkey_compressed_hex": pub_hex,
                })
            })
            .collect();
        let bad = serde_json::json!({ "keys": bad_entries });
        let bad_path = dir.join(format!("xpq1_bad_{}.json", std::process::id()));
        std::fs::write(&bad_path, serde_json::to_string(&bad).unwrap()).unwrap();
        assert!(
            test_sign_local_quotes(bad_path.to_str().unwrap(), NETWORK_ID, MARKET_ID, 1, 1)
                .is_err(),
            "a mismatched declared pubkey must be rejected"
        );
        let _ = std::fs::remove_file(&bad_path);
    }

    /// Standalone helper: generate 4 distinct DISPOSABLE testnet keypairs, write
    /// their SECRET keys (hex) + compressed pubkeys to a JSON file, and print the
    /// 4 compressed pubkeys sorted ascending (lexicographic by bytes). These are
    /// mechanism-proof keys, NOT custody. `#[ignore]` so it never runs in CI or
    /// writes files during a normal `cargo test`; run it explicitly with
    /// `-- --ignored --nocapture`. Output path: `$PRICE_PUBLISHER_KEYS_OUT` if set,
    /// else `<temp_dir>/test_publisher_keys.json`.
    #[test]
    #[ignore]
    fn emit_test_publisher_keys() {
        let keys = gen_distinct_keypairs(4);

        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut pubkeys: Vec<String> = Vec::new();
        for sk in &keys {
            let secret_hex = hex::encode(sk.to_bytes());
            let pub_hex = compressed_pubkey_hex(sk);
            pubkeys.push(pub_hex.clone());
            entries.push(serde_json::json!({
                "secret_hex": secret_hex,
                "pubkey_compressed_hex": pub_hex,
            }));
        }

        // Sort ascending, lexicographic by raw bytes (== lexicographic on the hex).
        let mut sorted = pubkeys.clone();
        sorted.sort();

        let doc = serde_json::json!({
            "note": "DISPOSABLE TESTNET mechanism-proof keys — NOT custody. Price-quote signers.",
            "scheme": "secp256k1 ECDSA over SHA-512Half(PRC\\0 preimage), DER low-S; XPQ1 blob",
            "keys": entries,
            "pubkeys_compressed_sorted_ascending": sorted,
        });

        let out_path = std::env::var("PRICE_PUBLISHER_KEYS_OUT").unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("test_publisher_keys.json")
                .to_string_lossy()
                .into_owned()
        });
        if let Some(parent) = std::path::Path::new(&out_path).parent() {
            std::fs::create_dir_all(parent).expect("create output dir");
        }
        std::fs::write(
            &out_path,
            serde_json::to_string_pretty(&doc).expect("serialize keys json"),
        )
        .expect("write keys json");

        println!("wrote secret keys to: {out_path}");
        println!("4 compressed pubkeys, sorted ascending:");
        for pk in &sorted {
            println!("{pk}");
        }
    }
}
