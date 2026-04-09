//! P2P layer for order flow replication between operators.
//!
//! Uses libp2p gossipsub:
//! - Sequencer publishes order batches
//! - Validators subscribe and replay deterministically
//!
//! Topic: "perp-dex/orders"

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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

// ── Network behaviour ───────────────────────────────────────────

const ORDERS_TOPIC: &str = "perp-dex/orders";
const ELECTION_TOPIC: &str = "perp-dex/election";

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
    /// Channel to send received batches to the orchestrator (validator).
    batch_tx: mpsc::Sender<OrderBatch>,
    /// Channel to receive batches to publish (sequencer).
    publish_rx: Option<mpsc::Receiver<OrderBatch>>,
    /// Election messages received from gossipsub → forwarded to election module.
    election_inbound_tx: mpsc::Sender<ElectionMessage>,
    /// Election messages to publish via gossipsub.
    election_outbound_rx: Option<mpsc::Receiver<ElectionMessage>>,
    /// Our peer ID.
    pub peer_id: PeerId,
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

        let mut node = P2PNode {
            swarm,
            orders_topic,
            election_topic,
            batch_tx,
            publish_rx: None,
            election_inbound_tx,
            election_outbound_rx: None,
            peer_id,
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
            .map_err(|e| anyhow::anyhow!("publish failed: {}", e))?;
        Ok(())
    }

    fn publish_election(&mut self, msg: &ElectionMessage) -> Result<()> {
        let data = serde_json::to_vec(msg).context("failed to serialize election msg")?;
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.election_topic.clone(), data)
            .map_err(|e| anyhow::anyhow!("election publish failed: {}", e))?;
        Ok(())
    }

    /// Run the event loop. Call this in a tokio::spawn.
    pub async fn run(&mut self) {
        // Take channels out of self for use in select!
        let mut publish_rx = self.publish_rx.take();
        let mut election_rx = self.election_outbound_rx.take();

        let orders_topic_hash = self.orders_topic.hash();
        let election_topic_hash = self.election_topic.hash();

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
                        // InsufficientPeers is expected when running solo (no P2P peers)
                        tracing::debug!("election publish: {}", e);
                    }
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
                        // Order batch from sequencer
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
                        // Election message
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
                    // Add peer's listen addresses to gossipsub
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
                    info!(peer = %peer_id, "connected");
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    warn!(peer = %peer_id, "disconnected");
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
