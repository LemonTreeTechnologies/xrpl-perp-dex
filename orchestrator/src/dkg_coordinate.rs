//! Phase 2.1c-D — DKG ceremony coordination over libp2p.
//!
//! Replaces the SSH-driven `dkg_bootstrap.rs` with a leader+followers
//! protocol that runs entirely over the existing libp2p mesh, per
//! `docs/multi-operator-architecture.md` §3.1 and §6.8. Each operator's
//! orchestrator daemon runs a follower handler (`run_follower`) that
//! reads inbound `DkgStepMessage` from gossipsub topic
//! `perp-dex/cluster/dkg-step` and drives the local enclave through
//! the round-1, round-1.5, round-2, finalize sequence. The leader is
//! whoever runs `POST /admin/dkg/start` on their loopback admin route
//! — that handler publishes the initial `Round1Start` and orchestrates
//! the subsequent step-transitions by observing the `*Done` ack
//! quorum on the same topic.
//!
//! Single-mode: every orchestrator runs the follower; the leader is a
//! per-ceremony role chosen by whichever operator initiates. No SSH,
//! no per-operator credential exchange — peer ECDH pubkeys come from
//! the on-chain `Domain` field discovered in 2.1c-C.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::p2p::{DkgStepMessage, ShareEnvelopeV2Message};
use crate::pool_path_a_client::PoolPathAClient;

/// 32-byte sentinel used as `group_id` during the bootstrap ceremony,
/// before a real FROST `group_id` exists. Mirrors the convention from
/// `docs/testnet-enclave-bump-procedure.md` §9.
pub const SENTINEL_GROUP_ID_ZEROS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Timeout for the leader to wait for the full ceremony to complete.
/// Typical happy path is ~30 s for N=3; 180 s gives generous headroom
/// before declaring a stall.
const CEREMONY_TIMEOUT_SECS: u64 = 180;

/// Per-step polling interval inside the leader's wait loop.
const STEP_POLL_INTERVAL_MS: u64 = 500;

/// Local node's identity used by the follower handler. Built once at
/// orchestrator startup from `local_signer` (xrpl_address + ECDH
/// pubkey) plus the on-chain-discovered roster (peer xrpl_address →
/// peer ECDH pubkey from `Domain`).
///
/// `ecdh_pubkey` is informational only — the follower never sends its
/// own pubkey in DKG-step messages because each peer's enclave
/// already has it in `peer_attest_cache` (populated by the periodic
/// peer-quote announcer).
#[derive(Debug, Clone)]
pub struct LocalIdentity {
    pub xrpl_address: String,
    #[allow(dead_code)]
    pub ecdh_pubkey: String,
    pub peers: HashMap<String, String>,
}

/// Active ceremony state — at most one in flight per node. Wrapped in
/// `Arc<Mutex<>>` so the share-v2 inbound importer can read this
/// node's `vss_commitments` to verify incoming bootstrap shares.
///
/// `threshold` is stored for forensic logging; the actual round-1
/// generation reads it from the `Round1Start` message directly.
#[derive(Debug, Default)]
pub struct ActiveCeremony {
    pub ceremony_id: String,
    #[allow(dead_code)]
    pub threshold: u32,
    pub n_participants: u32,
    pub pid_assignment: Vec<(String, u32)>,
    pub my_pid: u32,
    pub vss_commitments: HashMap<u32, String>,
    pub round1_done: HashMap<u32, ()>,
    pub round15_done: HashMap<u32, ()>,
    pub round2_done: HashMap<u32, ()>,
    pub finalize_done: HashMap<u32, String>,
    pub imported_from_pid: HashMap<u32, ()>,
}

pub struct CoordinatorState {
    pub client: PoolPathAClient,
    pub identity: LocalIdentity,
    pub dkg_step_pub: mpsc::Sender<DkgStepMessage>,
    pub share_v2_pub: mpsc::Sender<ShareEnvelopeV2Message>,
    pub active: Arc<Mutex<Option<ActiveCeremony>>>,
}

