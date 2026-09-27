//! The attested clock driver — trusted-price part (2), step 3.
//!
//! The enclave cannot tell what time it is. Its wall clock comes from the host, so a
//! signed price quote proves who set a price and never that the price is *current* —
//! signature checking alone only narrows "the operator picks the price" to "the operator
//! picks any real price, from any moment in history". `close_time` in a quorum-signed
//! XRPL ledger header is a time the enclave can verify for itself, because `ledger_hash`
//! binds the whole header and not just the state root.
//!
//! `ecall_perp_attested_clock_advance` has existed since 2026-09-23 with no caller
//! anywhere. This is the caller.
//!
//! **Runs on every node, not only the sequencer**, and that is the point rather than an
//! oversight: each node advances its OWN clock from a header it verified itself. A
//! cluster cosign here would buy nothing (any node can check the same header) and would
//! cost liveness on exactly the path whose stalling halts trading.

use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tracing::{error, info, warn};

use crate::deposit_spv::ValidationBuffer;
use crate::spv_proof::{build_xclk_blob, HEADER_LEN};

/// What happened to one advance attempt, so a driver can tell a permanent refusal from
/// the ordinary case of polling faster than ledgers close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockOutcome {
    /// The clock moved.
    Advanced,
    /// `-89`: this ledger is not newer than the one already sealed. Expected on every
    /// poll that lands between ledger closes — NOT an error, and must not be logged as
    /// one or the log becomes noise nobody reads.
    AlreadyAtOrAhead,
    /// `-70`/`-71`: no pinned UNL, or its quorum floor is not met. The enclave cannot
    /// check ANY header until an operator pins the validator set, so this is a setup
    /// state, not a transient fault — it will repeat forever until someone acts.
    NoPinnedUnl,
    /// Something is wrong with what we are feeding: the bundle did not parse, the header
    /// did not hash, or the validators' quorum did not verify.
    ///
    /// Two encodings reach us. `-201..-219` carry the CAUSE (the enclave's
    /// `XRPL_SPV_HOST_RC_BASE + XRPL_SPV_ERR_*`), and `-72`/`-73`/`-74` are the old
    /// flattened trio, still returned by any enclave built before 2026-09-27 — including
    /// the one deployed today, since a new code only arrives with an MRENCLAVE bump. Both
    /// are classified, because dropping the old ones would blind this driver against the
    /// enclave that is actually running.
    ///
    /// The BEHAVIOUR is the same for every cause on purpose: retry the next ledger. What
    /// the cause changes is what the log says, and that is the whole point — `-72` cost a
    /// source read to diagnose. [`spv_cause`] names it.
    BadBundle,
    /// `-90`: close_time went backwards. A healthy XRPL never produces this.
    TimeWentBackwards,
    /// Anything else, including transport failure.
    Other,
}

/// Classify the enclave's return code. Pure, so the mapping is testable without an
/// enclave — the thing a driver gets wrong is treating every non-zero as a failure and
/// then either spamming or, worse, backing off a path that was never broken.
pub fn classify_clock_rc(rc: i64) -> ClockOutcome {
    match rc {
        0 => ClockOutcome::Advanced,
        -89 => ClockOutcome::AlreadyAtOrAhead,
        -70 | -71 => ClockOutcome::NoPinnedUnl,
        -74..=-72 => ClockOutcome::BadBundle,
        -90 => ClockOutcome::TimeWentBackwards,
        rc if spv_cause(rc).is_some() => ClockOutcome::BadBundle,
        _ => ClockOutcome::Other,
    }
}

/// The base of the enclave's shared SPV cause band — `XRPL_SPV_HOST_RC_BASE` in
/// `xrpl_spv.h`. Mirrored, and pinned against the enclave by a test below.
pub const SPV_HOST_RC_BASE: i64 = -200;

