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

    #[test]
    fn a_missing_threshold_is_an_error_not_a_default() {
        // Defaulting to 0 would silently mean "keep using the operator feed", which is
        // precisely the wrong answer after anchoring.
        assert!(live_price_path(&serde_json::json!({"status": "ok"})).is_err());
        assert!(live_price_path(&serde_json::json!({"threshold": "4"})).is_err());
    }
}
