//! P2P layer for order flow replication between operators.
//!
//! Uses libp2p gossipsub:
//! - Sequencer publishes order batches
//! - Validators subscribe and replay deterministically
//! - Any operator can request cross-signing via signing relay
//!
//! Topics: "perp-dex/orders", "perp-dex/election", "perp-dex/signing"

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use std::path::Path;

use anyhow::{Context, Result};
use libp2p::{
    futures::StreamExt,
    gossipsub, identify,
    identity::Keypair,
    noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::election::ElectionMessage;
use crate::pool_path_a_client::ShareEnvelopeV2;

/// Load a libp2p Ed25519 identity from `path` if it exists, otherwise
/// generate a fresh one and persist it.
///
/// File format: protobuf-encoded keypair as produced by
/// `Keypair::to_protobuf_encoding()`. The file is created with mode 0600 to
/// keep the private key out of casual reach.
pub fn load_or_create_identity(path: &Path) -> Result<Keypair> {
    if let Ok(bytes) = std::fs::read(path) {
        match Keypair::from_protobuf_encoding(&bytes) {
            Ok(kp) => {
                info!(path = %path.display(), "loaded persistent libp2p identity");
                return Ok(kp);
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "existing identity file is corrupt — generating a new one"
                );
            }
        }
    }
    let kp = Keypair::generate_ed25519();
    let encoded = kp
        .to_protobuf_encoding()
        .context("failed to encode generated keypair")?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    std::fs::write(path, &encoded)
        .with_context(|| format!("failed to write identity to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    info!(path = %path.display(), "generated new persistent libp2p identity");
    Ok(kp)
}

// ── Message types ───────────────────────────────────────────────

/// Order batch published by sequencer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBatch {
    /// Monotonically increasing sequence number.
    pub seq_num: u64,
    /// Orders in this batch.
    pub orders: Vec<OrderMessage>,
    /// SHA-256 of state after applying this batch.
    pub state_hash: String,
    /// Unix timestamp (seconds).
    pub timestamp: u64,
    /// Sequencer's peer ID (for verification).
    pub sequencer_id: String,
}

/// Single order within a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderMessage {
    pub order_id: u64,
    pub user_id: String,
    pub side: String,
    pub order_type: String,
    pub price: String,
    pub size: String,
    pub leverage: u32,
    pub status: String,
    /// Fills produced by this order.
    pub fills: Vec<FillMessage>,
}

/// Fill (trade) produced by matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillMessage {
    pub trade_id: u64,
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub maker_user_id: String,
    pub price: String,
    pub size: String,
    pub taker_side: String,
}

// ── Signing relay messages ──────────────────────────────────────

/// Messages for cross-operator signing via P2P.
/// Replaces direct HTTP calls to remote enclaves — enclave stays localhost-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SigningMessage {
    Request {
        request_id: String,
        requester_peer_id: String,
        hash_hex: String,
        signer_xrpl_address: String,
    },
    Response {
        request_id: String,
        signer_xrpl_address: String,
        der_signature: Option<String>,
        compressed_pubkey: Option<String>,
        error: Option<String>,
    },
}

