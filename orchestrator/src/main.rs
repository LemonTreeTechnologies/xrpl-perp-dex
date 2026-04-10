//! Perp DEX Orchestrator — main entry point.
//!
//! Two concurrent tasks:
//!   1. API server (axum) — accepts orders from users
//!   2. Background loop — price feeds, deposit monitoring, liquidations, funding

mod api;
mod auth;
mod commitment;
mod db;
mod election;
mod orderbook;
mod p2p;
mod perp_client;
mod price_feed;
mod trading;
mod types;
mod withdrawal;
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

    /// Persistent libp2p identity key path. Auto-generated on first run so
    /// the operator's peer_id is stable across restarts (required for any
    /// allowlist or forensic correlation).
    #[arg(long, default_value = "p2p_identity.key")]
    p2p_key_path: PathBuf,

    /// Operator priority for sequencer election (0=highest, 2=lowest)
    #[arg(long, default_value_t = 0)]
    priority: u8,

    /// PostgreSQL connection URL (optional — history disabled if not set)
    #[arg(long)]
    database_url: Option<String>,
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
    db: &Option<db::Db>,
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
                    // Nudge the user's client to re-fetch positions.
                    let _ = ws_tx.send(WsEvent::PositionChanged {
                        user_id: user.to_string(),
                        reason: "liquidation".into(),
                    });
                    if let Some(db) = db {
                        db.insert_liquidation(pos_id, user, current_price).await;
                    }
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Resolve escrow address
    let escrow_address = match cli.escrow_address {
        Some(addr) => addr,
        None => {
            let config_data = std::fs::read_to_string(&cli.escrow_config).with_context(|| {
                format!(
                    "no --escrow-address and cannot read {}",
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
    let perp_for_api = PerpClient::new(&cli.enclave_url)?;
    let monitor = XrplMonitor::new(&cli.xrpl_url, &escrow_address);
    let http_client = reqwest::Client::new();

    // Try to load persisted state
    match perp.load_state().await {
        Ok(_) => info!("loaded persisted state"),
        Err(_) => info!("no persisted state, starting fresh"),
    }

    // Create trading engine — always wire batch publisher (gated by is_sequencer flag)
    let node_id = format!("{}:p{}", cli.p2p_listen, cli.priority);
    let (trade_batch_tx, mut trade_batch_rx) = tokio::sync::mpsc::channel::<p2p::OrderBatch>(100);
    let mut engine = TradingEngine::new(&cli.market, perp_for_api, &node_id);
    engine = engine.with_batch_publisher(trade_batch_tx.clone());

    let is_sequencer = Arc::new(AtomicBool::new(cli.priority == 0));
    let mark_price = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let funding_rate = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let last_funding_time = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (ws_tx, _) = tokio::sync::broadcast::channel::<WsEvent>(256);

    // Connect to PostgreSQL (optional — history disabled if not configured)
    let db = match &cli.database_url {
        Some(url) => db::Db::connect(url).await,
        None => {
            info!("no --database-url, trade history disabled");
            None
        }
    };

    // C5.1: Rebuild orderbook from persisted resting orders (failover recovery).
    if let Some(ref db) = db {
        let resting = db.load_resting_orders().await;
        if !resting.is_empty() {
            info!(count = resting.len(), "loading resting orders from PG");
            engine.load_orders(resting).await;
        }
    }

    let app_state = Arc::new(AppState {
        engine,
        perp: PerpClient::new(&cli.enclave_url)?,
        ws_tx: ws_tx.clone(),
        is_sequencer: is_sequencer.clone(),
        mark_price: mark_price.clone(),
        funding_rate: funding_rate.clone(),
        last_funding_time: last_funding_time.clone(),
        xrpl_url: cli.xrpl_url.clone(),
        escrow_address: escrow_address.clone(),
        db: db.clone(),
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
    let p2p_keypair = p2p::load_or_create_identity(&cli.p2p_key_path)
        .context("failed to load or create libp2p identity")?;
    let mut p2p_node = p2p::P2PNode::new(&cli.p2p_listen, p2p_keypair, batch_tx, election_inbound_tx)
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
    let (role_tx, mut role_rx) = tokio::sync::watch::channel(if cli.priority == 0 {
        election::Role::Sequencer
    } else {
        election::Role::Validator
    });
    let election_config = election::ElectionConfig {
        our_peer_id: p2p_node.peer_id.to_string(),
        our_priority: cli.priority,
        heartbeat_interval: Duration::from_secs(5),
        heartbeat_timeout: Duration::from_secs(15),
    };
    let mut election_state = election::ElectionState::new(
        election_config,
        election_outbound_tx,
        election_inbound_rx,
        role_tx,
    );
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

    // Validator: replay received batches from sequencer via P2P
    let is_seq_validator = is_sequencer.clone();
    let validator_perp = PerpClient::new(&cli.enclave_url)?;
    let validator_db = app_state.db.clone();
    let _validator_handle = tokio::spawn(async move {
        let mut last_seq: u64 = 0;
            let mut known_leader: Option<String> = None;
        while let Some(batch) = batch_rx.recv().await {
            if is_seq_validator.load(Ordering::Relaxed) {
                continue; // sequencer doesn't replay its own batches
            }

            let total_fills: usize = batch.orders.iter().map(|o| o.fills.len()).sum();

            // Verify batch source consistency — if we've seen batches from a sequencer,
            // reject batches from a different sequencer (potential rogue node)
            if !batch.sequencer_id.is_empty() {
                if let Some(ref known) = known_leader {
                    if *known != batch.sequencer_id {
                        warn!(
                            known = %known,
                            got = %batch.sequencer_id,
                            "batch from unexpected sequencer — ignoring"
                        );
                        continue;
                    }
                } else {
                    info!(sequencer = %batch.sequencer_id, "accepted sequencer identity");
                    known_leader = Some(batch.sequencer_id.clone());
                }
            }

            info!(
                seq = batch.seq_num,
                orders = batch.orders.len(),
                fills = total_fills,
                hash = %batch.state_hash,
                "replaying batch from sequencer"
            );

            // Check sequence ordering
            if batch.seq_num != last_seq + 1 && last_seq > 0 {
                warn!(
                    expected = last_seq + 1,
                    got = batch.seq_num,
                    "batch sequence gap detected"
                );
            }
            last_seq = batch.seq_num;

            // Replay each fill: open positions in local enclave
            // Batch-level timestamp is in seconds; trade rows want
            // milliseconds. This loses sub-second ordering between fills
            // in the same batch but is sufficient for historical display.
            let batch_timestamp_ms = batch.timestamp.saturating_mul(1000);
            for order in &batch.orders {
                for fill in &order.fills {
                    // Determine maker/taker sides
                    let (taker_side, maker_side) = match fill.taker_side.as_str() {
                        "long" => ("long", "short"),
                        _ => ("short", "long"),
                    };

                    // Open taker position
                    if let Err(e) = validator_perp
                        .open_position(
                            &order.user_id,
                            taker_side,
                            &fill.size,
                            &fill.price,
                            order.leverage,
                        )
                        .await
                    {
                        warn!(
                            trade_id = fill.trade_id,
                            user = %order.user_id,
                            "taker replay failed: {}",
                            e
                        );
                    }

                    // Open maker position
                    if let Err(e) = validator_perp
                        .open_position(
                            &fill.maker_user_id,
                            maker_side,
                            &fill.size,
                            &fill.price,
                            order.leverage,
                        )
                        .await
                    {
                        warn!(
                            trade_id = fill.trade_id,
                            user = %fill.maker_user_id,
                            "maker replay failed: {}",
                            e
                        );
                    }

                    // Passive replication: every validator writes the same
                    // trade row its local sequencer would have written. The
                    // ON CONFLICT (trade_id, market) DO NOTHING clause in
                    // insert_trade makes this safe even when the batch
                    // loops back to its originating sequencer after a role
                    // change. See docs/vault-design-followup.md § B3.1.
                    if let Some(db) = &validator_db {
                        let (price_ok, size_ok) = (
                            fill.price.parse::<crate::types::FP8>(),
                            fill.size.parse::<crate::types::FP8>(),
                        );
                        if let (Ok(price), Ok(size)) = (price_ok, size_ok) {
                            db.insert_trade(
                                fill.trade_id,
                                "XRP-RLUSD-PERP",
                                fill.maker_order_id,
                                fill.taker_order_id,
                                &fill.maker_user_id,
                                &order.user_id,
                                price,
                                size,
                                &fill.taker_side,
                                batch_timestamp_ms,
                            )
                            .await;
                        } else {
                            warn!(
                                trade_id = fill.trade_id,
                                "validator pg replay: failed to parse fill price/size"
                            );
                        }
                    }
                }
            }

            // Verify state hash — recompute from batch data and compare
            {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(batch.seq_num.to_le_bytes());
                for order in &batch.orders {
                    for fill in &order.fills {
                        hasher.update(fill.trade_id.to_le_bytes());
                        if let Ok(p) = fill.price.parse::<crate::types::FP8>() {
                            hasher.update(p.raw().to_le_bytes());
                        }
                        if let Ok(s) = fill.size.parse::<crate::types::FP8>() {
                            hasher.update(s.raw().to_le_bytes());
                        }
                    }
                }
                hasher.update(batch.timestamp.to_le_bytes());
                let local_hash = hex::encode(hasher.finalize());
                if local_hash != batch.state_hash {
                    error!(
                        expected = %batch.state_hash,
                        computed = %local_hash,
                        seq = batch.seq_num,
                        "STATE HASH MISMATCH — sequencer may be compromised"
                    );
                } else {
                    info!(seq = batch.seq_num, "state hash verified");
                }
            }
        }
    });

    // Background orchestration loop
    // Persist last_ledger to avoid re-processing deposits on restart
    let ledger_file = "/tmp/perp-9088/last_ledger.txt";
    let mut last_ledger: u32 = std::fs::read_to_string(ledger_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if last_ledger > 0 {
        info!(last_ledger, "resumed from persisted ledger index");
    }
    let mut current_price: f64 = 0.0;

    let mut last_price_update = Instant::now() - Duration::from_secs(cli.price_interval + 1);
    let mut last_liquidation_scan =
        Instant::now() - Duration::from_secs(cli.liquidation_interval + 1);
    let mut last_funding_instant = Instant::now();
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
                    let index_fp8 = float_to_fp8_string(price);
                    // Mark price = orderbook mid if available, else index
                    let mark = match app_state.engine.ticker().await {
                        (_, _, Some(mid)) => mid.to_f64(),
                        _ => price,
                    };
                    let mark_fp8 = float_to_fp8_string(mark);
                    if let Err(e) = perp.update_price(&mark_fp8, &index_fp8, now_ts).await {
                        error!("price update failed: {}", e);
                    }
                    app_state
                        .mark_price
                        .store(crate::types::FP8::from_f64(mark).raw(), Ordering::Relaxed);
                    let _ = app_state.ws_tx.send(WsEvent::Ticker {
                        mark_price: mark_fp8,
                        index_price: index_fp8,
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
                    } else if let Some(db) = &app_state.db {
                        db.insert_deposit(&deposit.sender, &deposit.amount, &deposit.tx_hash, new_ledger).await;
                    }
                }
                if new_ledger > last_ledger {
                    last_ledger = new_ledger;
                    let _ = std::fs::write(ledger_file, last_ledger.to_string());
                }
            }
            Err(e) => warn!("deposit scan failed: {}", e),
        }

        // Liquidation scanning
        if last_liquidation_scan.elapsed() >= liquidation_interval && current_price > 0.0 {
            run_liquidation_scan(&perp, current_price, &app_state.ws_tx, &app_state.db).await;
            last_liquidation_scan = Instant::now();
        }

        // Funding rate (every 8 hours)
        if last_funding_instant.elapsed() >= FUNDING_INTERVAL && current_price > 0.0 {
            // Mark price = orderbook mid (or last trade), Index price = Binance
            let mark = match app_state.engine.ticker().await {
                (_, _, Some(mid)) => mid.to_f64(),
                _ => current_price, // fallback to index if no orderbook
            };
            let rate = compute_funding_rate(mark, current_price);
            let fp8_rate = float_to_fp8_string(rate);
            match perp.apply_funding(&fp8_rate, now_ts).await {
                Ok(_) => {
                    info!(rate = %fp8_rate, "applied funding rate");
                    let rate_raw = crate::types::FP8::from_f64(rate).raw();
                    let mark_raw = app_state.mark_price.load(Ordering::Relaxed);
                    let index_raw = crate::types::FP8::from_f64(current_price).raw();
                    app_state.funding_rate.store(rate_raw, Ordering::Relaxed);
                    app_state.last_funding_time.store(now_ts, Ordering::Relaxed);
                    if let Some(db) = &app_state.db {
                        db.insert_funding_event(rate_raw, mark_raw, index_raw, now_ts).await;
                    }
                }
                Err(e) => error!("funding application failed: {}", e),
            }
            last_funding_instant = Instant::now();
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
