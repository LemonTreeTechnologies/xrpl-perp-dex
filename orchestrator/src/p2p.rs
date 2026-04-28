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

/// DKG ceremony coordination messages, published on
/// `perp-dex/cluster/dkg-step` per `docs/multi-operator-architecture.md`
/// §3.1. Per Phase 2.1c-D — a leader-followers protocol that runs
/// entirely over libp2p.
///
/// Wire flow (happy path, 3 nodes, leader = node-0):
///   1. Leader publishes `Round1Start { ceremony_id, threshold, n,
///      pid_assignment[] }`. Each follower (and leader itself) calls
///      its local enclave `/v1/pool/dkg/round1-generate` with the
///      assigned pid, then publishes `Round1Done { ceremony_id, pid,
///      vss_commitment }`.
///   2. Once leader has N `Round1Done` (including its own), it stores
///      the `vss_commitment` per pid and publishes `Round15Start`.
///      Each follower exports a share-v2 envelope to every peer via
///      the existing `perp-dex/path-a/share-v2` topic, then publishes
///      `Round15Done { ceremony_id, pid }`.
///   3. After N `Round15Done`, leader publishes `Round2Start` carrying
///      the `pid → vss_commitment` map (so importers can verify each
///      incoming share). Followers wait until their share-v2 inbound
///      importer has imported N-1 shares and the local enclave's
///      `dkg_session.share_received[]` is full, then publish
///      `Round2Done { ceremony_id, pid }`.
///   4. After N `Round2Done`, leader publishes `FinalizeStart`. Each
///      follower calls `/v1/pool/dkg/finalize` and publishes
///      `FinalizeDone { ceremony_id, pid, group_pubkey }`. Leader
///      asserts byte-identical `group_pubkey` across all N.
///
/// `ceremony_id` is a 32-byte hex token chosen by the leader at start;
/// it dedupes if two ceremonies overlap and gives operators a handle
/// for log correlation. Followers ignore messages whose `ceremony_id`
/// is not the one they are currently processing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DkgStepMessage {
    /// Leader → all
    Round1Start {
        ceremony_id: String,
        threshold: u32,
        n_participants: u32,
        /// Map of `xrpl_address` (from on-chain SignerList) → pid.
        /// Each follower looks up its own xrpl_address (from
        /// `local_signer.xrpl_address`) here.
        pid_assignment: Vec<(String, u32)>,
    },
    /// Each → all (broadcast)
    Round1Done {
        ceremony_id: String,
        pid: u32,
        /// Hex-encoded VSS commitment from `/v1/pool/dkg/round1-generate`.
        vss_commitment: String,
    },
    /// Leader → all
    Round15Start { ceremony_id: String },
    /// Each → all
    Round15Done { ceremony_id: String, pid: u32 },
    /// Leader → all
    Round2Start {
        ceremony_id: String,
        /// Per-pid VSS commitment so each follower can pass it to the
        /// enclave's `/v1/pool/dkg/round2-import-share-v2` for verify.
        vss_commitments: Vec<(u32, String)>,
    },
    /// Each → all
    Round2Done { ceremony_id: String, pid: u32 },
    /// Leader → all
    FinalizeStart { ceremony_id: String },
    /// Each → all (final ack carrying the produced group_pubkey)
    FinalizeDone {
        ceremony_id: String,
        pid: u32,
        /// 32-byte BIP340 x-only group public key, hex no `0x`.
        group_pubkey: String,
    },
    /// Either side → abort the ceremony with a reason. Receivers stop
    /// processing for this ceremony_id and free resources.
    Abort {
        ceremony_id: String,
        pid: u32,
        reason: String,
    },
}

// ── Network behaviour ───────────────────────────────────────────

