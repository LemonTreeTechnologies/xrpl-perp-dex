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
