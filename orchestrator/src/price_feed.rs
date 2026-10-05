//! Binance price feed for XRP/USDT.
//!
//! Rewrite of the `fetch_xrp_price()` function from `perp_orchestrator.py`.

use anyhow::{Context, Result};

const BINANCE_URL: &str = "https://api.binance.com/api/v3/ticker/price?symbol=XRPUSDT";

/// Fetch the current XRP/USDT spot price from Binance.
pub async fn fetch_xrp_price(client: &reqwest::Client) -> Result<f64> {
    let resp: serde_json::Value = client
        .get(BINANCE_URL)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .context("binance request failed")?
        .error_for_status()
        .context("binance returned error status")?
        .json()
        .await
        .context("binance response not valid JSON")?;

    let price_str = resp["price"]
        .as_str()
        .context("missing 'price' field in binance response")?;

    price_str
        .parse::<f64>()
        .context("failed to parse binance price as f64")
}

/// Which price path this enclave is running, asked of the enclave rather than configured.
///
/// The signed-median track is switched on by `PRICE_ANCHORED_PUBLISHER_COUNT` at COMPILE
/// time, and the same constant compiles the operator-fed `ecall_perp_update_price` into a
/// `-94` refusal. So the orchestrator must not carry its own copy of that switch: a second
/// knob that has to agree with a compile-time constant is one that eventually does not,
/// and this project has been bitten by the parallel-value-that-disagrees three times.
///
/// The enclave's publisher status reports `threshold`, which is non-zero exactly when the
/// signed path is compiled in — an equality the enclave holds with a `static_assert`, not
/// a comment. So we ask, and we believe the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePricePath {
    /// No publisher set anchored: the operator's feed is still the mark, and the enclave
    /// still accepts it. Honest, and not a good place to stay.
    OperatorFeed,
    /// Publishers are anchored. The signed consumer is the only door; pushing the
    /// operator's feed now earns `-94` and the mark simply stops.
    SignedMedian,
}

/// Read the path out of a `/perp/price-publishers/status` body.
///
/// Errs rather than guessing. A wrong guess in either direction is bad in a different
/// way: guessing OperatorFeed after anchoring keeps POSTing a door that answers -94 and
/// the mark goes stale; guessing SignedMedian before it stops the mark for no reason.
pub fn live_price_path(status: &serde_json::Value) -> anyhow::Result<LivePricePath> {
    use anyhow::Context;
    let threshold = status["threshold"]
        .as_u64()
        .context("publisher status has no numeric threshold")?;
    // `signed_path_enabled` is the enclave's own rendering of the same fact. If the two
    // ever disagree we are reading a response we do not understand, and the safe move is
    // to refuse rather than to pick one.
    if let Some(flag) = status["signed_path_enabled"].as_bool() {
        if flag != (threshold != 0) {
            anyhow::bail!(
                "publisher status is self-inconsistent: signed_path_enabled={flag} but \
                 threshold={threshold}"
            );
        }
    }
    Ok(if threshold != 0 {
        LivePricePath::SignedMedian
    } else {
        LivePricePath::OperatorFeed
    })
}

/// How recent an alarm has to be to read as "happening now" rather than "happened".
///
/// A DISPLAY choice, deliberately not a mirror of any enclave constant. The alarm gates
/// nothing, so this number decides only how a status line reads to a human, and copying
/// `PRICE_CONTRIB_WINDOW_SECS` here would create the parallel-value-that-disagrees defect
/// this module already warns about — for no gain, since nothing compares the two. Fifteen
/// minutes is generous enough that a brief quiet spell does not flip a live incident into
/// the past tense.
pub const DEPEG_CURRENT_SECS: u64 = 900;