/// Follower handler — runs as a tokio task per orchestrator. Reads
/// inbound DKG step messages from gossipsub, executes the local
/// enclave call appropriate to each step, and broadcasts the ack.
pub async fn run_follower(
    state: Arc<CoordinatorState>,
    mut inbound: mpsc::Receiver<DkgStepMessage>,
) {
    while let Some(msg) = inbound.recv().await {
        if let Err(e) = handle_message(&state, msg).await {
            error!("DKG step handling failed: {e}");
            let active = state.active.lock().await;
            if let Some(a) = active.as_ref() {
                let abort = DkgStepMessage::Abort {
                    ceremony_id: a.ceremony_id.clone(),
                    pid: a.my_pid,
                    reason: format!("{e}"),
                };
                drop(active);
                let _ = state.dkg_step_pub.send(abort).await;
            }
        }
    }
}

async fn handle_message(state: &CoordinatorState, msg: DkgStepMessage) -> Result<()> {
    match msg {
        DkgStepMessage::Round1Start {
            ceremony_id,
            threshold,
            n_participants,
            pid_assignment,
        } => {
            round1_start(
                state,
                ceremony_id,
                threshold,
                n_participants,
                pid_assignment,
            )
            .await
        }
        DkgStepMessage::Round1Done {
            ceremony_id,
            pid,
            vss_commitment,
        } => {
            let mut active = state.active.lock().await;
            if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
                a.vss_commitments.insert(pid, vss_commitment);
                a.round1_done.insert(pid, ());
                info!(pid, count = a.round1_done.len(), "Round1Done observed");
            }
            Ok(())
        }
        DkgStepMessage::Round15Start { ceremony_id } => round15_start(state, ceremony_id).await,
        DkgStepMessage::Round15Done { ceremony_id, pid } => {
            let mut active = state.active.lock().await;
            if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
                a.round15_done.insert(pid, ());
                info!(pid, count = a.round15_done.len(), "Round15Done observed");
            }
            Ok(())
        }
        DkgStepMessage::Round2Start { ceremony_id, .. } => round2_start(state, ceremony_id).await,
        DkgStepMessage::Round2Done { ceremony_id, pid } => {
            let mut active = state.active.lock().await;
            if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
                a.round2_done.insert(pid, ());
                info!(pid, count = a.round2_done.len(), "Round2Done observed");
            }
            Ok(())
        }
        DkgStepMessage::FinalizeStart { ceremony_id } => finalize_start(state, ceremony_id).await,
        DkgStepMessage::FinalizeDone {
            ceremony_id,
            pid,
            group_pubkey,
        } => {
            let mut active = state.active.lock().await;
            if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
                a.finalize_done.insert(pid, group_pubkey);
                info!(pid, count = a.finalize_done.len(), "FinalizeDone observed");
            }
            Ok(())
        }
        DkgStepMessage::Abort {
            ceremony_id,
            pid,
            reason,
        } => {
            let mut active = state.active.lock().await;
            if active
                .as_ref()
                .map(|a| a.ceremony_id == ceremony_id)
                .unwrap_or(false)
            {
                error!(pid, %reason, "ceremony aborted by peer");
                *active = None;
            }
            Ok(())
        }
    }
}

async fn round1_start(
    state: &CoordinatorState,
    ceremony_id: String,
    threshold: u32,
    n_participants: u32,
    pid_assignment: Vec<(String, u32)>,
) -> Result<()> {
    let my_pid = pid_assignment
        .iter()
        .find(|(addr, _)| addr == &state.identity.xrpl_address)
        .map(|(_, pid)| *pid)
        .with_context(|| {
            format!(
                "local xrpl_address {} not in pid_assignment",
                state.identity.xrpl_address
            )
        })?;

    info!(
        ceremony_id = %ceremony_id,
        my_pid, threshold, n_participants,
        "Round1Start: generating VSS commitment"
    );

    let vss = state
        .client
        .dkg_round1_generate(my_pid, threshold, n_participants)
        .await
        .context("dkg_round1_generate failed")?;

    let mut a = ActiveCeremony {
        ceremony_id: ceremony_id.clone(),
        threshold,
        n_participants,
        pid_assignment,
        my_pid,
        ..Default::default()
    };
    a.vss_commitments.insert(my_pid, vss.clone());
    a.round1_done.insert(my_pid, ());
    *state.active.lock().await = Some(a);

    state
        .dkg_step_pub
        .send(DkgStepMessage::Round1Done {
            ceremony_id,
            pid: my_pid,
            vss_commitment: vss,
        })
        .await
        .context("publish Round1Done failed")?;
    Ok(())
}

