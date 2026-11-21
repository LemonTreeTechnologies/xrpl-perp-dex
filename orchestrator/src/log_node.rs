//! Running the replicated log on a node: the startup decision, the watcher that keeps an unconfigured
//! node from running once its enclave is in log mode, the adapters from the follower to the enclave
//! client and the p2p channels, the follower loop, and the health it reports on `/v1/system/status`.
//!
//! Fail-closed rule: when the enclave is in log mode, this node runs only with member peers configured
//! (election and signing are open to any peer without them), the log transport enabled, and a local
//! signer (the APPLIED share is signed through it). A node missing any of these refuses to start, and a
//! running node without them exits the moment its enclave enters log mode.

use crate::log_client::{ApplyInput, Head, LogClient, LogError, Probe};
use crate::log_follower::{Alarm, Enclave, Follower, Net, Step};
use crate::log_transport::{LogInbound, LogOutbound, Outgoing};
use crate::log_wire::FetchKind;
use anyhow::{bail, Result};
use async_trait::async_trait;
use libp2p::PeerId;
use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// What this node is configured with for the log.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    pub member_peers: bool,
    pub store_dir: bool,
    pub signer: bool,
}

impl LogConfig {
    fn complete(&self) -> bool {
        self.member_peers && self.store_dir && self.signer
    }

    fn missing(&self) -> String {
        let mut m = vec![];
        if !self.member_peers {
            m.push("--member-peers");
        }
        if !self.store_dir {
            m.push("--log-store-dir");
        }
        if !self.signer {
            m.push("a local signer (signers-config)");
        }
        m.join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    /// No log here: run as before, with the watcher.
    Legacy,
    /// Run the follower.
    Follow,
}

/// Decide at startup. Refuses when the enclave is in log mode without a complete configuration, and when
/// the configuration is partial (a log store without the rest) — a half-configured node is a mistake
/// that should stop startup, not one that should be discovered at genesis.
pub fn startup_decision(p: &Probe, c: &LogConfig) -> Result<Setup> {
    match p {
        Probe::Head(h) if h.on => {
            if c.complete() {
                Ok(Setup::Follow)
            } else {
                bail!(
                    "the enclave is in replicated-log mode; refusing to start without {}",
                    c.missing()
                )
            }
        }
        Probe::Head(_) if c.complete() => Ok(Setup::Follow),
        Probe::Head(_) if c.store_dir => bail!(
            "--log-store-dir is set but the log configuration is incomplete: missing {}",
            c.missing()
        ),
        Probe::Head(_) => Ok(Setup::Legacy),
        Probe::Absent => {
            if c.store_dir {
                tracing::warn!("--log-store-dir is set but this enclave has no replicated log; running without it");
            }
            Ok(Setup::Legacy)
        }
    }
}

/// Read the enclave's log mode, retrying while it cannot be read; refuse when it never can — a node that
/// cannot tell whether its enclave is in log mode must not guess "no".
pub async fn probe_at_startup(
    c: &dyn ProbeSource,
    attempts: u32,
    every: Duration,
) -> Result<Probe> {
    let mut last = String::new();
    for i in 0..attempts {
        match c.probe().await {
            Ok(p) => return Ok(p),
            Err(e) => last = format!("{e:?}"),
        }
        if i + 1 < attempts {
            tokio::time::sleep(every).await;
        }
    }
    bail!("cannot read the enclave's log mode after {attempts} attempts (last: {last}); refusing to start")
}

#[async_trait]
pub trait ProbeSource: Send + Sync {
    async fn probe(&self) -> Result<Probe, LogError>;
}

#[async_trait]
impl ProbeSource for LogClient {
    async fn probe(&self) -> Result<Probe, LogError> {
        LogClient::probe(self).await
    }
}

/// For a node running without the log: returns once its enclave is in log mode (the caller then stops
/// the process). Unreadable reads are retried — the startup check already established the mode once.
pub async fn watch_for_log_mode(c: &dyn ProbeSource, every: Duration) {
    loop {
        if let Ok(Probe::Head(h)) = c.probe().await {
            if h.on {
                return;
            }
        }
        tokio::time::sleep(every).await;
    }
}

// ── Adapters ────────────────────────────────────────────────────────────

/// The enclave, as the follower sees it: the log client plus this node's signer credentials.
pub struct EnclaveAdapter {
    pub client: LogClient,
    pub account_id: String,
    pub session_key: [u8; 32],
}

#[async_trait]
impl Enclave for EnclaveAdapter {
    async fn head(&self) -> Result<Head, LogError> {
        self.client.head().await
    }
    async fn apply(&self, input: &ApplyInput<'_>) -> Result<Vec<u8>, LogError> {
        self.client
            .apply(input, &self.account_id, &self.session_key)
            .await
    }
    async fn check_applied(
        &self,
        seq: u64,
        chain_hash: &[u8; 32],
        bundle: &[u8],
        quorum: bool,
    ) -> Result<(), LogError> {
        self.client
            .check_applied(seq, chain_hash, bundle, quorum)
            .await
    }
}

/// The network, as the follower sees it: the p2p node's log channel.
pub struct NetAdapter {
    pub tx: mpsc::Sender<LogOutbound>,
    pub fetch_timeout: Duration,
}

#[async_trait]
impl Net for NetAdapter {
    async fn fetch(
        &self,
        kind: FetchKind,
        seq: u64,
        peer: PeerId,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(LogOutbound::Fetch {
                kind,
                seq,
                peer,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("p2p log channel closed"))?;
        match tokio::time::timeout(self.fetch_timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("fetch dropped"),
            Err(_) => bail!("fetch timed out"),
        }
    }
    async fn publish(&self, out: Outgoing) {
        if self.tx.send(LogOutbound::Publish(out)).await.is_err() {
            tracing::warn!("p2p log channel closed; publish dropped");
        }
    }
}

// ── Health ─────────────────────────────────────────────────────────────

/// Alarms kept for the status endpoint.
const RECENT_ALARMS: usize = 32;

#[derive(Debug, Clone, Default, Serialize)]
pub struct LogStatus {
    /// "legacy" | "follow"
    pub mode: &'static str,
    /// The enclave reports log mode.
    pub on: bool,
    pub enclave_seq: u64,
    pub stored_seq: Option<u64>,
    /// This node stopped following (code of the refusal).
    pub halted: Option<i64>,
    /// This node's replicated state differs from the log it was given: its reads are not the cluster's.
    pub diverged: bool,
    pub last_step: String,
    pub alarms_total: u64,
    pub recent_alarms: Vec<String>,
}

#[derive(Debug, Default)]
pub struct LogHealth(Mutex<LogStatus>);

impl LogHealth {
    pub fn new(mode: &'static str) -> Self {
        Self(Mutex::new(LogStatus {
            mode,
            ..LogStatus::default()
        }))
    }

