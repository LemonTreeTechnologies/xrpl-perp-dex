//! β3.2c / #127 / X-β3.2-3 — the newcomer join-brain (orchestrator side).
//!
//! The enclave transport (`bootstrap-bundle-{export,import}`) and its HTTP
//! adapters (`bootstrap_http`) are the mechanism; this module is the state +
//! decision logic that drives them over the gossip mesh:
//!   - **source role** (on `BootstrapMessage::Request`): serve only if in-sync
//!     (D-5) and elected (D-3), then export + publish `Deliver`.
//!   - **newcomer role** (on `BootstrapMessage::Deliver`): gate the bundle epoch
//!     against the mesh-observed current epoch (X-β3.2c-7), then import + seal.
//!
//! The orchestrator is UNTRUSTED: every security property is enclave-enforced
//! (admit()-gated peer-attest, ECIES AAD binding, M-1 quorum verify, escrow bound
//! in the message hash). This module adds NO trust — at worst it DoSes a join.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::bootstrap_http::{
    bootstrap_bundle_export, bootstrap_bundle_import, BootstrapEnvelope, BootstrapExportInput,
    BootstrapImportInput,
};
use crate::db::{CurrentMembershipBundle, Db};
use crate::membership_coordinator::EpochDigestSource;
use crate::membership_http::{HttpEpochDigestSource, HttpSyncStateSource};
use crate::membership_projection::SyncStateSource;
use crate::p2p::{unpack_membership_signers, BootstrapMessage, MembershipSignerWire};

/// REQ-β3.2c-impl (X-β3.2c-7): the current `authority_epoch` each DCAP-verified
/// peer last advertised on the peer-quote mesh. A newcomer uses [`max_fresh`] as
/// its INDEPENDENT reference for "the cluster's current epoch", so it can REJECT a
/// `Deliver` carrying a genuinely-signed but STALE bundle — which the enclave
/// alone would accept, then wedge on the fresh-only guard (`ALREADY_CONFIRMED`),
/// a state only an operator wipe recovers.
///
/// Only peers whose quote VERIFIED (admit()-passed → entered `peer_attest_cache`)
/// are recorded, so a debug / wrong-MRENCLAVE peer cannot inject an epoch. The
/// epoch value itself is an orchestrator-level plaintext claim (the host is
/// untrusted): a false-LOW is ignored by the `max`; a false-HIGH can at worst DoS
/// a join until the entry ages out of the freshness window (recoverable, exactly
/// the bound RESP-β3.2c requires). Letting a stale bundle through would require
/// EVERY verified peer to lie — i.e. a fully compromised cluster, out of the
/// threat model (the off-chain quorum is the trust root).
///
/// [`max_fresh`]: ObservedPeerEpochs::max_fresh
#[derive(Default)]
pub struct ObservedPeerEpochs {
    inner: Mutex<HashMap<String, (u64, Instant)>>,
}

impl ObservedPeerEpochs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified peer's advertised epoch. MUST be called only AFTER that
    /// peer's quote verifies (the writer is the peer-quote verifier task, in the
    /// `Ok(Some(mrenclave))` arm). Latest-wins per peer, stamped with the time so
    /// a peer that goes dark ages out of the freshness window.
    pub fn record(&self, peer_pubkey: &str, authority_epoch: u64) {
        if let Ok(mut m) = self.inner.lock() {
            m.insert(peer_pubkey.to_string(), (authority_epoch, Instant::now()));
        }
    }

    /// The maximum epoch advertised by any verified peer observed within `within`,
    /// or `None` if there is no fresh observation. `None` means the newcomer has
    /// no independent epoch reference yet and MUST keep waiting/retrying — it must
    /// never import on a bare source claim (that is the whole point of the gate).
    pub fn max_fresh(&self, within: Duration) -> Option<u64> {
        let m = self.inner.lock().ok()?;
        let now = Instant::now();
        m.values()
            .filter(|(_, seen)| now.duration_since(*seen) <= within)
            .map(|(e, _)| *e)
            .max()
    }

    /// Count of verified peers observed within `within` (the D-4 "≥1 verified
    /// peer" join trigger reads this — a newcomer only announces a `Request` once
    /// it has cross-verified at least one peer, which is also what makes the
    /// X-β3.2c-7 reference meaningful).
    pub fn fresh_peer_count(&self, within: Duration) -> usize {
        match self.inner.lock() {
            Ok(m) => {
                let now = Instant::now();
                m.values()
                    .filter(|(_, seen)| now.duration_since(*seen) <= within)
                    .count()
            }
            Err(_) => 0,
        }
    }
}