/// Events broadcast by sequencer for validator PG replication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateEvent {
    Deposit {
        user_id: String,
        amount: String,
        tx_hash: String,
        ledger_index: u32,
    },
    Funding {
        rate_raw: i64,
        mark_raw: i64,
        index_raw: i64,
        timestamp: u64,
        payments: Vec<FundingPayment>,
    },
    Liquidation {
        position_id: u64,
        user_id: String,
        price: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingPayment {
    pub user_id: String,
    pub position_id: i64,
    pub side: String,
    pub payment: i64,
}

/// Outbound signing request from withdrawal module to P2P.
#[derive(Debug)]
pub struct SigningRelay {
    pub request_id: String,
    pub hash_hex: String,
    pub signer_xrpl_address: String,
    pub response_tx: tokio::sync::oneshot::Sender<SigningMessage>,
}

/// Local signer credentials — used to handle incoming signing requests.
#[derive(Debug, Clone)]
pub struct LocalSigner {
    pub enclave_url: String,
    pub address: String,
    pub session_key: String,
    pub compressed_pubkey: String,
    pub xrpl_address: String,
}

// ── Path A: peer DCAP quote exchange ────────────────────────────

/// Path A peer-quote announcement. Published by each operator on ECDH
/// identity load/rotation and re-broadcast periodically (attest cache TTL
/// is 5 min → re-announce every ~4 min). Receivers pass `quote_hex` +
/// `peer_pubkey_hex` to `/v1/pool/attest/verify-peer-quote`; success
/// populates the local enclave's attest cache so subsequent v2 share
/// export/import requests for this peer succeed.
///
/// All hex is lowercase with no `0x` prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerQuoteMessage {
    Announce {
        /// 33-byte compressed secp256k1 ECDH identity pubkey.
        peer_pubkey: String,
        /// Shard identity this quote binds to.
        shard_id: u32,
        /// 32-byte FROST group_id this quote binds to.
        group_id: String,
        /// Raw DCAP quote bytes.
        quote: String,
        /// Announcement wall-clock (sender side). Used only for staleness
        /// log filtering; the enclave uses its own `now_ts` on verify.
        timestamp: u64,
    },
}

// ── Path A: v2 FROST share transport ────────────────────────────

/// Path A targeted delivery of an ECDH+AES-GCM-sealed FROST share envelope.
/// The ciphertext is already AEAD-bound to `recipient_pubkey`; peers whose
/// local ECDH pubkey does not match drop the message silently, and matching
/// peers forward to the local enclave via
/// `POST /v1/pool/frost/share-import-v2`.
///
/// `recipient_pubkey` is a broadcast-filter hint only — security comes
/// from the AEAD + sender attest-cache check the enclave performs on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShareEnvelopeV2Message {
    Deliver {
        /// 33-byte compressed ECDH pubkey of the intended recipient.
        recipient_pubkey: String,
        /// Shard this share belongs to.
        shard_id: u32,
        /// 32-byte FROST group_id.
        group_id: String,
        /// FROST signer_id the share corresponds to.
        signer_id: u32,
        /// Sealed envelope as returned by
        /// `POST /v1/pool/frost/share-export-v2`.
        envelope: ShareEnvelopeV2,
    },
}

// ── Network behaviour ───────────────────────────────────────────

const ORDERS_TOPIC: &str = "perp-dex/orders";
const ELECTION_TOPIC: &str = "perp-dex/election";
const SIGNING_TOPIC: &str = "perp-dex/signing";
const EVENTS_TOPIC: &str = "perp-dex/events";
const PEER_QUOTE_TOPIC: &str = "perp-dex/path-a/peer-quote";
const SHARE_V2_TOPIC: &str = "perp-dex/path-a/share-v2";

#[derive(NetworkBehaviour)]
struct PerpBehaviour {
    gossipsub: gossipsub::Behaviour,
    identify: identify::Behaviour,
}

// ── P2P Node ────────────────────────────────────────────────────