    pub fn snapshot(&self) -> LogStatus {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_mode(&self, mode: &'static str) {
        self.update(|s| s.mode = mode);
    }

    fn update(&self, f: impl FnOnce(&mut LogStatus)) {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn record(&self, f: &Follower, step: &Step, head: Option<&Head>, alarms: Vec<Alarm>) {
        self.update(|s| {
            if let Some(h) = head {
                s.on = h.on;
                s.enclave_seq = h.seq;
            }
            s.stored_seq = f.store().last_seq();
            s.halted = f.halted();
            s.diverged = matches!(f.halted(), Some(c) if c == crate::log_client::rc::DIVERGED);
            s.last_step = format!("{step:?}");
            for a in alarms {
                s.alarms_total = s.alarms_total.saturating_add(1);
                if s.recent_alarms.len() == RECENT_ALARMS {
                    s.recent_alarms.remove(0);
                }
                s.recent_alarms.push(format!("{a:?}"));
            }
        });
    }
}

// ── The loop ───────────────────────────────────────────────────────────

/// Drive the follower: handle peer messages as they arrive, and step whenever the tick fires; after an
/// applied step, step again at once (catching up), otherwise wait for the next tick.
pub async fn run_follower(
    mut f: Follower,
    enclave: &dyn Enclave,
    net: &dyn Net,
    mut inbound: mpsc::Receiver<LogInbound>,
    health: &LogHealth,
    tick: Duration,
) {
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            m = inbound.recv() => match m {
                Some(m) => {
                    f.on_inbound(enclave, m).await;
                    let alarms = f.take_alarms();
                    if !alarms.is_empty() {
                        health.record(&f, &Step::Idle, None, alarms);
                    }
                }
                None => {
                    tracing::error!("log inbound channel closed; the follower stops");
                    return;
                }
            },
            _ = ticker.tick() => {
                loop {
                    let step = f.step(enclave, net, Instant::now()).await;
                    let head = enclave.head().await.ok();
                    let alarms = f.take_alarms();
                    health.record(&f, &step, head.as_ref(), alarms);
                    if !matches!(step, Step::Applied { .. }) {
                        break;
                    }
                }
            }
        }
    }
}

/// The member peers the follower fetches from: every member except this node.
pub fn other_members(members: &[PeerId], me: &PeerId) -> Vec<PeerId> {
    members.iter().filter(|p| *p != me).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(on: bool) -> Probe {
        Probe::Head(Head {
            seq: 0,
            t_batch: 0,
            chain_hash: [0; 32],
            fenced: false,
            on,
        })
    }

    const ALL: LogConfig = LogConfig {
        member_peers: true,
        store_dir: true,
        signer: true,
    };

    #[test]
    fn log_mode_refuses_to_start_unless_every_piece_is_configured() {
        assert_eq!(startup_decision(&head(true), &ALL).unwrap(), Setup::Follow);
        for (c, missing) in [
            (
                LogConfig {
                    member_peers: false,
                    ..ALL
                },
                "--member-peers",
            ),
            (
                LogConfig {
                    store_dir: false,
                    ..ALL
                },
                "--log-store-dir",
            ),
            (
                LogConfig {
                    signer: false,
                    ..ALL
                },
                "local signer",
            ),
            (
                LogConfig {
                    member_peers: false,
                    store_dir: false,
                    signer: false,
                },
                "--member-peers",
            ),
        ] {
            let e = startup_decision(&head(true), &c).unwrap_err().to_string();
            assert!(
                e.contains("refusing to start") && e.contains(missing),
                "{e}"
            );
        }
    }

    #[test]
    fn before_genesis_a_complete_node_follows_a_bare_node_runs_and_a_partial_one_refuses() {
        assert_eq!(startup_decision(&head(false), &ALL).unwrap(), Setup::Follow);
        let bare = LogConfig {
            member_peers: false,
            store_dir: false,
            signer: true,
        };
        assert_eq!(
            startup_decision(&head(false), &bare).unwrap(),
            Setup::Legacy
        );
        // Member peers alone (no log store) is the pre-log hardening; it runs.
        let members_only = LogConfig {
            member_peers: true,
            store_dir: false,
            signer: true,
        };
        assert_eq!(
            startup_decision(&head(false), &members_only).unwrap(),
            Setup::Legacy
        );
        let partial = LogConfig {
            member_peers: false,
            store_dir: true,
            signer: true,
        };
        assert!(startup_decision(&head(false), &partial)
            .unwrap_err()
            .to_string()
            .contains("--member-peers"));
    }

    #[test]
    fn an_enclave_without_the_log_runs_as_before() {
        let bare = LogConfig {
            member_peers: false,
            store_dir: false,
            signer: false,
        };
        assert_eq!(
            startup_decision(&Probe::Absent, &bare).unwrap(),
            Setup::Legacy
        );
        assert_eq!(
            startup_decision(&Probe::Absent, &ALL).unwrap(),
            Setup::Legacy
        );
    }

    struct Seq(Mutex<Vec<Result<Probe, LogError>>>, Mutex<u32>);

    #[async_trait]
    impl ProbeSource for Seq {
        async fn probe(&self) -> Result<Probe, LogError> {
            *self.1.lock().unwrap() += 1;
            let mut v = self.0.lock().unwrap();
            if v.len() > 1 {
                v.remove(0)
            } else {
                match &v[0] {
                    Ok(p) => Ok(p.clone()),
                    Err(_) => Err(LogError::Transport("down".into())),
                }
            }
        }
    }

    fn down() -> Result<Probe, LogError> {
        Err(LogError::Transport("down".into()))
    }

    #[tokio::test]
    async fn an_unreadable_mode_is_retried_then_refused_never_guessed() {
        let s = Seq(
            Mutex::new(vec![down(), down(), Ok(head(true))]),
            Mutex::new(0),
        );
        assert_eq!(
            probe_at_startup(&s, 5, Duration::ZERO).await.unwrap(),
            head(true)
        );
        let s = Seq(Mutex::new(vec![down()]), Mutex::new(0));
        let e = probe_at_startup(&s, 4, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing to start"), "{e}");
        assert_eq!(*s.1.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn the_watcher_returns_only_once_the_enclave_is_in_log_mode() {
        let s = Seq(
            Mutex::new(vec![
                Ok(Probe::Absent),
                down(),
                Ok(head(false)),
                Ok(head(true)),
            ]),
            Mutex::new(0),
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            watch_for_log_mode(&s, Duration::ZERO),
        )
        .await
        .unwrap();
        assert_eq!(*s.1.lock().unwrap(), 4);
        let off = Seq(Mutex::new(vec![Ok(head(false))]), Mutex::new(0));
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            watch_for_log_mode(&off, Duration::from_millis(1))
        )
        .await
        .is_err());
    }

    #[test]
    fn other_members_excludes_this_node() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        assert_eq!(other_members(&[a, me, b], &me), vec![a, b]);
    }

    use crate::log_client::rc;
    use crate::log_follower::FollowerCfg;
    use crate::log_store::{encode_record, LogRecord, LogStore};

    /// An enclave at seq 0 that answers every apply with `code` (0 = applied, no share).
    struct OneApply {
        code: i64,
        seq: Mutex<u64>,
    }

    #[async_trait]
    impl Enclave for OneApply {
        async fn head(&self) -> Result<Head, LogError> {
            let seq = *self.seq.lock().unwrap();
            Ok(Head {
                seq,
                t_batch: 0,
                chain_hash: [seq as u8; 32],
                fenced: false,
                on: true,
            })
        }
        async fn apply(&self, _: &ApplyInput<'_>) -> Result<Vec<u8>, LogError> {
            if self.code == 0 {
                *self.seq.lock().unwrap() += 1;
                Ok(vec![])
            } else {
                Err(LogError::Enclave(crate::log_client::LogOutcome::from_rc(
                    self.code,
                )))
            }
        }
        async fn check_applied(
            &self,
            _: u64,
            _: &[u8; 32],
            _: &[u8],
            _: bool,
        ) -> Result<(), LogError> {
            Err(LogError::Enclave(crate::log_client::LogOutcome::from_rc(
                rc::BAD_SIG,
            )))
        }
    }

    /// Every peer serves records 1..=3.
    struct Serves;

    #[async_trait]
    impl Net for Serves {
        async fn fetch(
            &self,
            kind: FetchKind,
            seq: u64,
            _: PeerId,
        ) -> anyhow::Result<Option<Vec<u8>>> {
            if kind != FetchKind::Record || !(1..=3).contains(&seq) {
                return Ok(None);
            }
            Ok(Some(
                encode_record(&LogRecord {
                    seq,
                    chain_hash: [seq as u8; 32],
                    market_id: 0,
                    batch: vec![seq as u8],
                    quote_blob: vec![9; 40],
                    side_blobs: vec![],
                    sig_bundle: vec![7; 72],
                })
                .unwrap(),
            ))
        }
        async fn publish(&self, _: Outgoing) {}
    }

    async fn run_until(code: i64, tick: Duration, done: impl Fn(&LogStatus) -> bool) -> LogStatus {
        let d = tempfile::tempdir().unwrap();
        let f = Follower::new(
            FollowerCfg::default(),
            vec![PeerId::random()],
            LogStore::open(d.path()).unwrap(),
        );
        let health = LogHealth::new("follow");
        let (_tx, rx) = mpsc::channel(1);
        let e = OneApply {
            code,
            seq: Mutex::new(0),
        };
        let run = run_follower(f, &e, &Serves, rx, &health, tick);
        tokio::pin!(run);
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut run => panic!("the loop ended"),
                _ = &mut deadline => panic!("never reached: {:?}", health.snapshot()),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    let s = health.snapshot();
                    if done(&s) {
                        return s;
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn the_loop_catches_up_and_reports_its_position() {
        // One tick (the first fires at once; the next is an hour away): catching up must not wait for ticks.
        let s = run_until(0, Duration::from_secs(3600), |s| s.stored_seq == Some(3)).await;
        assert_eq!((s.enclave_seq, s.halted, s.diverged), (3, None, false));
    }

    #[tokio::test]
    async fn a_diverged_node_reports_diverged_and_its_alarm() {
        let s = run_until(rc::DIVERGED, Duration::from_millis(5), |s| {
            s.halted.is_some()
        })
        .await;
        assert!(s.diverged);
        assert_eq!(s.halted, Some(rc::DIVERGED));
        assert_eq!(s.alarms_total, 1);
        assert!(s.recent_alarms[0].contains("Halted"));
    }

    #[tokio::test]
    async fn a_fenced_halt_is_halted_but_not_diverged() {
        let s = run_until(rc::FENCED, Duration::from_millis(5), |s| s.halted.is_some()).await;
        assert_eq!((s.halted, s.diverged), (Some(rc::FENCED), false));
    }
}