/// Name the cause behind a `-201..-219` refusal, or `None` if `rc` is not in the band.
///
/// The enclave used to flatten every one of these into `-72`, and on 2026-09-27 that cost
/// a diagnosis: the clock refused 100% of ledgers and the only way to learn that the
/// producer was shipping unframed validations was to read the enclave's parser. An
/// operator will not do that, so the cause now reaches the log.
pub fn spv_cause(rc: i64) -> Option<&'static str> {
    Some(match rc - SPV_HOST_RC_BASE {
        -1 => "a null argument reached the SPV layer",
        -2 => {
            "truncated — a field ran past the end of the blob (an unframed \
               validations section looks exactly like this)"
        }
        -3 => "wrong magic for this transport",
        -4 => "unknown transport version",
        -5 => "trailing bytes after the last field",
        -6 => "a count exceeded its cap",
        -7 => "the header length is not 118",
        -8 => "under quorum — too few distinct valid validations",
        -9 => "a validation signs a different ledger hash",
        -10 => "a validation signature failed to verify",
        -11 => "a signer is not in the pinned UNL (ours may be stale)",
        -12 => "the SHAMap path does not hash to the state root",
        -13 => "the proof binds a key other than the derived keylet",
        -14 => "a non-canonical STAmount",
        -15 => "RLUSD from a non-pinned issuer",
        -16 => "an IOU→fixed-point conversion would overflow",
        -17 => "a required field is missing or malformed",
        -18 => "a variable-length prefix is out of range",
        -19 => "not a Payment, or the destination is not the escrow",
        // The list does not stop at -19. Mirroring only the first nineteen is how an
        // incomplete set looks complete; the cross-repo gate caught it here, in the
        // enclave's own band test, and in a static_assert that named the wrong extreme.
        -20 => "a partial payment — delivered less than the Amount",
        -21 => "the transaction did not succeed in the ledger",
        -22 => "an IOU deposit while XRP-only",
        -99 => "an internal SPV error",
        _ => return None,
    })
}

/// Counters for `/v1/system/status`, so "the clock is advancing" is something an operator
/// READS rather than infers from an absence of errors in a log.
#[derive(Debug, Default)]
pub struct ClockHealth {
    pub advances: AtomicU64,
    pub refusals: AtomicU64,
    /// The last non-zero code, verbatim. `0` means none yet.
    pub last_refusal_rc: AtomicI64,
    /// What the ENCLAVE says its clock is — read back from it, not what we last sent.
    pub attested_ledger_seq: AtomicU64,
    /// Seconds since the Ripple epoch (2000-01-01T00:00:00Z), NOT Unix.
    pub attested_close_time: AtomicU64,
    /// Whether a driver is configured to run here at all. Without this, a node with the
    /// driver switched off is indistinguishable from one whose clock is stuck.
    pub driver_enabled: std::sync::atomic::AtomicBool,
}

/// Seconds between the Unix and Ripple epochs. Named, because getting it wrong is a
/// 30-year error that still looks like a plausible timestamp.
pub const RIPPLE_EPOCH_OFFSET_SECS: u64 = 946_684_800;

pub struct ClockDriverConfig {
    pub http_url: String,
    pub ws_url: String,
    /// A ledger closes roughly every 4s; polling much faster only earns `-89`.
    pub interval_secs: u64,
}

impl ClockDriverConfig {
    /// Opt-in via `PERP_ATTESTED_CLOCK=1`, for the same reason the deposit driver is:
    /// until an operator has pinned the UNL, every advance refuses with `-70`, and a
    /// driver running by default would fill every upgraded node's log with a refusal it
    /// cannot fix on its own. The `driver_enabled` flag on [`ClockHealth`] makes the
    /// "off" state visible instead of silent.
    pub fn from_env() -> Option<Self> {
        if std::env::var("PERP_ATTESTED_CLOCK").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Self {
            http_url: std::env::var("XRPL_RPC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:5005".to_string()),
            ws_url: std::env::var("XRPL_WS_URL")
                .unwrap_or_else(|_| "ws://127.0.0.1:6006".to_string()),
            interval_secs: std::env::var("PERP_ATTESTED_CLOCK_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
        })
    }
}

