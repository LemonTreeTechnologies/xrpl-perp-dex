//! FROST threshold-Schnorr signing round driver.
//!
//! # Why this file exists
//!
//! Until now the cluster could *create* FROST key material and *move* it —
//! Pedersen DKG runs, shares seal per enclave, and Path A migrates them across
//! every MRENCLAVE bump — but it could never *use* it. The three signing ecalls
//! (`ecall_frost_nonce_gen`, `ecall_frost_partial_sign`,
//! `ecall_frost_partial_sig_agg`) are implemented and each enclave even exposes
//! them on its own loopback API (`/v1/pool/frost/*`), yet no code above the
//! enclave ever drove a round. The cluster was carefully preserving, across
//! every upgrade, a key that had never signed anything.
//!
//! This module is the missing driver. It is deliberately transport-agnostic:
//! `FrostParticipant` is the seam, so the same round logic runs against three
//! in-process fakes in tests and against three real peers in production.
//!
//! # What a round is
//!
//! FROST signing is two passes, and both passes must see the *same* signer set
//! in the *same* order:
//!
//!   1. every participant generates a nonce bound to `msg32` and publishes the
//!      public half (66 bytes);
//!   2. every participant, given the full set of public nonces, produces a
//!      partial signature (32 bytes);
//!   3. anyone aggregates the partials into one 64-byte BIP340 signature.
//!
//! Step 3 is stateless, so the aggregation is done on the local enclave.
//!
//! # The two traps this module exists to close
//!
//! **Ordering.** `pubnonces[i]` must correspond to `signer_ids[i]` in every
//! call, and every participant must be given the identical sequence. Nothing in
//! the enclave API enforces that — it trusts the caller. A driver that built the
//! set from, say, the order responses happened to arrive in would produce
//! partials that aggregate into a signature that simply fails to verify, with no
//! error from any individual step. [`SignerSet`] makes the canonical order
//! (ascending `signer_id`) the only constructible one.
//!
//! **Concurrent rounds.** The enclave keeps at most one live nonce per signer.
//! Asking a signer for a second nonce silently overwrites the first — the
//! enclave logs `Overwriting existing nonce` and carries on. If two rounds over
//! the *same* message overlap, round 1's published pubnonce no longer matches
//! the secnonce the enclave still holds, so round 1's partials are computed
//! against the wrong nonce. The message check inside `partial_sign` does not
//! catch this: the message is identical, only the nonce moved. The result is an
//! invalid aggregate signature and no failing step to point at.
//!
//! So a round takes a process-global claim and a second concurrent round is
//! **refused, not queued** — the same posture the leaf-buffer claim takes in the
//! enclave. Queuing would be worse than refusing: it hides contention that the
//! caller should see, and a caller who waits still has no guarantee that its own
//! nonces survived the wait.
//!
//! # What this module does NOT do
//!
//! It does not decide *what* to sign, and it does not put a FROST signature on
//! any chain. XRPL cannot accept one — XRPL verifies secp256k1 ECDSA and
//! ed25519, not BIP340 Schnorr — which is why XRPL settlement uses an
//! independent-ECDSA `SignerList` quorum and always will. The signature this
//! driver produces is a cluster-level Schnorr signature; its product home is the
//! Bitcoin/Taproot leg.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Serialized public nonce, as `secp256k1_frost_pubnonce_serialize` emits it.
pub const FROST_PUBNONCE_LEN: usize = 66;
/// Serialized partial signature.
pub const FROST_PARTIAL_SIG_LEN: usize = 32;
/// Final aggregated BIP340 signature.
pub const FROST_SIG_LEN: usize = 64;

/// The enclave's request-body read is a fixed `char buffer[4096]` for both
/// `partial-sign` and `sig-agg` (`pool_handler.cpp`). A body longer than that is
/// truncated and then fails JSON parsing, so it surfaces as a 400 rather than
/// corruption — but it is a hard ceiling on the signer set, and it is reached by
/// arithmetic, not by policy. Largest body is `sig-agg`, carrying per signer:
/// pubnonce 132 hex + partial_sig 64 hex + id, plus JSON punctuation. We refuse
/// above this bound ourselves so the failure names the real cause instead of
/// arriving as an opaque parse error from the enclave.
pub const MAX_SIGNERS_PER_ROUND: usize = 16;

