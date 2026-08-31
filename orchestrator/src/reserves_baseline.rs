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
//! This module carries the full orchestrator mechanism: the escrow query (validated
//! for the initiator; historical `ledger_index=L` exact-match for the receiver —
//! RESP-#103 C-Q1.2), the C-Q1.1 source-diversity primitives, the libp2p collector,
//! and the ceremony driver (`run_reserves_baseline_ceremony`: query → broadcast →
//! independent-confirm → diversity-assert → apply). The p2p relay itself lives in
//! `p2p.rs` (`ReservesBaselineRelay`, `handle_reserves_baseline_request`).
//!
//! The live admin-trigger (`POST /admin/reserves-baseline`) + per-node config
//! (`--rlusd-issuer`, `--shard-id`) are wired in `main.rs` / `membership_admin.rs`; the
//! ceremony runs post-β8→β9-migration on the real cluster.

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

/// Parse an XRPL decimal amount string ("100.50") into the perp FP8 unit (1.0 =
/// 1e8), string-based to avoid f64 precision loss — the SAME convention the deposit
/// scanner credits, so the baseline figure and the custody ledger share units.
pub fn decimal_to_fp8(s: &str) -> Result<i64> {
    let s = s.trim();
    let neg = s.starts_with('-');
    let body = s.trim_start_matches('-');
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        bail!("non-numeric decimal amount: {s}");
    }
    let mut frac = frac_part.to_string();
    if frac.len() > 8 {
        frac.truncate(8); // XRPL amounts can exceed 8 dp; the ledger unit is 8 dp
    } else {
        while frac.len() < 8 {
            frac.push('0');
        }
    }
    let int_v: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().context("int part")?
    };
    let frac_v: i64 = frac.parse().context("frac part")?;
    let v = int_v
        .checked_mul(100_000_000)
        .and_then(|x| x.checked_add(frac_v))
        .context("fp8 overflow")?;
    Ok(if neg { -v } else { v })
}

/// The escrow's attested reserve at a pinned validated ledger, in FP8.
pub struct EscrowBalances {
    pub ledger_index: u64,
    pub rlusd_fp8: i64,
    pub xrp_fp8: i64,
}

/// Which ledger an escrow query is pinned to.
enum LedgerSel {
    /// The initiator path: the current validated ledger (its index becomes the
    /// pinned `L` the whole ceremony agrees on).
    Validated,
    /// The receiver path (C-Q1.2): query AT a specific historical ledger `L` and
    /// require the server to answer FOR that exact ledger — no tolerance window.
    At(u64),
}

impl LedgerSel {
    fn json(&self) -> serde_json::Value {
        match self {
            LedgerSel::Validated => serde_json::json!("validated"),
            LedgerSel::At(l) => serde_json::json!(l),
        }
    }
}

/// Query the escrow's RLUSD (the escrow↔issuer trustline via `account_lines`) + XRP
/// (`account_info` Balance, drops → FP8 = drops·100) at the current VALIDATED ledger.
/// Baselines to the FULL escrow (auditor Q3: the escrow IS the total backing; the XRP
/// account reserve is over-custody = the safe direction). `escrow_r`/`rlusd_issuer_r`
/// are classic r-addresses. This is the INITIATOR path — the returned `ledger_index`
/// becomes the pinned `L` broadcast to the cluster.
pub async fn query_escrow_balances(
    xrpl_url: &str,
    escrow_r: &str,
    rlusd_issuer_r: &str,
) -> Result<EscrowBalances> {
    query_escrow_at(xrpl_url, escrow_r, rlusd_issuer_r, LedgerSel::Validated).await
}

