//! Perp DEX Orchestrator — main entry point.
//!
//! Two concurrent tasks:
//!   1. API server (axum) — accepts orders from users
//!   2. Background loop — price feeds, deposit monitoring, liquidations, funding

mod api;
mod auth;
mod cli_tools;
mod commitment;
mod db;
mod election;
mod http_helpers;
mod orderbook;
mod p2p;
mod path_a_redkg;
mod perp_client;
mod pool_path_a_client;
mod price_feed;
mod rate_limit;
pub mod shard_router;
mod singleton;
mod trading;
mod types;
mod vault_mm;
mod withdrawal;
mod ws;
mod xrpl_monitor;
mod xrpl_signer;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate a new signer identity from an SGX enclave.
    OperatorSetup {
        /// Enclave REST API base URL
        #[arg(long, default_value = "https://localhost:9088/v1")]
        enclave_url: String,
        /// Operator name (label for config files)
        #[arg(long)]
        name: String,
        /// Write signer entry JSON to this file
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Configure an XRPL escrow account with a 2-of-N SignerListSet.
    EscrowSetup {
        /// XRPL JSON-RPC URL
        #[arg(long, default_value = "https://s.altnet.rippletest.net:51234")]
        xrpl_url: String,
        /// Path to signers config JSON (must have "signers" array with xrpl_address)
        #[arg(long)]
        signers_config: PathBuf,
        /// XRPL escrow account seed (secret). Deprecated — prefer
        /// --escrow-seed-file because argv is visible to every user
        /// on the host via `ps`. O-L3.
        #[arg(long, conflicts_with = "escrow_seed_file")]
        escrow_seed: Option<String>,
        /// Path to a 0600-mode file containing the escrow seed on its
        /// first line. Preferred over --escrow-seed. O-L3.
        #[arg(long)]
        escrow_seed_file: Option<PathBuf>,
        /// Escrow r-address (optional — skips derivation from seed, needed for Ed25519 sEd seeds)
        #[arg(long)]
        escrow_address: Option<String>,
        /// Disable master key after setting SignerList (irreversible without multisig!)
        #[arg(long)]
        disable_master: bool,
    },

    /// Generate a signed curl command for any API endpoint.
    SignRequest {
        /// XRPL secp256k1 seed (secret)
        #[arg(long)]
        seed: String,
        /// HTTP method (GET, POST, DELETE)
        #[arg(long, default_value = "POST")]
        method: String,
        /// Full URL to sign against
        #[arg(long)]
        url: String,
        /// JSON request body (optional)
        #[arg(long)]
        body: Option<String>,
    },

    /// Submit an authenticated withdrawal request.
    Withdraw {
        /// API server URL
        #[arg(long, default_value = "http://localhost:3000")]
        api: String,
        /// XRPL secp256k1 seed (secret)
        #[arg(long)]
        seed: String,
        /// Withdrawal amount (FP8 string, e.g. "1.00000000")
        #[arg(long)]
        amount: String,
        /// Destination XRPL r-address
        #[arg(long)]
        destination: String,
        /// XRPL DestinationTag (required for exchange addresses)
        #[arg(long)]
        destination_tag: Option<u32>,
    },

    /// Query account balance with authentication.
    Balance {
        /// API server URL
        #[arg(long, default_value = "http://localhost:3000")]
        api: String,
        /// XRPL secp256k1 seed (secret)
        #[arg(long)]
        seed: String,
    },

    /// Create a signers_config.json from multiple operator entry files.
    ConfigInit {
        /// Operator entry JSON files (from operator-setup --output)
        #[arg(long, required = true, num_args = 1..)]
        entries: Vec<PathBuf>,
        /// XRPL escrow account r-address
        #[arg(long)]
        escrow_address: String,
        /// Multisig quorum (e.g. 2 for 2-of-3)
        #[arg(long, default_value_t = 2)]
        quorum: u32,
        /// Output config file path
        #[arg(long, default_value = "signers_config.json")]
        output: PathBuf,
    },

    /// Add a new operator to an existing signers_config.json.
    OperatorAdd {
        /// Enclave REST API base URL of the new operator
        #[arg(long)]
        enclave_url: String,
        /// Operator name
        #[arg(long)]
        name: String,
        /// Path to existing signers_config.json
        #[arg(long)]
        config: PathBuf,
        /// XRPL JSON-RPC URL (if set, re-submits SignerListSet)
        #[arg(long)]
        xrpl_url: Option<String>,
        /// Escrow seed (deprecated — use --escrow-seed-file). Required
        /// if --xrpl-url is set, to re-submit SignerListSet. O-L3.
        #[arg(long, conflicts_with = "escrow_seed_file")]
        escrow_seed: Option<String>,
        /// Path to a 0600-mode file containing the escrow seed on its
        /// first line. Preferred over --escrow-seed. O-L3.
        #[arg(long)]
        escrow_seed_file: Option<PathBuf>,
    },
}

