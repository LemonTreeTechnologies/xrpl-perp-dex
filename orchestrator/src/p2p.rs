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
        /// β4 Thread A (AC-β4-A1): for a `SignerListSet` the enclave's
        /// governance path REQUIRES the β1 off-chain quorum bundle proving
        /// the cluster authorised this membership epoch — the receiver
        /// forwards it verbatim to `/v1/pool/sign/governance-signerlistset`.
        /// `None` for value (`Payment`) requests, which need no bundle.
        /// `#[serde(default)]` so a message without the field still decodes.
        #[serde(default)]
        quorum_bundle: Option<String>,
    },
    Response {
        request_id: String,
        signer_xrpl_address: String,
        der_signature: Option<String>,
        compressed_pubkey: Option<String>,
        error: Option<String>,
    },
    /// REQ-8 PRG-2 part 3/4: Path A migration delegation request.
    /// Each operator's local enclave signs (domain || mrenclave_new
    /// || ceremony_nonce) with their pool account key, contributing a
    /// signature toward the M-of-N quorum that authorises the migration.
    ///
    /// X-C1 hardening parity with the XRPL Request variant: the
    /// receiver re-derives `SHA-256(domain || mrenclave_new || ceremony_nonce)`
    /// LOCALLY from the bytes carried here — never trusts a hash on
    /// the wire. domain string is fixed at `"PATHA_DELEGATION_v1"`
    /// (REQ-7 amendment 2026-05-07 (b)); mrenclave_new + ceremony_nonce
    /// are 32-byte raw bytes hex-encoded.
    ///
    /// `request_id` MUST start with the prefix `"pa-delegation-"`
    /// so Response routing on the receiver side can distinguish
    /// delegation responses (mpsc, multi-responder) from XRPL multisig
    /// responses (oneshot, single addressee).
    PathADelegationRequest {
        request_id: String,
        requester_peer_id: String,
        /// 64-char hex of the new MRENCLAVE the operator quorum is
        /// authorising for migration.
        mrenclave_new_hex: String,
        /// 64-char hex of the 32-byte ceremony_nonce.
        ceremony_nonce_hex: String,
        /// Hex of the signer's 20-byte AccountID — receiver checks
        /// this matches the local signer; mismatch → silently skip
        /// (gossipsub broadcast; not for us).
        signer_account_id_hex: String,
        signer_xrpl_address: String,
    },
    /// β1 (perp β-retrofit) off-chain membership-epoch authorisation request.
    /// Mirrors `PathADelegationRequest`: broadcast to all peers, each operator's
    /// local pool key signs over a LOCALLY re-derived
    /// `compute_membership_message_hash`. X-C1 parity: the full proposed signer
    /// set + quorum travel on the wire so every co-signer SEES the membership
    /// they authorise; the message hash is never trusted from the wire. The
    /// collected quorum bundle is the SAME wire format the enclave's
    /// `seal_verify_quorum_bundle` consumes.
    ///
    /// `request_id` MUST start with `"beta1-membership-"` so Response routing
    /// forwards to the membership collector's mpsc (multi-responder), not the
    /// XRPL multisig oneshot.
    MembershipEpochRequest {
        request_id: String,
        requester_peer_id: String,
        /// 20-byte escrow AccountID, lowercase hex (no `0x`).
        escrow_hex: String,
        proposed_epoch: u64,
        /// 32-byte hash-chain link to the CURRENT epoch, lowercase hex.
        prev_epoch_hash_hex: String,
        /// Full proposed signer set — receiver re-derives `set_hash` (X-C1).
        new_signers: Vec<MembershipSignerWire>,
        new_quorum: u32,
    },
    /// β4 Thread B — an operator-quorum request to authorise a MRENCLAVE
    /// allowlist operation (governance) OR to attest a reproducible build
    /// (repro). Same multi-responder shape as `MembershipEpochRequest`: each
    /// operator's local pool key signs over a message its OWN enclave re-derives
    /// from the STRUCTURED fields below (never a wire-supplied digest). The two
    /// `kind`s sign different messages, which is why one variant carries the
    /// union of fields and the receiver selects the enclave route by `kind`.
    ///
    /// `request_id` MUST start with `"mrenclave-gov-"` so the response router
    /// forwards replies to the governance collector's mpsc.
    MrenclaveGovernanceRequest {
        request_id: String,
        requester_peer_id: String,
        kind: MrenclaveSignKind,
        /// 32-byte target measurement, lowercase hex.
        mrenclave_hex: String,
        /// Governance only (ignored for repro): 1=add, 2=remove.
        op: u8,
        /// Governance only: the allowlist epoch this operation proposes.
        proposed_epoch: u64,
        /// Governance only: 32-byte chain link to the current allowlist head.
        prev_allowlist_hash_hex: String,
    },
    /// β3.2b apply-broadcast. After the initiator has collected ONE quorum
    /// bundle (β1) and confirmed ONE projection (β2), it broadcasts the SAME
    /// apply payload to every node, and each node applies it to its OWN loopback
    /// enclave. The enclaves are loopback-only (X-C1: `/pool/sign` is never
    /// network-exposed), so the apply CANNOT be an HTTP POST to a remote
    /// enclave — it must ride p2p, each node applying locally. This realises the
    /// cluster-wide (P) single-successor: one bundle, one broadcast, applied
    /// identically everywhere.
    ///
    /// `request_id` MUST start with `"beta-apply-"` so Response (ack) routing
    /// forwards to the apply collector's mpsc (multi-responder).
    MembershipApply {
        request_id: String,
        requester_peer_id: String,
        payload: MembershipApplyPayload,
    },
}

/// One signer in a β1 membership-epoch transition, as carried on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipSignerWire {
    /// 20-byte XRPL AccountID, lowercase hex (no `0x`).
    pub account_id_hex: String,
    pub weight: u32,
}

/// β3.2b — what a `MembershipApply` broadcast tells each node to apply to its
/// LOCAL enclave: either the β1 epoch seal or the β2 projection confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MembershipApplyPayload {
    /// Apply `ecall_seal_membership_epoch` locally (the SAME statement + bundle
    /// on every node — the (P) single-successor).
    Seal {
        escrow_hex: String,
        proposed_epoch: u64,
        prev_epoch_hash_hex: String,
        new_signers: Vec<MembershipSignerWire>,
        new_quorum: u32,
        quorum_bundle_hex: String,
    },
    /// Apply `ecall_record_projection_confirmation` locally (the validated
    /// SignerListSet projection of the just-sealed epoch).
    Confirm {
        escrow_hex: String,
        signed_xrpl_tx_blob_hex: String,
        tx_hash_hex: String,
        ledger_index: u64,
    },
    /// β4 Thread A genesis (RESP-β4-threadA-impl.1 option 2): apply
    /// `ecall_bootstrap_from_quorum_attestation` locally — seal epoch 1 from the
    /// β1 quorum attestation, with NO pre-seal XRPL SignerListSet signature
    /// (which the retired bare oracle can no longer produce).
    ///
    /// One signer set travels: at genesis the ATTESTING set IS the AUTHORITY set
    /// (the founding members attest their own founding epoch — self-authorising,
    /// which is exactly 6(e)'s incoming-quorum strength, no more). The receiver
    /// passes it as both, so the wire cannot express a mismatch.
    Bootstrap {
        escrow_hex: String,
        epoch: u64,
        prev_epoch_hash_hex: String,
        signers: Vec<MembershipSignerWire>,
        quorum: u32,
        quorum_bundle_hex: String,
    },
    /// β4 Thread B: apply `ecall_govern_trusted_mrenclaves` locally — admit or
    /// veto a measurement on the allowlist. Same reason as the others ride the
    /// apply-broadcast: the enclave admin API is loopback-only (X-C1), so the
    /// driving node cannot POST govern to remote enclaves; it broadcasts the ONE
    /// operation and every node applies it to its OWN localhost enclave + acks.
    /// `repro_bundle_hex` is empty for a veto.
    GovernMrenclave {
        // NB: field is `op_code`, not `op` — the enum's serde tag is "op".
        op_code: u8,
        mrenclave_hex: String,
        escrow_hex: String,
        proposed_epoch: u64,
        prev_allowlist_hash_hex: String,
        quorum_bundle_hex: String,
        repro_bundle_hex: String,
    },
}

/// Decode exactly 20 hex bytes → `[u8; 20]`, or `None` on any malformed input.
fn decode_20(s: &str) -> Option<[u8; 20]> {
    let b = hex::decode(s).ok()?;
    (b.len() == 20).then(|| {
        let mut a = [0u8; 20];
        a.copy_from_slice(&b);
        a
    })
}

/// Decode exactly 32 hex bytes → `[u8; 32]`, or `None` on any malformed input.
fn decode_32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s).ok()?;
    (b.len() == 32).then(|| {
        let mut a = [0u8; 32];
        a.copy_from_slice(&b);
        a
    })
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
    /// β4 Thread A (AC-β4-A1): the β1 quorum bundle authorising this membership
    /// epoch. REQUIRED for a `SignerListSet` (the enclave's governance signing
    /// path verifies it against the retained outgoing set); `None` for a
    /// `Payment`, which the value path signs without one.
    pub quorum_bundle: Option<String>,
}

/// REQ-8 PRG-2 part 3/4: outbound Path A delegation collection request.
///
/// Distinct from `SigningRelay` because delegation collection is
/// many-receivers (M-of-N operators reply concurrently), so the
/// response channel must be mpsc instead of oneshot. The
/// `LibP2PDelegationCollector` (in `path_a_delegation.rs`) constructs
/// one of these per migration ceremony, sends it down the
/// `set_path_a_delegation_channel` mpsc, and receives signed
/// delegation responses on `responses_tx` until either the timeout
/// expires or quorum is reached.
#[derive(Debug)]
pub struct PathADelegationRelay {
    /// Unique per-ceremony id; MUST start with `"pa-delegation-"`
    /// so the p2p response router knows to forward to this relay
    /// rather than the XRPL signing oneshot map.
    pub request_id: String,
    pub mrenclave_new: [u8; 32],
    pub ceremony_nonce: [u8; 32],
    /// Channel to receive `SigningMessage::Response` instances as
    /// peers reply. The collector closes the receive end when it has
    /// enough responses or the timeout fires.
    pub responses_tx: tokio::sync::mpsc::Sender<SigningMessage>,
}