/// RESP-#103 C-Q1.2 — the RECEIVER path: independently re-query the escrow AT the
/// pinned ledger `L` (XRPL historical `ledger_index=L`) and require an EXACT match —
/// the returned ledger MUST equal `L` (no tolerance). A server without ledger `L` in
/// history answers `lgrNotFound`; the caller treats any error here as "cannot confirm
/// at L" and REFUSES to sign (never falls back to `validated`, which would defeat the
/// pin). This is what makes ≥2 signatures prove ≥2 operators observed the SAME escrow
/// at the SAME ledger — not N nodes trusting one broadcast figure.
pub async fn query_escrow_balances_at_ledger(
    xrpl_url: &str,
    escrow_r: &str,
    rlusd_issuer_r: &str,
    ledger_index: u64,
) -> Result<EscrowBalances> {
    let b = query_escrow_at(
        xrpl_url,
        escrow_r,
        rlusd_issuer_r,
        LedgerSel::At(ledger_index),
    )
    .await?;
    if b.ledger_index != ledger_index {
        bail!(
            "escrow re-query resolved to ledger {} not the pinned L={} — refuse (no tolerance)",
            b.ledger_index,
            ledger_index
        );
    }
    Ok(b)
}

/// Shared XRPL query used by both the validated (initiator) and at-ledger (receiver)
/// paths. `sel` selects the `ledger_index` field sent to `account_info`/`account_lines`.
async fn query_escrow_at(
    xrpl_url: &str,
    escrow_r: &str,
    rlusd_issuer_r: &str,
    sel: LedgerSel,
) -> Result<EscrowBalances> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let ledger_field = sel.json();
    // account_info → XRP balance (drops) + the ledger index the server answered for.
    let info: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({"method":"account_info",
            "params":[{"account":escrow_r,"ledger_index":ledger_field}]}))
        .send()
        .await
        .context("account_info request")?
        .json()
        .await
        .context("account_info json")?;
    let ir = &info["result"];
    if let Some(e) = ir["error"].as_str() {
        bail!("account_info error: {e}");
    }
    let ledger_index = ir["ledger_index"]
        .as_u64()
        .context("missing ledger_index in account_info result")?;
    let drops: u64 = ir["account_data"]["Balance"]
        .as_str()
        .context("missing account_data.Balance")?
        .parse()
        .context("Balance drops parse")?;
    let xrp_fp8 = (drops as i64)
        .checked_mul(100)
        .context("xrp fp8 overflow")?; // 1 drop = 100 FP8

    // account_lines → the escrow's RLUSD balance on the issuer trustline, at the SAME
    // ledger selector so RLUSD and XRP are read from one consistent ledger state.
    let lines: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({"method":"account_lines",
            "params":[{"account":escrow_r,"ledger_index":sel.json()}]}))
        .send()
        .await
        .context("account_lines request")?
        .json()
        .await
        .context("account_lines json")?;
    let lr = &lines["result"];
    if let Some(e) = lr["error"].as_str() {
        bail!("account_lines error: {e}");
    }
    let mut rlusd_fp8 = 0i64;
    if let Some(arr) = lr["lines"].as_array() {
        for line in arr {
            if line["account"].as_str() == Some(rlusd_issuer_r) {
                let bal = line["balance"].as_str().unwrap_or("0");
                rlusd_fp8 = decimal_to_fp8(bal)?;
                break;
            }
        }
    }
    if rlusd_fp8 < 0 {
        bail!(
            "negative RLUSD escrow balance ({rlusd_fp8}) — escrow owes RLUSD? refuse to baseline"
        );
    }
    Ok(EscrowBalances {
        ledger_index,
        rlusd_fp8,
        xrp_fp8,
    })
}

/// RESP-#103 C-Q1.1 diversity token — a short, stable fingerprint of an XRPL
/// endpoint so the ceremony can assert ≥quorum DISTINCT sources WITHOUT putting
/// operator endpoint URLs on the wire or in the bundle (topology-neutral,
/// cf. firewall topology-neutrality). Normalizes scheme/case/path/trailing-slash so
/// two spellings of the same host fingerprint alike; different hosts fingerprint
/// distinctly. This is the practical testnet diversity signal — it cannot detect two
/// hosts that are actually one rippled behind a load balancer (the auditor's
/// "ideally operator-run, not a shared public cluster" caveat); real Byzantine
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