async fn round15_start(state: &CoordinatorState, ceremony_id: String) -> Result<()> {
    let (my_pid, pid_assignment) = {
        let active = state.active.lock().await;
        let a = active
            .as_ref()
            .filter(|a| a.ceremony_id == ceremony_id)
            .with_context(|| format!("Round15Start for unknown ceremony {ceremony_id}"))?;
        (a.my_pid, a.pid_assignment.clone())
    };

    info!(my_pid, "Round15Start: exporting v2 envelopes to peers");
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    for (peer_xrpl, peer_pid) in &pid_assignment {
        if *peer_pid == my_pid {
            continue;
        }
        let peer_ecdh = state
            .identity
            .peers
            .get(peer_xrpl)
            .with_context(|| format!("no ECDH pubkey for peer {peer_xrpl}"))?;
        let env = state
            .client
            .dkg_round1_export_share_v2(*peer_pid, peer_ecdh, 0, SENTINEL_GROUP_ID_ZEROS, now_ts)
            .await
            .with_context(|| format!("dkg_round1_export_share_v2 to {peer_xrpl}"))?;

        // Wrap the DKG envelope in the existing share-v2 message shape
        // so it rides the existing gossipsub topic. The recipient's
        // share-v2 importer discriminates on group_id (sentinel zeros
        // => DKG bootstrap → call dkg_round2_import_share_v2 instead
        // of frost_share_import_v2).
        let wrapped = crate::pool_path_a_client::ShareEnvelopeV2 {
            ceremony_nonce: env.ceremony_nonce,
            iv: env.iv,
            ct: env.ct,
            tag: env.tag,
            sender_pubkey: env.sender_pubkey,
            keygen_cache: String::new(),
            threshold: 0,
            n_participants: 0,
        };
        state
            .share_v2_pub
            .send(ShareEnvelopeV2Message::Deliver {
                recipient_pubkey: peer_ecdh.clone(),
                shard_id: 0,
                group_id: SENTINEL_GROUP_ID_ZEROS.to_string(),
                signer_id: my_pid,
                envelope: wrapped,
            })
            .await
            .context("publish DKG share envelope failed")?;
        info!(target_pid = peer_pid, "queued DKG share envelope");
    }

    {
        let mut active = state.active.lock().await;
        if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
            a.round15_done.insert(my_pid, ());
        }
    }

    state
        .dkg_step_pub
        .send(DkgStepMessage::Round15Done {
            ceremony_id,
            pid: my_pid,
        })
        .await
        .context("publish Round15Done failed")?;
    Ok(())
}

async fn round2_start(state: &CoordinatorState, ceremony_id: String) -> Result<()> {
    // The share-v2 importer (in main.rs) marks `imported_from_pid`
    // entries as DKG envelopes arrive. Round2Start says "leader thinks
    // round 1.5 is done; verify your imports are complete and ack".
    let (my_pid, want) = {
        let active = state.active.lock().await;
        let a = active
            .as_ref()
            .filter(|a| a.ceremony_id == ceremony_id)
            .with_context(|| format!("Round2Start for unknown ceremony {ceremony_id}"))?;
        (a.my_pid, a.n_participants.saturating_sub(1))
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let cnt = state
            .active
            .lock()
            .await
            .as_ref()
            .map(|a| a.imported_from_pid.len() as u32)
            .unwrap_or(0);
        if cnt >= want {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Round2 timeout: imported {cnt} of {want} expected DKG shares");
        }
        tokio::time::sleep(Duration::from_millis(STEP_POLL_INTERVAL_MS)).await;
    }

    info!(my_pid, want, "Round2 imports complete");
    {
        let mut active = state.active.lock().await;
        if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
            a.round2_done.insert(my_pid, ());
        }
    }
    state
        .dkg_step_pub
        .send(DkgStepMessage::Round2Done {
            ceremony_id,
            pid: my_pid,
        })
        .await
        .context("publish Round2Done failed")?;
    Ok(())
}

