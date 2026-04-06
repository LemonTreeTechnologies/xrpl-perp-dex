//! Perp DEX Orchestrator — main entry point.
//!
//! Two concurrent tasks:
//!   1. API server (axum) — accepts orders from users
//!   2. Background loop — price feeds, deposit monitoring, liquidations, funding

mod api;
mod auth;
mod commitment;
mod election;
mod enclave_client;
mod orderbook;
mod p2p;
mod perp_client;
mod price_feed;
mod trading;
mod types;
mod ws;
mod xrpl_monitor;
mod xrpl_signer;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};

use crate::api::AppState;
use crate::perp_client::PerpClient;
use crate::trading::TradingEngine;
use crate::types::float_to_fp8_string;
use crate::ws::WsEvent;
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

    /// API server listen address
    #[arg(long, default_value = "0.0.0.0:3000")]
    api_listen: String,

    /// Market name
    #[arg(long, default_value = "XRP-RLUSD-PERP")]
    market: String,

    /// P2P listen address (libp2p multiaddr)
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/4001")]
    p2p_listen: String,

    /// P2P peers to connect to (multiaddr, comma-separated)
    #[arg(long)]
    p2p_peers: Option<String>,

    /// Operator priority for sequencer election (0=highest, 2=lowest)
    #[arg(long, default_value_t = 0)]
    priority: u8,
}

// ── Funding rate ────────────────────────────────────────────────

const FUNDING_INTERVAL: Duration = Duration::from_secs(8 * 3600);
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(300);

fn compute_funding_rate(mark_price: f64, index_price: f64) -> f64 {
    if index_price <= 0.0 {
        return 0.0;
    }
    let premium = (mark_price - index_price) / index_price;
    premium.clamp(-0.0005, 0.0005)
}

// ── Liquidation scanning ────────────────────────────────────────

