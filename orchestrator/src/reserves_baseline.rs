//! AC-BASE-2″: orchestrator side of the one-time SPV custody-baseline ceremony.
//!
//! Recomputes the enclave's issuer-PINNED SPV baseline message hash (so the quorum
//! bundle it assembles verifies against `seal_verify_quorum_bundle_with_set`), recovers
//! each node's compressed secp256k1 pubkey from its recoverable signature (no extra
//! ecall needed — the 65-byte [r||s||v] is recoverable), and encodes the wire bundle the
//! enclave consumes. The message hash MUST match
//! `compute_perp_reserves_spv_baseline_message_hash` byte-for-byte — a golden vector
//! cross-checks it against the C++ (test `spv_baseline_hash_matches_enclave_golden`).
//!
//! This module carries the orchestrator mechanism: the C-Q1.1 source-diversity
//! primitives, the libp2p collector, and the ceremony driver
//! (`run_reserves_spv_baseline_ceremony`: fetch proof → broadcast → independent
//! SPV-cosign → diversity-assert → apply). The p2p relay lives in `p2p.rs`
//! (`SpvBaselineRelay`, `handle_spv_baseline_request`).
//!
//! #131 R-1: the SCALAR observation ceremony (a host-queried escrow figure confirmed by
//! a 2-of-3 operator quorum) was RETIRED with the enclave ecalls it drove. A quorum
//! agreeing on a number is an observation, never backing; custody is now written only
//! from a balance the enclave itself verifies against a >=80% pinned-UNL quorum.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// #131 AC-BASE-2" P2-c: the SPV baseline domain. The retired scalar baseline used
/// "PERP_RESERVES_BASELINE_v1" (#131 R-1); the domains are disjoint, so no historical
/// scalar bundle can authorise an SPV baseline.
const SPV_BASELINE_DOMAIN: &[u8] = b"PERP_RESERVES_SPV_BASELINE_v1"; // 29 bytes