async fn finalize_start(state: &CoordinatorState, ceremony_id: String) -> Result<()> {
    let my_pid = {
        let active = state.active.lock().await;
        active
            .as_ref()
            .filter(|a| a.ceremony_id == ceremony_id)
            .map(|a| a.my_pid)
            .with_context(|| format!("FinalizeStart for unknown ceremony {ceremony_id}"))?
    };

    let group_pubkey = state.client.dkg_finalize().await.context("dkg_finalize")?;
    info!(my_pid, group_pubkey = %group_pubkey, "Finalize done");

    {
        let mut active = state.active.lock().await;
        if let Some(a) = active.as_mut().filter(|a| a.ceremony_id == ceremony_id) {
            a.finalize_done.insert(my_pid, group_pubkey.clone());
        }
    }

    state
        .dkg_step_pub
        .send(DkgStepMessage::FinalizeDone {
            ceremony_id,
            pid: my_pid,
            group_pubkey,
        })
        .await
        .context("publish FinalizeDone failed")?;
    Ok(())
}

// ── Leader admin route ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StartDkgRequest {
    pub threshold: u32,
}

#[derive(Debug, Serialize)]
pub struct StartDkgResponse {
    pub ceremony_id: String,
    pub status: String,
    pub group_pubkey: Option<String>,
    pub message: Option<String>,
}

pub async fn handle_start_dkg(
    State(state): State<Arc<CoordinatorState>>,
    Json(req): Json<StartDkgRequest>,
) -> Result<Json<StartDkgResponse>, (StatusCode, String)> {
    let ceremony_id = Uuid::new_v4().simple().to_string();
    let pid_assignment = build_pid_assignment(&state.identity);
    let n = pid_assignment.len() as u32;
    validate_threshold(req.threshold, n).map_err(|m| (StatusCode::BAD_REQUEST, m))?;

    info!(
        ceremony_id = %ceremony_id,
        threshold = req.threshold,
        n,
        "leader: initiating DKG ceremony"
    );

    state
        .dkg_step_pub
        .send(DkgStepMessage::Round1Start {
            ceremony_id: ceremony_id.clone(),
            threshold: req.threshold,
            n_participants: n,
            pid_assignment,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("publish Round1Start failed: {e}"),
            )
        })?;

    drive_ceremony(&state, &ceremony_id, n).await
}

async fn drive_ceremony(
    state: &Arc<CoordinatorState>,
    ceremony_id: &str,
    n: u32,
) -> Result<Json<StartDkgResponse>, (StatusCode, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CEREMONY_TIMEOUT_SECS);
    let mut sent_round15 = false;
    let mut sent_round2 = false;
    let mut sent_finalize = false;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Ok(Json(StartDkgResponse {
                ceremony_id: ceremony_id.to_string(),
                status: "timeout".into(),
                group_pubkey: None,
                message: Some(format!("ceremony stalled past {CEREMONY_TIMEOUT_SECS}s")),
            }));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;

        let snapshot = {
            let active = state.active.lock().await;
            active
                .as_ref()
                .filter(|a| a.ceremony_id == ceremony_id)
                .map(|a| {
                    (
                        a.round1_done.len() as u32,
                        a.round15_done.len() as u32,
                        a.round2_done.len() as u32,
                        a.finalize_done.len() as u32,
                        a.vss_commitments.clone(),
                        a.finalize_done.clone(),
                    )
                })
        };
        let Some((r1, r15, r2, fin, vss, finalize_pubkeys)) = snapshot else {
            continue;
        };

        info!(r1, r15, r2, fin, n, "ceremony progress");

        if fin == n {
            let canonical = finalize_pubkeys
                .values()
                .next()
                .cloned()
                .unwrap_or_default();
            let consistent = finalize_pubkeys.values().all(|v| v == &canonical);
            *state.active.lock().await = None;
            return Ok(Json(StartDkgResponse {
                ceremony_id: ceremony_id.to_string(),
                status: if consistent {
                    "success".into()
                } else {
                    "diverged".into()
                },
                group_pubkey: if consistent { Some(canonical) } else { None },
                message: if consistent {
                    None
                } else {
                    Some("group_pubkeys diverged across nodes".into())
                },
            }));
        }

        if r1 == n && !sent_round15 {
            sent_round15 = true;
            let _ = state
                .dkg_step_pub
                .send(DkgStepMessage::Round15Start {
                    ceremony_id: ceremony_id.to_string(),
                })
                .await;
        } else if r15 == n && !sent_round2 {
            sent_round2 = true;
            let vss_pairs: Vec<(u32, String)> = vss.into_iter().collect();
            let _ = state
                .dkg_step_pub
                .send(DkgStepMessage::Round2Start {
                    ceremony_id: ceremony_id.to_string(),
                    vss_commitments: vss_pairs,
                })
                .await;
        } else if r2 == n && !sent_finalize {
            sent_finalize = true;
            let _ = state
                .dkg_step_pub
                .send(DkgStepMessage::FinalizeStart {
                    ceremony_id: ceremony_id.to_string(),
                })
                .await;
        }
    }
}