/// Pull the header and hash out of a `ledger` response taken with `binary: true`.
///
/// Pure and separate from the request so the shape rippled actually returns is pinned by
/// a test rather than by whatever the node happened to send the day this was written.
/// `ledger_data` is used verbatim — re-serialising from JSON fields would put field
/// ordering and rounding in our hands, and the bytes the validators signed are not ours
/// to reconstruct.
pub fn header_from_ledger_response(
    j: &serde_json::Value,
) -> Result<([u8; HEADER_LEN], String, u64)> {
    let result = &j["result"];
    if !result["validated"].as_bool().unwrap_or(false) {
        bail!("ledger response is not for a validated ledger");
    }
    let hex = result["ledger"]["ledger_data"]
        .as_str()
        .context("ledger response has no ledger_data (binary:true not set?)")?;
    let raw = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect::<std::result::Result<Vec<u8>, _>>()
        .context("ledger_data is not hex")?;
    if raw.len() != HEADER_LEN {
        bail!("ledger_data is {} bytes, expected {HEADER_LEN}", raw.len());
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&raw);

    let ledger_hash = result["ledger_hash"]
        .as_str()
        .context("ledger response has no ledger_hash")?
        .to_string();
    let index = result["ledger_index"]
        .as_u64()
        .or_else(|| result["ledger_index"].as_str().and_then(|s| s.parse().ok()))
        .context("ledger response has no usable ledger_index")?;

    // The sequence the validators signed is at offset 0 of the header they signed. If it
    // disagrees with the index rippled labelled the response with, the two halves of this
    // response are not about the same ledger and the validations we are about to look up
    // by hash would be attached to the wrong header.
    let header_seq = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
    if header_seq != index {
        bail!("ledger_data says sequence {header_seq} but the response says {index}");
    }
    Ok((header, ledger_hash, index))
}

/// One tick: ask for the latest validated ledger, find the validations we buffered for
/// it, and hand the pair to the enclave.
async fn advance_once(
    http: &reqwest::Client,
    cfg: &ClockDriverConfig,
    perp: &crate::perp_client::PerpClient,
    buffer: &std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
) -> Result<(u64, i64)> {
    let j: serde_json::Value = http
        .post(&cfg.http_url)
        .json(&serde_json::json!({"method": "ledger", "params": [
            {"ledger_index": "validated", "binary": true, "transactions": false}]}))
        .send()
        .await?
        .json()
        .await?;
    let (header, ledger_hash, index) = header_from_ledger_response(&j)?;

    let validations = {
        let b = buffer
            .lock()
            .map_err(|_| anyhow::anyhow!("buffer mutex poisoned"))?;
        b.get(&ledger_hash).cloned()
    };
    let Some(validations) = validations else {
        // Not an error to shout about every tick: we simply were not listening when this
        // ledger was validated, and rippled serves no validation history. The next
        // ledger will have them. Said once per occurrence at debug level by the caller.
        bail!("no buffered validations yet for ledger {index} ({ledger_hash})");
    };

    // The buffered blobs are raw STValidations and the enclave reads a FRAMED section
    // (pubkey33 | sig_len | sig | vbody_len | vbody). Handing over the raw concatenation
    // is what made this driver refuse 100% of ledgers with -72; the builder frames them
    // and derives the count from what it framed, so the two cannot disagree.
    let blob = build_xclk_blob(&header, &validations)?;

    let rc = match perp.attested_clock_advance(&blob).await {
        Ok(_) => 0i64,
        Err(e) => crate::perp_client::rc_from_error(&e.to_string()).unwrap_or(i64::MIN),
    };
    Ok((index, rc))
}