/// #131 AC-BASE-2" P2-c: SHA-256 over the exact preimage the enclave's
/// `compute_perp_reserves_spv_baseline_message_hash` hashes:
///   domain || le_u32(shard) || le_u64(ledger_seq) || ledger_hash[32]
///   || le_u64(custody_rlusd) || le_u64(custody_xrp) || rlusd_issuer[20]
///   || excluded_senders_hash[32]
/// Each cosigner signs THIS over the custody IT independently SPV-derived.
pub fn spv_baseline_message_hash(
    shard_id: u32,
    ledger_seq: u64,
    ledger_hash: &[u8; 32],
    custody_rlusd: i64,
    custody_xrp: i64,
    rlusd_issuer: &[u8; 20],
    excluded_senders_hash: &[u8; 32],
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(SPV_BASELINE_DOMAIN);
    h.update(shard_id.to_le_bytes());
    h.update(ledger_seq.to_le_bytes());
    h.update(ledger_hash);
    h.update((custody_rlusd as u64).to_le_bytes());
    h.update((custody_xrp as u64).to_le_bytes());
    h.update(rlusd_issuer);
    h.update(excluded_senders_hash);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// #131 AC-BASE-2″ P2-c: reproduce the SPV baseline message hash from the fields a
/// cosigner's enclave ECHOED back. The orchestrator is untrusted and derives NOTHING
/// from the proof itself — it only reproduces the hash to recover the returned
/// signature's pubkey (a wrong echoed field just fails recovery, and the applier
/// independently re-derives the whole figure from the proof at apply, so these echoed
/// values are never a trust surface).
pub fn spv_hash_from_cosign_reply(reply: &serde_json::Value) -> Result<[u8; 32]> {
    let shard_id = reply["shard_id"].as_u64().context("shard_id")? as u32;
    let ledger_seq = reply["ledger_seq"].as_u64().context("ledger_seq")?;
    let custody_xrp = reply["custody_xrp"].as_i64().context("custody_xrp")?;
    let custody_rlusd = reply["custody_rlusd"].as_i64().context("custody_rlusd")?;
    let ledger_hash =
        decode_fixed_hex::<32>(reply["ledger_hash"].as_str().context("ledger_hash")?)?;
    let issuer = decode_fixed_hex::<20>(reply["issuer"].as_str().context("issuer")?)?;
    let excluded_hash =
        decode_fixed_hex::<32>(reply["excluded_hash"].as_str().context("excluded_hash")?)?;
    Ok(spv_baseline_message_hash(
        shard_id,
        ledger_seq,
        &ledger_hash,
        custody_rlusd,
        custody_xrp,
        &issuer,
        &excluded_hash,
    ))
}

/// Decode a fixed-length hex string (optional `0x`) into `[u8; N]`, erroring on any
/// length mismatch (never silently truncate/pad a proof-derived field).
fn decode_fixed_hex<const N: usize>(s: &str) -> Result<[u8; N]> {
    let v = hex::decode(s.trim_start_matches("0x")).context("hex decode")?;
    if v.len() != N {
        bail!("expected {N} bytes, got {}", v.len());
    }
    let mut a = [0u8; N];
    a.copy_from_slice(&v);
    Ok(a)
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

/// backing is AC-BASE-2″ (in-enclave XRPL-SPV), mainnet-forward.
pub fn endpoint_fingerprint(url: &str) -> String {
    let lower = url.trim().to_ascii_lowercase();
    let no_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("wss://"))
        .or_else(|| lower.strip_prefix("ws://"))
        .unwrap_or(lower.as_str());
    let host = no_scheme
        .split('/')
        .next()
        .unwrap_or(no_scheme)
        .trim_end_matches('/');
    let mut h = Sha256::new();
    h.update(host.as_bytes());
    hex::encode(&h.finalize()[..8]) // 16 hex chars — enough to distinguish endpoints
}

/// RESP-#103 C-Q1.1 pre-flight: refuse to run the ceremony unless every configured
/// node points at a DISTINCT XRPL endpoint. Without this, all nodes could re-query
/// one source and the 2-of-3 collapses to a single-source observation-quorum (the
/// THORChain failure mode). Returns the distinct fingerprints in input order.
pub fn enforce_distinct_endpoints(endpoints: &[String]) -> Result<Vec<String>> {
    let fps: Vec<String> = endpoints.iter().map(|e| endpoint_fingerprint(e)).collect();
    let distinct: std::collections::BTreeSet<&String> = fps.iter().collect();
    if distinct.len() != endpoints.len() {
        bail!(
            "C-Q1.1: {} configured baseline endpoints resolve to only {} distinct XRPL source(s) \
             — refuse (each node MUST query a distinct source, else it is N-signers-one-source)",
            endpoints.len(),
            distinct.len()
        );
    }
    Ok(fps)
}

/// RESP-#103 C-Q1.1 diversity bookkeeping (the Q5 assertion): the accepted quorum
/// must span ≥`quorum` DISTINCT source fingerprints. Returns the distinct count on
/// success; errs if fewer than quorum distinct sources contributed — i.e. the 2-of-3
/// came from too few independent XRPL observations to be a genuine multi-observation.
pub fn assert_distinct_sources(fingerprints: &[String], quorum: usize) -> Result<usize> {
    let distinct: std::collections::BTreeSet<&String> = fingerprints.iter().collect();
    if distinct.len() < quorum {
        bail!(
            "diversity: {} accepted signature(s) span only {} distinct XRPL source(s), need ≥{} \
             — refuse (N-signers-one-source)",
            fingerprints.len(),
            distinct.len(),
            quorum
        );
    }
    Ok(distinct.len())
}

/// One node in the baseline ceremony roster: its enclave's baseline signing pubkey
/// (the compressed secp256k1 pubkey that ends up in the bundle) and its OWN XRPL
/// endpoint. The driver maps each accepted bundle entry back to its endpoint for the
/// C-Q1.1 diversity assertion.
#[derive(Debug, Clone)]
pub struct BaselineNode {
    /// 33-byte compressed secp256k1 pubkey, lowercase hex.
    pub compressed_pubkey_hex: String,
    /// This node's XRPL endpoint (the source its receiver independently re-queried).
    pub xrpl_endpoint: String,
}

/// RESP-#103 C-Q1.1 diversity bookkeeping: map each accepted bundle pubkey to its
/// roster endpoint and assert the accepted quorum spans ≥`quorum` DISTINCT sources. A
/// pubkey not in the roster is rejected (an unknown signer must not count toward
/// diversity). Returns the DISTINCT source fingerprints (first-seen order) — #131 AC-BASE
/// (b) records these in the sealed baseline marker so the "N independent observations"
/// claim is auditable.
pub fn assert_bundle_diversity(
    accepted_pubkeys: &[Vec<u8>],
    roster: &[BaselineNode],
    quorum: usize,
) -> Result<Vec<String>> {
    let mut fps = Vec::with_capacity(accepted_pubkeys.len());
    for pk in accepted_pubkeys {
        let pk_hex = hex::encode(pk);
        match roster
            .iter()
            .find(|n| n.compressed_pubkey_hex.eq_ignore_ascii_case(&pk_hex))
        {
            Some(n) => fps.push(endpoint_fingerprint(&n.xrpl_endpoint)),
            None => bail!(
                "accepted baseline signature from pubkey {pk_hex} not in the ceremony roster \
                 — refuse (cannot attribute it to a distinct XRPL source)"
            ),
        }
    }
    assert_distinct_sources(&fps, quorum)?; // enforce ≥quorum DISTINCT sources
    let mut seen = std::collections::HashSet::new();
    Ok(fps.into_iter().filter(|f| seen.insert(f.clone())).collect())
}

// ── #131 AC-BASE-2″ P2-c SPV ceremony ──────────────────────────────────────────
// The SPV variant broadcasts ONE validator-quorum-attested proof (not a host figure);
// each cosigner's enclave SPV-verifies it and cosigns the figure IT derives. The p2p
// relay lives in `p2p.rs` (`SpvBaselineRelay`, `handle_spv_baseline_request`).

/// Collector for the SPV baseline ceremony — mirrors `LibP2PReservesBaselineCollector`
/// but ships a proof blob + the agreed excluded set instead of a figure.
pub struct LibP2PSpvBaselineCollector {
    relay_tx: tokio::sync::mpsc::Sender<crate::p2p::SpvBaselineRelay>,
    timeout: std::time::Duration,
}

impl LibP2PSpvBaselineCollector {
    pub fn new(relay_tx: tokio::sync::mpsc::Sender<crate::p2p::SpvBaselineRelay>) -> Self {
        Self {
            relay_tx,
            // An operator enclave re-runs the WHOLE SPV verify to cosign: parse, ledger
            // hash, >=quorum secp256k1 validation checks and a SHAMap walk, inside SGX,
            // and the loop always sits out the full window. 30s was never derived from a
            // measurement. A baseline ceremony is operator-driven and rare: waiting costs
            // nothing, a too-tight window costs the ceremony.
            timeout: std::time::Duration::from_secs(150),
        }
    }

    #[allow(dead_code)]
    pub fn with_timeout(mut self, t: std::time::Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Broadcast ONE proof blob + the agreed excluded set; collect a 2-of-N bundle of SPV
    /// cosignatures. Returns `(wire_bundle, accepted_compressed_pubkeys)` — the pubkeys are
    /// distinct and the bundle is exactly what `seal_verify_quorum_bundle_with_set` consumes.
    pub async fn collect(
        &self,
        proof_blob_hex: String,
        excluded_hex: Vec<String>,
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
        use uuid::Uuid;
        let request_id = format!("reserves-spv-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = tokio::sync::mpsc::channel(32);

        self.relay_tx
            .send(crate::p2p::SpvBaselineRelay {
                request_id,
                proof_blob_hex,
                excluded_hex,
                responses_tx,
            })
            .await
            .context("send SpvBaselineRelay to p2p run-loop")?;

        // (compressed_pubkey, DER) per distinct responder.
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match tokio::time::timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                _ => break,
            };
            if let crate::p2p::SigningMessage::Response {
                der_signature: Some(der_hex),
                compressed_pubkey: Some(pk_hex),
                error: None,
                ..
            } = resp
            {
                let pk = hex::decode(&pk_hex).unwrap_or_default();
                let der = hex::decode(&der_hex).unwrap_or_default();
                if pk.len() == 33 && !der.is_empty() && !entries.iter().any(|(p, _)| *p == pk) {
                    entries.push((pk, der));
                }
            }
        }

        if entries.is_empty() {
            bail!(
                "collected zero SPV cosignatures within {:?} — no operator enclave verified \
                 the proof + cosigned",
                self.timeout
            );
        }
        let pubkeys: Vec<Vec<u8>> = entries.iter().map(|(p, _)| p.clone()).collect();
        Ok((build_quorum_bundle(&entries), pubkeys))
    }
}

/// #131 AC-BASE-2″ P2-c ceremony driver. The leader fetches ONE validator-quorum-attested
/// proof (the untrusted patched node produces it, the enclave verifies it), broadcasts it,
/// collects ≥`cosign_quorum` SPV cosignatures over the SPV-DERIVED figure, asserts cosigner
/// diversity, then applies on the LOCAL sequencer enclave (re-derive → verify 2-of-N →
/// seed custody := the SPV-PROVEN balance → seal the one-shot). Returns the apply response.
#[allow(clippy::too_many_arguments)]
pub async fn run_reserves_spv_baseline_ceremony(
    collector: &LibP2PSpvBaselineCollector,
    enclave_perp_v1_base: &str,
    fetch_cfg: &crate::spv_proof::SpvFetchConfig,
    roster: &[BaselineNode],
    cosign_quorum: usize,
    host_timestamp_ms: u64,
    excluded_account_ids: &[String],
) -> Result<serde_json::Value> {
    // (0) C-Q1.1 pre-flight: every roster entry must be a DISTINCT node. In the SPV
    // ceremony the endpoint is an identity label (all cosigners verify the SAME proof,
    // so it is no longer an independent XRPL query), and `assert_bundle_diversity` maps
    // accepted pubkeys back through it — two entries sharing one endpoint would collapse
    // the distinct count. Fail here with the real reason instead of at the diversity
    // assert with a confusing shortfall.
    let endpoints: Vec<String> = roster.iter().map(|n| n.xrpl_endpoint.clone()).collect();
    enforce_distinct_endpoints(&endpoints)?;

    // (1) Fetch ONE proof blob and host-sanity-verify its chaining before broadcasting.
    let blob = crate::spv_proof::fetch_spv_bundle(fetch_cfg)
        .await
        .context("fetch SPV proof bundle from the patched node")?;
    let proof_blob_hex = hex::encode(&blob);
    tracing::info!(
        blob_len = blob.len(),
        "#131 SPV baseline: fetched a validator-quorum-attested proof — broadcasting for cosign"
    );

    // (2) Broadcast + collect ≥quorum independent SPV cosignatures over the SAME proof.
    let (bundle, pubkeys) = collector
        .collect(proof_blob_hex.clone(), excluded_account_ids.to_vec())
        .await?;
    if pubkeys.len() < cosign_quorum {
        bail!(
            "collected {} SPV cosignature(s), need ≥{} — refuse",
            pubkeys.len(),
            cosign_quorum
        );
    }

    // (3) Diversity: the accepted quorum must span ≥quorum distinct cosigning operators.
    let source_fingerprints = assert_bundle_diversity(&pubkeys, roster, cosign_quorum)?;
    let distinct = source_fingerprints.len();

    // (4) Apply on the LOCAL sequencer enclave: it INDEPENDENTLY re-derives the figure from
    // the same proof, verifies the 2-of-N over it, seeds custody + seals the one-shot.
    let perp = crate::perp_client::PerpClient::new(enclave_perp_v1_base)?;
    let res = perp
        .reserves_spv_apply(
            &proof_blob_hex,
            host_timestamp_ms,
            &hex::encode(&bundle),
            &source_fingerprints,
            excluded_account_ids,
        )
        .await?;

    tracing::info!(
        distinct_cosigners = distinct,
        "#131 SPV baseline applied — custody seeded from an SPV-PROVEN escrow balance, \
         cosigned by {} independent operator enclaves",
        distinct
    );
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2-c SPV baseline hash — must match the C++ golden from
    /// tests/test_perp_reserves_baseline.cpp (spv_baseline_message_hash). If the two
    /// encodings drift, the live 2-of-3 quorum over the SPV-derived custody silently fails.
    #[test]
    fn spv_baseline_hash_matches_enclave_golden() {
        let mut issuer = [0u8; 20];
        let mut lh = [0u8; 32];
        let mut exhash = [0u8; 32];
        for (i, b) in issuer.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
        for i in 0..32 {
            lh[i] = 0x40 + i as u8;
            exhash[i] = 0xC0 + i as u8;
        }
        let h = spv_baseline_message_hash(
            0,
            84_000_000,
            &lh,
            123_456_789_012,
            55_550_000,
            &issuer,
            &exhash,
        );
        let expected = "d73be3361b83201d8fc2fd7d1338f7fb2cd00796e2a2d1a1530d3b1cfce9d129";
        assert_eq!(
            hex::encode(h),
            expected,
            "Rust SPV baseline hash must match the C++ enclave golden"
        );
    }

    /// The reply→hash reconstruction (host recovery path) must land on the SAME golden
    /// as `spv_baseline_message_hash` — locks the echoed JSON field names + the hex decode
    /// so a cosigner's pubkey recovery can never silently mis-parse the enclave's figures.
    #[test]
    fn spv_hash_from_cosign_reply_matches_golden() {
        let reply = serde_json::json!({
            "status": "success",
            "shard_id": 0,
            "ledger_seq": 84_000_000u64,
            "ledger_hash": "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
            "custody_xrp": 55_550_000i64,
            "custody_rlusd": 123_456_789_012i64,
            "issuer": "a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3",
            "excluded_hash": "c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf",
        });
        let h = spv_hash_from_cosign_reply(&reply).expect("reply → hash");
        assert_eq!(
            hex::encode(h),
            "d73be3361b83201d8fc2fd7d1338f7fb2cd00796e2a2d1a1530d3b1cfce9d129",
            "reply-reconstructed hash must match the enclave golden"
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

    #[test]
    fn endpoint_fingerprint_normalizes_and_distinguishes() {
        // scheme/case/trailing-slash spellings of the SAME host fingerprint alike
        let a = endpoint_fingerprint("https://Rippled-A.example:51234/");
        let b = endpoint_fingerprint("http://rippled-a.example:51234");
        assert_eq!(a, b, "spellings of the same host must fingerprint alike");
        // different hosts fingerprint distinctly
        assert_ne!(a, endpoint_fingerprint("https://rippled-b.example:51234"));
    }

    #[test]
    fn enforce_distinct_endpoints_rejects_shared_source() {
        // three genuinely distinct operator endpoints → ok
        assert!(enforce_distinct_endpoints(&[
            "https://a.rpc".into(),
            "https://b.rpc".into(),
            "https://c.rpc".into(),
        ])
        .is_ok());
        // two nodes secretly on ONE source (the THORChain failure mode) → refuse
        assert!(enforce_distinct_endpoints(&[
            "https://shared.rpc".into(),
            "https://shared.rpc/".into(), // same host, different spelling
            "https://c.rpc".into(),
        ])
        .is_err());
    }

    #[test]
    fn assert_distinct_sources_needs_quorum_distinct() {
        let two = vec!["fpA".to_string(), "fpB".to_string()];
        assert_eq!(assert_distinct_sources(&two, 2).unwrap(), 2);
        // two signatures but ONE source → not a genuine 2-observation
        let one_src = vec!["fpA".to_string(), "fpA".to_string()];
        assert!(assert_distinct_sources(&one_src, 2).is_err());
    }

    #[test]
    fn bundle_diversity_maps_pubkeys_and_needs_distinct_endpoints() {
        let roster = vec![
            BaselineNode {
                compressed_pubkey_hex: "02aa".into(),
                xrpl_endpoint: "https://a.rpc".into(),
            },
            BaselineNode {
                compressed_pubkey_hex: "02bb".into(),
                xrpl_endpoint: "https://b.rpc".into(),
            },
            BaselineNode {
                compressed_pubkey_hex: "02cc".into(),
                xrpl_endpoint: "https://a.rpc".into(),
            },
        ];
        // accepted 2-of-3 from nodes A + B (distinct endpoints) → passes, 2 sources
        let ab = vec![hex::decode("02aa").unwrap(), hex::decode("02bb").unwrap()];
        assert_eq!(assert_bundle_diversity(&ab, &roster, 2).unwrap().len(), 2);
        // accepted 2 from nodes A + C which SHARE one endpoint → refuse (Q5 diversity)
        let ac = vec![hex::decode("02aa").unwrap(), hex::decode("02cc").unwrap()];
        assert!(assert_bundle_diversity(&ac, &roster, 2).is_err());
        // a signature from a pubkey not in the roster → refuse (unknown source)
        let unknown = vec![hex::decode("02aa").unwrap(), hex::decode("02ff").unwrap()];
        assert!(assert_bundle_diversity(&unknown, &roster, 2).is_err());
    }
}