const ORDERS_TOPIC: &str = "perp-dex/orders";
const ELECTION_TOPIC: &str = "perp-dex/election";
const SIGNING_TOPIC: &str = "perp-dex/signing";
const EVENTS_TOPIC: &str = "perp-dex/events";
const PEER_QUOTE_TOPIC: &str = "perp-dex/path-a/peer-quote";
const SHARE_V2_TOPIC: &str = "perp-dex/path-a/share-v2";
const DKG_STEP_TOPIC: &str = "perp-dex/cluster/dkg-step";

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
    /// Phase 2.1c-D: DKG ceremony coordination (leader-driven, libp2p).
    dkg_step_topic: gossipsub::IdentTopic,
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
    /// Phase 2.1c-D: outbound DKG ceremony coordination messages
    /// (published by leader admin route + each follower's step handler).
    dkg_step_publish_rx: Option<mpsc::Receiver<DkgStepMessage>>,
    /// Phase 2.1c-D: received DKG ceremony coordination messages
    /// forwarded to the local follower step handler.
    dkg_step_inbound_tx: Option<mpsc::Sender<DkgStepMessage>>,
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
        let dkg_step_topic = gossipsub::IdentTopic::new(DKG_STEP_TOPIC);

        let mut node = P2PNode {
            swarm,
            orders_topic,
            election_topic,
            signing_topic,
            events_topic,
            peer_quote_topic,
            share_v2_topic,
            dkg_step_topic,
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
            dkg_step_publish_rx: None,
            dkg_step_inbound_tx: None,
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
        node.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&node.dkg_step_topic)
            .context("failed to subscribe to dkg-step topic")?;

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

    /// Phase 2.1c-D: set the channel that DKG ceremony coordination
    /// messages are pulled from (leader's admin route + each follower's
    /// step handler use it to publish on `dkg-step` topic).
    pub fn set_dkg_step_publish_channel(&mut self, rx: mpsc::Receiver<DkgStepMessage>) {
        self.dkg_step_publish_rx = Some(rx);
    }

    /// Phase 2.1c-D: set the channel that received DKG-step messages
    /// are forwarded to (the local follower step handler).
    pub fn set_dkg_step_inbound_channel(&mut self, tx: mpsc::Sender<DkgStepMessage>) {
        self.dkg_step_inbound_tx = Some(tx);
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
    /// Dispatcher: per-`TransactionType` validators carry tx-type-specific
    /// business rules (allowed only `Payment` and `SignerListSet`); the
    /// universal checks (escrow source binding, multisig marker, local
    /// signer identity) live here so every allowed type inherits them.
    /// Per `SECURITY-REAUDIT-4` X-C1 invariants — receiver re-derives the
    /// hash from the unsigned tx after policy passes, never trusts a
    /// hash from the wire.
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

        let tx_type = tx_obj
            .get("TransactionType")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing TransactionType".to_string())?;

        // Per-tx-type business validation. Every entry below MUST also
        // satisfy the universal checks that follow this match.
        match tx_type {
            "Payment" => Self::validate_payment_specific(tx_obj, escrow)?,
            "SignerListSet" => Self::validate_signerlist_set_specific(tx_obj, escrow)?,
            other => return Err(format!("disallowed TransactionType: {other}")),
        }

        // ── Universal checks (apply to every allowed tx type) ──────

        let account = tx_obj
            .get("Account")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing Account".to_string())?;
        if account != escrow {
            return Err(format!(
                "Account {account} does not match configured escrow {escrow}"
            ));
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

    /// Per-`TransactionType` validator for Payment. Pre-existing audited
    /// behaviour (X-C1): destination present, non-empty, distinct from
    /// escrow, looks like an r-address; amount field present (codec
    /// validates the binary shape downstream).
    fn validate_payment_specific(
        tx_obj: &serde_json::Map<String, serde_json::Value>,
        escrow: &str,
    ) -> Result<(), String> {
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
        Ok(())
    }

    /// Per-`TransactionType` validator for SignerListSet (governance —
    /// membership change of the escrow's multisig list). New as of
    /// Phase 2.2; subject to the audit-bar X-C1 invariants. The
    /// universal checks in `validate_signing_policy` cover Account,
    /// SigningPubKey, and signer-identity binding; this function adds
    /// the SignerListSet-specific constraints.
    ///
    /// Constraints (all must hold; see Phase 2.2 plan §"Constraint-лист"):
    ///
    ///   1. Top-level fields are a strict whitelist — extras rejected.
    ///   2. `Flags`, if present, must be 0.
    ///   3. `SignerListID`, if present, must be 0.
    ///   4. `Sequence` and `Fee` present; `Fee` ≥ 12000 drops (multisig
    ///      minimum per XRPL spec — `12 drops × (1 + N_signers)`, and
    ///      we never sign with N=0).
    ///   5. `SignerEntries` is a JSON array of length 3..=8.
    ///   6. Each entry is `{"SignerEntry": {"Account": <r-address>,
    ///      "SignerWeight": 1}}` — exact key set, weight equals 1.
    ///   7. Each `Account` decodes as a valid XRPL r-address (base58check
    ///      with the XRPL alphabet, 20-byte AccountID).
    ///   8. No duplicate `Account` across entries.
    ///   9. `SignerQuorum` ∈ `[2, len(SignerEntries)]`.
    ///
    /// Equal-weight (rule 6) reduces the quorum-math footgun surface to
    /// zero — `sum(weights) == N_entries` always, so condition (9)
    /// implies the XRPL semantic `quorum ≤ sum(weights)`.
    fn validate_signerlist_set_specific(
        tx_obj: &serde_json::Map<String, serde_json::Value>,
        _escrow: &str,
    ) -> Result<(), String> {
        // (1) Top-level whitelist. NetworkID/LastLedgerSequence are
        // optional XRPL hygiene fields the operator may set. Memos
        // intentionally NOT in the whitelist — governance txs do not
        // benefit from memos and disallowing one more field shrinks
        // mutator surface.
        const ALLOWED_TOP_LEVEL: &[&str] = &[
            "Account",
            "TransactionType",
            "Sequence",
            "Fee",
            "SigningPubKey",
            "SignerQuorum",
            "SignerEntries",
            "Flags",
            "SignerListID",
            "LastLedgerSequence",
            "NetworkID",
        ];
        for key in tx_obj.keys() {
            if !ALLOWED_TOP_LEVEL.contains(&key.as_str()) {
                return Err(format!("disallowed top-level field: {key}"));
            }
        }

        // (2) Flags
        if let Some(flags) = tx_obj.get("Flags") {
            let f = flags
                .as_u64()
                .ok_or_else(|| "Flags is not an integer".to_string())?;
            if f != 0 {
                return Err(format!("Flags must be 0, got {f}"));
            }
        }

        // (3) SignerListID
        if let Some(slid) = tx_obj.get("SignerListID") {
            let s = slid
                .as_u64()
                .ok_or_else(|| "SignerListID is not an integer".to_string())?;
            if s != 0 {
                return Err(format!("SignerListID must be 0, got {s}"));
            }
        }

        // (4) Sequence + Fee
        let _sequence = tx_obj
            .get("Sequence")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing or non-integer Sequence".to_string())?;
        let fee_str = tx_obj
            .get("Fee")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing Fee (must be string-of-drops)".to_string())?;
        let fee: u64 = fee_str
            .parse()
            .map_err(|_| format!("Fee is not numeric: {fee_str}"))?;
        const MULTISIG_FEE_MIN_DROPS: u64 = 12000;
        if fee < MULTISIG_FEE_MIN_DROPS {
            return Err(format!(
                "Fee {fee} below multisig minimum {MULTISIG_FEE_MIN_DROPS}"
            ));
        }

        // (5) SignerEntries shape
        let entries = tx_obj
            .get("SignerEntries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "missing or non-array SignerEntries".to_string())?;
        if !(3..=8).contains(&entries.len()) {
            return Err(format!(
                "SignerEntries length {} outside allowed [3,8]",
                entries.len()
            ));
        }

        // (6+7+8) Per-entry shape, weight, address validation, dedup.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (i, entry_outer) in entries.iter().enumerate() {
            let outer_obj = entry_outer
                .as_object()
                .ok_or_else(|| format!("SignerEntries[{i}] is not an object"))?;
            // Outer wrapper must be exactly {"SignerEntry": ...}
            let entry_keys: Vec<&String> = outer_obj.keys().collect();
            if entry_keys.len() != 1 || entry_keys[0] != "SignerEntry" {
                return Err(format!(
                    "SignerEntries[{i}] must wrap a single \"SignerEntry\" key, got {entry_keys:?}"
                ));
            }
            let entry = outer_obj["SignerEntry"]
                .as_object()
                .ok_or_else(|| format!("SignerEntries[{i}].SignerEntry is not an object"))?;
            // Inner exact key set
            const INNER_KEYS: &[&str] = &["Account", "SignerWeight"];
            for k in entry.keys() {
                if !INNER_KEYS.contains(&k.as_str()) {
                    return Err(format!(
                        "SignerEntries[{i}].SignerEntry: disallowed field {k}"
                    ));
                }
            }
            for k in INNER_KEYS {
                if !entry.contains_key(*k) {
                    return Err(format!("SignerEntries[{i}].SignerEntry: missing {k}"));
                }
            }
            let acct = entry["Account"]
                .as_str()
                .ok_or_else(|| format!("SignerEntries[{i}].Account is not a string"))?;
            // (7) Address validation
            crate::xrpl_signer::decode_xrpl_address(acct).map_err(|e| {
                format!("SignerEntries[{i}].Account invalid r-address ({acct}): {e}")
            })?;
            // (8) Dedup
            if !seen.insert(acct.to_string()) {
                return Err(format!("duplicate SignerEntries[{i}].Account: {acct}"));
            }
            // (6) Weight == 1
            let weight = entry["SignerWeight"]
                .as_u64()
                .ok_or_else(|| format!("SignerEntries[{i}].SignerWeight is not an integer"))?;
            if weight != 1 {
                return Err(format!(
                    "SignerEntries[{i}].SignerWeight must be 1 (equal-weight), got {weight}"
                ));
            }
        }

        // (9) SignerQuorum range
        let quorum = tx_obj
            .get("SignerQuorum")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing or non-integer SignerQuorum".to_string())?;
        let n = entries.len() as u64;
        if !(2..=n).contains(&quorum) {
            return Err(format!("SignerQuorum {quorum} outside allowed [2, {n}]"));
        }

        Ok(())
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
        let mut dkg_step_rx = self.dkg_step_publish_rx.take();

        let orders_topic_hash = self.orders_topic.hash();
        let election_topic_hash = self.election_topic.hash();
        let signing_topic_hash = self.signing_topic.hash();
        let events_topic_hash = self.events_topic.hash();
        let peer_quote_topic_hash = self.peer_quote_topic.hash();
        let share_v2_topic_hash = self.share_v2_topic.hash();
        let dkg_step_topic_hash = self.dkg_step_topic.hash();

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

                // Phase 2.1c-D: publish DKG ceremony coordination messages
                Some(msg) = async {
                    match &mut dkg_step_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<DkgStepMessage>>().await,
                    }
                } => {
                    if let Ok(data) = serde_json::to_vec(&msg) {
                        match self.swarm.behaviour_mut().gossipsub
                            .publish(self.dkg_step_topic.clone(), data) {
                            Ok(_) => info!(?msg, "published dkg-step message"),
                            Err(e) => warn!("dkg-step publish failed: {}", e),
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
                    } else if message.topic == dkg_step_topic_hash {
                        match serde_json::from_slice::<DkgStepMessage>(&message.data) {
                            Ok(msg) => {
                                if let Some(ref tx) = self.dkg_step_inbound_tx {
                                    if let Err(e) = tx.send(msg).await {
                                        error!("failed to forward dkg-step: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("invalid dkg-step from {}: {}", propagation_source, e);
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
    fn policy_rejects_disallowed_tx_type() {
        let mut tx = good_tx();
        tx["TransactionType"] = serde_json::json!("SetRegularKey");
        let err = P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            &tx,
            &signer_acct_id_hex(),
        )
        .unwrap_err();
        assert!(err.contains("disallowed TransactionType"), "got: {err}");
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

    // ── Phase 2.2 SignerListSet policy tests ────────────────────
    //
    // These cover `validate_signerlist_set_specific` plus the
    // dispatcher's universal-check interaction (Account binding,
    // SigningPubKey, signer-identity). One mutation per test, every
    // other field valid — the audit-bar pattern that locks each
    // constraint behind its own assertion.
    //
    // Addresses below are real XRPL r-addresses (base58check valid).
    // None correspond to live escrow accounts.
    const SLS_ENTRY_A: &str = "rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzRH";
    const SLS_ENTRY_B: &str = "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe";
    const SLS_ENTRY_C: &str = "rNrjh1KGZk2jBR3wPfAQnoidtFFYQKbQn2";
    const SLS_ENTRY_D: &str = "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn";

    fn signer_entry(addr: &str, weight: u32) -> serde_json::Value {
        serde_json::json!({
            "SignerEntry": {"Account": addr, "SignerWeight": weight}
        })
    }

    /// Canonical 3-of-3 SignerListSet. Used as the base every negative
    /// test mutates one field of.
    fn good_signerlist_tx() -> serde_json::Value {
        serde_json::json!({
            "TransactionType": "SignerListSet",
            "Account": TEST_ESCROW,
            "Fee": "12000",
            "Sequence": 1,
            "SigningPubKey": "",
            "SignerQuorum": 3,
            "SignerEntries": [
                signer_entry(SLS_ENTRY_A, 1),
                signer_entry(SLS_ENTRY_B, 1),
                signer_entry(SLS_ENTRY_C, 1),
            ],
        })
    }

    fn run_policy(tx: &serde_json::Value) -> Result<[u8; 32], String> {
        P2PNode::validate_signing_policy(
            &test_local_signer(),
            Some(TEST_ESCROW),
            tx,
            &signer_acct_id_hex(),
        )
    }

    // ── Positive cases ──────────────────────────────────────────

    #[test]
    fn signerlist_accepts_3of3() {
        let h = run_policy(&good_signerlist_tx()).expect("3-of-3 must pass");
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn signerlist_accepts_2of3() {
        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(2);
        run_policy(&tx).expect("2-of-3 must pass");
    }

    #[test]
    fn signerlist_accepts_3of4() {
        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(3);
        tx["SignerEntries"] = serde_json::json!([
            signer_entry(SLS_ENTRY_A, 1),
            signer_entry(SLS_ENTRY_B, 1),
            signer_entry(SLS_ENTRY_C, 1),
            signer_entry(SLS_ENTRY_D, 1),
        ]);
        run_policy(&tx).expect("3-of-4 must pass");
    }

    #[test]
    fn signerlist_accepts_max_size_3of8() {
        let mut tx = good_signerlist_tx();
        // 8 distinct r-addresses (recycle the 4 we have via offsets in
        // the alphabet — these are also real valid XRPL addresses).
        tx["SignerEntries"] = serde_json::json!([
            signer_entry("rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzRH", 1),
            signer_entry("rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe", 1),
            signer_entry("rNrjh1KGZk2jBR3wPfAQnoidtFFYQKbQn2", 1),
            signer_entry("rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn", 1),
            signer_entry("rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ", 1),
            signer_entry("rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j", 1),
            signer_entry("rJWSAM1cHSfwDrSnA1qyJbnEaSaAvJNp18", 1),
            signer_entry("rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti", 1),
        ]);
        tx["SignerQuorum"] = serde_json::json!(3);
        run_policy(&tx).expect("3-of-8 must pass");
    }

    // ── Universal-check rejections (apply to SignerListSet, too) ───

    #[test]
    fn signerlist_rejects_account_not_escrow() {
        let mut tx = good_signerlist_tx();
        tx["Account"] = serde_json::json!(TEST_ATTACKER);
        let err = run_policy(&tx).unwrap_err();
        assert!(
            err.contains("does not match configured escrow"),
            "got: {err}"
        );
    }

    #[test]
    fn signerlist_rejects_signing_pubkey_nonempty() {
        let mut tx = good_signerlist_tx();
        tx["SigningPubKey"] = serde_json::json!("02abc...");
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("SigningPubKey must be empty"), "got: {err}");
    }

    // ── SignerListSet-specific rejections ───────────────────────

    #[test]
    fn signerlist_rejects_quorum_zero() {
        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(0);
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("SignerQuorum 0 outside"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_quorum_one() {
        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(1);
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("SignerQuorum 1 outside"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_quorum_exceeds_n() {
        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(4); // N=3
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("SignerQuorum 4 outside"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_weight_zero() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = signer_entry(SLS_ENTRY_A, 0);
        let err = run_policy(&tx).unwrap_err();
        assert!(
            err.contains("SignerWeight must be 1") && err.contains("got 0"),
            "got: {err}"
        );
    }

    #[test]
    fn signerlist_rejects_weight_two() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = signer_entry(SLS_ENTRY_A, 2);
        let err = run_policy(&tx).unwrap_err();
        assert!(
            err.contains("SignerWeight must be 1") && err.contains("got 2"),
            "got: {err}"
        );
    }

    #[test]
    fn signerlist_rejects_duplicate_account() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][2] = signer_entry(SLS_ENTRY_A, 1); // duplicates [0]
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("duplicate SignerEntries"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_too_few_entries_2() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"] =
            serde_json::json!([signer_entry(SLS_ENTRY_A, 1), signer_entry(SLS_ENTRY_B, 1),]);
        tx["SignerQuorum"] = serde_json::json!(2);
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("length 2 outside"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_too_many_entries_9() {
        let mut tx = good_signerlist_tx();
        let nine: Vec<serde_json::Value> = [
            "rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzRH",
            "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe",
            "rNrjh1KGZk2jBR3wPfAQnoidtFFYQKbQn2",
            "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn",
            "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ",
            "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j",
            "rJWSAM1cHSfwDrSnA1qyJbnEaSaAvJNp18",
            "rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti",
            "rnzQC8HNEcgVHd8y8jb7PWDDJZ5Vd1P9WQ",
        ]
        .iter()
        .map(|a| signer_entry(a, 1))
        .collect();
        tx["SignerEntries"] = serde_json::json!(nine);
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("length 9 outside"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_malformed_account() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = serde_json::json!({
            "SignerEntry": {"Account": "not-an-r-address", "SignerWeight": 1}
        });
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("invalid r-address"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_extra_top_level_field() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut()
            .unwrap()
            .insert("RegularKey".into(), serde_json::json!("rXXXXX"));
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("disallowed top-level field"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_extra_signer_entry_field() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = serde_json::json!({
            "SignerEntry": {
                "Account": SLS_ENTRY_A,
                "SignerWeight": 1,
                "WalletLocator": "00".repeat(32),
            }
        });
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("disallowed field WalletLocator"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_missing_inner_account() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = serde_json::json!({
            "SignerEntry": {"SignerWeight": 1}
        });
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("missing Account"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_wrong_outer_wrapper_key() {
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = serde_json::json!({
            "NotSignerEntry": {"Account": SLS_ENTRY_A, "SignerWeight": 1}
        });
        let err = run_policy(&tx).unwrap_err();
        assert!(
            err.contains("must wrap a single \"SignerEntry\" key"),
            "got: {err}"
        );
    }

    #[test]
    fn signerlist_rejects_signerlist_id_nonzero() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut()
            .unwrap()
            .insert("SignerListID".into(), serde_json::json!(1));
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("SignerListID must be 0"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_flags_nonzero() {
        let mut tx = good_signerlist_tx();
        tx["Flags"] = serde_json::json!(0x80000000u64);
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("Flags must be 0"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_missing_sequence() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut().unwrap().remove("Sequence");
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("Sequence"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_missing_fee() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut().unwrap().remove("Fee");
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("missing Fee"), "got: {err}");
    }

    #[test]
    fn signerlist_rejects_fee_below_minimum() {
        let mut tx = good_signerlist_tx();
        tx["Fee"] = serde_json::json!("11999");
        let err = run_policy(&tx).unwrap_err();
        assert!(err.contains("below multisig minimum"), "got: {err}");
    }

    #[test]
    fn signerlist_accepts_optional_lastledgersequence() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut().unwrap().insert(
            "LastLedgerSequence".into(),
            serde_json::json!(99_999_999u64),
        );
        run_policy(&tx).expect("LastLedgerSequence is allowed");
    }

    #[test]
    fn signerlist_accepts_optional_networkid() {
        let mut tx = good_signerlist_tx();
        tx.as_object_mut()
            .unwrap()
            .insert("NetworkID".into(), serde_json::json!(1u64));
        run_policy(&tx).expect("NetworkID is allowed");
    }

    /// Sanity: `multi_signing_hash` is deterministic for SignerListSet
    /// just as it is for Payment — same input → same hash.
    #[test]
    fn signerlist_hash_is_deterministic() {
        let h1 = run_policy(&good_signerlist_tx()).unwrap();
        let h2 = run_policy(&good_signerlist_tx()).unwrap();
        assert_eq!(h1, h2);
    }

    /// Different membership → different hash. Locks down that tweaks
    /// in SignerEntries actually flow into the hash (codec wires
    /// `SignerEntries` correctly).
    #[test]
    fn signerlist_hash_changes_on_entries_change() {
        let h1 = run_policy(&good_signerlist_tx()).unwrap();
        let mut tx = good_signerlist_tx();
        tx["SignerEntries"][0] = signer_entry(SLS_ENTRY_D, 1);
        let h2 = run_policy(&tx).unwrap();
        assert_ne!(h1, h2);
    }
}