/// β1 (perp β-retrofit) outbound membership-epoch collection request.
///
/// Same multi-receiver shape as `PathADelegationRelay` (M-of-N operators
/// reply concurrently → mpsc, not oneshot). The `MembershipBundleCollector`
/// (in `membership_coordinator.rs`) builds one per ceremony from a prepared
/// `MembershipEpochStatement`, sends it down `set_membership_epoch_channel`,
/// and receives signed responses on `responses_tx` until quorum or timeout.
#[derive(Debug)]
pub struct MembershipEpochRelay {
    /// Unique per-ceremony id; MUST start with `"beta1-membership-"` so the
    /// p2p response router forwards replies here, not to the XRPL oneshot map.
    pub request_id: String,
    pub escrow: [u8; 20],
    pub proposed_epoch: u64,
    pub prev_epoch_hash: [u8; 32],
    pub new_signers: Vec<crate::membership_canonical::SignerEntry>,
    pub new_quorum: u32,
    /// Channel to receive `SigningMessage::Response` instances as peers reply.
    pub responses_tx: tokio::sync::mpsc::Sender<SigningMessage>,
}

/// β4 Thread B — which message a `MrenclaveGovernanceRequest` asks the receiver
/// to sign. The two sign DIFFERENT preimages on different enclave routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MrenclaveSignKind {
    /// Sign {op, mrenclave, epoch, prev_allowlist_hash} — the cluster's
    /// authorisation of the allowlist operation.
    Governance,
    /// Sign the measurement alone — "I rebuilt this binary bit-identically".
    Repro,
}

/// β4 Thread B outbound governance/repro collection request. Same multi-receiver
/// shape as `MembershipEpochRelay`; the `LibP2PGovernanceBundleCollector` builds
/// one per operation, sends it down `set_mrenclave_governance_channel`, and
/// receives signed responses until quorum or timeout. `op`/`proposed_epoch`/
/// `prev_allowlist_hash` are unused for `kind == Repro`.
#[derive(Debug)]
pub struct MrenclaveGovernanceRelay {
    /// Unique id; MUST start with `"mrenclave-gov-"` so the response router
    /// forwards replies here, not to the XRPL oneshot map.
    pub request_id: String,
    pub kind: MrenclaveSignKind,
    pub mrenclave: [u8; 32],
    pub op: u8,
    pub proposed_epoch: u64,
    pub prev_allowlist_hash: [u8; 32],
    /// Channel to receive `SigningMessage::Response` instances as peers reply.
    pub responses_tx: tokio::sync::mpsc::Sender<SigningMessage>,
}