/// RESP-#103 C-Q1.3 receiver decision, extracted so the refuse-before-sign rule is
/// unit-testable independent of the p2p/enclave path: the receiver may sign ONLY if
/// its OWN re-queried figure matches the broadcast EXACTLY at the SAME pinned ledger.
/// Any divergence (ledger, RLUSD, or XRP) ⇒ refuse.
pub fn figure_matches(
    observed: &EscrowBalances,
    ledger_index: u64,
    escrow_rlusd: i64,
    escrow_xrp: i64,
) -> bool {
    observed.ledger_index == ledger_index
        && observed.rlusd_fp8 == escrow_rlusd
        && observed.xrp_fp8 == escrow_xrp
}

/// #131 AC-BASE libp2p collector — mirrors `mrenclave_governance::LibP2PGovernanceBundleCollector`.
/// Sends ONE `ReservesBaselineRelay` down the p2p run-loop channel (which broadcasts the
/// request AND signs locally via the independent-re-query receiver path), gathers
/// distinct `(pubkey, DER)` Responses until quorum or timeout, and returns the wire
/// bundle + the accepted compressed pubkeys (the driver maps those to endpoints for the
/// C-Q1.1 diversity assertion).
pub struct LibP2PReservesBaselineCollector {
    relay_tx: tokio::sync::mpsc::Sender<crate::p2p::ReservesBaselineRelay>,
    timeout: std::time::Duration,
}