/// The loop. Never exits: a clock that gives up is a halt nobody asked for.
pub async fn run_attested_clock(
    cfg: ClockDriverConfig,
    perp: crate::perp_client::PerpClient,
    buffer: std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
    health: std::sync::Arc<ClockHealth>,
) {
    let http = reqwest::Client::new();
    health.driver_enabled.store(true, Ordering::Relaxed);
    info!(
        interval_secs = cfg.interval_secs,
        "attested-clock driver started (every node, not only the sequencer)"
    );
    // A setup-state refusal repeats forever by definition, so it is announced once and
    // then counted. Otherwise the one message an operator needs is buried under itself.
    let mut announced_no_unl = false;

    loop {
        match advance_once(&http, &cfg, &perp, &buffer).await {
            Ok((index, rc)) => match classify_clock_rc(rc) {
                ClockOutcome::Advanced => {
                    health.advances.fetch_add(1, Ordering::Relaxed);
                    announced_no_unl = false;
                    if let Ok(v) = perp.attested_clock_status().await {
                        let seq = v["attested_ledger_seq"].as_u64().unwrap_or(0);
                        let ct = v["attested_close_time_ripple_epoch"].as_u64().unwrap_or(0);
                        health.attested_ledger_seq.store(seq, Ordering::Relaxed);
                        health.attested_close_time.store(ct, Ordering::Relaxed);
                        info!(
                            metric = "attested_clock_advanced_total",
                            ledger = index,
                            attested_seq = seq,
                            close_time_unix = ct + RIPPLE_EPOCH_OFFSET_SECS,
                            "attested clock advanced"
                        );
                    }
                }
                ClockOutcome::AlreadyAtOrAhead => { /* polled between closes; expected */ }
                ClockOutcome::NoPinnedUnl => {
                    health.refusals.fetch_add(1, Ordering::Relaxed);
                    health.last_refusal_rc.store(rc, Ordering::Relaxed);
                    if !announced_no_unl {
                        announced_no_unl = true;
                        error!(
                            metric = "attested_clock_no_pinned_unl",
                            rc,
                            "attested clock CANNOT ADVANCE: this enclave has no pinned UNL \
                             (or its quorum floor is not met). Mark-dependent risk-taking \
                             stays halted until an operator pins the validator set. This \
                             will not fix itself."
                        );
                    }
                }
                other => {
                    health.refusals.fetch_add(1, Ordering::Relaxed);
                    health.last_refusal_rc.store(rc, Ordering::Relaxed);
                    warn!(
                        metric = "attested_clock_refused_total",
                        rc,
                        ledger = index,
                        outcome = ?other,
                        // Empty for an enclave old enough to still flatten to -72/-74.
                        // Saying so beats printing "unknown", which reads like a defect.
                        cause = spv_cause(rc).unwrap_or("(this enclave does not report a cause)"),
                        "attested clock refused"
                    );
                }
            },
            Err(e) => {
                tracing::debug!("attested-clock tick: {e}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(cfg.interval_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from our own rippled 3.1.3 on testnet, 2026-09-25 — a real response, not
    /// a hand-written one, because a fixture we invented could agree with a producer we
    /// invented and both be wrong about what the node sends.
    const REAL: &str = r#"{"result":{"ledger":{"closed":true,"ledger_data":"014106220163455E948617B432C5B74631AB7C120172A918C5A23CA5B32DE27202D3EA62E3A795500EE4A5EB3F6570C1F9CEA1889D00F8C4462F31820A87A723198F38EF301AE8B243E3FF074286408431A7AA5268500679DF31767F7832DC9775E4E15CB2566699B7583AF6324923543249235C0A00"},"ledger_hash":"B892DB7D9ACDF42C8D88B56E8F9B37871B816BBF0B97019A97BA626372BBEE52","ledger_index":21038626,"status":"success","validated":true}}"#;

    #[test]
    fn a_real_rippled_response_yields_a_118_byte_header() {
        let j: serde_json::Value = serde_json::from_str(REAL).unwrap();
        let (h, hash, idx) = header_from_ledger_response(&j).unwrap();
        assert_eq!(h.len(), HEADER_LEN);
        assert_eq!(idx, 21038626);
        assert_eq!(hash.len(), 64);
        // The two scalars the enclave reads, at the offsets it reads them from. If
        // rippled's serialization ever moved, this is where we find out — not in an
        // anonymous in-enclave refusal.
        assert_eq!(u32::from_be_bytes([h[0], h[1], h[2], h[3]]), 21_038_626);
        let close_time = u32::from_be_bytes([h[112], h[113], h[114], h[115]]) as u64;
        assert_eq!(close_time, 843_653_980);
        assert_eq!(close_time + RIPPLE_EPOCH_OFFSET_SECS, 1_790_338_780);
    }

    #[test]
    fn an_unvalidated_ledger_is_refused() {
        let mut j: serde_json::Value = serde_json::from_str(REAL).unwrap();
        j["result"]["validated"] = serde_json::json!(false);
        assert!(header_from_ledger_response(&j).is_err());
    }

    #[test]
    fn a_header_whose_sequence_disagrees_with_the_response_is_refused() {
        // The failure this catches: validations looked up by the response's hash would be
        // attached to a header about a different ledger.
        let mut j: serde_json::Value = serde_json::from_str(REAL).unwrap();
        j["result"]["ledger_index"] = serde_json::json!(21_038_627u64);
        let e = header_from_ledger_response(&j).unwrap_err().to_string();
        assert!(e.contains("says sequence"), "got: {e}");
    }

    #[test]
    fn a_short_or_unhexy_ledger_data_is_refused() {
        for bad in ["0141", "ZZ41062201", ""] {
            let mut j: serde_json::Value = serde_json::from_str(REAL).unwrap();
            j["result"]["ledger"]["ledger_data"] = serde_json::json!(bad);
            assert!(
                header_from_ledger_response(&j).is_err(),
                "ledger_data {bad:?} must not produce a header"
            );
        }
    }

    #[test]
    fn minus_89_is_not_an_error() {
        // The one that matters for log hygiene: a driver polling faster than ledgers
        // close earns -89 constantly and must not treat it as a fault.
        assert_eq!(classify_clock_rc(-89), ClockOutcome::AlreadyAtOrAhead);
        assert_eq!(classify_clock_rc(0), ClockOutcome::Advanced);
        assert_eq!(classify_clock_rc(-70), ClockOutcome::NoPinnedUnl);
        assert_eq!(classify_clock_rc(-71), ClockOutcome::NoPinnedUnl);
        for bad in [-72, -73, -74] {
            assert_eq!(classify_clock_rc(bad), ClockOutcome::BadBundle);
        }
        assert_eq!(classify_clock_rc(-90), ClockOutcome::TimeWentBackwards);
        assert_eq!(classify_clock_rc(-1), ClockOutcome::Other);
    }

    /// The whole band classifies, and every code in it can be NAMED.
    ///
    /// `-72` used to stand for six different structural refusals, and on 2026-09-27 that
    /// cost a diagnosis: the clock refused every ledger and the cause — unframed
    /// validations, which truncate the section walk — was legible only in the enclave's
    /// source. A code the operator cannot read is a code the operator cannot act on.
    /// Mirrors the enclave's `XRPL_SPV_ERR_*` values. Not contiguous — it runs 1..=22 and
    /// then jumps to 99 — which is exactly why the earlier `1..=19` loop looked complete
    /// and was not. The cross-repo gate asserts this list against the enclave's header.
    const SPV_CAUSE_OFFSETS: [i64; 23] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 99,
    ];

    #[test]
    fn the_whole_spv_cause_band_classifies_and_every_code_has_a_name() {
        let mut names = std::collections::HashSet::new();
        for e in SPV_CAUSE_OFFSETS {
            let rc = SPV_HOST_RC_BASE - e;
            assert_eq!(
                classify_clock_rc(rc),
                ClockOutcome::BadBundle,
                "rc {rc} must classify; an unclassified band member falls to Other and the \
                 driver treats a structural refusal as a transport blip"
            );
            let name = spv_cause(rc).unwrap_or_else(|| panic!("rc {rc} has no cause name"));
            assert!(
                names.insert(name),
                "rc {rc} reuses another cause's text — the band's only job is to tell \
                 causes APART, so two codes sharing a name is the -72 problem again"
            );
        }
        assert_eq!(names.len(), SPV_CAUSE_OFFSETS.len(), "one name per cause");
    }

    /// The band's edges. Without this, widening the range by one would go unnoticed and
    /// `-200` — which the enclave reserves for success, never a refusal — would start
    /// reading as a cause.
    #[test]
    fn the_band_ends_where_the_enclave_says_it_does() {
        assert_eq!(spv_cause(SPV_HOST_RC_BASE), None, "-200 is not a cause");
        assert!(
            spv_cause(SPV_HOST_RC_BASE - 1).is_some(),
            "-201 is the first cause"
        );
        assert!(
            spv_cause(SPV_HOST_RC_BASE - 22).is_some(),
            "-222 is a cause; the band does not end at -219, which is what the first \
             version of this test wrongly asserted"
        );
        assert!(
            spv_cause(SPV_HOST_RC_BASE - 99).is_some(),
            "-299 is the farthest cause"
        );
        // the gaps are gaps, not causes
        for gap in [23i64, 50, 98] {
            assert_eq!(
                spv_cause(SPV_HOST_RC_BASE - gap),
                None,
                "-{} is not a defined cause",
                200 + gap
            );
        }
        // and the band must not reach the ecalls' own codes, or a parse failure would be
        // indistinguishable from -89 "already at or ahead"
        for rc in [-70i64, -71, -72, -89, -90] {
            assert_eq!(spv_cause(rc), None, "rc {rc} is an ecall code, not a cause");
        }
    }
}