/// β3.2b outbound apply-broadcast. The membership-change driver builds one per
/// apply step (seal or confirm), sends it down `set_membership_apply_channel`;
/// the run-loop applies locally + broadcasts `MembershipApply` and forwards each
/// node's ack `Response` here until all expected nodes ack or the window closes.
#[derive(Debug)]
pub struct MembershipApplyRelay {
    /// Unique id; MUST start with `"beta-apply-"` so ack routing forwards here.
    pub request_id: String,
    pub payload: MembershipApplyPayload,
    /// Channel to receive per-node ack `Response`s.
    pub responses_tx: tokio::sync::mpsc::Sender<SigningMessage>,
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

/// Canonicalize a session key to bare lowercase hex (strip an optional `0x`).
/// GEN-3-R1 (RESP-β4-B-genesis-impl): the single normalization point. The enclave
/// has two decoders — `from_hex` (bare-only; the typed-sign routes, where a `0x`
/// mis-lengths the 32-byte key, fails `typed_sign_preamble`, and stalled genesis
/// with "refused (malformed set or invalid input)") and `hex_str_to_bytes`
/// (0x-tolerant; /pool/sign). Bare hex satisfies BOTH, so canonicalizing to bare
/// is always safe. Config / node-bootstrap op-JSON store session_key 0x-prefixed.
pub fn canonical_session_key(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

impl LocalSigner {
    /// The session key as bare hex for the enclave's typed-sign routes. Idempotent:
    /// `set_local_signer` already canonicalizes at the boundary, so this is a
    /// documenting accessor (defense in depth) over [`canonical_session_key`].
    pub fn session_key_hex(&self) -> &str {
        canonical_session_key(&self.session_key)
    }
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
    /// REQ-8 PRG-2 part 3/4: outbound Path A delegation collection
    /// requests from the migration ceremony driver.
    path_a_delegation_rx: Option<mpsc::Receiver<PathADelegationRelay>>,
    /// In-flight Path A delegation requests; mpsc per-request because
    /// many peers reply concurrently (M-of-N quorum collection).
    pending_path_a_delegation: HashMap<String, tokio::sync::mpsc::Sender<SigningMessage>>,
    /// β1: outbound membership-epoch collection requests from the ceremony
    /// driver (`MembershipBundleCollector`).
    membership_epoch_rx: Option<mpsc::Receiver<MembershipEpochRelay>>,
    /// In-flight β1 membership-epoch requests; mpsc per-request (M-of-N
    /// operators reply concurrently), mirroring `pending_path_a_delegation`.
    pending_membership_epoch: HashMap<String, tokio::sync::mpsc::Sender<SigningMessage>>,
    // β4 Thread B — governed MRENCLAVE allowlist collection
    mrenclave_governance_rx: Option<mpsc::Receiver<MrenclaveGovernanceRelay>>,
    pending_mrenclave_governance: HashMap<String, tokio::sync::mpsc::Sender<SigningMessage>>,
    /// β3.2b: outbound apply-broadcast requests (seal / confirm) from the
    /// membership-change driver — each node applies to its loopback enclave.
    membership_apply_rx: Option<mpsc::Receiver<MembershipApplyRelay>>,
    /// In-flight β3.2b apply broadcasts; mpsc per-request (every node acks).
    pending_membership_apply: HashMap<String, tokio::sync::mpsc::Sender<SigningMessage>>,
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
    /// F-5-P2P-M1 (perp RESP-5): replay guard for DKG-step messages.
    /// Keyed on `(ceremony_id, type_tag, optional pid)` → first-seen
    /// timestamp. Bounds: an attacker who compromises one operator host
    /// can replay a captured DkgStepMessage from a prior ceremony. Without
    /// the guard they could re-trigger expensive enclave work on every
    /// follower; with the guard the replay is dropped at the dispatch
    /// layer before reaching `run_follower`.
    recent_dkg_step_keys: HashMap<String, Instant>,
    /// F-5-P2P-M1 (perp RESP-5): per-peer rate limiter for DKG-step
    /// messages. Same shape as signing_request_rate; protects against
    /// a single peer flooding the topic.
    dkg_step_rate: HashMap<PeerId, VecDeque<Instant>>,
    /// F-5-P2P-L3 (perp RESP-5): replay guard for share-v2 envelope
    /// deliveries. Keyed on (group_id, signer_id, ceremony_nonce-prefix).
    recent_share_v2_keys: HashMap<String, Instant>,
    /// F-5-P2P-L3 (perp RESP-5): per-peer rate limiter for share-v2.
    share_v2_rate: HashMap<PeerId, VecDeque<Instant>>,
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

/// F-5-P2P-M1 / F-5-P2P-L3 tunables (perp RESP-5). DKG and share-v2
/// traffic is naturally bursty (one ceremony = one wave of messages,
/// then quiet) — so the replay TTL is longer than a typical ceremony
/// (which completes in 6-30 seconds) and the rate limit is generous
/// enough not to block legitimate ceremony traffic.
const DKG_STEP_REPLAY_TTL: Duration = Duration::from_secs(30 * 60);
const DKG_STEP_RATE_WINDOW: Duration = Duration::from_secs(60);
const DKG_STEP_RATE_MAX_PER_WINDOW: usize = 60;
const SHARE_V2_REPLAY_TTL: Duration = Duration::from_secs(30 * 60);
const SHARE_V2_RATE_WINDOW: Duration = Duration::from_secs(60);
const SHARE_V2_RATE_MAX_PER_WINDOW: usize = 60;

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
            path_a_delegation_rx: None,
            pending_path_a_delegation: HashMap::new(),
            membership_epoch_rx: None,
            pending_membership_epoch: HashMap::new(),
            mrenclave_governance_rx: None,
            pending_mrenclave_governance: HashMap::new(),
            membership_apply_rx: None,
            pending_membership_apply: HashMap::new(),
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
            recent_dkg_step_keys: HashMap::new(),
            dkg_step_rate: HashMap::new(),
            recent_share_v2_keys: HashMap::new(),
            share_v2_rate: HashMap::new(),
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

    /// REQ-8 PRG-2 part 3/4: wire the migration ceremony driver's
    /// delegation-collection channel. Caller (the admin migrate-state
    /// endpoint in PRG-2 part 4/4) sends one `PathADelegationRelay`
    /// per ceremony; the p2p run-loop publishes the corresponding
    /// `PathADelegationRequest` on the signing topic and forwards each
    /// peer's `Response` back via the relay's `responses_tx`.
    ///
    /// `#[allow(dead_code)]`: consumer (admin endpoint composing a
    /// `ComposedEnclaveApi<HttpEnclaveApi, LibP2PDelegationCollector>`)
    /// lands in PRG-2 part 4/4. Until then cargo's dead-code analysis
    /// flags this setter as unused — it's not, just not yet wired.
    #[allow(dead_code)]
    pub fn set_path_a_delegation_channel(&mut self, rx: mpsc::Receiver<PathADelegationRelay>) {
        self.path_a_delegation_rx = Some(rx);
    }

    /// β1: wire the membership-epoch ceremony driver's collection channel.
    /// Caller (the admin change-membership endpoint) sends one
    /// `MembershipEpochRelay` per ceremony; the run-loop publishes the
    /// corresponding `MembershipEpochRequest` on the signing topic and forwards
    /// each peer's `Response` back via the relay's `responses_tx`.
    ///
    /// `#[allow(dead_code)]`: the admin endpoint consumer lands in #119(d).
    #[allow(dead_code)]
    pub fn set_membership_epoch_channel(&mut self, rx: mpsc::Receiver<MembershipEpochRelay>) {
        self.membership_epoch_rx = Some(rx);
    }

    /// β4 Thread B: the governance/repro collection channel. Wired in main.rs
    /// with the operator trigger route (the next increment; same deferred
    /// deploy-wiring split β1's set_membership_epoch_channel used).
    #[allow(dead_code)]
    pub fn set_mrenclave_governance_channel(
        &mut self,
        rx: mpsc::Receiver<MrenclaveGovernanceRelay>,
    ) {
        self.mrenclave_governance_rx = Some(rx);
    }

    /// β3.2b: wire the membership-change driver's apply-broadcast channel. The
    /// driver sends one `MembershipApplyRelay` per apply step (seal / confirm);
    /// the run-loop applies it to the LOCAL enclave + broadcasts `MembershipApply`
    /// and forwards each node's ack `Response` back via `responses_tx`.
    #[allow(dead_code)]
    pub fn set_membership_apply_channel(&mut self, rx: mpsc::Receiver<MembershipApplyRelay>) {
        self.membership_apply_rx = Some(rx);
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
    pub fn set_local_signer(&mut self, mut signer: LocalSigner) {
        // GEN-3-R1 (RESP-β4-B-genesis-impl): canonicalize session_key to bare hex
        // at this SINGLE boundary, so a future `from_hex` route can't re-introduce
        // the 0-consents stall even via a direct `.session_key` read. See
        // [`canonical_session_key`].
        signer.session_key = canonical_session_key(&signer.session_key).to_string();
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
    /// **Not wired into `main.rs` (perp RESP-5 §C1 acknowledged):** the
    /// REQ-5 §1 row X-C1 claim that "peer allowlist + replay guard +
    /// per-peer rate limit all live on the dispatch path" was partially
    /// false at HEAD because this method had no caller. The four
    /// load-bearing defenses against the published X-C1 attack —
    /// (1) hash re-derivation from the unsigned tx, (2) per-tx-type
    /// policy with source-account binding, (3) replay guard,
    /// (4) per-peer rate limit — remain in place and are sufficient
    /// against the published PoC.
    ///
    /// Wiring the allowlist requires plumbing libp2p peer IDs through
    /// operator config (no deterministic mapping from `xrpl_address` to
    /// peer_id — they are independent identities). Two paths to wire:
    /// (a) extend `SignerEntry` schema with `libp2p_peer_id`, populated
    /// during `node-bootstrap` and discoverable via on-chain `Domain`
    /// or a new XRPL field; (b) collect peer_ids from authenticated
    /// peer-quote announcements over time and union them into the
    /// allowlist. Both are non-trivial. Tracked as separate work item.
    ///
    /// `#[allow(dead_code)]` is intentional and explicit (not the
    /// previously-misleading "feature pending" attribute): the function
    /// is preserved as the wiring entry point for the future allowlist
    /// implementation. The `#[allow]` documents the choice rather than
    /// hiding it.
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

    /// F-5-P2P-M1 (perp RESP-5): record a DKG-step message key as seen.
    /// Returns `false` if the same key was already in the window (replay).
    /// Key format: `<ceremony_id>:<type_tag>[:<pid>]`. Cleans entries
    /// older than `DKG_STEP_REPLAY_TTL` on insertion.
    fn mark_dkg_step_fresh(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.recent_dkg_step_keys
            .retain(|_, seen| now.duration_since(*seen) < DKG_STEP_REPLAY_TTL);
        if self.recent_dkg_step_keys.contains_key(key) {
            return false;
        }
        self.recent_dkg_step_keys.insert(key.to_string(), now);
        true
    }

    /// F-5-P2P-M1: per-peer rate limit on DKG-step inbound. Same shape
    /// as `check_signing_rate` but for the dkg-step topic.
    fn check_dkg_step_rate(&mut self, peer: &PeerId) -> bool {
        let now = Instant::now();
        let q = self.dkg_step_rate.entry(*peer).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= DKG_STEP_RATE_WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= DKG_STEP_RATE_MAX_PER_WINDOW {
            return false;
        }
        q.push_back(now);
        true
    }

    /// F-5-P2P-L3 (perp RESP-5): replay guard for share-v2 envelopes.
    fn mark_share_v2_fresh(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.recent_share_v2_keys
            .retain(|_, seen| now.duration_since(*seen) < SHARE_V2_REPLAY_TTL);
        if self.recent_share_v2_keys.contains_key(key) {
            return false;
        }
        self.recent_share_v2_keys.insert(key.to_string(), now);
        true
    }

    /// F-5-P2P-L3: per-peer rate limit on share-v2 inbound.
    fn check_share_v2_rate(&mut self, peer: &PeerId) -> bool {
        let now = Instant::now();
        let q = self.share_v2_rate.entry(*peer).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= SHARE_V2_RATE_WINDOW {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= SHARE_V2_RATE_MAX_PER_WINDOW {
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
    /// REQ-8 PRG-2 part 3/4: receiver-side handler for Path A delegation
    /// signing requests. Re-derives the canonical message hash LOCALLY
    /// from the carried bytes (X-C1 parity with handle_signing_request),
    /// then routes to the local enclave's `/pool/sign` to obtain the
    /// per-operator-key signature.
    ///
    /// Hash construction (REQ-7 amendment 2026-05-07 (b)):
    ///   message_hash = SHA-256(
    ///     "PATHA_DELEGATION_v1"  // 19 ASCII bytes, no terminator
    ///     || mrenclave_new (32)
    ///     || ceremony_nonce (32)
    ///   )
    ///
    /// The domain separator + raw bytes ensure the operator's per-key
    /// signature CANNOT be replayed as an XRPL transaction or any other
    /// context — distinct from XRPL's TXN/SMT/STX prefixes.
    async fn handle_path_a_delegation_request(
        local_signer: &LocalSigner,
        request_id: &str,
        mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
    ) -> SigningMessage {
        // β4 Thread A site D (RESP-β4 AC-β4-A2): the ENCLAVE re-derives the
        // delegation cover SHA-256("PATHA_DELEGATION_v1" || mrenclave_new ||
        // ceremony_nonce) from these raw bytes. We no longer compute a hash here
        // and hand it to a bare signing oracle — that oracle now REFUSES the
        // escrow-role key. X-C1's "never trust a hash on the wire" is therefore
        // enforced inside the enclave, not merely re-derived by the (untrusted)
        // orchestrator.
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

        let sign_url = format!("{}/pool/sign/patha-delegation", local_signer.enclave_url);
        let resp = http
            .post(&sign_url)
            .json(&serde_json::json!({
                "from": local_signer.address,
                "session_key": local_signer.session_key_hex(),
                "mrenclave_new": hex::encode(mrenclave_new),
                "ceremony_nonce": hex::encode(ceremony_nonce),
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
            compressed_pubkey: Some(local_signer.compressed_pubkey.clone()),
            error: None,
        }
    }

    /// β1: sign an off-chain membership-epoch transition locally. X-C1: the
    /// `message_hash` is RE-DERIVED here from the full proposed set + quorum +
    /// chain-link carried on the wire — never trusted as a hash. The local pool
    /// key signs the same 32-byte digest `ecall_seal_membership_epoch` will
    /// reconstruct and verify the collected bundle against.
    async fn handle_membership_epoch_request(
        local_signer: &LocalSigner,
        request_id: &str,
        escrow: &[u8; 20],
        proposed_epoch: u64,
        prev_epoch_hash: &[u8; 32],
        new_signers: &[crate::membership_canonical::SignerEntry],
        new_quorum: u32,
    ) -> SigningMessage {
        // β4 Thread A site C (RESP-β4 AC-β4-A2): the ENCLAVE re-derives the
        // domain-separated consent hash itself (compute_membership_message_hash
        // over compute_set_hash) from the structured fields below. We no longer
        // compute a digest here and hand it to a bare signing oracle — that oracle
        // now REFUSES the escrow-role key. X-C1's "never trust a hash on the wire"
        // is thus enforced inside the enclave, not merely re-derived by the
        // (untrusted) orchestrator.
        //
        // `signers` goes as a JSON array of {account_id, weight} — identical to the
        // seal-epoch body — and the enclave server packs it via pack_signers(), so
        // the sealed_signer_entry_t layout never crosses the language boundary.
        let signers_json: Vec<serde_json::Value> = new_signers
            .iter()
            .map(|s| {
                serde_json::json!({
                    "account_id": hex::encode(s.account_id),
                    "weight": s.weight,
                })
            })
            .collect();

        let http = match crate::http_helpers::loopback_http_client(Duration::from_secs(15)) {
            Ok(c) => c,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("http client: {e}"),
                )
            }
        };

        let sign_url = format!("{}/admin/signerlist/sign-consent", local_signer.enclave_url);
        let resp = http
            .post(&sign_url)
            .json(&serde_json::json!({
                "from": local_signer.address,
                "session_key": local_signer.session_key_hex(),
                "escrow_account_id": hex::encode(escrow),
                "signers": signers_json,
                "quorum_threshold": new_quorum,
                "proposed_epoch": proposed_epoch,
                "prev_epoch_hash": hex::encode(prev_epoch_hash),
            }))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("enclave request: {e}"),
                )
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("enclave response parse: {e}"),
                )
            }
        };
        if body["status"].as_str() != Some("success") {
            return Self::membership_sign_error(
                local_signer,
                request_id,
                format!("enclave: {}", body.get("message").unwrap_or(&body)),
            );
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
            compressed_pubkey: Some(local_signer.compressed_pubkey.clone()),
            error: None,
        }
    }

    /// β4 Thread B: sign a governance operation or a reproducible-build
    /// attestation on the LOCAL enclave's typed route. Which route (and which
    /// body) is selected by `kind`; the enclave re-derives the domain-separated
    /// message from these structured fields, so nothing is trusted from the wire.
    /// Reuses `membership_sign_error` — the Response shape is identical.
    #[allow(clippy::too_many_arguments)]
    async fn handle_mrenclave_governance_request(
        local_signer: &LocalSigner,
        request_id: &str,
        kind: MrenclaveSignKind,
        mrenclave: &[u8; 32],
        op: u8,
        proposed_epoch: u64,
        prev_allowlist_hash: &[u8; 32],
    ) -> SigningMessage {
        let http = match crate::http_helpers::loopback_http_client(Duration::from_secs(15)) {
            Ok(c) => c,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("http client: {e}"),
                )
            }
        };

        let (path, body) = match kind {
            MrenclaveSignKind::Governance => (
                "/admin/mrenclaves/sign-governance",
                serde_json::json!({
                    "from": local_signer.address,
                    "session_key": local_signer.session_key_hex(),
                    "op": op,
                    "mrenclave": hex::encode(mrenclave),
                    "proposed_epoch": proposed_epoch,
                    "prev_allowlist_hash": hex::encode(prev_allowlist_hash),
                }),
            ),
            MrenclaveSignKind::Repro => (
                "/admin/mrenclaves/sign-repro-proof",
                serde_json::json!({
                    "from": local_signer.address,
                    "session_key": local_signer.session_key_hex(),
                    "mrenclave": hex::encode(mrenclave),
                }),
            ),
        };

        let sign_url = format!("{}{}", local_signer.enclave_url, path);
        let resp = match http.post(&sign_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("enclave request: {e}"),
                )
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("enclave response parse: {e}"),
                )
            }
        };
        if body["status"].as_str() != Some("success") {
            return Self::membership_sign_error(
                local_signer,
                request_id,
                format!("enclave: {}", body.get("message").unwrap_or(&body)),
            );
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
            compressed_pubkey: Some(local_signer.compressed_pubkey.clone()),
            error: None,
        }
    }

    /// Uniform error `Response` for the β1 membership signing path.
    fn membership_sign_error(
        local_signer: &LocalSigner,
        request_id: &str,
        msg: String,
    ) -> SigningMessage {
        SigningMessage::Response {
            request_id: request_id.to_string(),
            signer_xrpl_address: local_signer.xrpl_address.clone(),
            der_signature: None,
            compressed_pubkey: None,
            error: Some(msg),
        }
    }

    /// β3.2b: apply a `MembershipApply` broadcast to THIS node's LOCAL enclave
    /// (the enclaves are loopback-only). Reuses the audited HTTP adapters pointed
    /// at localhost. The ack is a `Response` with `error: None` on success (the
    /// apply collector treats `error.is_none()` as applied; distinct from the
    /// bundle collector which reads `der_signature`).
    async fn handle_membership_apply(
        local_signer: &LocalSigner,
        request_id: &str,
        payload: &MembershipApplyPayload,
    ) -> SigningMessage {
        use crate::membership_canonical::SignerEntry;
        use crate::membership_coordinator::{
            prepare_statement, EpochSealSink, GenesisBootstrapSink,
        };
        use crate::membership_http::{HttpEpochSealSink, HttpProjectionConfirmer};
        use crate::membership_projection::ProjectionConfirmer;

        // local enclave admin base. local_signer.enclave_url ends in "/v1"; the
        // membership_http paths re-add "/v1", so strip it to the bare base.
        let base = local_signer
            .enclave_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .trim_end_matches('/')
            .to_string();
        // X-C1 defense-in-depth: apply ONLY ever targets THIS node's loopback
        // enclave. A non-loopback base means a misconfigured local_signer — fail
        // closed rather than POST a seal/confirm to a remote enclave.
        if let Err(e) = crate::http_helpers::ensure_loopback_url(&base) {
            return Self::membership_sign_error(
                local_signer,
                request_id,
                format!("apply: non-loopback enclave base: {e}"),
            );
        }
        let client = match crate::http_helpers::loopback_http_client(Duration::from_secs(30)) {
            Ok(c) => c,
            Err(e) => {
                return Self::membership_sign_error(
                    local_signer,
                    request_id,
                    format!("http client: {e}"),
                )
            }
        };

        let result: anyhow::Result<()> = match payload {
            MembershipApplyPayload::Seal {
                escrow_hex,
                proposed_epoch,
                prev_epoch_hash_hex,
                new_signers,
                new_quorum,
                quorum_bundle_hex,
            } => {
                let escrow = match decode_20(escrow_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.seal: bad escrow_hex".into(),
                        )
                    }
                };
                let prev = match decode_32(prev_epoch_hash_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.seal: bad prev_epoch_hash_hex".into(),
                        )
                    }
                };
                let mut signers = Vec::with_capacity(new_signers.len());
                for s in new_signers {
                    match decode_20(&s.account_id_hex) {
                        Some(account_id) => signers.push(SignerEntry {
                            account_id,
                            weight: s.weight,
                        }),
                        None => {
                            return Self::membership_sign_error(
                                local_signer,
                                request_id,
                                "apply.seal: bad signer account_id_hex".into(),
                            )
                        }
                    }
                }
                let bundle = match hex::decode(quorum_bundle_hex) {
                    Ok(b) => b,
                    Err(_) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.seal: bad quorum_bundle_hex".into(),
                        )
                    }
                };
                // Reconstruct the statement (proposed = current+1 ⇒ current =
                // proposed-1, current_digest = prev_epoch_hash).
                let statement = match prepare_statement(
                    escrow,
                    proposed_epoch.saturating_sub(1),
                    prev,
                    signers,
                    *new_quorum,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            format!("apply.seal: prepare {e:?}"),
                        )
                    }
                };
                HttpEpochSealSink::new(client)
                    .seal_on_node(&base, &statement, &bundle)
                    .await
            }
            MembershipApplyPayload::Bootstrap {
                escrow_hex,
                epoch,
                prev_epoch_hash_hex,
                signers,
                quorum,
                quorum_bundle_hex,
            } => {
                let escrow = match decode_20(escrow_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.bootstrap: bad escrow_hex".into(),
                        )
                    }
                };
                let prev = match decode_32(prev_epoch_hash_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.bootstrap: bad prev_epoch_hash_hex".into(),
                        )
                    }
                };
                let mut entries = Vec::with_capacity(signers.len());
                for s in signers {
                    match decode_20(&s.account_id_hex) {
                        Some(account_id) => entries.push(SignerEntry {
                            account_id,
                            weight: s.weight,
                        }),
                        None => {
                            return Self::membership_sign_error(
                                local_signer,
                                request_id,
                                "apply.bootstrap: bad signer account_id_hex".into(),
                            )
                        }
                    }
                }
                let bundle = match hex::decode(quorum_bundle_hex) {
                    Ok(b) => b,
                    Err(_) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.bootstrap: bad quorum_bundle hex".into(),
                        )
                    }
                };
                // X-C1: re-derive the statement LOCALLY from the carried fields
                // (same discipline as the seal arm) — the founding epoch is 1, so
                // current_epoch = epoch-1 = 0 and current_digest = prev (zero).
                let statement = match prepare_statement(
                    escrow,
                    epoch.saturating_sub(1),
                    prev,
                    entries,
                    *quorum,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            format!("apply.bootstrap: prepare {e:?}"),
                        )
                    }
                };
                crate::membership_http::HttpGenesisBootstrapSink::new(client)
                    .bootstrap_on_node(&base, &statement, &bundle)
                    .await
            }
            MembershipApplyPayload::GovernMrenclave {
                op_code,
                mrenclave_hex,
                escrow_hex,
                proposed_epoch,
                prev_allowlist_hash_hex,
                quorum_bundle_hex,
                repro_bundle_hex,
            } => {
                let mrenclave = match decode_32(mrenclave_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.govern: bad mrenclave_hex".into(),
                        )
                    }
                };
                let escrow = match decode_20(escrow_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.govern: bad escrow_hex".into(),
                        )
                    }
                };
                let prev = match decode_32(prev_allowlist_hash_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.govern: bad prev_allowlist_hash_hex".into(),
                        )
                    }
                };
                let quorum_bundle = match hex::decode(quorum_bundle_hex) {
                    Ok(b) => b,
                    Err(_) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.govern: bad quorum_bundle hex".into(),
                        )
                    }
                };
                let repro_bundle = match hex::decode(repro_bundle_hex) {
                    Ok(b) => b,
                    Err(_) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.govern: bad repro_bundle hex".into(),
                        )
                    }
                };
                let govop = crate::mrenclave_governance::GovernanceOp {
                    op: *op_code,
                    mrenclave,
                    escrow,
                    proposed_epoch: *proposed_epoch,
                    prev_allowlist_hash: prev,
                };
                crate::membership_http::HttpGovernSink::new(client)
                    .govern_on_node(&base, &govop, &quorum_bundle, &repro_bundle)
                    .await
            }
            MembershipApplyPayload::Confirm {
                escrow_hex,
                signed_xrpl_tx_blob_hex,
                tx_hash_hex,
                ledger_index,
            } => {
                let escrow = match decode_20(escrow_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.confirm: bad escrow_hex".into(),
                        )
                    }
                };
                let tx_hash = match decode_32(tx_hash_hex) {
                    Some(a) => a,
                    None => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.confirm: bad tx_hash_hex".into(),
                        )
                    }
                };
                let blob = match hex::decode(signed_xrpl_tx_blob_hex) {
                    Ok(b) => b,
                    Err(_) => {
                        return Self::membership_sign_error(
                            local_signer,
                            request_id,
                            "apply.confirm: bad blob hex".into(),
                        )
                    }
                };
                HttpProjectionConfirmer::new(client)
                    .record_confirmation(&base, &escrow, &blob, &tx_hash, *ledger_index)
                    .await
            }
        };

        match result {
            Ok(()) => SigningMessage::Response {
                request_id: request_id.to_string(),
                signer_xrpl_address: local_signer.xrpl_address.clone(),
                der_signature: None,
                compressed_pubkey: None,
                error: None, // ack: applied
            },
            Err(e) => {
                Self::membership_sign_error(local_signer, request_id, format!("apply: {e:#}"))
            }
        }
    }

    async fn handle_signing_request(
        local_signer: &LocalSigner,
        escrow_xrpl_address: Option<&str>,
        request_id: &str,
        unsigned_tx: &serde_json::Value,
        signer_account_id_hex: &str,
        quorum_bundle: Option<&str>,
    ) -> SigningMessage {
        let reject = |msg: String| SigningMessage::Response {
            request_id: request_id.to_string(),
            signer_xrpl_address: local_signer.xrpl_address.clone(),
            der_signature: None,
            compressed_pubkey: None,
            error: Some(msg),
        };

        // The orchestrator-side policy stays as the first line of defence (fail
        // fast, with a precise reason). Its derived hash is no longer sent
        // anywhere: since β4 Thread A the ENCLAVE re-derives the signing hash
        // from the blob below, and the bare hash-signing oracle refuses the
        // escrow-role key outright (AC-β4-A2).
        if let Err(e) = Self::validate_signing_policy(
            local_signer,
            escrow_xrpl_address,
            unsigned_tx,
            signer_account_id_hex,
        ) {
            warn!(req_id = %request_id, error = %e, "X-C1: signing request rejected by policy");
            return reject(format!("policy: {e}"));
        }

        // Serialise the transaction exactly as the codec does inside
        // multi_signing_data (for_signing = true). This is the preimage the
        // enclave hashes with the SMT prefix and its OWN AccountID — we send the
        // preimage, never a digest.
        let tx_obj = match unsigned_tx.as_object() {
            Some(o) => o,
            None => return reject("unsigned_tx is not a JSON object".to_string()),
        };
        let mut blob = Vec::new();
        if let Err(e) =
            xrpl_mithril_codec::serializer::serialize_json_object(tx_obj, &mut blob, true)
        {
            return reject(format!("serialise for signing failed: {e:?}"));
        }
        let tx_blob_hex = hex::encode(&blob);

        // Route by transaction type: value and governance are DISTINCT enclave
        // paths (AC-β4-A1), and the governance path additionally requires the β1
        // quorum bundle authorising this membership epoch.
        let tx_type = tx_obj
            .get("TransactionType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let (sign_path, sign_body) = match tx_type {
            "Payment" => (
                "/pool/sign/withdrawal-payment",
                serde_json::json!({
                    "from": local_signer.address,
                    "session_key": local_signer.session_key_hex(),
                    "tx_blob": tx_blob_hex,
                }),
            ),
            "SignerListSet" => {
                let Some(bundle) = quorum_bundle else {
                    return reject(
                        "SignerListSet request carries no quorum_bundle — the enclave \
                         governance path requires the β1 bundle for this epoch"
                            .to_string(),
                    );
                };
                (
                    "/pool/sign/governance-signerlistset",
                    serde_json::json!({
                        "from": local_signer.address,
                        "session_key": local_signer.session_key_hex(),
                        "tx_blob": tx_blob_hex,
                        "quorum_bundle": bundle,
                    }),
                )
            }
            other => return reject(format!("disallowed TransactionType: {other}")),
        };
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

        let sign_url = format!("{}{}", local_signer.enclave_url, sign_path);
        let resp = http.post(&sign_url).json(&sign_body).send().await;

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
        let mut path_a_delegation_rx = self.path_a_delegation_rx.take();
        let mut membership_epoch_rx = self.membership_epoch_rx.take();
        let mut mrenclave_governance_rx = self.mrenclave_governance_rx.take();
        let mut membership_apply_rx = self.membership_apply_rx.take();
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
                                relay.quorum_bundle.as_deref(),
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
                        quorum_bundle: relay.quorum_bundle,
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

                // REQ-8 PRG-2 part 3/4: Path A delegation collection
                Some(relay) = async {
                    match &mut path_a_delegation_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<PathADelegationRelay>>().await,
                    }
                } => {
                    // If our local signer is one of the operators, sign locally too
                    // (gossipsub doesn't deliver to the publisher).
                    if let Some(ref local) = self.local_signer {
                        let local_response = Self::handle_path_a_delegation_request(
                            local,
                            &relay.request_id,
                            &relay.mrenclave_new,
                            &relay.ceremony_nonce,
                        ).await;
                        let _ = relay.responses_tx.send(local_response).await;
                    }

                    let msg = SigningMessage::PathADelegationRequest {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        mrenclave_new_hex: hex::encode(relay.mrenclave_new),
                        ceremony_nonce_hex: hex::encode(relay.ceremony_nonce),
                        // Empty signer fields broadcast to all peers; each
                        // peer's local signer is the addressee. The X-C1
                        // policy gate is in handle_path_a_delegation_request
                        // (re-derives hash locally; never trusts wire).
                        signer_account_id_hex: String::new(),
                        signer_xrpl_address: String::new(),
                    };
                    match self.publish_signing(&msg) {
                        Ok(_) => {
                            self.pending_path_a_delegation.insert(
                                relay.request_id, relay.responses_tx);
                        }
                        Err(e) => {
                            warn!("path-a delegation publish failed: {}", e);
                            let _ = relay.responses_tx.send(SigningMessage::Response {
                                request_id: relay.request_id,
                                signer_xrpl_address: String::new(),
                                der_signature: None,
                                compressed_pubkey: None,
                                error: Some(format!("P2P publish failed: {e}")),
                            }).await;
                        }
                    }
                }

                // β1: membership-epoch collection (mirrors delegation arm)
                Some(relay) = async {
                    match &mut membership_epoch_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<MembershipEpochRelay>>().await,
                    }
                } => {
                    // Local signer is also a co-signer; gossipsub doesn't
                    // deliver to the publisher, so sign locally too.
                    if let Some(ref local) = self.local_signer {
                        let local_response = Self::handle_membership_epoch_request(
                            local,
                            &relay.request_id,
                            &relay.escrow,
                            relay.proposed_epoch,
                            &relay.prev_epoch_hash,
                            &relay.new_signers,
                            relay.new_quorum,
                        ).await;
                        let _ = relay.responses_tx.send(local_response).await;
                    }

                    let new_signers_wire: Vec<MembershipSignerWire> = relay
                        .new_signers
                        .iter()
                        .map(|s| MembershipSignerWire {
                            account_id_hex: hex::encode(s.account_id),
                            weight: s.weight,
                        })
                        .collect();
                    let msg = SigningMessage::MembershipEpochRequest {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        escrow_hex: hex::encode(relay.escrow),
                        proposed_epoch: relay.proposed_epoch,
                        prev_epoch_hash_hex: hex::encode(relay.prev_epoch_hash),
                        new_signers: new_signers_wire,
                        new_quorum: relay.new_quorum,
                    };
                    match self.publish_signing(&msg) {
                        Ok(_) => {
                            self.pending_membership_epoch.insert(
                                relay.request_id, relay.responses_tx);
                        }
                        Err(e) => {
                            warn!("β1 membership-epoch publish failed: {}", e);
                            let _ = relay.responses_tx.send(SigningMessage::Response {
                                request_id: relay.request_id,
                                signer_xrpl_address: String::new(),
                                der_signature: None,
                                compressed_pubkey: None,
                                error: Some(format!("P2P publish failed: {e}")),
                            }).await;
                        }
                    }
                }

                // β4 Thread B: broadcast a governance/repro signing request to the
                // operator quorum; each signs on its OWN enclave typed route.
                Some(relay) = async {
                    match &mut mrenclave_governance_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<MrenclaveGovernanceRelay>>().await,
                    }
                } => {
                    // Local signer is a co-signer too; gossipsub doesn't deliver
                    // to the publisher, so sign locally as well.
                    if let Some(ref local) = self.local_signer {
                        let local_response = Self::handle_mrenclave_governance_request(
                            local,
                            &relay.request_id,
                            relay.kind,
                            &relay.mrenclave,
                            relay.op,
                            relay.proposed_epoch,
                            &relay.prev_allowlist_hash,
                        ).await;
                        let _ = relay.responses_tx.send(local_response).await;
                    }

                    let msg = SigningMessage::MrenclaveGovernanceRequest {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        kind: relay.kind,
                        mrenclave_hex: hex::encode(relay.mrenclave),
                        op: relay.op,
                        proposed_epoch: relay.proposed_epoch,
                        prev_allowlist_hash_hex: hex::encode(relay.prev_allowlist_hash),
                    };
                    match self.publish_signing(&msg) {
                        Ok(_) => {
                            self.pending_mrenclave_governance.insert(
                                relay.request_id, relay.responses_tx);
                        }
                        Err(e) => {
                            warn!("β4 mrenclave-governance publish failed: {}", e);
                            let _ = relay.responses_tx.send(SigningMessage::Response {
                                request_id: relay.request_id,
                                signer_xrpl_address: String::new(),
                                der_signature: None,
                                compressed_pubkey: None,
                                error: Some(format!("P2P publish failed: {e}")),
                            }).await;
                        }
                    }
                }

                // β3.2b: membership-apply broadcast (seal / confirm). One node
                // collects/assembles, then broadcasts the SAME apply to all so
                // every node applies it to its OWN loopback enclave + acks.
                Some(relay) = async {
                    match &mut membership_apply_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<MembershipApplyRelay>>().await,
                    }
                } => {
                    // The local node is also a member: gossipsub doesn't deliver
                    // to the publisher, so apply locally too.
                    if let Some(ref local) = self.local_signer {
                        let local_ack = Self::handle_membership_apply(
                            local, &relay.request_id, &relay.payload).await;
                        let _ = relay.responses_tx.send(local_ack).await;
                    }

                    let msg = SigningMessage::MembershipApply {
                        request_id: relay.request_id.clone(),
                        requester_peer_id: self.peer_id.to_string(),
                        payload: relay.payload.clone(),
                    };
                    match self.publish_signing(&msg) {
                        Ok(_) => {
                            self.pending_membership_apply.insert(
                                relay.request_id, relay.responses_tx);
                        }
                        Err(e) => {
                            warn!("β3.2b membership-apply publish failed: {}", e);
                            let _ = relay.responses_tx.send(SigningMessage::Response {
                                request_id: relay.request_id,
                                signer_xrpl_address: String::new(),
                                der_signature: None,
                                compressed_pubkey: None,
                                error: Some(format!("P2P publish failed: {e}")),
                            }).await;
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
                    // mpsc senders for path-a delegation: same closed-receiver
                    // detection. Collector drops the receive end on timeout
                    // or quorum-met; we GC entries here to bound memory.
                    self.pending_path_a_delegation.retain(|_, tx| !tx.is_closed());
                    // β1 membership-epoch collectors: same GC.
                    self.pending_membership_epoch.retain(|_, tx| !tx.is_closed());
                    self.pending_mrenclave_governance.retain(|_, tx| !tx.is_closed());
                    // β3.2b membership-apply collectors: same GC.
                    self.pending_membership_apply.retain(|_, tx| !tx.is_closed());
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
                                quorum_bundle,
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
                                    quorum_bundle.as_deref(),
                                ).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish signing response: {}", e);
                                }
                            }
                            Ok(SigningMessage::Response {
                                request_id,
                                ..
                            }) => {
                                // REQ-8 PRG-2 part 3/4: route by request_id
                                // prefix. Path A delegation responses go to
                                // mpsc (multi-receiver); XRPL multisig
                                // responses go to oneshot.
                                if request_id.starts_with("pa-delegation-") {
                                    if let Some(tx) =
                                        self.pending_path_a_delegation.get(&request_id) {
                                        if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                            let _ = tx.send(msg).await;
                                        }
                                    }
                                } else if request_id.starts_with("beta1-membership-") {
                                    if let Some(tx) =
                                        self.pending_membership_epoch.get(&request_id) {
                                        if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                            let _ = tx.send(msg).await;
                                        }
                                    }
                                } else if request_id.starts_with("mrenclave-gov-") {
                                    if let Some(tx) =
                                        self.pending_mrenclave_governance.get(&request_id) {
                                        if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                            let _ = tx.send(msg).await;
                                        }
                                    }
                                } else if request_id.starts_with("beta-apply-") {
                                    if let Some(tx) =
                                        self.pending_membership_apply.get(&request_id) {
                                        if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                            let _ = tx.send(msg).await;
                                        }
                                    }
                                } else if let Some(tx) = self.pending_signing.remove(&request_id) {
                                    if let Ok(msg) = serde_json::from_slice::<SigningMessage>(&message.data) {
                                        let _ = tx.send(msg);
                                    }
                                }
                            }
                            Ok(SigningMessage::PathADelegationRequest {
                                request_id,
                                requester_peer_id,
                                mrenclave_new_hex,
                                ceremony_nonce_hex,
                                signer_account_id_hex: _,
                                signer_xrpl_address: _,
                            }) => {
                                // REQ-8 PRG-2 part 3/4: Path A delegation
                                // signing request from a peer's ceremony
                                // driver. We sign IF we have a local signer
                                // (every operator is a co-signer in the
                                // M-of-N quorum); the X-C1 policy gate is
                                // in handle_path_a_delegation_request which
                                // re-derives the message hash locally from
                                // the bytes carried here.
                                let local_opt = self.local_signer.clone();
                                let Some(local) = local_opt else {
                                    continue;
                                };

                                // X-C1: peer allowlist + per-peer rate
                                // limit + replay guard, mirroring the
                                // existing XRPL signing path.
                                if let Some(ref allow) = self.allowed_signing_peers {
                                    if !allow.contains(&propagation_source) {
                                        warn!(
                                            req_id = %request_id,
                                            from = %propagation_source,
                                            "X-C1: path-a delegation request from peer outside allowlist — dropped"
                                        );
                                        continue;
                                    }
                                }
                                if !self.check_signing_rate(&propagation_source) {
                                    warn!(req_id = %request_id, "X-C1: path-a delegation rate-limited");
                                    continue;
                                }
                                if !self.mark_signing_request_fresh(&request_id) {
                                    warn!(req_id = %request_id, "X-C1: duplicate path-a delegation request_id — dropped");
                                    continue;
                                }

                                let mre_bytes = match hex::decode(&mrenclave_new_hex) {
                                    Ok(b) if b.len() == 32 => {
                                        let mut a = [0u8; 32];
                                        a.copy_from_slice(&b);
                                        a
                                    }
                                    _ => {
                                        warn!(req_id = %request_id, "path-a delegation: bad mrenclave_new_hex");
                                        continue;
                                    }
                                };
                                let nonce_bytes = match hex::decode(&ceremony_nonce_hex) {
                                    Ok(b) if b.len() == 32 => {
                                        let mut a = [0u8; 32];
                                        a.copy_from_slice(&b);
                                        a
                                    }
                                    _ => {
                                        warn!(req_id = %request_id, "path-a delegation: bad ceremony_nonce_hex");
                                        continue;
                                    }
                                };

                                info!(
                                    req_id = %request_id,
                                    from = %requester_peer_id,
                                    propagation = %propagation_source,
                                    "path-a delegation request received — signing locally"
                                );
                                let response = Self::handle_path_a_delegation_request(
                                    &local,
                                    &request_id,
                                    &mre_bytes,
                                    &nonce_bytes,
                                ).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish path-a delegation response: {}", e);
                                }
                            }
                            Ok(SigningMessage::MembershipEpochRequest {
                                request_id,
                                requester_peer_id,
                                escrow_hex,
                                proposed_epoch,
                                prev_epoch_hash_hex,
                                new_signers,
                                new_quorum,
                            }) => {
                                // β1: off-chain membership-epoch authorisation
                                // from a peer's ceremony driver. Sign IF we
                                // have a local signer (every operator is a
                                // co-signer); the X-C1 gate is in
                                // handle_membership_epoch_request, which
                                // re-derives the message hash locally from the
                                // full proposed set carried here.
                                let local_opt = self.local_signer.clone();
                                let Some(local) = local_opt else {
                                    continue;
                                };

                                // X-C1: peer allowlist + rate limit + replay
                                // guard, mirroring the existing signing paths.
                                if let Some(ref allow) = self.allowed_signing_peers {
                                    if !allow.contains(&propagation_source) {
                                        warn!(
                                            req_id = %request_id,
                                            from = %propagation_source,
                                            "X-C1: β1 membership request from peer outside allowlist — dropped"
                                        );
                                        continue;
                                    }
                                }
                                if !self.check_signing_rate(&propagation_source) {
                                    warn!(req_id = %request_id, "X-C1: β1 membership request rate-limited");
                                    continue;
                                }
                                if !self.mark_signing_request_fresh(&request_id) {
                                    warn!(req_id = %request_id, "X-C1: duplicate β1 membership request_id — dropped");
                                    continue;
                                }

                                let escrow = match decode_20(&escrow_hex) {
                                    Some(a) => a,
                                    None => {
                                        warn!(req_id = %request_id, "β1 membership: bad escrow_hex");
                                        continue;
                                    }
                                };
                                let prev_epoch_hash = match decode_32(&prev_epoch_hash_hex) {
                                    Some(a) => a,
                                    None => {
                                        warn!(req_id = %request_id, "β1 membership: bad prev_epoch_hash_hex");
                                        continue;
                                    }
                                };
                                let mut signers =
                                    Vec::with_capacity(new_signers.len());
                                let mut bad = false;
                                for s in &new_signers {
                                    match decode_20(&s.account_id_hex) {
                                        Some(account_id) => signers.push(
                                            crate::membership_canonical::SignerEntry {
                                                account_id,
                                                weight: s.weight,
                                            },
                                        ),
                                        None => {
                                            bad = true;
                                            break;
                                        }
                                    }
                                }
                                if bad {
                                    warn!(req_id = %request_id, "β1 membership: bad signer account_id_hex");
                                    continue;
                                }

                                info!(
                                    req_id = %request_id,
                                    from = %requester_peer_id,
                                    propagation = %propagation_source,
                                    epoch = proposed_epoch,
                                    signers = signers.len(),
                                    "β1 membership-epoch request received — signing locally"
                                );
                                let response = Self::handle_membership_epoch_request(
                                    &local,
                                    &request_id,
                                    &escrow,
                                    proposed_epoch,
                                    &prev_epoch_hash,
                                    &signers,
                                    new_quorum,
                                ).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish β1 membership response: {}", e);
                                }
                            }
                            Ok(SigningMessage::MrenclaveGovernanceRequest {
                                request_id,
                                requester_peer_id,
                                kind,
                                mrenclave_hex,
                                op,
                                proposed_epoch,
                                prev_allowlist_hash_hex,
                            }) => {
                                // β4 Thread B: a governance/repro signing request
                                // from a peer's governance driver. Sign IF we have
                                // a local signer; the enclave re-derives the
                                // domain-separated message from the structured
                                // fields, so nothing is trusted from the wire.
                                let local_opt = self.local_signer.clone();
                                let Some(local) = local_opt else {
                                    continue;
                                };
                                if let Some(ref allow) = self.allowed_signing_peers {
                                    if !allow.contains(&propagation_source) {
                                        warn!(
                                            req_id = %request_id,
                                            from = %propagation_source,
                                            "X-C1: β4 mrenclave-gov request from peer outside allowlist — dropped"
                                        );
                                        continue;
                                    }
                                }
                                if !self.check_signing_rate(&propagation_source) {
                                    warn!(req_id = %request_id, "X-C1: β4 mrenclave-gov request rate-limited");
                                    continue;
                                }
                                if !self.mark_signing_request_fresh(&request_id) {
                                    warn!(req_id = %request_id, "X-C1: duplicate β4 mrenclave-gov request_id — dropped");
                                    continue;
                                }
                                let mrenclave = match decode_32(&mrenclave_hex) {
                                    Some(a) => a,
                                    None => {
                                        warn!(req_id = %request_id, "β4 mrenclave-gov: bad mrenclave_hex");
                                        continue;
                                    }
                                };
                                let prev_allowlist_hash = match decode_32(&prev_allowlist_hash_hex) {
                                    Some(a) => a,
                                    None => {
                                        warn!(req_id = %request_id, "β4 mrenclave-gov: bad prev_allowlist_hash_hex");
                                        continue;
                                    }
                                };
                                info!(
                                    req_id = %request_id,
                                    from = %requester_peer_id,
                                    propagation = %propagation_source,
                                    ?kind,
                                    "β4 mrenclave-governance request received — signing locally"
                                );
                                let response = Self::handle_mrenclave_governance_request(
                                    &local,
                                    &request_id,
                                    kind,
                                    &mrenclave,
                                    op,
                                    proposed_epoch,
                                    &prev_allowlist_hash,
                                ).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish β4 mrenclave-gov response: {}", e);
                                }
                            }
                            Ok(SigningMessage::MembershipApply {
                                request_id,
                                requester_peer_id,
                                payload,
                            }) => {
                                // β3.2b: a membership-apply broadcast from the
                                // driving node. Apply to THIS node's loopback
                                // enclave (seal or confirm) and publish an ack.
                                // handle_membership_apply re-derives every value
                                // from the carried payload and targets localhost
                                // only (X-C1 loopback guard inside).
                                let local_opt = self.local_signer.clone();
                                let Some(local) = local_opt else {
                                    continue;
                                };

                                // X-C1: peer allowlist + rate limit + replay
                                // guard, mirroring the signing paths.
                                if let Some(ref allow) = self.allowed_signing_peers {
                                    if !allow.contains(&propagation_source) {
                                        warn!(
                                            req_id = %request_id,
                                            from = %propagation_source,
                                            "X-C1: β membership-apply from peer outside allowlist — dropped"
                                        );
                                        continue;
                                    }
                                }
                                if !self.check_signing_rate(&propagation_source) {
                                    warn!(req_id = %request_id, "X-C1: β membership-apply rate-limited");
                                    continue;
                                }
                                if !self.mark_signing_request_fresh(&request_id) {
                                    warn!(req_id = %request_id, "X-C1: duplicate β membership-apply request_id — dropped");
                                    continue;
                                }

                                info!(
                                    req_id = %request_id,
                                    from = %requester_peer_id,
                                    propagation = %propagation_source,
                                    "β membership-apply received — applying to local enclave"
                                );
                                let response = Self::handle_membership_apply(
                                    &local, &request_id, &payload).await;
                                if let Err(e) = self.publish_signing(&response) {
                                    error!("failed to publish β membership-apply ack: {}", e);
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
                                // F-5-P2P-L3: per-peer rate limit before any
                                // other work. Enforce BEFORE recipient filter
                                // so a flooding peer can't burn cycles.
                                if !self.check_share_v2_rate(&propagation_source) {
                                    warn!(
                                        from = %propagation_source,
                                        "F-5-P2P-L3: share-v2 rate-limited; message dropped"
                                    );
                                    continue;
                                }
                                // Recipient filter: if we know our ECDH pubkey,
                                // silently drop envelopes addressed to others.
                                let ShareEnvelopeV2Message::Deliver {
                                    ref recipient_pubkey,
                                    ref group_id,
                                    signer_id,
                                    ref envelope,
                                    ..
                                } = msg;
                                if let Some(ref local_pk) = self.local_ecdh_pubkey {
                                    if recipient_pubkey.to_lowercase() != *local_pk {
                                        continue;
                                    }
                                }
                                // F-5-P2P-L3: replay guard keyed on the unique
                                // share envelope identifier. ceremony_nonce
                                // comes from sgx_read_rand inside the sender's
                                // enclave, so collision under honest behaviour
                                // is cryptographically negligible.
                                let nonce = &envelope.ceremony_nonce;
                                let replay_key = format!("{group_id}:{signer_id}:{nonce}");
                                if !self.mark_share_v2_fresh(&replay_key) {
                                    warn!(
                                        from = %propagation_source,
                                        key = %replay_key,
                                        "F-5-P2P-L3: share-v2 replay dropped"
                                    );
                                    continue;
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
                                // F-5-P2P-M1: per-peer rate limit before parsing /
                                // forwarding. Bounds spam.
                                if !self.check_dkg_step_rate(&propagation_source) {
                                    warn!(
                                        from = %propagation_source,
                                        "F-5-P2P-M1: dkg-step rate-limited; message dropped"
                                    );
                                    continue;
                                }
                                // F-5-P2P-M1: replay guard keyed on
                                // (ceremony_id, type_tag[, pid]). Identical
                                // messages within REPLAY_TTL are dropped at
                                // the dispatch layer before reaching
                                // run_follower's enclave-call path.
                                let replay_key = match &msg {
                                    DkgStepMessage::Round1Start { ceremony_id, .. } => {
                                        format!("{ceremony_id}:R1Start")
                                    }
                                    DkgStepMessage::Round1Done { ceremony_id, pid, .. } => {
                                        format!("{ceremony_id}:R1Done:{pid}")
                                    }
                                    DkgStepMessage::Round15Start { ceremony_id } => {
                                        format!("{ceremony_id}:R15Start")
                                    }
                                    DkgStepMessage::Round15Done { ceremony_id, pid } => {
                                        format!("{ceremony_id}:R15Done:{pid}")
                                    }
                                    DkgStepMessage::Round2Start { ceremony_id, .. } => {
                                        format!("{ceremony_id}:R2Start")
                                    }
                                    DkgStepMessage::Round2Done { ceremony_id, pid } => {
                                        format!("{ceremony_id}:R2Done:{pid}")
                                    }
                                    DkgStepMessage::FinalizeStart { ceremony_id } => {
                                        format!("{ceremony_id}:FinStart")
                                    }
                                    DkgStepMessage::FinalizeDone { ceremony_id, pid, .. } => {
                                        format!("{ceremony_id}:FinDone:{pid}")
                                    }
                                    DkgStepMessage::Abort { ceremony_id, pid, .. } => {
                                        format!("{ceremony_id}:Abort:{pid}")
                                    }
                                };
                                if !self.mark_dkg_step_fresh(&replay_key) {
                                    warn!(
                                        from = %propagation_source,
                                        key = %replay_key,
                                        "F-5-P2P-M1: dkg-step replay dropped"
                                    );
                                    continue;
                                }
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
    /// β4 Thread A: stand-in for the β1 quorum bundle the governance signing
    /// path requires. Opaque to the orchestrator — the ENCLAVE verifies it.
    const TEST_BUNDLE_HEX: &str = "beefcafe";
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

    // ── Phase 2.2-B wire-level integration tests ─────────────────
    //
    // These exercise `handle_signing_request` end-to-end against a real
    // axum mock server, hitting the same code path production traffic
    // takes when a SigningMessage::Request lands on a peer's run loop.
    // Coverage goals (Audit re-audit-4 Appendix B/C bar):
    //
    //   1. Wire shape — a SignerListSet tx survives serde round-trip
    //      through SigningMessage::Request unchanged; the receiver's
    //      policy gets the exact JSON the requester sent.
    //   2. Policy fires before the enclave is contacted — a malformed
    //      tx never leaves a hash hitting `/pool/sign`.
    //   3. On policy pass, the receiver's enclave HTTP call carries the
    //      hex of `multi_signing_hash(tx, signer_account_id)` and the
    //      Response carries DER-encoded signature back to the caller.
    //
    // The mock enclave returns a deterministic fake `r`/`s` so the test
    // can assert exact DER bytes; the production
    // `xrpl_signer::der_encode_signature` is exercised verbatim.

    use axum::{routing::post, Json, Router};
    use serde_json::Value as JsonValue;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// β4 Thread A wire contract (AC-β4-A2): the enclave receives the
    /// for-signing BLOB and re-derives the hash itself — a digest must never
    /// reach it again. Asserting `hash` is absent turns any regression back to
    /// hash-on-the-wire into a test failure. `require_bundle` additionally
    /// enforces AC-β4-A1 on the governance route.
    fn assert_typed_sign_body(body: &JsonValue, require_bundle: bool) {
        assert!(
            body.get("hash").is_none(),
            "regression: a digest reached the enclave — the typed contract sends tx_blob only"
        );
        let blob = body["tx_blob"].as_str().unwrap_or("");
        assert!(
            !blob.is_empty() && blob.len() % 2 == 0 && blob.chars().all(|c| c.is_ascii_hexdigit()),
            "enclave received a malformed tx_blob: {blob}"
        );
        if require_bundle {
            assert!(
                body["quorum_bundle"]
                    .as_str()
                    .is_some_and(|b| !b.is_empty()),
                "governance signing must carry the β1 quorum bundle"
            );
        }
    }

    fn canned_signature() -> Json<JsonValue> {
        Json(serde_json::json!({
            "status": "success",
            "signature": {
                // Deterministic 32-byte r/s — the tests assert on the DER bytes.
                "r": "11".repeat(32),
                "s": "22".repeat(32),
            }
        }))
    }

    /// Spawns an axum mock enclave exposing the two typed signing routes,
    /// recording each hit and returning a canned signature envelope. Returns
    /// the base URL (no route suffix) and a hit-counter handle.
    async fn spawn_mock_enclave() -> (String, std::sync::Arc<AtomicUsize>) {
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits_value = hits.clone();
        let hits_gov = hits.clone();
        let app = Router::new()
            .route(
                "/v1/pool/sign/withdrawal-payment",
                post(move |Json(body): Json<JsonValue>| {
                    let hits = hits_value.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        assert_typed_sign_body(&body, false);
                        canned_signature()
                    }
                }),
            )
            .route(
                "/v1/pool/sign/governance-signerlistset",
                post(move |Json(body): Json<JsonValue>| {
                    let hits = hits_gov.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        assert_typed_sign_body(&body, true);
                        canned_signature()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1"), hits)
    }

    #[tokio::test]
    async fn wire_signerlist_passes_policy_and_yields_signature() {
        let (base_url, hits) = spawn_mock_enclave().await;
        let mut signer = test_local_signer();
        signer.enclave_url = base_url;

        let resp = P2PNode::handle_signing_request(
            &signer,
            Some(TEST_ESCROW),
            "wire-signerlist-1",
            &good_signerlist_tx(),
            &signer_acct_id_hex(),
            Some(TEST_BUNDLE_HEX),
        )
        .await;

        match resp {
            SigningMessage::Response {
                request_id,
                der_signature,
                compressed_pubkey,
                error,
                ..
            } => {
                assert_eq!(request_id, "wire-signerlist-1");
                assert!(error.is_none(), "expected no error, got: {error:?}");
                let der = der_signature.expect("signature must be present");
                let pk = compressed_pubkey.expect("pubkey must be present");
                // Production der_encode wraps r||s in DER. Length is
                // 70-72 bytes hex-encoded → 140-144 chars uppercase.
                assert!(
                    der.len() >= 140 && der.len() <= 144 && der == der.to_uppercase(),
                    "unexpected DER shape: {der}"
                );
                assert_eq!(pk, signer.compressed_pubkey.to_uppercase());
                assert_eq!(hits.load(Ordering::SeqCst), 1, "enclave hit exactly once");
            }
            _ => panic!("expected Response variant"),
        }
    }

    #[tokio::test]
    async fn wire_signerlist_with_quorum_one_rejected_before_enclave() {
        let (base_url, hits) = spawn_mock_enclave().await;
        let mut signer = test_local_signer();
        signer.enclave_url = base_url;

        let mut tx = good_signerlist_tx();
        tx["SignerQuorum"] = serde_json::json!(1);

        let resp = P2PNode::handle_signing_request(
            &signer,
            Some(TEST_ESCROW),
            "wire-signerlist-quorum1",
            &tx,
            &signer_acct_id_hex(),
            Some(TEST_BUNDLE_HEX),
        )
        .await;

        match resp {
            SigningMessage::Response {
                der_signature,
                error,
                ..
            } => {
                assert!(der_signature.is_none(), "policy must reject before signing");
                let e = error.expect("error message present");
                assert!(e.contains("policy: SignerQuorum 1 outside"), "got: {e}");
                assert_eq!(
                    hits.load(Ordering::SeqCst),
                    0,
                    "enclave MUST NOT be hit on policy reject"
                );
            }
            _ => panic!("expected Response variant"),
        }
    }

    #[tokio::test]
    async fn wire_signerlist_with_attacker_source_rejected_before_enclave() {
        let (base_url, hits) = spawn_mock_enclave().await;
        let mut signer = test_local_signer();
        signer.enclave_url = base_url;

        let mut tx = good_signerlist_tx();
        tx["Account"] = serde_json::json!(TEST_ATTACKER);

        let resp = P2PNode::handle_signing_request(
            &signer,
            Some(TEST_ESCROW),
            "wire-signerlist-attacker",
            &tx,
            &signer_acct_id_hex(),
            Some(TEST_BUNDLE_HEX),
        )
        .await;

        match resp {
            SigningMessage::Response {
                der_signature,
                error,
                ..
            } => {
                assert!(der_signature.is_none());
                let e = error.expect("error present");
                assert!(e.contains("does not match configured escrow"), "got: {e}");
                assert_eq!(hits.load(Ordering::SeqCst), 0);
            }
            _ => panic!("expected Response variant"),
        }
    }

    /// β4 Thread A (AC-β4-A1, RESP-β4-impl §5): a SignerListSet request WITHOUT
    /// the β1 quorum bundle must be refused here — before the enclave is touched
    /// — rather than failing opaquely inside it. Asserts the enclave saw zero
    /// hits, so the rejection is genuinely local.
    #[tokio::test]
    async fn wire_signerlist_without_bundle_rejected_before_enclave() {
        let (base_url, hits) = spawn_mock_enclave().await;
        let mut signer = test_local_signer();
        signer.enclave_url = base_url;

        let resp = P2PNode::handle_signing_request(
            &signer,
            Some(TEST_ESCROW),
            "wire-signerlist-no-bundle",
            &good_signerlist_tx(),
            &signer_acct_id_hex(),
            None, // governance request with no bundle
        )
        .await;

        match resp {
            SigningMessage::Response {
                der_signature,
                error,
                ..
            } => {
                assert!(der_signature.is_none(), "must not produce a signature");
                let err = error.expect("expected a rejection");
                assert!(
                    err.contains("quorum_bundle"),
                    "error should name the missing bundle, got: {err}"
                );
            }
            _ => panic!("expected Response"),
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the enclave must not be contacted for a bundle-less SignerListSet"
        );
    }

    #[tokio::test]
    async fn signing_message_round_trips_signerlist_through_serde() {
        // Locks the wire contract: the `SigningMessage::Request`
        // serialization preserves a `SignerListSet` payload byte-equal
        // through one JSON round-trip. Without this, a future change
        // to SigningMessage's serde shape (e.g. adding a discriminator)
        // could silently break governance flow.
        let req = SigningMessage::Request {
            request_id: "rt-1".into(),
            requester_peer_id: "peer-A".into(),
            unsigned_tx: good_signerlist_tx(),
            signer_account_id_hex: signer_acct_id_hex(),
            signer_xrpl_address: test_local_signer().xrpl_address,
            quorum_bundle: Some(TEST_BUNDLE_HEX.to_string()),
        };
        let wire = serde_json::to_string(&req).expect("serialize");
        let parsed: SigningMessage = serde_json::from_str(&wire).expect("deserialize");
        match parsed {
            SigningMessage::Request {
                request_id,
                unsigned_tx,
                quorum_bundle,
                ..
            } => {
                assert_eq!(request_id, "rt-1");
                assert_eq!(unsigned_tx, good_signerlist_tx());
                // β4 Thread A: the β1 bundle must survive the wire — without it
                // the receiver's enclave refuses to cosign the SignerListSet.
                assert_eq!(quorum_bundle.as_deref(), Some(TEST_BUNDLE_HEX));
            }
            _ => panic!("expected Request"),
        }
    }

    // ── β1 membership-epoch transport ────────────────────────────

    #[test]
    fn decode_20_and_32_enforce_exact_length() {
        assert!(decode_20(&"ab".repeat(20)).is_some());
        assert!(decode_20(&"ab".repeat(19)).is_none()); // 19 bytes
        assert!(decode_20(&"ab".repeat(21)).is_none()); // 21 bytes
        assert!(decode_20("zz").is_none()); // not hex
        assert!(decode_32(&"cd".repeat(32)).is_some());
        assert!(decode_32(&"cd".repeat(31)).is_none());
    }

    /// β1 MembershipEpochRequest survives a serde round-trip with its full
    /// signer set intact — the wire shape every co-signer re-derives the
    /// message hash from. A drift in this shape silently breaks the
    /// membership-change flow, so it is pinned.
    #[test]
    fn membership_epoch_request_round_trips() {
        let req = SigningMessage::MembershipEpochRequest {
            request_id: "beta1-membership-xyz".into(),
            requester_peer_id: "peer-A".into(),
            escrow_hex: "aa".repeat(20),
            proposed_epoch: 5,
            prev_epoch_hash_hex: "bb".repeat(32),
            new_signers: vec![
                MembershipSignerWire {
                    account_id_hex: "01".repeat(20),
                    weight: 1,
                },
                MembershipSignerWire {
                    account_id_hex: "02".repeat(20),
                    weight: 2,
                },
            ],
            new_quorum: 2,
        };
        let wire = serde_json::to_string(&req).expect("serialize");
        // snake_case tag, per the enum's serde attr.
        assert!(wire.contains("\"type\":\"membership_epoch_request\""));
        let parsed: SigningMessage = serde_json::from_str(&wire).expect("deserialize");
        match parsed {
            SigningMessage::MembershipEpochRequest {
                request_id,
                proposed_epoch,
                new_signers,
                new_quorum,
                ..
            } => {
                assert_eq!(request_id, "beta1-membership-xyz");
                assert_eq!(proposed_epoch, 5);
                assert_eq!(new_signers.len(), 2);
                assert_eq!(new_signers[1].weight, 2);
                assert_eq!(new_quorum, 2);
            }
            _ => panic!("expected MembershipEpochRequest"),
        }
    }

    /// β3.2b: the apply-broadcast wire contract (seal + confirm payloads) is the
    /// cross-node transport for the loopback-enclave apply, so it is pinned.
    #[test]
    fn membership_apply_seal_round_trips() {
        let msg = SigningMessage::MembershipApply {
            request_id: "beta-apply-xyz".into(),
            requester_peer_id: "peer-A".into(),
            payload: MembershipApplyPayload::Seal {
                escrow_hex: "aa".repeat(20),
                proposed_epoch: 7,
                prev_epoch_hash_hex: "bb".repeat(32),
                new_signers: vec![MembershipSignerWire {
                    account_id_hex: "01".repeat(20),
                    weight: 1,
                }],
                new_quorum: 1,
                quorum_bundle_hex: "deadbeef".into(),
            },
        };
        let wire = serde_json::to_string(&msg).expect("serialize");
        assert!(wire.contains("\"type\":\"membership_apply\""));
        // payload is internally tagged by `op`.
        assert!(wire.contains("\"op\":\"seal\""));
        let parsed: SigningMessage = serde_json::from_str(&wire).expect("deserialize");
        match parsed {
            SigningMessage::MembershipApply {
                request_id,
                payload,
                ..
            } => {
                assert_eq!(request_id, "beta-apply-xyz");
                match payload {
                    MembershipApplyPayload::Seal {
                        proposed_epoch,
                        new_quorum,
                        quorum_bundle_hex,
                        ..
                    } => {
                        assert_eq!(proposed_epoch, 7);
                        assert_eq!(new_quorum, 1);
                        assert_eq!(quorum_bundle_hex, "deadbeef");
                    }
                    _ => panic!("expected Seal payload"),
                }
            }
            _ => panic!("expected MembershipApply"),
        }
    }

    #[test]
    fn membership_apply_confirm_round_trips() {
        let msg = SigningMessage::MembershipApply {
            request_id: "beta-apply-conf".into(),
            requester_peer_id: "peer-B".into(),
            payload: MembershipApplyPayload::Confirm {
                escrow_hex: "aa".repeat(20),
                signed_xrpl_tx_blob_hex: "1234".into(),
                tx_hash_hex: "ee".repeat(32),
                ledger_index: 9_001,
            },
        };
        let wire = serde_json::to_string(&msg).expect("serialize");
        assert!(wire.contains("\"op\":\"confirm\""));
        let parsed: SigningMessage = serde_json::from_str(&wire).expect("deserialize");
        match parsed {
            SigningMessage::MembershipApply { payload, .. } => match payload {
                MembershipApplyPayload::Confirm {
                    tx_hash_hex,
                    ledger_index,
                    ..
                } => {
                    assert_eq!(tx_hash_hex, "ee".repeat(32));
                    assert_eq!(ledger_index, 9_001);
                }
                _ => panic!("expected Confirm payload"),
            },
            _ => panic!("expected MembershipApply"),
        }
    }

    /// REGRESSION (β4-B genesis, 2026-08-02): the enclave typed-sign routes decode
    /// `session_key` with `from_hex`, which expects NO `0x` prefix. Config /
    /// node-bootstrap op-JSON store it 0x-prefixed. `session_key_hex()` MUST strip
    /// the prefix — otherwise the server rejects every membership consent with
    /// "refused (malformed set or invalid input)" (the key mis-lengths past 32
    /// bytes and fails `typed_sign_preamble` before the ecall), the collector
    /// gathers 0 consents, and the whole β membership / genesis flow stalls.
    #[test]
    fn session_key_hex_strips_0x_prefix() {
        let signer = LocalSigner {
            enclave_url: "https://localhost:9088/v1".into(),
            address: "0x85f9".into(),
            session_key: "0x5bb00f4c".into(),
            compressed_pubkey: String::new(),
            xrpl_address: String::new(),
        };
        assert_eq!(signer.session_key_hex(), "5bb00f4c");
        // Idempotent when already bare (no double-strip, no panic).
        let bare = LocalSigner {
            session_key: "deadbeef".into(),
            ..signer
        };
        assert_eq!(bare.session_key_hex(), "deadbeef");
    }

    /// GEN-3-R1: the single normalization function — the class fix that both the
    /// boundary (`set_local_signer`, which stores the result so a later direct
    /// `.session_key` read is also bare) and the accessor (`session_key_hex`) call.
    #[test]
    fn canonical_session_key_normalizes_and_is_idempotent() {
        assert_eq!(canonical_session_key("0x5bb00f4c"), "5bb00f4c"); // strip
        assert_eq!(canonical_session_key("5bb00f4c"), "5bb00f4c"); // already bare
        assert_eq!(canonical_session_key(""), ""); // empty, no panic
    }
}