impl LibP2PReservesBaselineCollector {
    pub fn new(relay_tx: tokio::sync::mpsc::Sender<crate::p2p::ReservesBaselineRelay>) -> Self {
        Self {
            relay_tx,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Override the collection window (default 30s). Kept for parity with the other
    /// libp2p collectors + integration-test control; the admin driver uses the default.
    #[allow(dead_code)]
    pub fn with_timeout(mut self, t: std::time::Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Collect a 2-of-N baseline bundle over the pinned figure. Returns
    /// `(wire_bundle, accepted_compressed_pubkeys)`; the pubkeys are distinct and the
    /// bundle is exactly what `seal_verify_quorum_bundle_with_set` consumes.
    pub async fn collect(
        &self,
        escrow: [u8; 20],
        rlusd_issuer: [u8; 20],
        ledger_index: u64,
        escrow_rlusd: i64,
        escrow_xrp: i64,
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
        use uuid::Uuid;
        let request_id = format!("reserves-baseline-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = tokio::sync::mpsc::channel(32);

        self.relay_tx
            .send(crate::p2p::ReservesBaselineRelay {
                request_id,
                escrow,
                rlusd_issuer,
                ledger_index,
                escrow_rlusd,
                escrow_xrp,
                responses_tx,
            })
            .await
            .context("send ReservesBaselineRelay to p2p run-loop")?;

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
                "collected zero baseline responses within {:?} — no operator independently \
                 confirmed the escrow figure at the pinned ledger",
                self.timeout
            );
        }
        let pubkeys: Vec<Vec<u8>> = entries.iter().map(|(p, _)| p.clone()).collect();
        Ok((build_quorum_bundle(&entries), pubkeys))
    }
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

/// #131 AC-BASE ceremony driver. Reads the escrow at the current validated ledger
/// (pinning `L`), broadcasts the figure to the operator quorum (each independently
/// re-queries at `L` and refuses on mismatch — C-Q1.2/1.3), collects ≥`quorum`
/// distinct confirmations, asserts source diversity (C-Q1.1), then applies the bundle
/// on the LOCAL sequencer enclave (verify 2-of-3 → seed custody := attested escrow →
/// seal the one-shot marker). Returns the enclave apply response.
#[allow(clippy::too_many_arguments)]
pub async fn run_reserves_baseline_ceremony(
    collector: &LibP2PReservesBaselineCollector,
    enclave_perp_v1_base: &str,
    initiator_xrpl_url: &str,
    escrow_r: &str,
    rlusd_issuer_r: &str,
    roster: &[BaselineNode],
    quorum: usize,
    host_timestamp_ms: u64,
    excluded_account_ids: &[String],
) -> Result<serde_json::Value> {
    // C-Q1.1 pre-flight: every configured node MUST point at a distinct XRPL source.
    let endpoints: Vec<String> = roster.iter().map(|n| n.xrpl_endpoint.clone()).collect();
    enforce_distinct_endpoints(&endpoints)?;

    // The initiator reads the escrow at the current validated ledger; its index pins L.
    let base = query_escrow_balances(initiator_xrpl_url, escrow_r, rlusd_issuer_r).await?;
    let escrow = crate::xrpl_signer::decode_xrpl_address(escrow_r)?;
    let issuer = crate::xrpl_signer::decode_xrpl_address(rlusd_issuer_r)?;

    tracing::info!(
        ledger = base.ledger_index,
        rlusd = base.rlusd_fp8,
        xrp = base.xrp_fp8,
        "#131 baseline: pinned figure at validated ledger — broadcasting for independent confirmation"
    );

    // Broadcast + collect ≥quorum INDEPENDENT confirmations at L.
    let (bundle, pubkeys) = collector
        .collect(
            escrow,
            issuer,
            base.ledger_index,
            base.rlusd_fp8,
            base.xrp_fp8,
        )
        .await?;
    if pubkeys.len() < quorum {
        bail!(
            "collected {} distinct baseline confirmation(s), need ≥{} — refuse",
            pubkeys.len(),
            quorum
        );
    }

    // C-Q1.1 diversity: the accepted quorum must span ≥quorum distinct XRPL sources.
    // #131 AC-BASE (b): the distinct source fingerprints are recorded in the sealed
    // baseline marker (auditable "N independent observations"; host-declared disclosure).
    let source_fingerprints = assert_bundle_diversity(&pubkeys, roster, quorum)?;
    let distinct = source_fingerprints.len();

    // Apply on the LOCAL sequencer enclave (loopback). This is NOT a p2p apply-broadcast
    // (governance's shape) — the baseline marker is sealed on the sequencer that holds
    // the authoritative perp state.
    let perp = crate::perp_client::PerpClient::new(enclave_perp_v1_base)?;
    let res = perp
        .reserves_baseline_apply(
            base.ledger_index,
            &hex::encode(escrow),
            &hex::encode(issuer),
            base.rlusd_fp8,
            base.xrp_fp8,
            host_timestamp_ms,
            &hex::encode(&bundle),
            &source_fingerprints,
            excluded_account_ids,
        )
        .await?;

    tracing::info!(
        ledger = base.ledger_index,
        distinct_sources = distinct,
        "#131 baseline applied — custody seeded from {} independent operator observations at L",
        distinct
    );
    Ok(res)
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
    fn decimal_fp8_conversion() {
        assert_eq!(decimal_to_fp8("0").unwrap(), 0);
        assert_eq!(decimal_to_fp8("1").unwrap(), 100_000_000);
        assert_eq!(decimal_to_fp8("100.50").unwrap(), 10_050_000_000);
        assert_eq!(decimal_to_fp8("0.00000001").unwrap(), 1);
        assert_eq!(decimal_to_fp8("123.456789012").unwrap(), 12_345_678_901); // >8dp truncates
        assert_eq!(decimal_to_fp8("-5.25").unwrap(), -525_000_000);
        assert!(decimal_to_fp8("abc").is_err());
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

    // ── RESP-#103 C-Q1.3: receiver refuse-before-sign on figure divergence ──

    fn bal(l: u64, rlusd: i64, xrp: i64) -> EscrowBalances {
        EscrowBalances {
            ledger_index: l,
            rlusd_fp8: rlusd,
            xrp_fp8: xrp,
        }
    }

    #[test]
    fn figure_matches_exact_only() {
        // exact match at the pinned ledger → sign
        assert!(figure_matches(
            &bal(84_000_000, 1000, 2000),
            84_000_000,
            1000,
            2000
        ));
        // divergent RLUSD → refuse (a lying/compromised source feeding a false figure)
        assert!(!figure_matches(
            &bal(84_000_000, 1001, 2000),
            84_000_000,
            1000,
            2000
        ));
        // divergent XRP → refuse
        assert!(!figure_matches(
            &bal(84_000_000, 1000, 2001),
            84_000_000,
            1000,
            2000
        ));
        // right figure but WRONG ledger → refuse (no tolerance window, C-Q1.2)
        assert!(!figure_matches(
            &bal(83_999_999, 1000, 2000),
            84_000_000,
            1000,
            2000
        ));
    }

    // ── RESP-#103 C-Q1.1: source diversity is enforced + asserted ──

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
