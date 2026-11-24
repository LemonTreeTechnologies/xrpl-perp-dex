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
use crate::log_sync::{ServerInfo, SyncGate, SyncStatus, SyncVerdict};
use crate::log_transport::{LogInbound, LogOutbound, Outgoing};
use crate::log_view::{ViewBook, ViewEvent};
use crate::log_wire::{FetchKind, Parsed, ViewReport};
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
    /// --vault-mm or --vault-dn: the vault places orders from the node itself, outside the log.
    pub vault: bool,
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
            if c.vault {
                bail!(
                    "the enclave is in replicated-log mode; refusing to start with --vault-mm/--vault-dn \
                     (the vault places orders from this node, outside the log)"
                )
            }
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
    async fn view(&self) -> Result<crate::log_client::View, LogError> {
        self.client.view().await
    }
    async fn apply(&self, input: &ApplyInput<'_>) -> Result<Vec<u8>, LogError> {
        self.client
            .apply(input, &self.account_id, &self.session_key)
            .await
    }
    async fn check_applied(
        &self,
        seq: u64,
        term: u64,
        chain_hash: &[u8; 32],
        bundle: &[u8],
        quorum: bool,
    ) -> Result<(), LogError> {
        self.client
            .check_applied(seq, term, chain_hash, bundle, quorum)
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
    /// The leader must hold its next commit: member views disagree with no majority (C1).
    pub view_hold: bool,
    /// The last seq every member reported the same view for.
    pub view_agreed_seq: Option<u64>,
    /// This node's XRPL sync gate.
    pub sync: SyncStatus,
    pub alarms_total: u64,
    pub recent_alarms: Vec<String>,
}

impl LogStatus {
    fn state_is_not_the_clusters(&self) -> bool {
        self.diverged || self.halted.is_some()
    }
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

    /// Whether this node must refuse trading reads and writes: its replicated state is known not to be
    /// the cluster's. That is any halt — the follower latches a halt only for a code it can no longer follow
    /// past (diverged, fenced, a prologue failure, a refusal while fenced), after which the state stays
    /// frozen and falls further behind. A transient refusal (gap, stale, not yet) never latches a halt: the
    /// follower keeps fetching and catches up. Any halt, not a list of codes, so a new halt code cannot
    /// leave this open.
    pub fn refuses_trading(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state_is_not_the_clusters()
    }

