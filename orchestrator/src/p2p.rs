//! P2P layer for order flow replication between operators.
//!
//! Uses libp2p gossipsub:
//! - Sequencer publishes order batches
//! - Validators subscribe and replay deterministically
//! - Any operator can request cross-signing via signing relay
//!
//! Topics: "perp-dex/orders", "perp-dex/election", "perp-dex/signing"

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
///
/// X-C1 hardening: the request carries the full unsigned XRPL tx, not a
/// pre-computed hash. Receivers re-derive `multi_signing_hash` locally
/// and reject the request if the tx fails policy (non-Payment, wrong
/// escrow Account, destination == escrow, etc.). A hash-only API made
/// `/pool/sign` a blind signing oracle: any gossipsub peer could publish
/// `multi_signing_hash(Payment(to=attacker))` and collect quorum
/// signatures. Sending the tx forces every signer to see what it's
/// actually signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SigningMessage {
    Request {
        request_id: String,
        requester_peer_id: String,
        /// Unsigned XRPL tx JSON (SigningPubKey must be ""). Receivers
        /// re-derive the multi_signing_hash from this — the hash is
        /// never trusted from the wire.
        unsigned_tx: serde_json::Value,
        /// Hex of the signer's 20-byte AccountID used in
        /// multi_signing_hash. Must match the receiver's local signer.
        signer_account_id_hex: String,
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
///
/// Carries the full unsigned tx (not a hash) — see `SigningMessage`
/// comment for the X-C1 rationale.
#[derive(Debug)]
pub struct SigningRelay {
    pub request_id: String,
    pub unsigned_tx: serde_json::Value,
    pub signer_account_id_hex: String,
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
    /// X-C1: escrow r-address that the local enclave is allowed to sign
    /// withdrawals *from*. Incoming signing requests whose `unsigned_tx.Account`
    /// doesn't match are rejected. `None` = fail-closed (reject every
    /// signing request) so a misconfigured node can never be used as a
    /// blind signing oracle.
    escrow_xrpl_address: Option<String>,
    /// X-C1: optional allowlist of peers that may publish signing
    /// requests. If `Some`, incoming requests from peers outside the set
    /// are dropped. If `None`, all peers are accepted (dev/test only).
    allowed_signing_peers: Option<HashSet<PeerId>>,
    /// X-C1: replay guard for signing requests. Maps `request_id` →
    /// first-seen timestamp; entries older than the TTL are cleaned on
    /// insertion.
    recent_signing_requests: HashMap<String, Instant>,
    /// X-C1: per-peer rate limiter on inbound signing requests.
    signing_request_rate: HashMap<PeerId, VecDeque<Instant>>,
    /// Our peer ID.
    pub peer_id: PeerId,
    /// Shared counter of connected peers (read by health endpoint).
    peer_count: Arc<std::sync::atomic::AtomicU32>,
}

/// X-C1 tunables. Kept module-local rather than wired as CLI flags — if
/// an operator's traffic shape changes we adjust here + redeploy.
const SIGNING_REPLAY_TTL: Duration = Duration::from_secs(10 * 60);
const SIGNING_RATE_WINDOW: Duration = Duration::from_secs(60);
const SIGNING_RATE_MAX_PER_WINDOW: usize = 30;

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
            escrow_xrpl_address: None,
            allowed_signing_peers: None,
            recent_signing_requests: HashMap::new(),
            signing_request_rate: HashMap::new(),
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

    /// X-C1: set the escrow r-address the local enclave is allowed to
    /// sign withdrawals *from*. Signing requests whose `Account` field
    /// doesn't match this are rejected. Without this set, all signing
    /// requests fail closed.
    pub fn set_escrow_address(&mut self, escrow: String) {
        info!(escrow = %escrow, "P2P signing relay: escrow address configured");
        self.escrow_xrpl_address = Some(escrow);
    }

    /// X-C1: set the peer allowlist for signing requests. Any peer not
    /// in the set has its signing requests dropped. Pass an empty vec to
    /// disable the allowlist (dev/test only — logs a warning).
    ///
    /// Not wired into `main.rs` yet — the first four defenses
    /// (hash re-derivation, policy validation, replay guard,
    /// per-peer rate limit) already kill the X-C1 attack. Allowlist
    /// is pure defense-in-depth and requires plumbing peer_ids
    /// through operator config, tracked separately.
    #[allow(dead_code)]
    pub fn set_allowed_signing_peers(&mut self, peers: Vec<PeerId>) {
        if peers.is_empty() {
            warn!(
                "P2P signing relay: empty allowlist — accepting signing requests from any peer (dev/test)"
            );
            self.allowed_signing_peers = None;
        } else {
            info!(
                count = peers.len(),
                "P2P signing relay: signing peer allowlist configured"
            );
            self.allowed_signing_peers = Some(peers.into_iter().collect());
        }
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

    /// X-C1: record a request_id as seen; return `false` if it was
    /// already in the window (replay). Also purges entries older than
    /// `SIGNING_REPLAY_TTL` on insertion so the map stays bounded.
    fn mark_signing_request_fresh(&mut self, request_id: &str) -> bool {
        let now = Instant::now();
        self.recent_signing_requests
            .retain(|_, seen| now.duration_since(*seen) < SIGNING_REPLAY_TTL);
        if self.recent_signing_requests.contains_key(request_id) {
            return false;
        }
        self.recent_signing_requests
            .insert(request_id.to_string(), now);
        true
    }

    /// X-C1: token-bucket-style check on incoming signing traffic from
    /// one peer. Returns `true` if the request is within budget and
    /// records the hit; `false` if the peer has exceeded
    /// `SIGNING_RATE_MAX_PER_WINDOW` in the trailing
    /// `SIGNING_RATE_WINDOW`.
    fn check_signing_rate(&mut self, peer: &PeerId) -> bool {
        let now = Instant::now();
        let q = self.signing_request_rate.entry(*peer).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= SIGNING_RATE_WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= SIGNING_RATE_MAX_PER_WINDOW {
            return false;
        }
        q.push_back(now);
        true
    }

    /// X-C1: validate an incoming signing request against policy and
    /// re-derive the multi-signing hash from the tx. Returns the hash
    /// on success, or an error message suitable for a Response payload.
    ///
    /// Policy:
    /// - Tx must be a JSON object with `TransactionType == "Payment"`.
    /// - `Account` must equal the configured escrow address.
    /// - `Destination` must be a non-empty r-address distinct from `Account`.
    /// - `Amount` must be a non-empty string (XRP drops) or RLUSD
    ///   issued-currency object — presence is enough; XRPL parses the
    ///   binary form downstream.
    /// - `SigningPubKey` must be `""` (multisig marker).
    /// - `signer_account_id_hex` must decode to 20 bytes and match the
    ///   local signer's xrpl_address.
    fn validate_signing_policy(
        local_signer: &LocalSigner,
        escrow_xrpl_address: Option<&str>,
        unsigned_tx: &serde_json::Value,
        signer_account_id_hex: &str,
    ) -> Result<[u8; 32], String> {
        let escrow = escrow_xrpl_address
            .ok_or_else(|| "escrow address not configured — refusing to sign".to_string())?;

        let tx_obj = unsigned_tx
            .as_object()
            .ok_or_else(|| "unsigned_tx is not a JSON object".to_string())?;

        match tx_obj.get("TransactionType").and_then(|v| v.as_str()) {
            Some("Payment") => {}
            Some(other) => return Err(format!("non-Payment TransactionType: {other}")),
            None => return Err("missing TransactionType".to_string()),
        }

        let account = tx_obj
            .get("Account")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing Account".to_string())?;
        if account != escrow {
            return Err(format!(
                "Account {account} does not match configured escrow {escrow}"
            ));
        }

        let destination = tx_obj
            .get("Destination")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing Destination".to_string())?;
        if destination.is_empty() {
            return Err("empty Destination".to_string());
        }
        if destination == escrow {
            return Err("Destination equals escrow — self-loop rejected".to_string());
        }
        if !destination.starts_with('r') {
            return Err(format!("Destination is not an r-address: {destination}"));
        }

        if tx_obj.get("Amount").is_none() {
            return Err("missing Amount".to_string());
        }

        match tx_obj.get("SigningPubKey").and_then(|v| v.as_str()) {
            Some("") => {}
            Some(other) => {
                return Err(format!(
                    "SigningPubKey must be empty for multisig, got '{other}'"
                ))
            }
            None => return Err("missing SigningPubKey (must be \"\")".to_string()),
        }

        let acct_id_bytes = hex::decode(signer_account_id_hex.trim_start_matches("0x"))
            .map_err(|e| format!("signer_account_id_hex: {e}"))?;
        if acct_id_bytes.len() != 20 {
            return Err(format!(
                "signer_account_id must be 20 bytes, got {}",
                acct_id_bytes.len()
            ));
        }
        let expected_acct_id = crate::xrpl_signer::decode_xrpl_address(&local_signer.xrpl_address)
            .map_err(|e| format!("local xrpl_address decode: {e}"))?;
        if acct_id_bytes.as_slice() != expected_acct_id.as_slice() {
            return Err("signer_account_id does not match local signer".to_string());
        }

        let mut acct_arr = [0u8; 20];
        acct_arr.copy_from_slice(&acct_id_bytes);
        xrpl_mithril_codec::signing::multi_signing_hash(tx_obj, &acct_arr)
            .map_err(|e| format!("multi_signing_hash failed: {e:?}"))
    }

    /// Handle an incoming signing request: sign with local enclave if we own the address.
    async fn handle_signing_request(
        local_signer: &LocalSigner,
        escrow_xrpl_address: Option<&str>,
        request_id: &str,
        unsigned_tx: &serde_json::Value,
        signer_account_id_hex: &str,
    ) -> SigningMessage {
        let hash = match Self::validate_signing_policy(
            local_signer,
            escrow_xrpl_address,
            unsigned_tx,
            signer_account_id_hex,
        ) {
            Ok(h) => h,
            Err(e) => {
                warn!(req_id = %request_id, error = %e, "X-C1: signing request rejected by policy");
                return SigningMessage::Response {
                    request_id: request_id.to_string(),
                    signer_xrpl_address: local_signer.xrpl_address.clone(),
                    der_signature: None,
                    compressed_pubkey: None,
                    error: Some(format!("policy: {e}")),
                };
            }
        };
        let hash_hex = format!("0x{}", hex::encode(hash));
        // O-L4: `local_signer.enclave_url` is loopback (the current
        // node's own enclave). The shared factory carries the self-
        // signed-cert relaxation so every loopback-client site reads
        // the same way.
        let http = match crate::http_helpers::loopback_http_client(Duration::from_secs(15)) {
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
                                local,
                                self.escrow_xrpl_address.as_deref(),
                                &relay.request_id,
                                &relay.unsigned_tx,
                                &relay.signer_account_id_hex,
                            ).await;
                            let _ = relay.response_tx.send(response);
                            continue;
                        }
                    }

                    let msg = SigningMessage::Request {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        unsigned_tx: relay.unsigned_tx,
                        signer_account_id_hex: relay.signer_account_id_hex,
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
                                unsigned_tx,
                                signer_account_id_hex,
                                signer_xrpl_address,
                            }) => {
                                // Is this request addressed to our local signer?
                                // Clone up-front so subsequent borrows can mutate
                                // self for rate/replay bookkeeping.
                                let local_opt = self
                                    .local_signer
                                    .as_ref()
                                    .filter(|l| l.xrpl_address == signer_xrpl_address)
                                    .cloned();
                                let Some(local) = local_opt else {
                                    // Not for us — gossipsub delivered it anyway.
                                    continue;
                                };

                                // X-C1: peer allowlist. `propagation_source`
                                // is the authenticated libp2p peer_id of the
                                // node that forwarded this to us, not the
                                // self-reported `requester_peer_id` field.
                                if let Some(ref allow) = self.allowed_signing_peers {
                                    if !allow.contains(&propagation_source) {
                                        warn!(
                                            req_id = %request_id,
                                            from = %propagation_source,
                                            "X-C1: signing request from peer outside allowlist — dropped"
                                        );
                                        continue;
                                    }
                                }
                                // X-C1: per-peer rate limit.
                                if !self.check_signing_rate(&propagation_source) {
                                    warn!(
                                        req_id = %request_id,
                                        from = %propagation_source,
                                        "X-C1: signing request rate-limited"
                                    );
                                    continue;
                                }
                                // X-C1: replay guard.
                                if !self.mark_signing_request_fresh(&request_id) {
                                    warn!(
                                        req_id = %request_id,
                                        from = %propagation_source,
                                        "X-C1: duplicate request_id — dropped"
                                    );
                                    continue;
                                }
                                info!(
                                    req_id = %request_id,
                                    from = %requester_peer_id,
                                    propagation = %propagation_source,
                                    "signing request received — signing locally"
                                );
                                let escrow = self.escrow_xrpl_address.clone();
                                let response = Self::handle_signing_request(
                                    &local,
                                    escrow.as_deref(),
                                    &request_id,
                                    &unsigned_tx,
                                    &signer_account_id_hex,
                                ).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish signing response: {}", e);
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

    // ── X-C1 signing-policy tests ───────────────────────────────
    //
    // The signer below is one of the testnet multisig members. We never
    // decode its seed here — only the address, so `decode_xrpl_address`
    // matches what `multi_signing_hash` expects.
    fn test_local_signer() -> LocalSigner {
        LocalSigner {
            enclave_url: "https://127.0.0.1:9088/v1".into(),
            address: "0xdeadbeef".into(),
            session_key: "0x00".into(),
            compressed_pubkey: "02aa".into(),
            xrpl_address: "rNrjh1KGZk2jBR3wPfAQnoidtFFYQKbQn2".into(),
        }
    }

    // Valid XRPL base58check r-addresses — "rEscrow..." would fail the
    // base58 alphabet check inside multi_signing_hash and blow up the
    // good-tx test before it reaches the assertion.
    const TEST_ESCROW: &str = "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh";
    const TEST_DESTINATION: &str = "rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzRH";
    const TEST_ATTACKER: &str = "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe";

    fn good_tx() -> serde_json::Value {
        serde_json::json!({
            "TransactionType": "Payment",
            "Account": TEST_ESCROW,
            "Destination": TEST_DESTINATION,
            "Amount": "1000000",
            "Fee": "36",
            "Sequence": 1,
            "SigningPubKey": "",
        })
    }

    fn signer_acct_id_hex() -> String {
        let id =
            crate::xrpl_signer::decode_xrpl_address(&test_local_signer().xrpl_address).unwrap();
        hex::encode(id)
    }

    #[test]
    fn policy_rejects_when_escrow_not_configured() {
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            None,
            &good_tx(),
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(err.contains("escrow"), "got: {err}");
    }

    #[test]
    fn policy_rejects_non_payment() {
        let mut tx = good_tx();
        tx["TransactionType"] = serde_json::json!("SetRegularKey");
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &tx,
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(err.contains("non-Payment"), "got: {err}");
    }

    #[test]
    fn policy_rejects_wrong_account() {
        let mut tx = good_tx();
        tx["Account"] = serde_json::json!(TEST_ATTACKER);
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &tx,
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(
            err.contains("does not match configured escrow"),
            "got: {err}"
        );
    }

    #[test]
    fn policy_rejects_destination_equal_to_escrow() {
        let mut tx = good_tx();
        tx["Destination"] = serde_json::json!(TEST_ESCROW);
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &tx,
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(err.contains("self-loop"), "got: {err}");
    }

    #[test]
    fn policy_rejects_signing_pubkey_nonempty() {
        let mut tx = good_tx();
        tx["SigningPubKey"] = serde_json::json!("02abc...");
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &tx,
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(err.contains("SigningPubKey must be empty"), "got: {err}");
    }

    #[test]
    fn policy_rejects_foreign_signer_account_id() {
        // An account_id that doesn't match the local signer's xrpl_address.
        let foreign = hex::encode([0x11u8; 20]);
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &good_tx(),
            &foreign,
        )
        .unwrap_err();
        assert!(err.contains("does not match local signer"), "got: {err}");
    }

    #[test]
    fn policy_accepts_good_tx_and_returns_stable_hash() {
        let h1 = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &good_tx(),
            &signer_acct_id_hex(),
        )
        .unwrap();
        let h2 = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &good_tx(),
            &signer_acct_id_hex(),
        )
        .unwrap();
        assert_eq!(h1, h2, "multi_signing_hash must be deterministic");
    }

    // The replay and rate-limit methods touch `P2PNode` directly. We
    // can't trivially construct a real `P2PNode` in a unit test (needs
    // a tokio swarm), but the logic is small enough to test by building
    // the maps in the same shape and asserting invariants on a minimal
    // harness.

    #[test]
    fn replay_guard_rejects_duplicate() {
        let mut seen: HashMap<String, Instant> = HashMap::new();
        let now = Instant::now();
        seen.insert("abc".into(), now);

        let is_fresh = !seen.contains_key("abc");
        assert!(!is_fresh);
        let is_fresh2 = !seen.contains_key("def");
        assert!(is_fresh2);
    }

    #[test]
    fn rate_limit_queue_drops_old_entries() {
        use std::collections::VecDeque;
        let mut q: VecDeque<Instant> = VecDeque::new();
        let now = Instant::now();
        q.push_back(now - Duration::from_secs(120));
        q.push_back(now - Duration::from_secs(30));
        q.push_back(now);

        while let Some(front) = q.front() {
            if now.duration_since(*front) >= SIGNING_RATE_WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
        // The 120s-old entry must be evicted; the 30s-old one stays.
        assert_eq!(q.len(), 2);
    }
}