/// Build a pid_assignment from {self} ∪ peers, sorted ascending by
/// xrpl_address. Pids are assigned contiguously 0..n-1 by position.
/// The sort order is the only convention; operators on different
/// nodes derive the same assignment because the on-chain SignerList
/// (the discovery source for `peers`) is itself ordered by the
/// canonical XRPL ledger.
fn build_pid_assignment(identity: &LocalIdentity) -> Vec<(String, u32)> {
    let mut all: Vec<String> = identity.peers.keys().cloned().collect();
    all.push(identity.xrpl_address.clone());
    all.sort();
    all.into_iter()
        .enumerate()
        .map(|(i, addr)| (addr, i as u32))
        .collect()
}

/// Threshold must satisfy 2 ≤ threshold ≤ n. K=1 is rejected because
/// it defeats the multisig safety property of FROST.
fn validate_threshold(threshold: u32, n: u32) -> Result<(), String> {
    if threshold < 2 || threshold > n {
        Err(format!("invalid threshold {threshold} for n={n}"))
    } else {
        Ok(())
    }
}

pub fn router(state: Arc<CoordinatorState>) -> Router {
    Router::new()
        .route("/admin/dkg/start", post(handle_start_dkg))
        .with_state(state)
}

pub async fn spawn_admin_listener(listen_addr: String, state: Arc<CoordinatorState>) -> Result<()> {
    let parsed: std::net::SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid --dkg-admin-listen address {listen_addr:?}"))?;
    if !parsed.ip().is_loopback() {
        anyhow::bail!(
            "--dkg-admin-listen must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }
    let listener = tokio::net::TcpListener::bind(parsed)
        .await
        .with_context(|| format!("dkg-admin bind on {parsed} failed"))?;
    info!(listen = %parsed, "DKG admin listener started");
    let app = router(state);
    axum::serve(listener, app)
        .await
        .context("dkg-admin serve error")?;
    Ok(())
}

// ── Share-v2 inbound discriminator ─────────────────────────────

/// Replaces the simple `frost_share_import_v2` call inside the
/// share-v2 importer task in `main.rs`. When the envelope carries
/// `group_id == SENTINEL_GROUP_ID_ZEROS` we treat it as a DKG round-1
/// share and call `dkg_round2_import_share_v2` against this node's
/// enclave, looking up the sender's `vss_commitment` from the active
/// ceremony state. Otherwise we fall through to the legacy path.
///
/// Marks `active.imported_from_pid[from_pid]` on success so
/// `round2_start`'s wait loop can advance.
pub async fn route_inbound_share_v2(
    client: &PoolPathAClient,
    active: &Arc<Mutex<Option<ActiveCeremony>>>,
    msg: ShareEnvelopeV2Message,
) -> Result<()> {
    let ShareEnvelopeV2Message::Deliver {
        shard_id,
        group_id,
        signer_id,
        envelope,
        ..
    } = msg;
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if group_id == SENTINEL_GROUP_ID_ZEROS {
        // DKG bootstrap path — look up sender's VSS commitment.
        let vss = {
            let guard = active.lock().await;
            let a = guard
                .as_ref()
                .context("DKG bootstrap envelope arrived but no active ceremony")?;
            a.vss_commitments
                .get(&signer_id)
                .cloned()
                .with_context(|| {
                    format!("no vss_commitment known for sender pid {signer_id} yet")
                })?
        };
        let dkg_env = crate::pool_path_a_client::DkgShareEnvelope {
            ceremony_nonce: envelope.ceremony_nonce,
            iv: envelope.iv,
            ct: envelope.ct,
            tag: envelope.tag,
            sender_pubkey: envelope.sender_pubkey.clone(),
        };
        client
            .dkg_round2_import_share_v2(
                signer_id,
                &envelope.sender_pubkey,
                shard_id,
                &group_id,
                now_ts,
                &dkg_env,
                &vss,
            )
            .await
            .context("dkg_round2_import_share_v2 failed")?;
        let mut guard = active.lock().await;
        if let Some(a) = guard.as_mut() {
            a.imported_from_pid.insert(signer_id, ());
        }
        info!(signer_id, "imported DKG bootstrap share");
    } else {
        // Post-DKG FROST share rotation path — unchanged behaviour.
        match client
            .frost_share_import_v2(&envelope, shard_id, &group_id, signer_id, now_ts)
            .await
        {
            Ok(true) => info!(signer_id, shard_id, "imported v2 FROST share"),
            Ok(false) => warn!(signer_id, "v2 share import refused (403)"),
            Err(e) => warn!(signer_id, "v2 share import error: {}", e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_with_peers(local: &str, peers: &[&str]) -> LocalIdentity {
        let mut p = HashMap::new();
        for (i, addr) in peers.iter().enumerate() {
            p.insert(addr.to_string(), format!("peer-pubkey-{i}"));
        }
        LocalIdentity {
            xrpl_address: local.to_string(),
            ecdh_pubkey: "local-pubkey".into(),
            peers: p,
        }
    }

    #[test]
    fn pid_assignment_sorts_alphabetically_and_indexes_zero_based() {
        let id = id_with_peers(
            "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn",
            &[
                "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j",
                "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ",
            ],
        );
        let pa = build_pid_assignment(&id);
        assert_eq!(pa.len(), 3);
        // Canonical lexicographic sort: rK… < rL… < rw…
        assert_eq!(pa[0].0, "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ");
        assert_eq!(pa[0].1, 0);
        assert_eq!(pa[1].0, "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j");
        assert_eq!(pa[1].1, 1);
        assert_eq!(pa[2].0, "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn");
        assert_eq!(pa[2].1, 2);
    }

    #[test]
    fn pid_assignment_includes_local_when_no_peers() {
        let id = id_with_peers("rAlone1234567890123456789012345", &[]);
        let pa = build_pid_assignment(&id);
        assert_eq!(pa.len(), 1);
        assert_eq!(pa[0].1, 0);
    }

    #[test]
    fn validate_threshold_accepts_2_of_3() {
        validate_threshold(2, 3).unwrap();
    }

    #[test]
    fn validate_threshold_accepts_n_of_n() {
        // K = N is mathematically permitted by FROST (no Byzantine
        // fault tolerance, but liveness if all sign).
        validate_threshold(3, 3).unwrap();
    }

    #[test]
    fn validate_threshold_rejects_one() {
        let err = validate_threshold(1, 3).unwrap_err();
        assert!(err.contains("invalid threshold"));
    }

    #[test]
    fn validate_threshold_rejects_above_n() {
        let err = validate_threshold(4, 3).unwrap_err();
        assert!(err.contains("invalid threshold"));
    }

    #[test]
    fn validate_threshold_rejects_zero() {
        let err = validate_threshold(0, 3).unwrap_err();
        assert!(err.contains("invalid threshold"));
    }
}