async fn run_liquidation_scan(
    perp: &PerpClient,
    current_price: f64,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) {
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
                Ok(_) => {
                    info!(position_id = pos_id, user, "liquidated position");
                    let _ = ws_tx.send(WsEvent::Liquidation {
                        position_id: pos_id,
                        user_id: user.to_string(),
                        price: float_to_fp8_string(current_price),
                    });
                }
                Err(e) => error!(position_id = pos_id, "liquidation failed: {}", e),
            }
        }
    }
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
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
                    format!("no --escrow-address and cannot read {}", cli.escrow_config.display())
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
    let perp_for_api = PerpClient::new(&cli.enclave_url)?;
    let monitor = XrplMonitor::new(&cli.xrpl_url, &escrow_address);
    let http_client = reqwest::Client::new();

    // Try to load persisted state
    match perp.load_state().await {
        Ok(_) => info!("loaded persisted state"),
        Err(_) => info!("no persisted state, starting fresh"),
    }

    // Create trading engine — always wire batch publisher (gated by is_sequencer flag)
    let (trade_batch_tx, mut trade_batch_rx) = tokio::sync::mpsc::channel::<p2p::OrderBatch>(100);
    let mut engine = TradingEngine::new(&cli.market, perp_for_api);
    engine = engine.with_batch_publisher(trade_batch_tx.clone());

    let is_sequencer = Arc::new(AtomicBool::new(cli.priority == 0));
    let (ws_tx, _) = tokio::sync::broadcast::channel::<WsEvent>(256);
    let app_state = Arc::new(AppState {
        engine,
        perp: PerpClient::new(&cli.enclave_url)?,
        ws_tx: ws_tx.clone(),
        is_sequencer: is_sequencer.clone(),
    });

    // Start API server
    let api_listen = cli.api_listen.clone();
    let api_state = app_state.clone();
    let _api_handle = tokio::spawn(async move {
        let router = api::router(api_state);
        let listener = tokio::net::TcpListener::bind(&api_listen).await.unwrap();
        info!(listen = %api_listen, "API server started");
        axum::serve(listener, router).await.unwrap();
    });

    // Start P2P node (gossipsub for order flow replication + election)
    let (batch_tx, mut batch_rx) = tokio::sync::mpsc::channel::<p2p::OrderBatch>(100);
    let (election_inbound_tx, election_inbound_rx) =
        tokio::sync::mpsc::channel::<election::ElectionMessage>(100);
    let mut p2p_node = p2p::P2PNode::new(&cli.p2p_listen, batch_tx, election_inbound_tx)
        .await
        .context("failed to start P2P node")?;

    // Wire P2P publishing channels
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<p2p::OrderBatch>(100);
    p2p_node.set_publish_channel(pub_rx);

    let (election_outbound_tx, election_outbound_rx) =
        tokio::sync::mpsc::channel::<election::ElectionMessage>(100);
    p2p_node.set_election_publish_channel(election_outbound_rx);

    // Forward trade batches to P2P — only when sequencer
    let is_seq_fwd = is_sequencer.clone();
    let _fwd_handle = tokio::spawn(async move {
        while let Some(batch) = trade_batch_rx.recv().await {
            if is_seq_fwd.load(Ordering::Relaxed) {
                if let Err(e) = pub_tx.send(batch).await {
                    warn!("failed to forward batch to P2P: {}", e);
                }
            }
        }
    });

    // Connect to peers
    if let Some(peers_str) = &cli.p2p_peers {
        for peer in peers_str.split(',') {
            let peer = peer.trim();
            if !peer.is_empty() {
                match p2p_node.dial(peer) {
                    Ok(_) => info!(peer = %peer, "dialing P2P peer"),
                    Err(e) => warn!(peer = %peer, "failed to dial: {}", e),
                }
            }
        }
    }

    info!(
        priority = cli.priority,
        initial_role = if cli.priority == 0 { "sequencer" } else { "validator" },
        peer_id = %p2p_node.peer_id,
        "P2P started"
    );

    // Start election state machine
    let (role_tx, mut role_rx) = tokio::sync::watch::channel(
        if cli.priority == 0 {
            election::Role::Sequencer
        } else {
            election::Role::Validator
        },
    );
    let election_config = election::ElectionConfig {
        our_peer_id: p2p_node.peer_id.to_string(),
        our_priority: cli.priority,
        heartbeat_interval: Duration::from_secs(5),
        heartbeat_timeout: Duration::from_secs(15),
    };
    let mut election_state =
        election::ElectionState::new(election_config, election_outbound_tx, election_inbound_rx, role_tx);
    let _election_handle = tokio::spawn(async move {
        election_state.run().await;
    });

    // Role change watcher — flips is_sequencer AtomicBool
    let is_seq_watcher = is_sequencer.clone();
    let _role_handle = tokio::spawn(async move {
        while role_rx.changed().await.is_ok() {
            let new_role = *role_rx.borrow();
            match new_role {
                election::Role::Sequencer => {
                    info!("ROLE CHANGE → Sequencer");
                    is_seq_watcher.store(true, Ordering::Relaxed);
                }
                election::Role::Validator => {
                    info!("ROLE CHANGE → Validator");
                    is_seq_watcher.store(false, Ordering::Relaxed);
                }
            }
        }
    });

    // P2P event loop
    let _p2p_handle = tokio::spawn(async move {
        p2p_node.run().await;
    });

    // Validator: process received batches from P2P (only when not sequencer)
    let is_seq_validator = is_sequencer.clone();
    let _validator_handle = tokio::spawn(async move {
        while let Some(batch) = batch_rx.recv().await {
            if !is_seq_validator.load(Ordering::Relaxed) {
                info!(
                    seq = batch.seq_num,
                    orders = batch.orders.len(),
                    fills = batch.orders.iter().map(|o| o.fills.len()).sum::<usize>(),
                    hash = %batch.state_hash,
                    "received batch from sequencer — replaying"
                );
                // TODO: replay orders against local order book
                // TODO: call enclave open_position for each fill
                // TODO: verify state_hash matches after replay
            }
        }
    });

    // Background orchestration loop
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

        // Price update
        if last_price_update.elapsed() >= price_interval {
            match price_feed::fetch_xrp_price(&http_client).await {
                Ok(price) => {
                    current_price = price;
                    let fp8 = float_to_fp8_string(price);
                    if let Err(e) = perp.update_price(&fp8, &fp8, now_ts).await {
                        error!("price update failed: {}", e);
                    }
                    let _ = app_state.ws_tx.send(WsEvent::Ticker {
                        mark_price: fp8.clone(),
                        index_price: fp8,
                        timestamp: now_ts,
                    });
                }
                Err(e) => warn!("price fetch failed: {}", e),
            }
            last_price_update = Instant::now();
        }

        // Deposit scanning
        match monitor.scan_deposits(last_ledger).await {
            Ok((deposits, new_ledger)) => {
                for deposit in &deposits {
                    if let Err(e) = perp
                        .deposit(&deposit.sender, &deposit.amount, &deposit.tx_hash)
                        .await
                    {
                        error!(sender = %deposit.sender, "deposit credit failed: {}", e);
                    }
                }
                last_ledger = new_ledger;
            }
            Err(e) => warn!("deposit scan failed: {}", e),
        }

        // Liquidation scanning
        if last_liquidation_scan.elapsed() >= liquidation_interval && current_price > 0.0 {
            run_liquidation_scan(&perp, current_price, &app_state.ws_tx).await;
            last_liquidation_scan = Instant::now();
        }

        // Funding rate (every 8 hours)
        if last_funding_time.elapsed() >= FUNDING_INTERVAL && current_price > 0.0 {
            let rate = compute_funding_rate(current_price, current_price);
            let fp8_rate = float_to_fp8_string(rate);
            match perp.apply_funding(&fp8_rate, now_ts).await {
                Ok(_) => info!(rate = %fp8_rate, "applied funding rate"),
                Err(e) => error!("funding application failed: {}", e),
            }
            last_funding_time = Instant::now();
        }

        // State save (every 5 minutes)
        if last_state_save.elapsed() >= STATE_SAVE_INTERVAL {
            if let Err(e) = perp.save_state().await {
                warn!("state save failed: {}", e);
            }
            last_state_save = Instant::now();
        }
    }
}