pub struct P2PNode {
    swarm: Swarm<PerpBehaviour>,
    orders_topic: gossipsub::IdentTopic,
    election_topic: gossipsub::IdentTopic,
    signing_topic: gossipsub::IdentTopic,
    events_topic: gossipsub::IdentTopic,
    peer_quote_topic: gossipsub::IdentTopic,
    share_v2_topic: gossipsub::IdentTopic,
    /// Channel to send received batches to the orchestrator (validator).
    batch_tx: mpsc::Sender<OrderBatch>,
    /// Channel to receive batches to publish (sequencer).
    publish_rx: Option<mpsc::Receiver<OrderBatch>>,
    /// Election messages received from gossipsub → forwarded to election module.
    election_inbound_tx: mpsc::Sender<ElectionMessage>,
    /// Election messages to publish via gossipsub.
    election_outbound_rx: Option<mpsc::Receiver<ElectionMessage>>,
    /// Outbound signing requests from withdrawal module.
    signing_request_rx: Option<mpsc::Receiver<SigningRelay>>,
    /// In-flight signing requests waiting for P2P responses.
    pending_signing: HashMap<String, tokio::sync::oneshot::Sender<SigningMessage>>,
    /// Channel for outbound state events (sequencer publishes).
    events_publish_rx: Option<mpsc::Receiver<StateEvent>>,
    /// Channel for received state events (validator consumes).
    events_inbound_tx: Option<mpsc::Sender<StateEvent>>,
    /// Path A: outbound peer-quote announcements (published by local periodic task).
    peer_quote_publish_rx: Option<mpsc::Receiver<PeerQuoteMessage>>,
    /// Path A: received peer-quote announcements forwarded to verifier task.
    peer_quote_inbound_tx: Option<mpsc::Sender<PeerQuoteMessage>>,
    /// Path A: outbound v2 share envelopes (published by share-export task).
    share_v2_publish_rx: Option<mpsc::Receiver<ShareEnvelopeV2Message>>,
    /// Path A: received share envelopes forwarded to import task
    /// (only messages matching `local_ecdh_pubkey` are delivered — hint-only).
    share_v2_inbound_tx: Option<mpsc::Sender<ShareEnvelopeV2Message>>,
    /// Path A: local ECDH pubkey hex (33B lowercase) used as the recipient
    /// filter on the v2 share topic. `None` = forward every received message.
    local_ecdh_pubkey: Option<String>,
    /// Local signer credentials for handling incoming signing requests.
    local_signer: Option<LocalSigner>,
    /// Our peer ID.
    pub peer_id: PeerId,
    /// Shared counter of connected peers (read by health endpoint).
    peer_count: Arc<std::sync::atomic::AtomicU32>,
}