    pub fn set_mode(&self, mode: &'static str) {
        self.update(|s| s.mode = mode);
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut LogStatus)) {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Record the follower's state, the step just taken (if any), the head, and drain its alarms.
    fn record(&self, f: &mut Follower, step: Option<&Step>, head: Option<&Head>, hold: bool) {
        let alarms = f.take_alarms();
        self.update(|s| {
            if let Some(h) = head {
                s.on = h.on;
                s.enclave_seq = h.seq;
            }
            s.stored_seq = f.store().last_seq();
            s.halted = f.halted();
            s.diverged = matches!(f.halted(), Some(c) if c == crate::log_client::rc::DIVERGED);
            if let Some(step) = step {
                s.last_step = format!("{step:?}");
            }
            s.view_hold = hold;
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

/// This node's own XRPL endpoint, for the sync gate.
#[async_trait]
pub trait XrplSource: Send + Sync {
    async fn server_info(&self) -> Result<ServerInfo>;
}

pub struct XrplHttp {
    pub http: reqwest::Client,
    pub url: String,
}

#[async_trait]
impl XrplSource for XrplHttp {
    async fn server_info(&self) -> Result<ServerInfo> {
        crate::log_sync::server_info(&self.http, &self.url).await
    }
}

/// What the loop runs against.
pub struct LoopParts<'a> {
    pub enclave: &'a dyn Enclave,
    pub net: &'a dyn Net,
    pub xrpl: &'a dyn XrplSource,
    pub health: &'a LogHealth,
    /// This node's peer id (its own view reports are filed under it).
    pub me: PeerId,
    pub tick: Duration,
}

/// Act on the view compare's findings: a minority of one halts this node; the rest are alarms.
fn on_view_events(f: &mut Follower, health: &LogHealth, events: Vec<ViewEvent>) {
    for e in events {
        match e {
            ViewEvent::Agreed { seq } => health.update(|s| s.view_agreed_seq = Some(seq)),
            ViewEvent::SelfOdd { seq } => f.halt_view_minority(seq),
            ViewEvent::PeerOdd { seq, peer } => f.alarm(Alarm::ViewPeerOdd { seq, peer }),
            ViewEvent::Split { seq } => f.alarm(Alarm::ViewSplit { seq }),
            ViewEvent::Equivocation { seq, peer } => f.alarm(Alarm::ViewEquivocation { seq, peer }),
        }
    }
}

/// Drive the follower. Peer messages are handled as they arrive (view reports go to the view compare).
/// On each tick: if this node's XRPL node is not in sync, wait; otherwise step, and after an applied
/// entry report this node's view, then step again at once (catching up) until there is nothing to apply.
pub async fn run_follower(
    mut f: Follower,
    mut views: ViewBook,
    mut gate: SyncGate,
    mut inbound: mpsc::Receiver<LogInbound>,
    p: LoopParts<'_>,
) {
    let mut ticker = tokio::time::interval(p.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            m = inbound.recv() => match m {
                Some(LogInbound { author, msg: Parsed::View(r) }) => {
                    let anchor = f.store().last_seq().unwrap_or(0);
                    let ev = views.add(author, r, anchor);
                    on_view_events(&mut f, p.health, ev);
                    p.health.record(&mut f, None, None, views.hold());
                }
                Some(m) => {
                    f.on_inbound(p.enclave, m).await;
                    p.health.record(&mut f, None, None, views.hold());
                }
                None => {
                    tracing::error!("log inbound channel closed; the follower stops");
                    return;
                }
            },
            _ = ticker.tick() => {
                let verdict = match p.xrpl.server_info().await {
                    Ok(si) => {
                        let v = gate.evaluate(&si, Instant::now());
                        let st = gate.status(&si, &v);
                        p.health.update(|s| s.sync = st);
                        v
                    }
                    Err(e) => {
                        let v = SyncVerdict::Wait { reason: format!("server_info: {e:#}") };
                        p.health.update(|s| s.sync.verdict = Some(v.clone()));
                        v
                    }
                };
                if verdict != SyncVerdict::InSync {
                    let w = Step::Wait("this node's XRPL node is not in sync");
                    p.health.record(&mut f, Some(&w), None, views.hold());
                    continue;
                }
                loop {
                    let step = f.step(p.enclave, p.net, Instant::now()).await;
                    if let Step::Applied { seq } = step {
                        if let Ok(v) = p.enclave.view().await {
                            if v.seq == seq {
                                let r = ViewReport { seq, chain_hash: v.chain_hash, digest: v.digest };
                                p.net.publish(Outgoing::view(&r)).await;
                                let ev = views.add(p.me, r, seq);
                                on_view_events(&mut f, p.health, ev);
                            }
                        }
                    }
                    let head = p.enclave.head().await.ok();
                    p.health.record(&mut f, Some(&step), head.as_ref(), views.hold());
                    if !matches!(step, Step::Applied { .. }) || f.halted().is_some() {
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
            term: 0,
            fenced: false,
            on,
        })
    }

    const ALL: LogConfig = LogConfig {
        member_peers: true,
        store_dir: true,
        signer: true,
        vault: false,
    };

    #[test]
    fn log_mode_refuses_to_start_with_the_vault_flags() {
        let vault = LogConfig { vault: true, ..ALL };
        let e = startup_decision(&head(true), &vault)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--vault-mm/--vault-dn"), "{e}");
        // Before genesis the vault still runs (legacy trading), with or without the log configured.
        assert_eq!(
            startup_decision(&head(false), &vault).unwrap(),
            Setup::Follow
        );
        assert_eq!(
            startup_decision(&Probe::Absent, &vault).unwrap(),
            Setup::Legacy
        );
    }

    #[test]
    fn every_halt_refuses_trading_and_a_following_node_does_not() {
        use crate::log_client::{rc, LogOutcome};
        assert!(!LogHealth::new("follow").refuses_trading());
        // Every code the follower halts on (the Halted class), each on its own. (A transient such as STALE
        // never latches a halt: log_follower's stale tests assert halted() stays None.)
        for code in [
            rc::DIVERGED,
            rc::FENCED,
            rc::PROLOGUE_FAILED,
            rc::ERR_FENCED,
        ] {
            assert!(matches!(LogOutcome::from_rc(code), LogOutcome::Halted(_)));
            let h = LogHealth::new("follow");
            h.update(|s| s.halted = Some(code));
            assert!(h.refuses_trading(), "halted on {code} must refuse");
        }
    }

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
                    vault: false,
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
            vault: false,
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
            vault: false,
        };
        assert_eq!(
            startup_decision(&head(false), &members_only).unwrap(),
            Setup::Legacy
        );
        let partial = LogConfig {
            member_peers: false,
            store_dir: true,
            signer: true,
            vault: false,
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
            vault: false,
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
                term: 0,
                fenced: false,
                on: true,
            })
        }
        async fn view(&self) -> Result<crate::log_client::View, LogError> {
            let seq = *self.seq.lock().unwrap();
            Ok(crate::log_client::View {
                seq,
                chain_hash: [seq as u8; 32],
                digest: [seq as u8; 32],
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

    /// Every peer serves records 1..=3; publishes are recorded.
    #[derive(Default)]
    struct Serves(Mutex<Vec<crate::log_wire::LogGossip>>);

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
        async fn publish(&self, out: Outgoing) {
            self.0.lock().unwrap().push(out.gossip().clone());
        }
    }

    struct FixedXrpl(ServerInfo);

    #[async_trait]
    impl XrplSource for FixedXrpl {
        async fn server_info(&self) -> Result<ServerInfo> {
            Ok(self.0.clone())
        }
    }

    fn synced() -> ServerInfo {
        ServerInfo {
            server_state: "full".into(),
            peers: 10,
            validated_seq: 100,
            validated_age_s: 1,
        }
    }

    type Until = Box<dyn Fn(&LogStatus) -> bool>;

    /// Run the loop; for each stage send its messages, then wait until its condition holds.
    async fn run_stages(
        code: i64,
        tick: Duration,
        xrpl: ServerInfo,
        stages: Vec<(Vec<LogInbound>, Until)>,
        me: PeerId,
        members: Vec<PeerId>,
    ) -> (LogStatus, Vec<crate::log_wire::LogGossip>) {
        let d = tempfile::tempdir().unwrap();
        let others = other_members(&members, &me);
        let f = Follower::new(
            FollowerCfg::default(),
            others,
            LogStore::open(d.path()).unwrap(),
        );
        let health = LogHealth::new("follow");
        let (tx, rx) = mpsc::channel(16);
        let e = OneApply {
            code,
            seq: Mutex::new(0),
        };
        let net = Serves::default();
        let x = FixedXrpl(xrpl);
        let parts = LoopParts {
            enclave: &e,
            net: &net,
            xrpl: &x,
            health: &health,
            me,
            tick,
        };
        let run = run_follower(
            f,
            ViewBook::new(me, members, 16),
            SyncGate::new(Default::default()),
            rx,
            parts,
        );
        tokio::pin!(run);
        for (send, until) in stages {
            for m in send {
                tx.send(m).await.unwrap();
            }
            let deadline = tokio::time::sleep(Duration::from_secs(5));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut run => panic!("the loop ended"),
                    _ = &mut deadline => panic!("never reached: {:?}", health.snapshot()),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if until(&health.snapshot()) {
                            break;
                        }
                    }
                }
            }
        }
        let published = net.0.lock().unwrap().clone();
        (health.snapshot(), published)
    }

    async fn run_until(
        code: i64,
        tick: Duration,
        done: impl Fn(&LogStatus) -> bool + 'static,
    ) -> LogStatus {
        let me = PeerId::random();
        run_stages(
            code,
            tick,
            synced(),
            vec![(vec![], Box::new(done))],
            me,
            vec![me, PeerId::random()],
        )
        .await
        .0
    }

    fn view_from(author: PeerId, seq: u64, d: u8) -> LogInbound {
        LogInbound {
            author,
            msg: Parsed::View(ViewReport {
                seq,
                chain_hash: [seq as u8; 32],
                digest: [d; 32],
            }),
        }
    }

    #[tokio::test]
    async fn the_loop_catches_up_and_reports_its_position() {
        // One tick (the first fires at once; the next is an hour away): catching up must not wait for ticks.
        let s = run_until(0, Duration::from_secs(3600), |s| s.stored_seq == Some(3)).await;
        assert_eq!((s.enclave_seq, s.halted, s.diverged), (3, None, false));
    }

    #[tokio::test]
    async fn after_each_applied_seq_this_node_publishes_its_view() {
        let me = PeerId::random();
        let (_, published) = run_stages(
            0,
            Duration::from_secs(3600),
            synced(),
            vec![(vec![], Box::new(|s: &LogStatus| s.stored_seq == Some(3)))],
            me,
            vec![me, PeerId::random(), PeerId::random()],
        )
        .await;
        let views: Vec<_> = published
            .iter()
            .filter_map(|g| match g {
                crate::log_wire::LogGossip::View { seq, digest, .. } => {
                    Some((*seq, digest.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            views,
            (1..=3)
                .map(|q| (q, hex::encode([q as u8; 32])))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn an_out_of_sync_xrpl_node_applies_nothing() {
        let me = PeerId::random();
        let behind = ServerInfo {
            server_state: "tracking".into(),
            ..synced()
        };
        let (s, _) = run_stages(
            0,
            Duration::from_millis(5),
            behind,
            vec![(
                vec![],
                Box::new(|s: &LogStatus| s.last_step.contains("not in sync")),
            )],
            me,
            vec![me, PeerId::random()],
        )
        .await;
        assert_eq!(s.stored_seq, None);
        assert!(
            matches!(&s.sync.verdict, Some(SyncVerdict::Wait { reason }) if reason.contains("tracking"))
        );
    }

    #[tokio::test]
    async fn this_node_in_the_view_minority_halts_as_diverged() {
        let (me, a, b) = (PeerId::random(), PeerId::random(), PeerId::random());
        // This node's view at seq 1 has digest [1; 32]; both peers report [9; 32].
        let (s, _) = run_stages(
            0,
            Duration::from_secs(3600),
            synced(),
            vec![
                (vec![], Box::new(|s: &LogStatus| s.stored_seq == Some(3))),
                (
                    vec![view_from(a, 1, 9), view_from(b, 1, 9)],
                    Box::new(|s: &LogStatus| s.halted.is_some()),
                ),
            ],
            me,
            vec![me, a, b],
        )
        .await;
        assert!(s.diverged);
        assert!(
            s.recent_alarms.iter().any(|a| a.contains("ViewMinority")),
            "{:?}",
            s.recent_alarms
        );
    }

    #[tokio::test]
    async fn an_odd_peer_is_an_alarm_and_this_node_continues() {
        let (me, a, b) = (PeerId::random(), PeerId::random(), PeerId::random());
        let (s, _) = run_stages(
            0,
            Duration::from_secs(3600),
            synced(),
            vec![
                (vec![], Box::new(|s: &LogStatus| s.stored_seq == Some(3))),
                (
                    vec![view_from(a, 1, 1), view_from(b, 1, 9)],
                    Box::new(|s: &LogStatus| {
                        s.recent_alarms.iter().any(|x| x.contains("ViewPeerOdd"))
                    }),
                ),
            ],
            me,
            vec![me, a, b],
        )
        .await;
        assert_eq!((s.halted, s.diverged, s.view_hold), (None, false, false));
    }

    #[tokio::test]
    async fn every_member_agreeing_is_recorded() {
        let (me, a, b) = (PeerId::random(), PeerId::random(), PeerId::random());
        let (s, _) = run_stages(
            0,
            Duration::from_secs(3600),
            synced(),
            vec![
                (vec![], Box::new(|s: &LogStatus| s.stored_seq == Some(3))),
                (
                    vec![view_from(a, 2, 2), view_from(b, 2, 2)],
                    Box::new(|s: &LogStatus| s.view_agreed_seq == Some(2)),
                ),
            ],
            me,
            vec![me, a, b],
        )
        .await;
        assert!(s.recent_alarms.is_empty());
    }

    #[tokio::test]
    async fn a_disagreement_without_a_majority_sets_the_hold() {
        let (me, a, b) = (PeerId::random(), PeerId::random(), PeerId::random());
        let (s, _) = run_stages(
            0,
            Duration::from_secs(3600),
            synced(),
            vec![
                (vec![], Box::new(|s: &LogStatus| s.stored_seq == Some(3))),
                (
                    vec![view_from(a, 3, 7)],
                    Box::new(|s: &LogStatus| s.view_hold),
                ),
            ],
            me,
            vec![me, a, b],
        )
        .await;
        assert_eq!((s.halted, s.view_hold), (None, true));
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