/// One participant's contribution, always carried together with the id it
/// belongs to so the two can never be zipped up out of step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution<const N: usize> {
    pub signer_id: u32,
    pub bytes: [u8; N],
}

/// A validated, canonically ordered signer set for one round.
///
/// Construction is the only way to get one, and construction enforces every
/// property the enclave assumes but does not check.
#[derive(Clone, Debug)]
pub struct SignerSet {
    nonces: Vec<Contribution<FROST_PUBNONCE_LEN>>,
}

impl SignerSet {
    /// Validate and canonicalise. Rejects an empty set, a set above the
    /// enclave's body ceiling, and duplicate signer ids.
    ///
    /// Duplicates matter more than they look: two entries for one signer would
    /// make the enclave process that signer's nonce twice while it holds only
    /// one secnonce, producing an aggregate that cannot verify.
    pub fn new(mut nonces: Vec<Contribution<FROST_PUBNONCE_LEN>>) -> Result<Self> {
        if nonces.is_empty() {
            bail!("FROST signer set is empty");
        }
        if nonces.len() > MAX_SIGNERS_PER_ROUND {
            bail!(
                "FROST signer set has {} signers, above the enclave request-body ceiling of {}",
                nonces.len(),
                MAX_SIGNERS_PER_ROUND
            );
        }
        nonces.sort_by_key(|c| c.signer_id);
        if nonces.windows(2).any(|w| w[0].signer_id == w[1].signer_id) {
            bail!("FROST signer set contains a duplicate signer_id");
        }
        Ok(Self { nonces })
    }

    /// `pub(crate)`, not `pub`: this is an internal module of a binary crate,
    /// and a public `len` would oblige a public `is_empty` that nothing calls.
    pub(crate) fn len(&self) -> usize {
        self.nonces.len()
    }

    /// Signer ids in canonical order.
    pub fn signer_ids(&self) -> Vec<u32> {
        self.nonces.iter().map(|c| c.signer_id).collect()
    }

    /// Public nonces, hex, in the same canonical order as [`Self::signer_ids`].
    pub fn pubnonces_hex(&self) -> Vec<String> {
        self.nonces.iter().map(|c| hex::encode(c.bytes)).collect()
    }

    pub fn contains(&self, signer_id: u32) -> bool {
        self.nonces.iter().any(|c| c.signer_id == signer_id)
    }
}

/// One node's enclave, addressed for FROST purposes.
///
/// In production each participant other than the local one is reached through
/// the cluster transport; in tests it is an in-process fake. The round driver
/// cannot tell the difference, which is the point.
#[async_trait]
pub trait FrostParticipant: Send + Sync {
    fn signer_id(&self) -> u32;

    /// Pass 1. Generate a nonce bound to `msg32`, return the public half.
    async fn nonce_gen(&self, msg32: &[u8; 32]) -> Result<[u8; FROST_PUBNONCE_LEN]>;

    /// Pass 2. Produce this signer's partial signature over `msg32` given the
    /// full canonical signer set.
    async fn partial_sign(
        &self,
        msg32: &[u8; 32],
        set: &SignerSet,
    ) -> Result<[u8; FROST_PARTIAL_SIG_LEN]>;
}

/// Aggregation is stateless, so it is a separate capability from participation —
/// any enclave holding the group can do it, including one that did not sign.
#[async_trait]
pub trait FrostAggregator: Send + Sync {
    async fn sig_agg(
        &self,
        msg32: &[u8; 32],
        set: &SignerSet,
        partials: &[Contribution<FROST_PARTIAL_SIG_LEN>],
    ) -> Result<[u8; FROST_SIG_LEN]>;
}

/// Process-global single-flight claim. See the module docs for why this refuses
/// rather than queues.
static ROUND_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII holder for the claim, so an early return or a `?` cannot leak it and
/// wedge every future round.
struct RoundClaim;

impl RoundClaim {
    fn try_acquire() -> Option<Self> {
        ROUND_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RoundClaim)
    }
}