// ── pure decision logic (testable) ───────────────────────────────

/// D-3 election: this node's rank in the authority set sorted by account_id
/// ascending, or `None` if it is not a member of that set (a non-member must
/// never serve). Rank 0 = the elected primary responder; higher ranks are the
/// jitter-fallback order used when the primary is down.
pub fn election_rank(my_account_id_hex: &str, authority: &[MembershipSignerWire]) -> Option<usize> {
    let me = my_account_id_hex.to_lowercase();
    let mut ids: Vec<String> = authority
        .iter()
        .map(|s| s.account_id_hex.to_lowercase())
        .collect();
    ids.sort();
    ids.dedup();
    ids.iter().position(|id| *id == me)
}

/// X-β3.2c-7 gate outcome for one `Deliver`.
#[derive(Debug, PartialEq, Eq)]
pub enum EpochGate {
    /// Bundle epoch equals the mesh-observed current epoch → safe to import.
    Accept,
    /// Bundle epoch differs from the reference — a stale (older) bundle is the
    /// wedge risk the gate exists for; an ahead (newer) one is a race / false-high
    /// peer. Either way: do NOT import (the enclave alone would accept a stale one
    /// and then wedge on the fresh-only guard).
    Reject { deliver: u64, reference: u64 },
    /// No fresh verified-peer epoch observed yet → the newcomer has no independent
    /// reference and MUST wait/retry rather than trust the source's bare claim.
    NoReference,
}

/// REQ-β3.2c-impl (X-β3.2c-7): gate a `Deliver`'s bundle epoch against the
/// mesh-observed current authority_epoch (`max_fresh` over admit()-verified
/// peers). RESP-β3.2c: "refuse to import a `Deliver` whose bundle epoch ≠ the
/// mesh-observed current authority_epoch."
pub fn epoch_gate(deliver_epoch: u64, reference: Option<u64>) -> EpochGate {
    match reference {
        None => EpochGate::NoReference,
        Some(r) if deliver_epoch == r => EpochGate::Accept,
        Some(r) => EpochGate::Reject {
            deliver: deliver_epoch,
            reference: r,
        },
    }
}

// ── the join-brain task (source + newcomer roles) ────────────────

/// Peer-attest cache TTL (5 min); a peer epoch older than this is not a fresh
/// reference. Matches the enclave's `peer_attest_cache` lifetime.
const PEER_EPOCH_TTL: Duration = Duration::from_secs(300);
/// Newcomer re-`Request` cadence while unbootstrapped (RESP-β3.2c D-4: ~30 s).
const REQUEST_INTERVAL: Duration = Duration::from_secs(30);
/// Select-loop housekeeping cadence (fire due jitter serves, run the trigger).
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(2);
/// Per-rank jitter delay for a non-primary source (D-3 self-heal).
const ELECTION_JITTER: Duration = Duration::from_secs(3);
/// After this many unanswered `Request`s (~ MAX * 30 s) the newcomer HALTS for
/// the operator rather than silently give up on a security-relevant join (D-4).
const MAX_REQUEST_RETRIES: u32 = 20;
/// Post-bootstrap sync-drift self-check cadence (hardening RESP-β3.2c-impl A):
/// even after sealing, a node compares its epoch to the verified-peer mesh.
const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// A sealed epoch behind the mesh for longer than this is flagged as a likely
/// eclipse-sealed stale membership. A merely-lagging node catches up within
/// seconds via the Seal apply, so a persistent lag is the eclipse-to-stale
/// residual RESP-β3.2c-impl recorded, not a normal transition.
const DRIFT_HALT_AFTER: Duration = Duration::from_secs(120);