/// What the depeg alarm says, read from ONE `/perp/price-publishers/status` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepegVerdict {
    /// Nothing has been measured yet. NOT the same as "no divergence": a fresh process has
    /// no samples, and so does one whose USD venues have all gone quiet. The distinction is
    /// why `samples` exists at all.
    NotMeasured,
    /// Measured, never over the threshold.
    Quiet,
    /// Fired, but not recently. Worth reading the logs for; not worth waking anyone.
    FiredEarlier { age_secs: u64 },
    /// Fired inside the current window. This is the one to act on.
    FiringNow { age_secs: u64 },
    /// Fired, and the age cannot be established because the attested clock is unusable
    /// (zero, or behind the alarm it timestamped). Reported as its own case rather than
    /// folded into either side: guessing "earlier" would hide a live incident, and guessing
    /// "now" would cry wolf every time the clock is off.
    FiredAgeUnknown,
}

/// The depeg fields of a publisher status, with the arithmetic done once and guarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepegHealth {
    pub verdict: DepegVerdict,
    /// Last measured distance of the non-USD venue from the USD median, in basis points.
    /// Meaningless unless `samples > 0`.
    pub divergence_bp: u64,
    pub samples: u64,
    /// Accepted price updates that could NOT be measured — no USD baseline at the time.
    /// A number that climbs here means the alarm is going blind while the mark keeps moving.
    pub unmeasured: u64,
    pub alarm_count: u64,
}

/// Read a u64 the enclave sent as a decimal STRING.
///
/// Strings, not numbers, for every 64-bit field — the same convention the masks use, and for
/// the same reason: a JSON number cannot hold a u64 exactly above 2^53, and a silently
/// rounded counter is worse than a parse error. So a number here is refused rather than
/// coerced; accepting both forms is how a wire format quietly acquires two.
fn u64_field(status: &serde_json::Value, key: &str) -> anyhow::Result<u64> {
    use anyhow::Context;
    status[key]
        .as_str()
        .with_context(|| format!("publisher status field `{key}` is not a decimal string"))?
        .parse::<u64>()
        .with_context(|| format!("publisher status field `{key}` is not a u64"))
}

/// Read the depeg alarm out of a `/perp/price-publishers/status` body.
///
/// Errs rather than defaulting, for the reason the threshold reader does: every plausible
/// default here means "nothing is wrong", which is exactly the answer a monitor must not
/// invent for itself.
pub fn depeg_health(status: &serde_json::Value) -> anyhow::Result<DepegHealth> {
    use anyhow::Context;
    let divergence_bp = status["unit_divergence_bp"]
        .as_u64()
        .context("publisher status has no numeric unit_divergence_bp")?;
    let samples = u64_field(status, "unit_samples")?;
    let alarm_count = u64_field(status, "unit_alarm_count")?;
    let last_alarm = u64_field(status, "unit_last_alarm_time")?;
    let updates = u64_field(status, "updates_since_start")?;
    let now = u64_field(status, "attested_now")?;

    // Self-inconsistency is refused, not reconciled — same rule as the threshold reader.
    // More samples than accepted updates cannot happen if both counters come from the same
    // path, so seeing it means we are reading a response we do not understand.
    let unmeasured = updates.checked_sub(samples).with_context(|| {
        format!(
            "publisher status is self-inconsistent: unit_samples={samples} exceeds \
                 updates_since_start={updates}"
        )
    })?;

    let verdict = if alarm_count > 0 {
        // Subtraction via checked_sub, in the expression: a release build wraps, and a
        // wrapped age would read as an alarm from 584 billion years ago — i.e. as calm.
        match now.checked_sub(last_alarm) {
            // now == 0 means no attested time was ever taken, and that cannot age anything.
            Some(age) if now != 0 => {
                if age <= DEPEG_CURRENT_SECS {
                    DepegVerdict::FiringNow { age_secs: age }
                } else {
                    DepegVerdict::FiredEarlier { age_secs: age }
                }
            }
            _ => DepegVerdict::FiredAgeUnknown,
        }
    } else if samples > 0 {
        DepegVerdict::Quiet
    } else {
        DepegVerdict::NotMeasured
    };

    Ok(DepegHealth {
        verdict,
        divergence_bp,
        samples,
        unmeasured,
        alarm_count,
    })
}