impl Drop for RoundClaim {
    fn drop(&mut self) {
        ROUND_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Outcome of a completed round.
#[derive(Clone, Debug)]
pub struct FrostRoundOutcome {
    pub signature: [u8; FROST_SIG_LEN],
    pub signer_ids: Vec<u32>,
    pub elapsed: Duration,
    /// Independently re-checked against the group key, outside the enclave.
    pub verified: bool,
}

/// Run one complete FROST signing round and verify the result independently.
///
/// `threshold` is the group's `t`; the round refuses to start with fewer
/// participants than that rather than discovering it inside the enclave.
///
/// `group_pubkey_xonly` is used only for the independent verification in the
/// final step. That verification is the whole point of the round: without it we
/// would be taking the enclave's word that the enclave signed correctly, which
/// proves nothing about the group key anyone else would check against.
pub async fn run_round(
    participants: &[&dyn FrostParticipant],
    aggregator: &dyn FrostAggregator,
    msg32: &[u8; 32],
    threshold: usize,
    group_pubkey_xonly: &[u8; 32],
) -> Result<FrostRoundOutcome> {
    let Some(_claim) = RoundClaim::try_acquire() else {
        bail!(
            "a FROST round is already in flight; refusing to start a second one \
             (a concurrent round would overwrite this signer's live nonce and \
             silently invalidate both rounds)"
        );
    };

    if participants.len() < threshold {
        bail!(
            "FROST round needs at least the threshold of {threshold} participants, got {}",
            participants.len()
        );
    }

    let started = Instant::now();

    // ── Pass 1: nonces ──────────────────────────────────────────────────
    let mut nonces = Vec::with_capacity(participants.len());
    for p in participants {
        let bytes = p
            .nonce_gen(msg32)
            .await
            .with_context(|| format!("FROST nonce_gen failed for signer {}", p.signer_id()))?;
        nonces.push(Contribution {
            signer_id: p.signer_id(),
            bytes,
        });
    }
    let set = SignerSet::new(nonces)?;

    // ── Pass 2: partial signatures ──────────────────────────────────────
    //
    // Every participant is handed the identical `set`, so every one of them
    // derives the same aggregate nonce and the same challenge.
    let mut partials: Vec<Contribution<FROST_PARTIAL_SIG_LEN>> = Vec::with_capacity(set.len());
    for p in participants {
        if !set.contains(p.signer_id()) {
            // Unreachable via `run_round` (the set is built from these very
            // participants) but asserted rather than assumed: a partial from a
            // signer outside the set aggregates into garbage.
            bail!(
                "signer {} produced a partial signature but is not in the signer set",
                p.signer_id()
            );
        }
        let bytes = p
            .partial_sign(msg32, &set)
            .await
            .with_context(|| format!("FROST partial_sign failed for signer {}", p.signer_id()))?;
        partials.push(Contribution {
            signer_id: p.signer_id(),
            bytes,
        });
    }
    partials.sort_by_key(|c| c.signer_id);

    // ── Aggregate ───────────────────────────────────────────────────────
    let signature = aggregator
        .sig_agg(msg32, &set, &partials)
        .await
        .context("FROST sig_agg failed")?;

    // ── Independent verification ────────────────────────────────────────
    let verified = verify_bip340(msg32, &signature, group_pubkey_xonly);
    let elapsed = started.elapsed();

    if verified {
        info!(
            signers = ?set.signer_ids(),
            elapsed_ms = elapsed.as_millis() as u64,
            "FROST round produced a valid BIP340 signature"
        );
    } else {
        // Loud, because this is the failure mode every individual step reports
        // as success.
        warn!(
            signers = ?set.signer_ids(),
            elapsed_ms = elapsed.as_millis() as u64,
            "FROST round completed but the aggregate signature FAILED independent \
             BIP340 verification against the group key"
        );
    }

    Ok(FrostRoundOutcome {
        signature,
        signer_ids: set.signer_ids(),
        elapsed,
        verified,
    })
}

/// Verify a BIP340 Schnorr signature against an x-only public key, using a
/// library that shares no code with the enclave's signing path.
pub fn verify_bip340(
    msg32: &[u8; 32],
    sig64: &[u8; FROST_SIG_LEN],
    pubkey_xonly: &[u8; 32],
) -> bool {
    // Any 64 bytes are structurally a Schnorr signature; validity is decided at
    // verification, so this conversion is infallible by design.
    let sig = secp256k1::schnorr::Signature::from_byte_array(*sig64);
    let Ok(pk) = secp256k1::XOnlyPublicKey::from_byte_array(*pubkey_xonly) else {
        return false;
    };
    secp256k1::Secp256k1::verification_only()
        .verify_schnorr(&sig, msg32, &pk)
        .is_ok()
}

// ── Enclave-facing implementations ──────────────────────────────────────
//
// Each node's enclave is loopback-only by construction (X-C1), so these talk to
// `127.0.0.1` and nothing else. Reaching a *peer's* signer is therefore not a
// matter of pointing these at another host — it needs the cluster transport,
// which is the next wedge.

/// One signer, reached through its own enclave's loopback HTTP API.
pub struct HttpFrostParticipant {
    base_url: String,
    signer_id: u32,
    client: reqwest::Client,
}

impl HttpFrostParticipant {
    pub fn new(base_url: impl Into<String>, signer_id: u32) -> Self {
        Self {
            base_url: base_url.into(),
            signer_id,
            client: reqwest::Client::new(),
        }
    }
}

fn hex32(v: &serde_json::Value, field: &str, want: usize) -> Result<Vec<u8>> {
    let s = v[field]
        .as_str()
        .with_context(|| format!("enclave response missing `{field}`"))?;
    let b = hex::decode(s.trim_start_matches("0x"))
        .with_context(|| format!("enclave response `{field}` is not hex"))?;
    if b.len() != want {
        bail!(
            "enclave response `{field}` is {} bytes, expected {want}",
            b.len()
        );
    }
    Ok(b)
}

#[async_trait]
impl FrostParticipant for HttpFrostParticipant {
    fn signer_id(&self) -> u32 {
        self.signer_id
    }

    async fn nonce_gen(&self, msg32: &[u8; 32]) -> Result<[u8; FROST_PUBNONCE_LEN]> {
        let body = serde_json::json!({
            "signer_id": self.signer_id,
            "message": hex::encode(msg32),
        });
        let v: serde_json::Value = self
            .client
            .post(format!("{}/v1/pool/frost/nonce-gen", self.base_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let b = hex32(&v, "pubnonce", FROST_PUBNONCE_LEN)?;
        let mut out = [0u8; FROST_PUBNONCE_LEN];
        out.copy_from_slice(&b);
        Ok(out)
    }

    async fn partial_sign(
        &self,
        msg32: &[u8; 32],
        set: &SignerSet,
    ) -> Result<[u8; FROST_PARTIAL_SIG_LEN]> {
        let body = serde_json::json!({
            "signer_id": self.signer_id,
            "message": hex::encode(msg32),
            "pubnonces": set.pubnonces_hex(),
            "signer_ids": set.signer_ids(),
        });
        let v: serde_json::Value = self
            .client
            .post(format!("{}/v1/pool/frost/partial-sign", self.base_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let b = hex32(&v, "partial_sig", FROST_PARTIAL_SIG_LEN)?;
        let mut out = [0u8; FROST_PARTIAL_SIG_LEN];
        out.copy_from_slice(&b);
        Ok(out)
    }
}

/// Aggregation endpoint. Stateless in the enclave, so any reachable enclave
/// holding the group can serve it.
pub struct HttpFrostAggregator {
    base_url: String,
    client: reqwest::Client,
}

impl HttpFrostAggregator {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl FrostAggregator for HttpFrostAggregator {
    async fn sig_agg(
        &self,
        msg32: &[u8; 32],
        set: &SignerSet,
        partials: &[Contribution<FROST_PARTIAL_SIG_LEN>],
    ) -> Result<[u8; FROST_SIG_LEN]> {
        // Partials are sent in the same canonical order as the set; the enclave
        // zips the three arrays positionally.
        let body = serde_json::json!({
            "message": hex::encode(msg32),
            "partial_sigs": partials.iter().map(|c| hex::encode(c.bytes)).collect::<Vec<_>>(),
            "pubnonces": set.pubnonces_hex(),
            "signer_ids": set.signer_ids(),
        });
        let v: serde_json::Value = self
            .client
            .post(format!("{}/v1/pool/frost/sig-agg", self.base_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        // The handler names this field `signature`; accept `sig` as well rather
        // than fail a round over a field-name drift.
        let field = if v.get("signature").is_some() {
            "signature"
        } else {
            "sig"
        };
        let b = hex32(&v, field, FROST_SIG_LEN)?;
        let mut out = [0u8; FROST_SIG_LEN];
        out.copy_from_slice(&b);
        Ok(out)
    }
}

// ── Admin diagnostic surface ────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct FrostRoundRequest {
    /// Loopback enclave base URL, e.g. `http://127.0.0.1:9088`.
    pub enclave_url: String,
    /// 32-byte message, hex.
    pub message: String,
    /// Signers to include, by id.
    pub signer_ids: Vec<u32>,
    /// The group's `t`.
    pub threshold: usize,
    /// x-only group public key, hex — what the signature is checked against.
    pub group_pubkey: String,
}

#[derive(serde::Serialize)]
pub struct FrostRoundResponse {
    pub status: &'static str,
    pub signer_ids: Vec<u32>,
    pub elapsed_ms: u64,
    pub verified: bool,
    pub signature: Option<String>,
}

/// `POST /admin/frost/round` — run one round and report whether the aggregate
/// verifies against the group key.
///
/// This is the probe `deployment-procedure.md` §11.5 step 7 has always asked
/// for. It is a *diagnostic*: it signs whatever message the operator passes and
/// puts nothing on any chain.
pub async fn handle_frost_round(
    axum::Json(req): axum::Json<FrostRoundRequest>,
) -> std::result::Result<axum::Json<FrostRoundResponse>, (axum::http::StatusCode, String)> {
    let bad = |e: anyhow::Error| (axum::http::StatusCode::BAD_REQUEST, e.to_string());

    // The enclave surface is loopback-only; refuse to be pointed anywhere else.
    if !(req.enclave_url.contains("127.0.0.1") || req.enclave_url.contains("localhost")) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "enclave_url must be loopback".to_string(),
        ));
    }

    let msg = hex::decode(req.message.trim_start_matches("0x"))
        .map_err(|e| bad(anyhow::anyhow!("message is not hex: {e}")))?;
    let msg32: [u8; 32] = msg
        .try_into()
        .map_err(|_| bad(anyhow::anyhow!("message must be exactly 32 bytes")))?;

    let gp = hex::decode(req.group_pubkey.trim_start_matches("0x"))
        .map_err(|e| bad(anyhow::anyhow!("group_pubkey is not hex: {e}")))?;
    let group32: [u8; 32] = gp
        .try_into()
        .map_err(|_| bad(anyhow::anyhow!("group_pubkey must be exactly 32 bytes")))?;

    let parts: Vec<HttpFrostParticipant> = req
        .signer_ids
        .iter()
        .map(|id| HttpFrostParticipant::new(req.enclave_url.clone(), *id))
        .collect();
    let refs: Vec<&dyn FrostParticipant> =
        parts.iter().map(|p| p as &dyn FrostParticipant).collect();
    let agg = HttpFrostAggregator::new(req.enclave_url.clone());

    let out = run_round(&refs, &agg, &msg32, req.threshold, &group32)
        .await
        .map_err(|e| (axum::http::StatusCode::CONFLICT, e.to_string()))?;

    Ok(axum::Json(FrostRoundResponse {
        status: if out.verified {
            "verified"
        } else {
            "unverified"
        },
        signer_ids: out.signer_ids,
        elapsed_ms: out.elapsed.as_millis() as u64,
        verified: out.verified,
        signature: Some(hex::encode(out.signature)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{Keypair, Secp256k1};
    use std::sync::Mutex;

    /// The round claim is process-global by design, and `cargo test` runs these
    /// in parallel threads of one process — so a test that drives a round would
    /// otherwise be refused by a *different* test's live round. That refusal is
    /// the guard working correctly; serialising here keeps the tests honest
    /// about production behaviour instead of weakening the guard to suit them.
    /// `tokio::sync::Mutex`, not `std`: these tests hold the guard across
    /// `.await`, which a std guard may not be.
    static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // ── SignerSet invariants ────────────────────────────────────────────

    fn contrib(id: u32, fill: u8) -> Contribution<FROST_PUBNONCE_LEN> {
        Contribution {
            signer_id: id,
            bytes: [fill; FROST_PUBNONCE_LEN],
        }
    }

    #[test]
    fn signer_set_canonicalises_order_regardless_of_arrival() {
        // Responses arriving 2, 0, 1 must still produce ascending order, and the
        // nonce must travel with its own id — not with the position it arrived
        // in. Distinct fills make a mis-zip visible.
        let set =
            SignerSet::new(vec![contrib(2, 0xcc), contrib(0, 0xaa), contrib(1, 0xbb)]).unwrap();
        assert_eq!(set.signer_ids(), vec![0, 1, 2]);
        let hexes = set.pubnonces_hex();
        assert!(hexes[0].starts_with("aa"), "signer 0 kept its own nonce");
        assert!(hexes[1].starts_with("bb"), "signer 1 kept its own nonce");
        assert!(hexes[2].starts_with("cc"), "signer 2 kept its own nonce");
    }

    #[test]
    fn signer_set_rejects_duplicate_ids() {
        let err = SignerSet::new(vec![contrib(1, 0x11), contrib(1, 0x22)]).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn signer_set_rejects_empty() {
        assert!(SignerSet::new(vec![]).is_err());
    }

    #[test]
    fn signer_set_rejects_above_enclave_body_ceiling() {
        let too_many: Vec<_> = (0..=MAX_SIGNERS_PER_ROUND as u32)
            .map(|i| contrib(i, 0x01))
            .collect();
        let err = SignerSet::new(too_many).unwrap_err();
        assert!(err.to_string().contains("ceiling"), "got: {err}");
    }

    // ── BIP340 verification is really verifying ─────────────────────────

    fn real_keypair() -> (Keypair, [u8; 32]) {
        let secp = Secp256k1::new();
        let kp = Keypair::from_seckey_byte_array(&secp, [0x42; 32]).unwrap();
        let xonly = kp.x_only_public_key().0.serialize();
        (kp, xonly)
    }

    #[test]
    fn verify_bip340_accepts_a_real_signature_and_rejects_a_tampered_one() {
        let secp = Secp256k1::new();
        let (kp, xonly) = real_keypair();
        let msg = [0x7au8; 32];
        let sig = secp.sign_schnorr_no_aux_rand(&msg, &kp).to_byte_array();

        assert!(
            verify_bip340(&msg, &sig, &xonly),
            "genuine signature must verify"
        );

        // Probe each rejection path individually — a single "bad input" test
        // would pass even if only one of them worked.
        let mut flipped = sig;
        flipped[0] ^= 0x01;
        assert!(!verify_bip340(&msg, &flipped, &xonly), "tampered signature");

        let mut other_msg = msg;
        other_msg[0] ^= 0x01;
        assert!(!verify_bip340(&other_msg, &sig, &xonly), "wrong message");

        let mut other_key = xonly;
        other_key[0] ^= 0x01;
        assert!(!verify_bip340(&msg, &sig, &other_key), "wrong key");
    }

    // ── Round driver ────────────────────────────────────────────────────

    /// A participant that records exactly what the driver handed it, so the
    /// tests can assert on the *inputs* the enclave would have seen — that is
    /// where the ordering bug would live.
    struct RecordingParticipant {
        id: u32,
        seen_sets: Mutex<Vec<(Vec<u32>, Vec<String>)>>,
    }

    impl RecordingParticipant {
        fn new(id: u32) -> Self {
            Self {
                id,
                seen_sets: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl FrostParticipant for RecordingParticipant {
        fn signer_id(&self) -> u32 {
            self.id
        }
        async fn nonce_gen(&self, _msg32: &[u8; 32]) -> Result<[u8; FROST_PUBNONCE_LEN]> {
            // Fill encodes the signer id so a mis-zip is detectable.
            Ok([0xa0 | (self.id as u8); FROST_PUBNONCE_LEN])
        }
        async fn partial_sign(
            &self,
            _msg32: &[u8; 32],
            set: &SignerSet,
        ) -> Result<[u8; FROST_PARTIAL_SIG_LEN]> {
            self.seen_sets
                .lock()
                .unwrap()
                .push((set.signer_ids(), set.pubnonces_hex()));
            Ok([self.id as u8; FROST_PARTIAL_SIG_LEN])
        }
    }

    /// Aggregator that returns a genuine signature over the message, so the
    /// driver's verification step is exercised for real rather than stubbed.
    struct RealSigningAggregator {
        kp: Keypair,
        seen_partials: Mutex<Vec<Vec<u32>>>,
    }

    #[async_trait]
    impl FrostAggregator for RealSigningAggregator {
        async fn sig_agg(
            &self,
            msg32: &[u8; 32],
            _set: &SignerSet,
            partials: &[Contribution<FROST_PARTIAL_SIG_LEN>],
        ) -> Result<[u8; FROST_SIG_LEN]> {
            self.seen_partials
                .lock()
                .unwrap()
                .push(partials.iter().map(|c| c.signer_id).collect());
            let secp = Secp256k1::new();
            Ok(secp
                .sign_schnorr_no_aux_rand(msg32, &self.kp)
                .to_byte_array())
        }
    }

    #[tokio::test]
    async fn round_hands_every_participant_the_same_canonical_set() {
        let _serial = TEST_SERIAL.lock().await;
        let (kp, xonly) = real_keypair();
        // Deliberately out of order: the driver must not propagate this.
        let p2 = RecordingParticipant::new(2);
        let p0 = RecordingParticipant::new(0);
        let p1 = RecordingParticipant::new(1);
        let parts: Vec<&dyn FrostParticipant> = vec![&p2, &p0, &p1];
        let agg = RealSigningAggregator {
            kp,
            seen_partials: Mutex::new(Vec::new()),
        };

        let out = run_round(&parts, &agg, &[0x11; 32], 2, &xonly)
            .await
            .unwrap();

        assert!(out.verified, "round must verify against the group key");
        assert_eq!(out.signer_ids, vec![0, 1, 2]);

        // Every participant saw the identical set, in ascending order, with each
        // nonce still attached to its own signer.
        for p in [&p2, &p0, &p1] {
            let seen = p.seen_sets.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, vec![0, 1, 2], "signer {} saw wrong order", p.id);
            assert!(seen[0].1[0].starts_with("a0"));
            assert!(seen[0].1[1].starts_with("a1"));
            assert!(seen[0].1[2].starts_with("a2"));
        }

        // Partials reach the aggregator in canonical order too.
        assert_eq!(agg.seen_partials.lock().unwrap()[0], vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn round_refuses_below_threshold_before_touching_any_enclave() {
        let _serial = TEST_SERIAL.lock().await;
        let (kp, xonly) = real_keypair();
        let p0 = RecordingParticipant::new(0);
        let parts: Vec<&dyn FrostParticipant> = vec![&p0];
        let agg = RealSigningAggregator {
            kp,
            seen_partials: Mutex::new(Vec::new()),
        };

        let err = run_round(&parts, &agg, &[0x11; 32], 2, &xonly)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("threshold"), "got: {err}");
        // The refusal must happen before any nonce is burned.
        assert!(p0.seen_sets.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_second_concurrent_round_is_refused_not_queued() {
        let _serial = TEST_SERIAL.lock().await;
        // Hold the claim the way a live round would, then prove a second round
        // is turned away rather than silently corrupting the first.
        let claim = RoundClaim::try_acquire().expect("first claim");
        assert!(
            RoundClaim::try_acquire().is_none(),
            "the claim must be exclusive"
        );

        let (kp, xonly) = real_keypair();
        let p0 = RecordingParticipant::new(0);
        let p1 = RecordingParticipant::new(1);
        let parts: Vec<&dyn FrostParticipant> = vec![&p0, &p1];
        let agg = RealSigningAggregator {
            kp,
            seen_partials: Mutex::new(Vec::new()),
        };

        let err = run_round(&parts, &agg, &[0x11; 32], 2, &xonly)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already in flight"), "got: {err}");
        // Crucially: no nonce was requested, so the in-flight round is untouched.
        assert!(p0.seen_sets.lock().unwrap().is_empty());

        drop(claim);
        // And the claim is released, not leaked.
        assert!(RoundClaim::try_acquire().is_some());
    }

    #[tokio::test]
    async fn round_reports_unverified_when_the_aggregate_does_not_match_the_group_key() {
        let _serial = TEST_SERIAL.lock().await;
        // The signature is genuine but under a DIFFERENT key — exactly what a
        // mis-ordered or mis-matched round produces. Every individual step
        // succeeds; only the independent check catches it.
        let (kp, _xonly) = real_keypair();
        let secp = Secp256k1::new();
        let other = Keypair::from_seckey_byte_array(&secp, [0x43; 32]).unwrap();
        let wrong_group_key = other.x_only_public_key().0.serialize();

        let p0 = RecordingParticipant::new(0);
        let p1 = RecordingParticipant::new(1);
        let parts: Vec<&dyn FrostParticipant> = vec![&p0, &p1];
        let agg = RealSigningAggregator {
            kp,
            seen_partials: Mutex::new(Vec::new()),
        };

        let out = run_round(&parts, &agg, &[0x11; 32], 2, &wrong_group_key)
            .await
            .unwrap();
        assert!(
            !out.verified,
            "a signature under the wrong key must not be reported as verified"
        );
    }
}
