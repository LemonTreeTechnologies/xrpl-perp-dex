//! Perp DEX Orchestrator — main entry point.
//!
//! Rewrite of `perp_orchestrator.py`. Handles:
//!   - Price feed polling (Binance XRP/USDT)
//!   - XRPL deposit monitoring
//!   - Periodic liquidation scanning
//!   - Funding rate computation and application (every 8 hours)
//!   - Periodic state persistence

mod enclave_client;
mod perp_client;
mod price_feed;
mod types;
mod xrpl_monitor;
mod xrpl_signer;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};

use crate::perp_client::PerpClient;
use crate::types::float_to_fp8_string;
use crate::xrpl_monitor::XrplMonitor;

// ── CLI ─────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "perp-dex-orchestrator", about = "Perp DEX Orchestrator")]
struct Cli {
    /// Enclave REST API base URL
    #[arg(long, default_value = "https://localhost:9088/v1")]
    enclave_url: String,

    /// XRPL JSON-RPC URL
    #[arg(long, default_value = "https://s.altnet.rippletest.net:51234")]
    xrpl_url: String,

    /// XRPL escrow account r-address
    #[arg(long)]
    escrow_address: Option<String>,

    /// Path to escrow config JSON file (fallback for --escrow-address)
    #[arg(long, default_value = "/tmp/perp-9088/escrow_account.json")]
    escrow_config: PathBuf,

    /// Price update interval in seconds
    #[arg(long, default_value_t = 5)]
    price_interval: u64,

    /// Liquidation scan interval in seconds
    #[arg(long, default_value_t = 10)]
    liquidation_interval: u64,
}

// ── Funding rate ────────────────────────────────────────────────

const FUNDING_INTERVAL: Duration = Duration::from_secs(8 * 3600); // 8 hours
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes

/// Simple funding rate: premium + interest, clamped to +/- 0.05%.
fn compute_funding_rate(mark_price: f64, index_price: f64) -> f64 {
    if index_price <= 0.0 {
        return 0.0;
    }
    let premium = (mark_price - index_price) / index_price;
    let interest = 0.0001 / 8.0; // 0.01% per day / 3 periods
    let rate = premium + interest;
    rate.clamp(-0.0005, 0.0005)
}

// ── Liquidation scanning ────────────────────────────────────────

async fn run_liquidation_scan(perp: &PerpClient, current_price: f64) {
    let result = match perp.check_liquidations().await {
        Ok(r) => r,
        Err(e) => {
            warn!("liquidation scan failed: {}", e);
            return;
        }
    };

    let count = result["count"].as_u64().unwrap_or(0);
    if count == 0 {
        return;
    }

    warn!(count, "found liquidatable positions");

    if let Some(positions) = result["liquidatable"].as_array() {
        for pos in positions {
            let pos_id = match pos["position_id"].as_u64() {
                Some(id) => id,
                None => continue,
            };
            let user = pos["user_id"].as_str().unwrap_or("unknown");

            match perp
                .liquidate(pos_id, &float_to_fp8_string(current_price))
                .await
            {
                Ok(_) => info!(position_id = pos_id, user, "liquidated position"),
                Err(e) => error!(position_id = pos_id, "liquidation failed: {}", e),
            }
        }
    }
}

// ── Main loop ───────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Resolve escrow address
    let escrow_address = match cli.escrow_address {
        Some(addr) => addr,
        None => {
            let config_data = std::fs::read_to_string(&cli.escrow_config)
                .with_context(|| {
                    format!(
                        "no --escrow-address and cannot read config at {}",
                        cli.escrow_config.display()
                    )
                })?;
            let config: serde_json::Value =
                serde_json::from_str(&config_data).context("invalid escrow config JSON")?;
            config["xrpl_address"]
                .as_str()
                .context("missing xrpl_address in escrow config")?
                .to_string()
        }
    };

    // Initialize clients
    let perp = PerpClient::new(&cli.enclave_url)?;
    let monitor = XrplMonitor::new(&cli.xrpl_url, &escrow_address);
    let http_client = reqwest::Client::new();

    // Try to load persisted state
    match perp.load_state().await {
        Ok(_) => info!("loaded persisted state"),
        Err(_) => info!("no persisted state found, starting fresh"),
    }

    let mut last_ledger: u32 = 0;
    let mut current_price: f64 = 0.0;

    let mut last_price_update = Instant::now() - Duration::from_secs(cli.price_interval + 1);
    let mut last_liquidation_scan =
        Instant::now() - Duration::from_secs(cli.liquidation_interval + 1);
    let mut last_funding_time = Instant::now();
    let mut last_state_save = Instant::now();

    let price_interval = Duration::from_secs(cli.price_interval);
    let liquidation_interval = Duration::from_secs(cli.liquidation_interval);

    info!(escrow = %escrow_address, "orchestrator started");

    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tick.tick().await;
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // ── Price update ────────────────────────────────────────
        if last_price_update.elapsed() >= price_interval {
            match price_feed::fetch_xrp_price(&http_client).await {
                Ok(price) => {
                    current_price = price;
                    let fp8 = float_to_fp8_string(price);
                    if let Err(e) = perp.update_price(&fp8, &fp8, now_ts).await {
                        error!("price update failed: {}", e);
                    }
                }
                Err(e) => {
                    warn!("price fetch failed: {}", e);
                }
            }
            last_price_update = Instant::now();
        }

        // ── Deposit scanning ────────────────────────────────────
        match monitor.scan_deposits(last_ledger).await {
            Ok((deposits, new_ledger)) => {
                for deposit in &deposits {
                    if let Err(e) = perp
                        .deposit(&deposit.sender, &deposit.amount, &deposit.tx_hash)
                        .await
                    {
                        error!(
                            sender = %deposit.sender,
                            "deposit credit failed: {}", e
                        );
                    }
                }
                last_ledger = new_ledger;
            }
            Err(e) => {
                warn!("deposit scan failed: {}", e);
            }
        }

        // ── Liquidation scanning ────────────────────────────────
        if last_liquidation_scan.elapsed() >= liquidation_interval && current_price > 0.0 {
            run_liquidation_scan(&perp, current_price).await;
            last_liquidation_scan = Instant::now();
        }

        // ── Funding rate (every 8 hours) ────────────────────────
        if last_funding_time.elapsed() >= FUNDING_INTERVAL && current_price > 0.0 {
            let rate = compute_funding_rate(current_price, current_price);
            let fp8_rate = float_to_fp8_string(rate);
            match perp.apply_funding(&fp8_rate, now_ts).await {
                Ok(_) => info!(rate = %fp8_rate, "applied funding rate"),
                Err(e) => error!("funding application failed: {}", e),
            }
            last_funding_time = Instant::now();
        }

        // ── Periodic state save (every 5 minutes) ──────────────
        if last_state_save.elapsed() >= STATE_SAVE_INTERVAL {
            if let Err(e) = perp.save_state().await {
                warn!("state save failed: {}", e);
            }
            last_state_save = Instant::now();
        }
    }
}