#[derive(Parser, Debug)]
struct RunArgs {
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
    #[arg(long, default_value = "XRP-USD-PERP")]
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

    /// Path to signers config JSON for multisig withdrawals. The file must
    /// contain `{"signers": [...], "quorum": 2}` with each signer's
    /// enclave_url, address, session_key, compressed_pubkey, xrpl_address.
    /// If not set, withdrawals fall back to single-operator mode.
    #[arg(long)]
    signers_config: Option<PathBuf>,

    /// Enable the Market Making Vault (automated liquidity provider).
    /// The vault deposits initial margin and continuously quotes bid/ask
    /// around the mark price on the CLOB.
    #[arg(long)]
    vault_mm: bool,

    /// Vault MM half-spread (fraction, e.g. 0.0025 = 0.25% each side).
    #[arg(long, default_value_t = 0.0025)]
    vault_mm_spread: f64,

    /// Vault MM order size per level (FP8 string).
    #[arg(long, default_value = "100.00000000")]
    vault_mm_size: String,

    /// Vault MM number of price levels per side.
    #[arg(long, default_value_t = 3)]
    vault_mm_levels: usize,

    /// O-M5: cap on aggregate vault inventory (XRP). Pauses quoting
    /// when gross inventory (MM) or |net delta| (DN) would exceed this.
    /// Guards against unbounded pyramiding on a one-sided sweep.
    #[arg(long, default_value_t = 50.0)]
    vault_mm_max_inventory: f64,

    /// Enable Delta Neutral vault (quotes both sides, biases to reduce net delta).
    #[arg(long)]
    vault_dn: bool,

    /// Path to shards.toml config. If not set, a single-shard router is
    /// created from --enclave-url with shard_id=0.
    #[arg(long)]
    shards_config: Option<PathBuf>,

    /// Local-only admin HTTP listener for the Path A re-DKG share-v2
    /// export driver. Accepts `POST /admin/path-a/share-export`. Must
    /// bind to a loopback address; remote bind is rejected at startup.
    /// Defaults to off — the admin surface only exists when set.
    #[arg(long)]
    admin_listen: Option<String>,
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
    events_tx: &tokio::sync::mpsc::Sender<p2p::StateEvent>,
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
                    let _ = events_tx.try_send(p2p::StateEvent::Liquidation {
                        position_id: pos_id,
                        user_id: user.to_string(),
                        price: current_price,
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::OperatorSetup {
            enclave_url,
            name,
            output,
        }) => {
            return cli_tools::operator_setup(&enclave_url, &name, output.as_deref()).await;
        }
        Some(Command::EscrowSetup {
            xrpl_url,
            signers_config,
            escrow_seed,
            escrow_seed_file,
            escrow_address,
            disable_master,
        }) => {
            let seed = cli_tools::resolve_escrow_seed(
                escrow_seed.as_deref(),
                escrow_seed_file.as_deref(),
            )?;
            return cli_tools::escrow_setup(
                &xrpl_url,
                &signers_config,
                &seed,
                escrow_address.as_deref(),
                disable_master,
            )
            .await;
        }
        Some(Command::SignRequest {
            seed,
            method,
            url,
            body,
        }) => {
            return cli_tools::sign_request(&seed, &method, &url, body.as_deref()).await;
        }
        Some(Command::Withdraw {
            api,
            seed,
            amount,
            destination,
            destination_tag,
        }) => {
            return cli_tools::cli_withdraw(&api, &seed, &amount, &destination, destination_tag)
                .await;
        }
        Some(Command::Balance { api, seed }) => {
            return cli_tools::cli_balance(&api, &seed).await;
        }
        Some(Command::ConfigInit {
            entries,
            escrow_address,
            quorum,
            output,
        }) => {
            return cli_tools::config_init(&entries, &escrow_address, quorum, &output).await;
        }
        Some(Command::OperatorAdd {
            enclave_url,
            name,
            config,
            xrpl_url,
            escrow_seed,
            escrow_seed_file,
        }) => {
            // O-L3: resolve seed from --escrow-seed-file when provided.
            // Both flags are optional for operator_add (xrpl_url may be
            // unset, in which case no seed is needed); only validate
            // when at least one was given.
            let seed = if escrow_seed.is_some() || escrow_seed_file.is_some() {
                Some(cli_tools::resolve_escrow_seed(
                    escrow_seed.as_deref(),
                    escrow_seed_file.as_deref(),
                )?)
            } else {
                None
            };
            return cli_tools::operator_add(
                &enclave_url,
                &name,
                &config,
                xrpl_url.as_deref(),
                seed.as_deref(),
            )
            .await;
        }
        None => {
            // Default: run orchestrator with flattened RunArgs
        }
    }