/// Everything the join-brain task needs. The orchestrator is UNTRUSTED — none of
/// this grants trust; the enclave enforces source posture + M-1 quorum + escrow
/// binding on every import (this task can at worst DoS a join).
pub struct JoinBrain {
    /// Retention store (source role). `None` disables serving.
    pub db: Option<Db>,
    /// Verified-peer epoch observations (X-β3.2c-7 reference + join trigger).
    pub observed: Arc<ObservedPeerEpochs>,
    /// Loopback client + admin base for the local enclave (X-C1).
    pub client: reqwest::Client,
    pub enclave_admin_base: String,
    /// This cluster's escrow (20-byte, lowercase hex) — the D-6 "right cluster"
    /// config check (also enclave-enforced via the ECIES AAD).
    pub escrow_hex: String,
    pub shard_id: u32,
    pub group_id_hex: String,
    /// This node's 33-byte compressed ECDH identity pubkey (lowercase hex).
    pub my_ecdh_pk: String,
    /// This node's 20-byte account_id (lowercase hex) for D-3 election.
    pub my_account_id_hex: String,
    /// Publishes `Request`/`Deliver` onto the bootstrap gossip topic.
    pub publish_tx: mpsc::Sender<BootstrapMessage>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True iff this node's local enclave reports a sealed membership epoch (i.e. it
/// is already a member, not a fresh newcomer). Drives the source-vs-newcomer mode.
async fn query_bootstrapped(brain: &JoinBrain) -> bool {
    let src = HttpEpochDigestSource::new(brain.client.clone(), brain.enclave_admin_base.clone());
    src.current_epoch().await.is_ok()
}

/// This node's current sealed authority epoch, or `None` if it is not
/// bootstrapped / the enclave is unreachable. Drives the post-bootstrap
/// sync-drift self-check (hardening RESP-β3.2c-impl A).
async fn current_sealed_epoch(brain: &JoinBrain) -> Option<u64> {
    let src = HttpEpochDigestSource::new(brain.client.clone(), brain.enclave_admin_base.clone());
    src.current_epoch().await.ok().map(|(e, _digest)| e)
}

/// Source role, gate + load: return `(retained tuple, my election rank)` iff this
/// node is in-sync (D-5), holds the current-epoch tuple, and is a member of the
/// authority set. `None` → stay silent (another in-sync source answers, or the
/// newcomer retries).
async fn source_prepare(brain: &JoinBrain) -> Option<(CurrentMembershipBundle, usize)> {
    let db = brain.db.as_ref()?;
    // D-5 sync-gate: only serve when the LIVE enclave is in-sync.
    let sync = HttpSyncStateSource::new(brain.client.clone(), brain.enclave_admin_base.clone())
        .sync_state()
        .await
        .ok()??;
    if !sync.in_sync {
        return None;
    }
    let tuple = db.load_current_membership_bundle().await?;
    // Serve ONLY the current epoch (defence in depth over the live sync-gate).
    if tuple.authority_epoch != sync.authority_epoch {
        return None;
    }
    let authority = unpack_membership_signers(&tuple.authority_signers_hex);
    let rank = election_rank(&brain.my_account_id_hex, &authority)?;
    Some((tuple, rank))
}

/// Source role, serve: wrap the retained bundle for `requester_pk` via the local
/// enclave and publish the `Deliver`. Any failure is logged, never fatal.
async fn source_serve(brain: &JoinBrain, requester_pk: &str, tuple: &CurrentMembershipBundle) {
    let authority = unpack_membership_signers(&tuple.authority_signers_hex);
    let export = BootstrapExportInput {
        newcomer_pk_hex: requester_pk,
        shard_id: brain.shard_id,
        group_id_hex: &brain.group_id_hex,
        escrow_hex: &tuple.escrow_hex,
        prev_epoch_hash_hex: &tuple.prev_epoch_hash_hex,
        quorum_bundle_hex: &tuple.quorum_bundle_hex,
        now_ts: now_unix(),
        authority_signers: &authority,
        authority_quorum: tuple.authority_quorum,
        authority_epoch: tuple.authority_epoch,
    };
    match bootstrap_bundle_export(&brain.client, &brain.enclave_admin_base, &export).await {
        Ok(env) => {
            let deliver = BootstrapMessage::Deliver {
                recipient_pubkey: requester_pk.to_string(),
                shard_id: brain.shard_id,
                group_id: brain.group_id_hex.clone(),
                escrow_account_id: tuple.escrow_hex.clone(),
                authority_signers: tuple.authority_signers_hex.clone(),
                authority_signer_count: tuple.authority_signer_count,
                authority_quorum: tuple.authority_quorum,
                authority_epoch: tuple.authority_epoch,
                confirmed_epoch: tuple.confirmed_epoch,
                prev_epoch_hash: tuple.prev_epoch_hash_hex.clone(),
                attesting_signers: tuple.attesting_signers_hex.clone(),
                attesting_signer_count: tuple.attesting_signer_count,
                attesting_quorum: tuple.attesting_quorum,
                ceremony_nonce: env.ceremony_nonce,
                iv: env.iv,
                ct: env.ct,
                tag: env.tag,
                sender_pk: env.sender_pk,
            };
            if brain.publish_tx.send(deliver).await.is_err() {
                warn!("join-brain: bootstrap publish channel closed");
            } else {
                info!(
                    epoch = tuple.authority_epoch,
                    newcomer = %requester_pk,
                    "join-brain: served bootstrap bundle to newcomer"
                );
            }
        }
        Err(e) => {
            warn!(newcomer = %requester_pk, "join-brain: bootstrap-bundle-export failed: {e}")
        }
    }
}

/// Newcomer role: gate the `Deliver`'s epoch (X-β3.2c-7), then import + seal via
/// the local enclave. Returns `true` iff the node is now sealed (join complete).
#[allow(clippy::too_many_arguments)]
async fn newcomer_handle_deliver(
    brain: &JoinBrain,
    escrow_hex: &str,
    shard_id: u32,
    group_id_hex: &str,
    authority_signers_packed: &str,
    authority_quorum: u32,
    authority_epoch: u64,
    confirmed_epoch: u64,
    prev_epoch_hash_hex: &str,
    attesting_signers_packed: &str,
    attesting_quorum: u32,
    envelope: BootstrapEnvelope,
) -> bool {
    // D-6: right cluster? (also enclave-enforced via the AAD; a cheap pre-check.)
    if shard_id != brain.shard_id || !group_id_hex.eq_ignore_ascii_case(&brain.group_id_hex) {
        return false;
    }
    if !escrow_hex.eq_ignore_ascii_case(&brain.escrow_hex) {
        warn!(deliver_escrow = %escrow_hex, "join-brain: Deliver escrow != configured — different cluster; ignoring");
        return false;
    }
    // X-β3.2c-7: independent epoch reference from the verified-peer mesh.
    let reference = brain.observed.max_fresh(PEER_EPOCH_TTL);
    match epoch_gate(authority_epoch, reference) {
        EpochGate::NoReference => {
            debug!("join-brain: no verified-peer epoch reference yet — waiting");
            return false;
        }
        EpochGate::Reject { deliver, reference } => {
            warn!(
                deliver, reference,
                "join-brain X-β3.2c-7: rejecting Deliver — bundle epoch != mesh-current (stale/ahead); not importing"
            );
            return false;
        }
        EpochGate::Accept => {}
    }
    let authority = unpack_membership_signers(authority_signers_packed);
    let attesting = unpack_membership_signers(attesting_signers_packed);
    let import = BootstrapImportInput {
        sender_pk_hex: &envelope.sender_pk,
        shard_id: brain.shard_id,
        group_id_hex: &brain.group_id_hex,
        escrow_hex,
        prev_epoch_hash_hex,
        now_ts: now_unix(),
        authority_signers: &authority,
        authority_quorum,
        authority_epoch,
        confirmed_epoch,
        attesting_signers: &attesting,
        attesting_quorum,
        envelope: &envelope,
    };
    match bootstrap_bundle_import(&brain.client, &brain.enclave_admin_base, &import).await {
        Ok(sealed_epoch) => {
            // Defence in depth: the gate ensured Deliver.epoch == reference; if the
            // sealed epoch somehow differs, this is a failed join (operator wipe).
            if let Some(r) = reference {
                if sealed_epoch != r {
                    error!(
                        sealed_epoch, reference = r,
                        "join-brain X-β3.2c-7: sealed epoch != mesh-current — FAILED join; HALT for operator wipe/re-join"
                    );
                    return false;
                }
            }
            info!(
                epoch = sealed_epoch,
                "join-brain: newcomer sealed membership from bootstrap bundle — join complete"
            );
            true
        }
        Err(e) => {
            warn!("join-brain: bootstrap-bundle-import failed (transient — will retry a fresh Deliver): {e}");
            false
        }
    }
}

/// Publish a `Request` announcing this newcomer wants to join.
async fn publish_request(brain: &JoinBrain) {
    let req = BootstrapMessage::Request {
        requester_pubkey: brain.my_ecdh_pk.clone(),
        shard_id: brain.shard_id,
        group_id: brain.group_id_hex.clone(),
    };
    if brain.publish_tx.send(req).await.is_err() {
        warn!("join-brain: bootstrap publish channel closed (newcomer request)");
    } else {
        info!("join-brain: newcomer published bootstrap Request");
    }
}

/// REQ-β3.2c-impl — the newcomer join-brain run-loop. Consumes inbound
/// `BootstrapMessage`s (source role on `Request`, newcomer role on `Deliver`) and
/// drives the newcomer trigger on a housekeeping tick. One task per node; an
/// already-bootstrapped node acts only as a source, a fresh node only as a
/// newcomer (until it seals, after which it becomes source-capable).
pub async fn run_join_brain(brain: JoinBrain, mut inbound_rx: mpsc::Receiver<BootstrapMessage>) {
    let mut hk = tokio::time::interval(HOUSEKEEPING_INTERVAL);
    hk.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut bootstrapped = query_bootstrapped(&brain).await;
    let mut last_request: Option<Instant> = None;
    let mut request_retries: u32 = 0;
    let mut halted = false;
    // Requester pubkeys already delivered-to (by us or an observed peer Deliver):
    // suppress duplicate + still-pending jitter serves.
    let mut served: HashSet<String> = HashSet::new();
    // Non-primary delayed serves: requester → time it becomes due.
    let mut pending_serves: HashMap<String, Instant> = HashMap::new();
    // Hardening A (post-bootstrap sync-drift self-check) state.
    let mut last_drift_check: Option<Instant> = None;
    let mut drift_since: Option<Instant> = None;
    let mut drift_flagged = false;

    info!(
        bootstrapped,
        shard_id = brain.shard_id,
        "join-brain: started"
    );

    loop {
        tokio::select! {
            maybe = inbound_rx.recv() => {
                let Some(msg) = maybe else {
                    info!("join-brain: inbound channel closed — exiting");
                    break;
                };
                match msg {
                    BootstrapMessage::Request { requester_pubkey, shard_id, group_id } => {
                        if shard_id != brain.shard_id
                            || !group_id.eq_ignore_ascii_case(&brain.group_id_hex)
                            || requester_pubkey.eq_ignore_ascii_case(&brain.my_ecdh_pk)
                        {
                            continue;
                        }
                        // Hardening A: a drift-flagged (likely eclipse-stale) node
                        // must NOT serve — it could hand out a stale bundle.
                        if drift_flagged {
                            continue;
                        }
                        match source_prepare(&brain).await {
                            Some((tuple, 0)) => {
                                // Elected primary → serve immediately.
                                source_serve(&brain, &requester_pubkey, &tuple).await;
                                served.insert(requester_pubkey);
                            }
                            Some((_tuple, rank)) => {
                                // Non-primary → schedule a jittered fallback serve.
                                let due = Instant::now() + ELECTION_JITTER * (rank as u32);
                                pending_serves.entry(requester_pubkey).or_insert(due);
                            }
                            None => { /* not in-sync / no tuple / not a member — stay silent */ }
                        }
                    }
                    BootstrapMessage::Deliver {
                        recipient_pubkey,
                        shard_id,
                        group_id,
                        escrow_account_id,
                        authority_signers,
                        authority_signer_count: _,
                        authority_quorum,
                        authority_epoch,
                        confirmed_epoch,
                        prev_epoch_hash,
                        attesting_signers,
                        attesting_signer_count: _,
                        attesting_quorum,
                        ceremony_nonce,
                        iv,
                        ct,
                        tag,
                        sender_pk,
                    } => {
                        // Any Deliver for a requester means someone served it →
                        // cancel our pending jitter serve + suppress a duplicate.
                        served.insert(recipient_pubkey.clone());
                        pending_serves.remove(&recipient_pubkey);
                        if recipient_pubkey.eq_ignore_ascii_case(&brain.my_ecdh_pk) && !bootstrapped {
                            let env = BootstrapEnvelope { ceremony_nonce, iv, ct, tag, sender_pk };
                            if newcomer_handle_deliver(
                                &brain,
                                &escrow_account_id,
                                shard_id,
                                &group_id,
                                &authority_signers,
                                authority_quorum,
                                authority_epoch,
                                confirmed_epoch,
                                &prev_epoch_hash,
                                &attesting_signers,
                                attesting_quorum,
                                env,
                            ).await {
                                bootstrapped = true;
                            }
                        }
                    }
                }
            }
            _ = hk.tick() => {
                // Fire any due jitter serves whose requester nobody has answered.
                let now = Instant::now();
                let due: Vec<String> = pending_serves
                    .iter()
                    .filter(|(k, t)| **t <= now && !served.contains(*k))
                    .map(|(k, _)| k.clone())
                    .collect();
                for requester in due {
                    pending_serves.remove(&requester);
                    if served.contains(&requester) {
                        continue;
                    }
                    // Hardening A: a drift-flagged node drops its pending serves.
                    if drift_flagged {
                        continue;
                    }
                    if let Some((tuple, _rank)) = source_prepare(&brain).await {
                        source_serve(&brain, &requester, &tuple).await;
                        served.insert(requester);
                    }
                }

                // Newcomer trigger: while unbootstrapped and having ≥1 verified
                // peer (so an X-β3.2c-7 reference exists), re-Request every ~30 s.
                if !bootstrapped && !halted {
                    if query_bootstrapped(&brain).await {
                        bootstrapped = true;
                    } else if brain.observed.fresh_peer_count(PEER_EPOCH_TTL) >= 1 {
                        let due = last_request
                            .is_none_or(|t| now.duration_since(t) >= REQUEST_INTERVAL);
                        if due {
                            publish_request(&brain).await;
                            last_request = Some(now);
                            request_retries += 1;
                            if request_retries >= MAX_REQUEST_RETRIES {
                                error!(
                                    retries = request_retries,
                                    "join-brain: join did not complete after {MAX_REQUEST_RETRIES} Requests — HALT for operator (never silently give up on a membership join)"
                                );
                                halted = true;
                            }
                        }
                    }
                }

                // Hardening A (RESP-β3.2c-impl): post-bootstrap sync-drift
                // self-check. An eclipse-sealed stale node does NOT self-recover
                // (a bootstrapped node stops acting as a newcomer, and it missed
                // the intermediate Seal applies), so it must self-flag: while
                // bootstrapped, compare our sealed epoch to the verified-peer
                // mesh; if we stay BEHIND for > DRIFT_HALT_AFTER, loudly flag +
                // stop serving. A merely-lagging node catches up in seconds via
                // the Seal apply, so only a persistent lag trips this.
                if bootstrapped {
                    let check_due = last_drift_check
                        .is_none_or(|t| now.duration_since(t) >= DRIFT_CHECK_INTERVAL);
                    if check_due {
                        last_drift_check = Some(now);
                        if let (Some(reference), Some(mine)) = (
                            brain.observed.max_fresh(PEER_EPOCH_TTL),
                            current_sealed_epoch(&brain).await,
                        ) {
                            if mine < reference {
                                let since = *drift_since.get_or_insert(now);
                                if now.duration_since(since) >= DRIFT_HALT_AFTER && !drift_flagged {
                                    error!(
                                        sealed = mine, mesh = reference,
                                        "join-brain: post-bootstrap sync-drift — sealed epoch has been BEHIND the verified-peer mesh for >{}s; this node likely eclipse-sealed a stale membership. Halting source role — operator wipe/re-join required.",
                                        DRIFT_HALT_AFTER.as_secs()
                                    );
                                    drift_flagged = true;
                                }
                            } else {
                                // Caught up (== reference, or ahead) → clear.
                                drift_since = None;
                                drift_flagged = false;
                            }
                        }
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
    fn max_fresh_takes_highest_verified_epoch() {
        let obs = ObservedPeerEpochs::new();
        assert_eq!(obs.max_fresh(Duration::from_secs(300)), None);
        assert_eq!(obs.fresh_peer_count(Duration::from_secs(300)), 0);

        obs.record("peerA", 4);
        obs.record("peerB", 5);
        obs.record("peerC", 3);
        // max over verified peers = 5 → a Deliver at epoch 3 or 4 (stale) is below
        // the reference and must be rejected by the caller's gate.
        assert_eq!(obs.max_fresh(Duration::from_secs(300)), Some(5));
        assert_eq!(obs.fresh_peer_count(Duration::from_secs(300)), 3);

        // latest-wins per peer: peerC re-announces a higher epoch.
        obs.record("peerC", 6);
        assert_eq!(obs.max_fresh(Duration::from_secs(300)), Some(6));
        assert_eq!(obs.fresh_peer_count(Duration::from_secs(300)), 3);
    }

    #[test]
    fn stale_observations_age_out() {
        let obs = ObservedPeerEpochs::new();
        obs.record("peerA", 9);
        // a zero-length freshness window means every past observation is stale.
        assert_eq!(obs.max_fresh(Duration::from_nanos(0)), None);
        assert_eq!(obs.fresh_peer_count(Duration::from_nanos(0)), 0);
    }

    fn sig(id: &str) -> MembershipSignerWire {
        MembershipSignerWire {
            account_id_hex: id.repeat(20),
            weight: 1,
        }
    }

    #[test]
    fn election_rank_is_lowest_account_id_first() {
        // set = {cc, aa, bb}; sorted ascending → aa(0), bb(1), cc(2).
        let authority = vec![sig("cc"), sig("aa"), sig("bb")];
        assert_eq!(election_rank(&"aa".repeat(20), &authority), Some(0));
        assert_eq!(election_rank(&"bb".repeat(20), &authority), Some(1));
        assert_eq!(election_rank(&"cc".repeat(20), &authority), Some(2));
        // case-insensitive match.
        assert_eq!(election_rank(&"AA".repeat(20), &authority), Some(0));
        // a non-member never serves.
        assert_eq!(election_rank(&"dd".repeat(20), &authority), None);
    }

    #[test]
    fn epoch_gate_accepts_equal_rejects_otherwise_waits_without_reference() {
        // no verified-peer reference yet → wait (never trust the bare source claim).
        assert_eq!(epoch_gate(5, None), EpochGate::NoReference);
        // equal to mesh-current → accept.
        assert_eq!(epoch_gate(5, Some(5)), EpochGate::Accept);
        // stale (older) → reject: the wedge case the gate exists for.
        assert_eq!(
            epoch_gate(4, Some(5)),
            EpochGate::Reject {
                deliver: 4,
                reference: 5
            }
        );
        // ahead (newer than any verified peer advertised) → also reject.
        assert_eq!(
            epoch_gate(6, Some(5)),
            EpochGate::Reject {
                deliver: 6,
                reference: 5
            }
        );
    }
}