/// `price-health` — the operator's answer to "has the dollar venue stopped meaning dollars".
///
/// This exists because the alarm was otherwise a number nobody reads. The enclave measures
/// it, the server publishes it, and until this command there was no caller: an unread signal
/// is the same as no signal, which is the shape this project already got bitten by with
/// ecalls that had no host caller.
///
/// Exits non-zero only for the two states that warrant waking someone — an alarm inside the
/// current window, and an alarm whose age cannot be established. A past alarm and a quiet
/// feed both exit zero and still print what they are.
pub async fn cli_price_health(enclave_url: &str) -> anyhow::Result<()> {
    let perp = crate::perp_client::PerpClient::new(enclave_url)?;
    let status = perp.price_publisher_status().await?;

    // ASKED, not assumed. In a build with no publishers anchored there is nothing to measure
    // and "not measured" would read as a fault; it is the honest and expected answer there.
    let path = live_price_path(&status)?;
    if path == LivePricePath::OperatorFeed {
        println!("signed price path: NOT LIVE in this build — no publishers anchored.");
        println!("  Nothing to measure, and nothing wrong. The depeg alarm begins reporting");
        println!("  when the anchoring ceremony runs.");
        return Ok(());
    }

    let h = depeg_health(&status)?;
    println!("signed price path: LIVE");
    println!("  last measured divergence : {} bp", h.divergence_bp);
    println!("  measured samples         : {}", h.samples);
    if h.unmeasured > 0 {
        println!(
            "  UNMEASURED updates       : {} — accepted with no USD baseline to \
                  compare against, so the alarm was blind for those",
            h.unmeasured
        );
    }
    println!("  alarms since start       : {}", h.alarm_count);

    match h.verdict {
        DepegVerdict::NotMeasured => {
            println!("\nverdict: NOT MEASURED YET — this is not a clean bill of health.");
            println!("  Either the process just started, or no update has carried two USD");
            println!("  publishers to form a baseline. Zero divergence is not being claimed.");
            Ok(())
        }
        DepegVerdict::Quiet => {
            println!("\nverdict: QUIET — measured, and never over the alarm threshold.");
            Ok(())
        }
        DepegVerdict::FiredEarlier { age_secs } => {
            println!("\nverdict: FIRED EARLIER — last alarm {age_secs}s ago, not current.");
            println!("  Worth reading the logs around it. Not worth waking anyone.");
            Ok(())
        }
        DepegVerdict::FiringNow { age_secs } => {
            println!("\nverdict: FIRING NOW — last alarm {age_secs}s ago.");
            println!(
                "  The non-USD venue has drifted {} bp from the USD median.",
                h.divergence_bp
            );
            println!("  The mark is NOT halted, and was never gated on this signal — the");
            println!("  threshold is what protects the median, and it still holds. So this is");
            println!("  a venue decision, not an emergency stop: re-run the venue review, and");
            println!("  consider taking that publisher out of service via the RESTRICT dial if");
            println!("  the drift persists.");
            anyhow::bail!(
                "depeg alarm is current ({age_secs}s ago, {} bp)",
                h.divergence_bp
            )
        }
        DepegVerdict::FiredAgeUnknown => {
            println!("\nverdict: FIRED, AGE UNKNOWN — the attested clock cannot age it.");
            println!("  An alarm was recorded but `attested_now` is zero or behind the alarm's");
            println!("  own timestamp, so there is no way to tell a live incident from an old");
            println!("  one. Treat as live until the clock is explained.");
            anyhow::bail!("depeg alarm present and unageable (attested clock unusable)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_threshold_means_the_operator_feed_is_still_the_mark() {
        let j = serde_json::json!({
            "status": "ok", "anchored": 0, "threshold": 0, "signed_path_enabled": false
        });
        assert_eq!(live_price_path(&j).unwrap(), LivePricePath::OperatorFeed);
    }

    #[test]
    fn a_nonzero_threshold_means_the_signed_consumer_is_the_only_door() {
        let j = serde_json::json!({
            "status": "ok", "anchored": 5, "threshold": 4, "signed_path_enabled": true
        });
        assert_eq!(live_price_path(&j).unwrap(), LivePricePath::SignedMedian);
    }

    #[test]
    fn a_self_inconsistent_status_is_refused_rather_than_resolved() {
        // Reading a response we do not understand and picking a side is how a node ends up
        // pushing prices at a door that stopped accepting them.
        for (th, flag) in [(0u64, true), (4u64, false)] {
            let j = serde_json::json!({ "threshold": th, "signed_path_enabled": flag });
            let e = live_price_path(&j).unwrap_err().to_string();
            assert!(e.contains("self-inconsistent"), "got: {e}");
        }
    }

    /// The shape the enclave's handler actually emits, in ONE place, so a drift in the wire
    /// format breaks every test below instead of being absorbed by each one separately.
    ///
    /// SAID PLAINLY: this is a fixture, and the producer lives in the other repo, so nothing
    /// in THIS repo can prove it matches. The co-deluded-fixture risk is real and the two can
    /// only ever be compared against a live response.
    ///
    /// What covers it: the producer asserts the same field names and the same JSON types from
    /// its own side, against a real running server, in its SIM smoke gate. So a rename or a
    /// form change breaks there loudly instead of arriving here as a missing field — and a
    /// missing field is the dangerous direction, because every absent depeg value would
    /// otherwise default to a reading of "nothing is wrong". The refusals below (a number in
    /// place of a decimal string is rejected, not coerced; every field is required) are this
    /// side's half of the same contract.
    fn status(
        div_bp: u64,
        samples: u64,
        alarms: u64,
        last: u64,
        now: u64,
        updates: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "status": "ok", "anchored": 4, "threshold": 3, "signed_path_enabled": true,
            "disabled_mask": "0", "contributed_mask": "15", "contributed_count": 4,
            "updates_since_start": updates.to_string(),
            "unit_divergence_bp": div_bp,
            "unit_samples": samples.to_string(),
            "unit_alarm_count": alarms.to_string(),
            "unit_last_alarm_time": last.to_string(),
            "attested_now": now.to_string(),
        })
    }

    #[test]
    fn no_samples_reads_as_unmeasured_and_not_as_calm() {
        // The distinction the whole sample counter exists for: a fresh process and a process
        // whose USD venues have all gone quiet both show divergence 0, and only one is fine.
        let h = depeg_health(&status(0, 0, 0, 0, 1_000_000, 0)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::NotMeasured);
    }

    #[test]
    fn measured_and_never_over_threshold_is_quiet() {
        let h = depeg_health(&status(13, 400, 0, 0, 1_000_000, 400)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::Quiet);
        assert_eq!(h.divergence_bp, 13);
        assert_eq!(h.unmeasured, 0);
    }

    #[test]
    fn a_recent_alarm_is_firing_now() {
        let h = depeg_health(&status(312, 400, 2, 1_000_000 - 60, 1_000_000, 400)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiringNow { age_secs: 60 });
        assert_eq!(h.alarm_count, 2);
    }

    #[test]
    fn an_old_alarm_is_past_tense() {
        let h = depeg_health(&status(14, 400, 2, 1_000_000 - 7_200, 1_000_000, 400)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiredEarlier { age_secs: 7_200 });
    }

    #[test]
    fn the_window_boundary_is_inclusive_and_the_next_second_is_not() {
        let at = DEPEG_CURRENT_SECS;
        let h = depeg_health(&status(50, 9, 1, 1_000_000 - at, 1_000_000, 9)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiringNow { age_secs: at });
        let h = depeg_health(&status(50, 9, 1, 1_000_000 - at - 1, 1_000_000, 9)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiredEarlier { age_secs: at + 1 });
    }

    #[test]
    fn an_alarm_with_an_unusable_clock_is_neither_now_nor_earlier() {
        // Clock never set: nothing can age the alarm. Calling it "earlier" would hide a live
        // incident and calling it "now" would cry wolf whenever the clock is off — and on
        // this cluster the attested clock has in fact been off.
        let h = depeg_health(&status(400, 5, 1, 900_000, 0, 5)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiredAgeUnknown);
        // Clock BEHIND the alarm it timestamped: a release build would wrap this subtraction
        // into an age of ~584 billion years, which reads as perfectly calm.
        let h = depeg_health(&status(400, 5, 1, 1_000_000, 999_000, 5)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiredAgeUnknown);
        // BOTH zero, which is the case `checked_sub` alone does NOT cover: 0 - 0 is Some(0),
        // so without the `now != 0` guard a dead clock reports a brand-new incident, age zero.
        // Probing found this guard hollow until this line existed — the two cases above both
        // land in the None arm and never reach it.
        //
        // The enclave cannot currently emit it: an accepted signed update requires a non-zero
        // attested clock, so an alarm timestamp of 0 is unreachable there. That argument is
        // not available HERE. This reader parses whatever arrives, the producer lives in
        // another repository, and a guard justified by a caller's behaviour dies the moment
        // the caller changes.
        let h = depeg_health(&status(400, 5, 1, 0, 0, 5)).unwrap();
        assert_eq!(h.verdict, DepegVerdict::FiredAgeUnknown);
    }

    #[test]
    fn unmeasured_updates_are_reported_rather_than_hidden() {
        // 400 accepted updates, 390 measurable: the alarm went blind for ten of them while
        // the mark kept moving. That is the number that says the signal has holes in it.
        let h = depeg_health(&status(20, 390, 0, 0, 1_000_000, 400)).unwrap();
        assert_eq!(h.unmeasured, 10);
    }

    #[test]
    fn more_samples_than_updates_is_refused_not_reconciled() {
        let e = depeg_health(&status(20, 401, 0, 0, 1_000_000, 400))
            .unwrap_err()
            .to_string();
        assert!(e.contains("self-inconsistent"), "got: {e}");
    }

    #[test]
    fn a_u64_field_sent_as_a_json_number_is_refused_not_coerced() {
        // The convention is u64-as-string, because a JSON number cannot hold a u64 exactly
        // above 2^53 and a silently rounded counter is worse than a parse error. Accepting
        // both forms is how a wire format ends up with two.
        let mut j = status(20, 5, 0, 0, 1_000_000, 5);
        j["unit_samples"] = serde_json::json!(5);
        assert!(depeg_health(&j)
            .unwrap_err()
            .to_string()
            .contains("decimal string"));
    }

    #[test]
    fn every_depeg_field_is_required() {
        // Each one defaults to a reading of "nothing is wrong", which is precisely the
        // answer a monitor must never invent. Dropped INDIVIDUALLY: a loop that removed all
        // of them at once would pass even if only the first were checked.
        for key in [
            "unit_divergence_bp",
            "unit_samples",
            "unit_alarm_count",
            "unit_last_alarm_time",
            "attested_now",
            "updates_since_start",
        ] {
            let mut j = status(20, 5, 1, 999_000, 1_000_000, 5);
            j.as_object_mut().unwrap().remove(key);
            assert!(
                depeg_health(&j).is_err(),
                "a missing `{key}` was not refused"
            );
        }
    }

    #[test]
    fn a_missing_threshold_is_an_error_not_a_default() {
        // Defaulting to 0 would silently mean "keep using the operator feed", which is
        // precisely the wrong answer after anchoring.
        assert!(live_price_path(&serde_json::json!({"status": "ok"})).is_err());
        assert!(live_price_path(&serde_json::json!({"threshold": "4"})).is_err());
    }
}