impl P2PNode {
    /// Create a new P2P node with the given libp2p identity.
    ///
    /// `listen_addr`: e.g., "/ip4/0.0.0.0/tcp/4001"
    /// `keypair`:     persistent identity (use [`load_or_create_identity`])
    pub async fn new(
        listen_addr: &str,
        keypair: Keypair,
        batch_tx: mpsc::Sender<OrderBatch>,
        election_inbound_tx: mpsc::Sender<ElectionMessage>,
        peer_count: Arc<std::sync::atomic::AtomicU32>,
    ) -> Result<Self> {
        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|key| {
                // Gossipsub config
                let message_id_fn = |message: &gossipsub::Message| {
                    let mut hasher = DefaultHasher::new();
                    message.data.hash(&mut hasher);
                    gossipsub::MessageId::from(hasher.finish().to_string())
                };

                let gossipsub_config = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(5))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .message_id_fn(message_id_fn)
                    .build()
                    .expect("valid gossipsub config");

                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossipsub_config,
                )
                .expect("valid gossipsub behaviour");

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/perp-dex/0.1.0".to_string(),
                    key.public(),
                ));

                PerpBehaviour {
                    gossipsub,
                    identify,
                }
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let peer_id = *swarm.local_peer_id();
        let orders_topic = gossipsub::IdentTopic::new(ORDERS_TOPIC);
        let election_topic = gossipsub::IdentTopic::new(ELECTION_TOPIC);
        let signing_topic = gossipsub::IdentTopic::new(SIGNING_TOPIC);
        let events_topic = gossipsub::IdentTopic::new(EVENTS_TOPIC);
        let peer_quote_topic = gossipsub::IdentTopic::new(PEER_QUOTE_TOPIC);
        let share_v2_topic = gossipsub::IdentTopic::new(SHARE_V2_TOPIC);

        let mut node = P2PNode {
            swarm,
            orders_topic,
            election_topic,
            signing_topic,
            events_topic,
            peer_quote_topic,
            share_v2_topic,
            batch_tx,
            publish_rx: None,
            election_inbound_tx,
            election_outbound_rx: None,
            signing_request_rx: None,
            pending_signing: HashMap::new(),
            events_publish_rx: None,
            events_inbound_tx: None,
            peer_quote_publish_rx: None,
            peer_quote_inbound_tx: None,
            share_v2_publish_rx: None,
            share_v2_inbound_tx: None,
            local_ecdh_pubkey: None,
            local_signer: None,
            peer_id,
            peer_count,
        };

        // Subscribe to topics
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.orders_topic)
            .context("failed to subscribe to orders topic")?;
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.election_topic)
            .context("failed to subscribe to election topic")?;
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.signing_topic)
            .context("failed to subscribe to signing topic")?;
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.events_topic)
            .context("failed to subscribe to events topic")?;
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.peer_quote_topic)
            .context("failed to subscribe to peer-quote topic")?;
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.share_v2_topic)
            .context("failed to subscribe to share-v2 topic")?;

        // Listen
        let addr: Multiaddr = listen_addr.parse().context("invalid listen address")?;
        node.swarm.listen_on(addr)?;

        info!(peer_id = %node.peer_id, "P2P node created");
        Ok(node)
    }

    /// Set publish channel (sequencer mode).
    pub fn set_publish_channel(&mut self, rx: mpsc::Receiver<OrderBatch>) {
        self.publish_rx = Some(rx);
    }

    /// Set election publish channel.
    pub fn set_election_publish_channel(&mut self, rx: mpsc::Receiver<ElectionMessage>) {
        self.election_outbound_rx = Some(rx);
    }

    /// Set signing request channel (withdrawal module sends requests here).
    pub fn set_signing_channel(&mut self, rx: mpsc::Receiver<SigningRelay>) {
        self.signing_request_rx = Some(rx);
    }

    /// Set events publish channel (sequencer sends events to broadcast).
    pub fn set_events_publish_channel(&mut self, rx: mpsc::Receiver<StateEvent>) {
        self.events_publish_rx = Some(rx);
    }

    /// Set events inbound channel (validator receives events to apply).
    pub fn set_events_inbound_channel(&mut self, tx: mpsc::Sender<StateEvent>) {
        self.events_inbound_tx = Some(tx);
    }

    /// Path A: set the channel a local periodic task uses to publish own
    /// peer-quote announcements onto gossipsub.
    pub fn set_peer_quote_publish_channel(&mut self, rx: mpsc::Receiver<PeerQuoteMessage>) {
        self.peer_quote_publish_rx = Some(rx);
    }

    /// Path A: set the channel received peer-quote announcements are
    /// forwarded to (consumer calls `/v1/pool/attest/verify-peer-quote`).
    pub fn set_peer_quote_inbound_channel(&mut self, tx: mpsc::Sender<PeerQuoteMessage>) {
        self.peer_quote_inbound_tx = Some(tx);
    }

    /// Path A: set the channel a local export task uses to publish v2 share
    /// envelopes destined for a specific recipient peer.
    pub fn set_share_v2_publish_channel(&mut self, rx: mpsc::Receiver<ShareEnvelopeV2Message>) {
        self.share_v2_publish_rx = Some(rx);
    }

    /// Path A: set the channel received share envelopes addressed to us are
    /// forwarded to (consumer calls `/v1/pool/frost/share-import-v2`).
    pub fn set_share_v2_inbound_channel(&mut self, tx: mpsc::Sender<ShareEnvelopeV2Message>) {
        self.share_v2_inbound_tx = Some(tx);
    }

    /// Path A: set our local ECDH pubkey (33-byte compressed, lowercase hex).
    /// Used as the recipient filter on the v2 share topic — messages whose
    /// `recipient_pubkey` doesn't match are dropped before forwarding.
    pub fn set_local_ecdh_pubkey(&mut self, pk_hex: String) {
        info!(ecdh_pubkey = %pk_hex, "P2P: local ECDH pubkey configured");
        self.local_ecdh_pubkey = Some(pk_hex.to_lowercase());
    }

    /// Set local signer credentials for handling incoming signing requests.
    pub fn set_local_signer(&mut self, signer: LocalSigner) {
        info!(xrpl_addr = %signer.xrpl_address, "P2P signing relay: local signer configured");
        self.local_signer = Some(signer);
    }

    /// Connect to a peer (bootstrap).
    pub fn dial(&mut self, addr: &str) -> Result<()> {
        let multiaddr: Multiaddr = addr.parse().context("invalid peer address")?;
        self.swarm.dial(multiaddr)?;
        Ok(())
    }

    /// Publish an order batch (sequencer only).
    pub fn publish_batch(&mut self, batch: &OrderBatch) -> Result<()> {
        let data = serde_json::to_vec(batch).context("failed to serialize batch")?;
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.orders_topic.clone(), data)
            .map_err(|e| anyhow::anyhow!("publish failed: {e}"))?;
        Ok(())
    }

    fn publish_election(&mut self, msg: &ElectionMessage) -> Result<()> {
        let data = serde_json::to_vec(msg).context("failed to serialize election msg")?;
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.election_topic.clone(), data)
            .map_err(|e| anyhow::anyhow!("election publish failed: {e}"))?;
        Ok(())
    }

    fn publish_signing(&mut self, msg: &SigningMessage) -> Result<()> {
        let data = serde_json::to_vec(msg).context("failed to serialize signing msg")?;
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.signing_topic.clone(), data)
            .map_err(|e| anyhow::anyhow!("signing publish failed: {e}"))?;
        Ok(())
    }

    /// Handle an incoming signing request: sign with local enclave if we own the address.
    async fn handle_signing_request(
        local_signer: &LocalSigner,
        request_id: &str,
        hash_hex: &str,
    ) -> SigningMessage {
        let http = match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return SigningMessage::Response {
                    request_id: request_id.to_string(),
                    signer_xrpl_address: local_signer.xrpl_address.clone(),
                    der_signature: None,
                    compressed_pubkey: None,
                    error: Some(format!("http client: {e}")),
                };
            }
        };

        let sign_url = format!("{}/pool/sign", local_signer.enclave_url);
        let resp = http
            .post(&sign_url)
            .json(&serde_json::json!({
                "from": local_signer.address,
                "hash": hash_hex,
                "session_key": local_signer.session_key,
            }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                return SigningMessage::Response {
                    request_id: request_id.to_string(),
                    signer_xrpl_address: local_signer.xrpl_address.clone(),
                    der_signature: None,
                    compressed_pubkey: None,
                    error: Some(format!("enclave request: {e}")),
                };
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return SigningMessage::Response {
                    request_id: request_id.to_string(),
                    signer_xrpl_address: local_signer.xrpl_address.clone(),
                    der_signature: None,
                    compressed_pubkey: None,
                    error: Some(format!("enclave response parse: {e}")),
                };
            }
        };

        if body["status"].as_str() != Some("success") {
            return SigningMessage::Response {
                request_id: request_id.to_string(),
                signer_xrpl_address: local_signer.xrpl_address.clone(),
                der_signature: None,
                compressed_pubkey: None,
                error: Some(format!("enclave: {}", body.get("message").unwrap_or(&body))),
            };
        }

        let r_hex = body["signature"]["r"].as_str().unwrap_or("");
        let s_hex = body["signature"]["s"].as_str().unwrap_or("");
        let r_bytes = hex::decode(r_hex).unwrap_or_default();
        let s_bytes = hex::decode(s_hex).unwrap_or_default();
        let der = crate::xrpl_signer::der_encode_signature(&r_bytes, &s_bytes);

        SigningMessage::Response {
            request_id: request_id.to_string(),
            signer_xrpl_address: local_signer.xrpl_address.clone(),
            der_signature: Some(hex::encode_upper(&der)),
            compressed_pubkey: Some(local_signer.compressed_pubkey.to_uppercase()),
            error: None,
        }
    }

    /// Run the event loop. Call this in a tokio::spawn.
    pub async fn run(&mut self) {
        // Take channels out of self for use in select!
        let mut publish_rx = self.publish_rx.take();
        let mut election_rx = self.election_outbound_rx.take();
        let mut signing_rx = self.signing_request_rx.take();
        let mut events_rx = self.events_publish_rx.take();
        let mut peer_quote_rx = self.peer_quote_publish_rx.take();
        let mut share_v2_rx = self.share_v2_publish_rx.take();

        let orders_topic_hash = self.orders_topic.hash();
        let election_topic_hash = self.election_topic.hash();
        let signing_topic_hash = self.signing_topic.hash();
        let events_topic_hash = self.events_topic.hash();
        let peer_quote_topic_hash = self.peer_quote_topic.hash();
        let share_v2_topic_hash = self.share_v2_topic.hash();

        let mut signing_cleanup = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                // Handle publish requests from sequencer
                Some(batch) = async {
                    match &mut publish_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<OrderBatch>>().await,
                    }
                } => {
                    match self.publish_batch(&batch) {
                        Ok(_) => info!(
                            seq = batch.seq_num,
                            orders = batch.orders.len(),
                            "published batch via gossipsub"
                        ),
                        Err(e) => warn!("gossipsub publish failed: {}", e),
                    }
                }

                // Handle election messages to publish
                Some(msg) = async {
                    match &mut election_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<ElectionMessage>>().await,
                    }
                } => {
                    if let Err(e) = self.publish_election(&msg) {
                        tracing::debug!("election publish: {}", e);
                    }
                }

                // Handle signing relay requests from withdrawal module
                Some(relay) = async {
                    match &mut signing_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<SigningRelay>>().await,
                    }
                } => {
                    // If the request is for our own local signer, handle locally
                    // (gossipsub doesn't deliver messages back to the sender)
                    if let Some(ref local) = self.local_signer {
                        if local.xrpl_address == relay.signer_xrpl_address {
                            info!(
                                req_id = %relay.request_id,
                                "signing locally (own address)"
                            );
                            let response = Self::handle_signing_request(
                                local, &relay.request_id, &relay.hash_hex,
                            ).await;
                            let _ = relay.response_tx.send(response);
                            continue;
                        }
                    }

                    let msg = SigningMessage::Request {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        hash_hex: relay.hash_hex,
                        signer_xrpl_address: relay.signer_xrpl_address,
                    };
                    match self.publish_signing(&msg) {
                        Ok(_) => {
                            self.pending_signing.insert(relay.request_id, relay.response_tx);
                        }
                        Err(e) => {
                            warn!("signing publish failed: {}", e);
                            let _ = relay.response_tx.send(SigningMessage::Response {
                                request_id: "".into(),
                                signer_xrpl_address: "".into(),
                                der_signature: None,
                                compressed_pubkey: None,
                                error: Some(format!("P2P publish failed: {e}")),
                            });
                        }
                    }
                }

                // Publish state events (sequencer → validators)
                Some(event) = async {
                    match &mut events_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<StateEvent>>().await,
                    }
                } => {
                    if let Ok(data) = serde_json::to_vec(&event) {
                        if let Err(e) = self.swarm.behaviour_mut().gossipsub
                            .publish(self.events_topic.clone(), data) {
                            warn!("events publish failed: {}", e);
                        }
                    }
                }

                // Path A: publish own peer-quote announcement
                Some(msg) = async {
                    match &mut peer_quote_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<PeerQuoteMessage>>().await,
                    }
                } => {
                    if let Ok(data) = serde_json::to_vec(&msg) {
                        match self.swarm.behaviour_mut().gossipsub
                            .publish(self.peer_quote_topic.clone(), data) {
                            Ok(_) => {
                                let PeerQuoteMessage::Announce { ref peer_pubkey, shard_id, .. } = msg;
                                info!(
                                    peer_pubkey = %peer_pubkey,
                                    shard_id = shard_id,
                                    "published peer-quote announcement"
                                );
                            }
                            Err(e) => warn!("peer-quote publish failed: {}", e),
                        }
                    }
                }

                // Path A: publish v2 share envelope to targeted recipient
                Some(msg) = async {
                    match &mut share_v2_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<ShareEnvelopeV2Message>>().await,
                    }
                } => {
                    if let Ok(data) = serde_json::to_vec(&msg) {
                        match self.swarm.behaviour_mut().gossipsub
                            .publish(self.share_v2_topic.clone(), data) {
                            Ok(_) => {
                                let ShareEnvelopeV2Message::Deliver {
                                    ref recipient_pubkey, shard_id, signer_id, ..
                                } = msg;
                                info!(
                                    recipient_pubkey = %recipient_pubkey,
                                    shard_id = shard_id,
                                    signer_id = signer_id,
                                    "published v2 share envelope"
                                );
                            }
                            Err(e) => warn!("share-v2 publish failed: {}", e),
                        }
                    }
                }

                // Cleanup timed-out signing requests
                _ = signing_cleanup.tick() => {
                    // oneshot senders that are closed (receiver dropped) get cleaned up
                    self.pending_signing.retain(|_, tx| !tx.is_closed());
                }

                // Handle swarm events
                event = self.swarm.select_next_some() => {
            match event {
                SwarmEvent::Behaviour(PerpBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message {
                        propagation_source,
                        message,
                        ..
                    },
                )) => {
                    if message.topic == orders_topic_hash {
                        match serde_json::from_slice::<OrderBatch>(&message.data) {
                            Ok(batch) => {
                                info!(
                                    seq = batch.seq_num,
                                    orders = batch.orders.len(),
                                    from = %propagation_source,
                                    "received order batch"
                                );
                                if let Err(e) = self.batch_tx.send(batch).await {
                                    error!("failed to forward batch: {}", e);
                                }
                            }
                            Err(e) => {
                                warn!("invalid batch from {}: {}", propagation_source, e);
                            }
                        }
                    } else if message.topic == election_topic_hash {
                        match serde_json::from_slice::<ElectionMessage>(&message.data) {
                            Ok(msg) => {
                                if let Err(e) = self.election_inbound_tx.send(msg).await {
                                    error!("failed to forward election msg: {}", e);
                                }
                            }
                            Err(e) => {
                                warn!("invalid election msg from {}: {}", propagation_source, e);
                            }
                        }
                    } else if message.topic == signing_topic_hash {
                        match serde_json::from_slice::<SigningMessage>(&message.data) {
                            Ok(SigningMessage::Request {
                                request_id,
                                requester_peer_id,
                                hash_hex,
                                signer_xrpl_address,
                            }) => {
                                // Check if this request is for our local signer
                                if let Some(ref local) = self.local_signer {
                                    if local.xrpl_address == signer_xrpl_address {
                                        info!(
                                            req_id = %request_id,
                                            from = %requester_peer_id,
                                            "signing request received — signing locally"
                                        );
                                        let local = local.clone();
                                        let req_id = request_id.clone();
                                        let hash = hash_hex.clone();
                                        let response = Self::handle_signing_request(
                                            &local, &req_id, &hash,
                                        ).await;
                                        if let Err(e) = self.publish_signing(&response) {
                                            error!("failed to publish signing response: {}", e);
                                        }
                                    }
                                }
                            }
                            Ok(SigningMessage::Response {
                                request_id,
                                ..
                            }) => {
                                // Deliver to waiting withdrawal task
                                if let Some(tx) = self.pending_signing.remove(&request_id) {
                                    if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                        let _ = tx.send(msg);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("invalid signing msg from {}: {}", propagation_source, e);
                            }
                        }
                    } else if message.topic == events_topic_hash {
                        match serde_json::from_slice::<StateEvent>(&message.data) {
                            Ok(event) => {
                                if let Some(ref tx) = self.events_inbound_tx {
                                    if let Err(e) = tx.send(event).await {
                                        error!("failed to forward state event: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("invalid state event from {}: {}", propagation_source, e);
                            }
                        }
                    } else if message.topic == peer_quote_topic_hash {
                        match serde_json::from_slice::<PeerQuoteMessage>(&message.data) {
                            Ok(msg) => {
                                if let Some(ref tx) = self.peer_quote_inbound_tx {
                                    if let Err(e) = tx.send(msg).await {
                                        error!("failed to forward peer-quote: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("invalid peer-quote from {}: {}", propagation_source, e);
                            }
                        }
                    } else if message.topic == share_v2_topic_hash {
                        match serde_json::from_slice::<ShareEnvelopeV2Message>(&message.data) {
                            Ok(msg) => {
                                // Recipient filter: if we know our ECDH pubkey,
                                // silently drop envelopes addressed to others.
                                let ShareEnvelopeV2Message::Deliver {
                                    ref recipient_pubkey, ..
                                } = msg;
                                if let Some(ref local_pk) = self.local_ecdh_pubkey {
                                    if recipient_pubkey.to_lowercase() != *local_pk {
                                        continue;
                                    }
                                }
                                if let Some(ref tx) = self.share_v2_inbound_tx {
                                    if let Err(e) = tx.send(msg).await {
                                        error!("failed to forward share-v2: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("invalid share-v2 from {}: {}", propagation_source, e);
                            }
                        }
                    }
                }
                SwarmEvent::Behaviour(PerpBehaviourEvent::Identify(identify::Event::Received {
                    peer_id,
                    info,
                    ..
                })) => {
                    info!(
                        peer = %peer_id,
                        protocol = %info.protocol_version,
                        "peer identified"
                    );
                    for addr in info.listen_addrs {
                        self.swarm
                            .behaviour_mut()
                            .gossipsub
                            .add_explicit_peer(&peer_id);
                        info!(peer = %peer_id, addr = %addr, "added gossipsub peer");
                    }
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    info!(addr = %address, "listening on");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    self.peer_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    info!(peer = %peer_id, peers = self.peer_count.load(std::sync::atomic::Ordering::Relaxed), "connected");
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    self.peer_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    warn!(peer = %peer_id, peers = self.peer_count.load(std::sync::atomic::Ordering::Relaxed), "disconnected");
                }
                _ => {}
            }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_batch_serialization() {
        let batch = OrderBatch {
            seq_num: 1,
            orders: vec![OrderMessage {
                order_id: 42,
                user_id: "rAlice".into(),
                side: "long".into(),
                order_type: "limit".into(),
                price: "0.55000000".into(),
                size: "100.00000000".into(),
                leverage: 5,
                status: "filled".into(),
                fills: vec![FillMessage {
                    trade_id: 1,
                    maker_order_id: 10,
                    taker_order_id: 42,
                    maker_user_id: "rBob".into(),
                    price: "0.55000000".into(),
                    size: "100.00000000".into(),
                    taker_side: "long".into(),
                }],
            }],
            state_hash: "abc123".into(),
            timestamp: 1743500000,
            sequencer_id: "12D3KooW...".into(),
        };

        let json = serde_json::to_string(&batch).unwrap();
        let decoded: OrderBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.seq_num, 1);
        assert_eq!(decoded.orders.len(), 1);
        assert_eq!(decoded.orders[0].fills.len(), 1);
        assert_eq!(decoded.sequencer_id, "12D3KooW...");
    }

    #[test]
    fn sequencer_id_preserved_in_batch() {
        let batch = OrderBatch {
            seq_num: 42,
            orders: vec![],
            state_hash: "hash".into(),
            timestamp: 0,
            sequencer_id: "/ip4/0.0.0.0/tcp/4001:p0".into(),
        };
        let json = serde_json::to_string(&batch).unwrap();
        let decoded: OrderBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sequencer_id, "/ip4/0.0.0.0/tcp/4001:p0");
        assert!(!decoded.sequencer_id.is_empty());
    }
}