    let cli = cli.run;
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

    // Initialize shard router — sets shard_id on each enclave at startup
    let shard_router = match &cli.shards_config {
        Some(path) => shard_router::ShardRouter::from_config(path).await?,
        None => shard_router::ShardRouter::single(&cli.enclave_url, 0).await?,
    };
    let shard_router = Arc::new(shard_router);

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

    // Load multisig signers config (optional — without it, withdrawals are disabled)
    let signers_config = match &cli.signers_config {
        Some(path) => {
            let data = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read signers config: {}", path.display()))?;
            let mut cfg: withdrawal::SignersConfig =
                serde_json::from_str(&data).context("invalid signers config JSON")?;
            if cfg.escrow_address.is_empty() {
                cfg.escrow_address = escrow_address.clone();
            }
            info!(
                signers = cfg.signers.len(),
                quorum = cfg.quorum,
                "loaded multisig signers config"
            );
            Some(cfg)
        }
        None => {
            info!("no --signers-config, multisig withdrawals disabled");
            None
        }
    };

    // Create P2P signing relay channel
    let (signing_tx, signing_rx) = tokio::sync::mpsc::channel::<p2p::SigningRelay>(32);

    let peer_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

    let maintenance_mode = Arc::new(std::sync::atomic::AtomicBool::new(matches!(
        std::env::var("PERP_MAINTENANCE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )));

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
        signers_config: signers_config.clone(),
        signing_tx: if signers_config.is_some() {
            Some(signing_tx)
        } else {
            None
        },
        db: db.clone(),
        shard_router: shard_router.clone(),
        peer_count: peer_count.clone(),
        start_time: Instant::now(),
        maintenance_mode: maintenance_mode.clone(),
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
    let mut p2p_node = p2p::P2PNode::new(
        &cli.p2p_listen,
        p2p_keypair,
        batch_tx,
        election_inbound_tx,
        peer_count.clone(),
    )
    .await
    .context("failed to start P2P node")?;

    // Wire P2P publishing channels
    let (pub_tx, pub_rx) = tokio::sync::mpsc::channel::<p2p::OrderBatch>(100);
    p2p_node.set_publish_channel(pub_rx);

    let (election_outbound_tx, election_outbound_rx) =
        tokio::sync::mpsc::channel::<election::ElectionMessage>(100);
    p2p_node.set_election_publish_channel(election_outbound_rx);

    // Wire P2P state events replication
    let (events_pub_tx, events_pub_rx) = tokio::sync::mpsc::channel::<p2p::StateEvent>(256);
    let (events_inbound_tx, mut events_inbound_rx) =
        tokio::sync::mpsc::channel::<p2p::StateEvent>(256);
    p2p_node.set_events_publish_channel(events_pub_rx);
    p2p_node.set_events_inbound_channel(events_inbound_tx);

    // Wire Path A (peer DCAP attestation + v2 FROST share transport).
    // Publish senders are held for future drivers (periodic announce,
    // re-DKG share export); inbound receivers drive the verify / import
    // tasks below.
    let (peer_quote_pub_tx, peer_quote_pub_rx) =
        tokio::sync::mpsc::channel::<p2p::PeerQuoteMessage>(32);
    let (peer_quote_in_tx, mut peer_quote_in_rx) =
        tokio::sync::mpsc::channel::<p2p::PeerQuoteMessage>(32);
    p2p_node.set_peer_quote_publish_channel(peer_quote_pub_rx);
    p2p_node.set_peer_quote_inbound_channel(peer_quote_in_tx);

    let (share_v2_pub_tx, share_v2_pub_rx) =
        tokio::sync::mpsc::channel::<p2p::ShareEnvelopeV2Message>(32);
    let (share_v2_in_tx, mut share_v2_in_rx) =
        tokio::sync::mpsc::channel::<p2p::ShareEnvelopeV2Message>(32);
    p2p_node.set_share_v2_publish_channel(share_v2_pub_rx);
    p2p_node.set_share_v2_inbound_channel(share_v2_in_tx);

    // Best-effort: fetch local ECDH identity pubkey to drive the share-v2
    // recipient filter. Non-fatal — older enclaves or SGX-less dev boxes
    // will simply have Path A unreachable.
    let ecdh_bootstrap = pool_path_a_client::PoolPathAClient::new(&cli.enclave_url)?;
    match ecdh_bootstrap.ecdh_pubkey().await {
        Ok(pk) => {
            info!(ecdh_pubkey = %pk, "fetched local ECDH identity for Path A");
            p2p_node.set_local_ecdh_pubkey(pk);
        }
        Err(e) => {
            warn!(
                "Path A: failed to fetch local ECDH pubkey ({}) — \
                 recipient filter disabled, share-v2 import will still \
                 be enforced by the enclave attest cache",
                e
            );
        }
    }

    // Spawn Path A peer-quote verifier — received announcements are
    // passed to /v1/pool/attest/verify-peer-quote to populate the
    // enclave's attest cache.
    let verifier_client = pool_path_a_client::PoolPathAClient::new(&cli.enclave_url)?;
    let _peer_quote_verifier = tokio::spawn(async move {
        while let Some(p2p::PeerQuoteMessage::Announce {
            peer_pubkey,
            shard_id,
            group_id,
            quote,
            timestamp: _,
        }) = peer_quote_in_rx.recv().await
        {
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match verifier_client
                .attest_verify_peer_quote(&quote, &peer_pubkey, shard_id, &group_id, now_ts)
                .await
            {
                Ok(Some(mre)) => {
                    info!(peer_pubkey = %peer_pubkey, mrenclave = %mre, "verified peer quote")
                }
                Ok(None) => {
                    warn!(peer_pubkey = %peer_pubkey, "peer quote verification refused (403)")
                }
                Err(e) => {
                    warn!(peer_pubkey = %peer_pubkey, "peer quote verify error: {}", e)
                }
            }
        }
    });

    // Spawn Path A share-v2 importer — envelopes addressed to us (the
    // recipient filter already dropped the rest in p2p.rs) are passed to
    // /v1/pool/frost/share-import-v2.
    let importer_client = pool_path_a_client::PoolPathAClient::new(&cli.enclave_url)?;
    let _share_v2_importer = tokio::spawn(async move {
        while let Some(p2p::ShareEnvelopeV2Message::Deliver {
            recipient_pubkey: _,
            shard_id,
            group_id,
            signer_id,
            envelope,
        }) = share_v2_in_rx.recv().await
        {
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match importer_client
                .frost_share_import_v2(&envelope, shard_id, &group_id, signer_id, now_ts)
                .await
            {
                Ok(true) => info!(signer_id, shard_id, "imported v2 FROST share"),
                Ok(false) => warn!(signer_id, "v2 share import refused (403)"),
                Err(e) => warn!(signer_id, "v2 share import error: {}", e),
            }
        }
    });

    // Path A peer-quote announcer — one per shard with a configured FROST
    // group_id in shards.toml. Loops every PEER_QUOTE_INTERVAL_SECS, fetches
    // ECDH pubkey + report_data + DCAP quote, sends Announce. Attest-cache
    // TTL is 5 min, so the interval is set a minute under that.
    const PEER_QUOTE_INTERVAL_SECS: u64 = 240;
    for group in shard_router.path_a_groups() {
        let announcer_client = pool_path_a_client::PoolPathAClient::new(&group.enclave_url)?;
        let pub_tx = peer_quote_pub_tx.clone();
        let shard_id = group.shard_id;
        let group_id = group.group_id_hex.clone();
        let _announcer = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(PEER_QUOTE_INTERVAL_SECS));
            loop {
                tick.tick().await;
                let now_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let my_pk = match announcer_client.ecdh_pubkey().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(shard_id, "announcer ecdh_pubkey failed: {}", e);
                        continue;
                    }
                };
                let rd = match announcer_client.ecdh_report_data(shard_id, &group_id).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(shard_id, "announcer ecdh_report_data failed: {}", e);
                        continue;
                    }
                };
                let quote = match announcer_client.attestation_quote(&rd).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(shard_id, "announcer attestation_quote failed: {}", e);
                        continue;
                    }
                };
                let msg = p2p::PeerQuoteMessage::Announce {
                    peer_pubkey: my_pk,
                    shard_id,
                    group_id: group_id.clone(),
                    quote,
                    timestamp: now_ts,
                };
                if pub_tx.send(msg).await.is_err() {
                    warn!(shard_id, "announcer channel closed — exiting");
                    break;
                }
                info!(shard_id, group_id = %group_id, "queued peer-quote announce");
            }
        });
    }

    // Path A re-DKG share-v2 export driver — spawned only when the
    // operator opts in via --admin-listen. Binds loopback-only; the
    // export function it fronts is also callable as a library from any
    // future in-process driver (e.g., automated re-DKG orchestration).
    if let Some(admin_listen) = cli.admin_listen.clone() {
        let admin_state = Arc::new(path_a_redkg::AdminState {
            client: pool_path_a_client::PoolPathAClient::new(&cli.enclave_url)?,
            share_v2_pub_tx: share_v2_pub_tx.clone(),
            groups: shard_router.path_a_groups().to_vec(),
        });
        let _admin_handle = tokio::spawn(async move {
            if let Err(e) = path_a_redkg::spawn_admin_listener(admin_listen, admin_state).await {
                error!("Path A admin listener exited: {}", e);
            }
        });
    }
    // Keep the share-v2 publish sender alive even when the admin
    // listener is disabled — future in-process drivers (automated
    // re-DKG) will clone it. Dropping it would close the outbound
    // gossipsub arm in p2p.rs.
    let _share_v2_pub_tx = share_v2_pub_tx;
    // Drop the peer-quote publish sender handle here: any configured
    // announcer holds its own clone, and unconfigured shards never needed it.
    drop(peer_quote_pub_tx);

    // Validator: apply inbound state events to local PG
    let validator_events_db = db.clone();
    let _events_handle = tokio::spawn(async move {
        while let Some(event) = events_inbound_rx.recv().await {
            let Some(ref db) = validator_events_db else {
                continue;
            };
            match event {
                p2p::StateEvent::Deposit {
                    ref user_id,
                    ref amount,
                    ref tx_hash,
                    ledger_index,
                } => {
                    info!(user = %user_id, amount, "replicated deposit event");
                    db.insert_deposit(user_id, amount, tx_hash, ledger_index)
                        .await;
                }
                p2p::StateEvent::Funding {
                    rate_raw,
                    mark_raw,
                    index_raw,
                    timestamp,
                    ref payments,
                } => {
                    info!(rate = rate_raw, "replicated funding event");
                    db.insert_funding_event(rate_raw, mark_raw, index_raw, timestamp)
                        .await;
                    for p in payments {
                        db.insert_funding_payment(
                            &p.user_id,
                            p.position_id,
                            &p.side,
                            p.payment,
                            rate_raw,
                            mark_raw,
                            timestamp,
                        )
                        .await;
                    }
                }
                p2p::StateEvent::Liquidation {
                    position_id,
                    ref user_id,
                    price,
                } => {
                    info!(pos = position_id, user = %user_id, "replicated liquidation event");
                    db.insert_liquidation(position_id, user_id, price).await;
                }
            }
        }
    });

    // Wire P2P signing relay
    if let Some(ref cfg) = signers_config {
        p2p_node.set_signing_channel(signing_rx);
        if let Some(ref local) = cfg.local_signer {
            p2p_node.set_local_signer(p2p::LocalSigner {
                enclave_url: local.enclave_url.clone(),
                address: local.address.clone(),
                session_key: local.session_key.clone(),
                compressed_pubkey: local.compressed_pubkey.clone(),
                xrpl_address: local.xrpl_address.clone(),
            });
        } else if let Some(first) = cfg.signers.first() {
            p2p_node.set_local_signer(p2p::LocalSigner {
                enclave_url: cli.enclave_url.clone(),
                address: first.address.clone(),
                session_key: first.session_key.clone(),
                compressed_pubkey: first.compressed_pubkey.clone(),
                xrpl_address: first.xrpl_address.clone(),
            });
        }
        // X-C1: the escrow address in signers-config tells the P2P node
        // which Account a signing request is allowed to draw from. Without
        // it every incoming request would fail the policy check.
        if !cfg.escrow_address.is_empty() {
            p2p_node.set_escrow_address(cfg.escrow_address.clone());
        } else {
            warn!("X-C1: signers-config has no escrow_address — all incoming P2P signing requests will be rejected");
        }
    }

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
    // O-H3: leader-tracking watch channel, consumed by the validator
    // replay task to gate `batch.sequencer_id` against the currently
    // elected leader (replaces trust-on-first-use).
    let (leader_tx, leader_rx) = tokio::sync::watch::channel::<Option<String>>(None);
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
        leader_tx,
    );
    let _election_handle = tokio::spawn(async move {
        election_state.run().await;
    });

    // Clone role_rx for singletons before the watcher consumes it
    let role_rx_vault_mm = role_rx.clone();
    let role_rx_vault_dn = role_rx.clone();

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
    let validator_leader_rx = leader_rx.clone();
    let _validator_handle = tokio::spawn(async move {
        let mut last_seq: u64 = 0;
        // O-H2: per-sequencer mismatch counter. Exposed via tracing events
        // tagged `metric = "state_hash_mismatches_total"` so log scrapers
        // can alert on a compromised or buggy sequencer.
        let mut state_hash_mismatches: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        while let Some(batch) = batch_rx.recv().await {
            if is_seq_validator.load(Ordering::Relaxed) {
                continue; // sequencer doesn't replay its own batches
            }

            let total_fills: usize = batch.orders.iter().map(|o| o.fills.len()).sum();

            // O-H3: gate the batch source on the *elected* leader from the
            // election state machine — no TOFU on first-observed peer.
            // A rogue peer cannot lock itself in as sequencer by beating
            // the real leader to the first publish; the election channel
            // is the source of truth.
            let elected = validator_leader_rx.borrow().clone();
            match elected {
                Some(ref elected_id) if *elected_id == batch.sequencer_id => {
                    // elected leader matches sender → proceed
                }
                Some(ref elected_id) => {
                    warn!(
                        elected = %elected_id,
                        got = %batch.sequencer_id,
                        seq = batch.seq_num,
                        "batch from non-elected sequencer — ignoring"
                    );
                    continue;
                }
                None => {
                    warn!(
                        got = %batch.sequencer_id,
                        seq = batch.seq_num,
                        "no elected leader yet — ignoring batch"
                    );
                    continue;
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

            // O-H2: verify state_hash BEFORE any replay. A mismatch means
            // the batch contents the sequencer signed-off don't match what
            // we'd produce locally — replaying would corrupt validator
            // state and poison the trade-history PG row. Skip the batch
            // entirely and surface on a counter metric.
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
                    let count = state_hash_mismatches
                        .entry(batch.sequencer_id.clone())
                        .and_modify(|c| *c += 1)
                        .or_insert(1);
                    error!(
                        metric = "state_hash_mismatches_total",
                        sequencer_id = %batch.sequencer_id,
                        value = *count,
                        expected = %batch.state_hash,
                        computed = %local_hash,
                        seq = batch.seq_num,
                        "STATE HASH MISMATCH — sequencer may be compromised; skipping replay"
                    );
                    continue;
                }
                info!(seq = batch.seq_num, "state hash verified");
            }

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
                                "XRP-USD-PERP",
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

    // Market Making Vault — singleton (only runs on sequencer)
    let _vault_mm_singleton = if cli.vault_mm {
        let vault_config = vault_mm::VaultMmConfig {
            half_spread: cli.vault_mm_spread,
            order_size: cli.vault_mm_size.clone(),
            levels: cli.vault_mm_levels,
            strategy: vault_mm::VaultStrategy::MarketMaking,
            max_inventory: cli.vault_mm_max_inventory,
            ..Default::default()
        };
        vault_mm::seed_vault_deposit(&perp, &vault_config).await;
        let vault_state = app_state.clone();
        Some(singleton::spawn("vault-mm", role_rx_vault_mm, move || {
            let state = vault_state.clone();
            let cfg = vault_config.clone();
            async move { vault_mm::run_vault_mm(state, cfg).await }
        }))
    } else {
        None
    };

    // Delta Neutral Vault — singleton (only runs on sequencer)
    let _vault_dn_singleton = if cli.vault_dn {
        let vault_config = vault_mm::VaultMmConfig {
            user_id: "vault:dn".into(),
            half_spread: cli.vault_mm_spread,
            order_size: cli.vault_mm_size.clone(),
            levels: cli.vault_mm_levels,
            strategy: vault_mm::VaultStrategy::DeltaNeutral,
            max_delta: 500.0,
            max_inventory: cli.vault_mm_max_inventory,
            ..Default::default()
        };
        vault_mm::seed_vault_deposit(&perp, &vault_config).await;
        let vault_state = app_state.clone();
        Some(singleton::spawn("vault-dn", role_rx_vault_dn, move || {
            let state = vault_state.clone();
            let cfg = vault_config.clone();
            async move { vault_mm::run_vault_mm(state, cfg).await }
        }))
    } else {
        None
    };

    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tick.tick().await;

        // Sequencer-only work: price feed, deposits, liquidations, funding.
        // Validators only replay batches from the sequencer (handled above).
        if !is_sequencer.load(Ordering::Relaxed) {
            // Validators still save their own sealed state periodically
            if last_state_save.elapsed() >= STATE_SAVE_INTERVAL {
                if let Err(e) = perp.save_state().await {
                    warn!("state save failed: {}", e);
                }
                last_state_save = Instant::now();
            }
            continue;
        }

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
                    } else {
                        if let Some(db) = &app_state.db {
                            db.insert_deposit(
                                &deposit.sender,
                                &deposit.amount,
                                &deposit.tx_hash,
                                new_ledger,
                            )
                            .await;
                        }
                        let _ = events_pub_tx.try_send(p2p::StateEvent::Deposit {
                            user_id: deposit.sender.clone(),
                            amount: deposit.amount.clone(),
                            tx_hash: deposit.tx_hash.clone(),
                            ledger_index: new_ledger,
                        });
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
            run_liquidation_scan(
                &perp,
                current_price,
                &app_state.ws_tx,
                &app_state.db,
                &events_pub_tx,
            )
            .await;
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
                Ok(resp) => {
                    info!(rate = %fp8_rate, "applied funding rate");
                    let rate_raw = crate::types::FP8::from_f64(rate).raw();
                    let mark_raw = app_state.mark_price.load(Ordering::Relaxed);
                    let index_raw = crate::types::FP8::from_f64(current_price).raw();
                    app_state.funding_rate.store(rate_raw, Ordering::Relaxed);
                    app_state.last_funding_time.store(now_ts, Ordering::Relaxed);
                    let mut funding_payments = Vec::new();
                    if let Some(db) = &app_state.db {
                        db.insert_funding_event(rate_raw, mark_raw, index_raw, now_ts)
                            .await;
                        if let Some(payments) = resp.get("payments").and_then(|v| v.as_array()) {
                            for p in payments {
                                let user_id = p["user_id"].as_str().unwrap_or("");
                                let pos_id = p["position_id"].as_i64().unwrap_or(0);
                                let side = p["side"].as_str().unwrap_or("");
                                let payment = p["payment"]
                                    .as_str()
                                    .and_then(|s| s.parse::<crate::types::FP8>().ok())
                                    .map(|fp| fp.raw())
                                    .unwrap_or(0);
                                db.insert_funding_payment(
                                    user_id, pos_id, side, payment, rate_raw, mark_raw, now_ts,
                                )
                                .await;
                                funding_payments.push(p2p::FundingPayment {
                                    user_id: user_id.to_string(),
                                    position_id: pos_id,
                                    side: side.to_string(),
                                    payment,
                                });
                            }
                            info!(count = payments.len(), "persisted funding payments");
                        }
                    }
                    let _ = events_pub_tx.try_send(p2p::StateEvent::Funding {
                        rate_raw,
                        mark_raw,
                        index_raw,
                        timestamp: now_ts,
                        payments: funding_payments,
                    });
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
