//! The follower: applies the replicated log strictly in seq order.
//!
//! One [`Follower::step`] tries the next seq (`head + 1`): fetch the record from member peers (bounded,
//! each peer once), check it against its announcement, hand it to the enclave, and act on the enclave's
//! answer:
//!
//! - applied → store the record durably, THEN announce it and publish this node's APPLIED share;
//! - BAD_SIG / BAD_BLOB / a refused record → try the next peer; when every peer's copy fails, raise an
//!   alarm for that seq once (BAD_SIG from every copy means a non-member or forged signature on the log).
//!   A step asks each peer at most once; the caller paces the steps — never a tight retry loop;
//! - STALE → fetch a quorum certificate (checked in the enclave) and retry with it; while none exists,
//!   wait, and alarm once the seq has been stale for `stale_alarm_after`;
//! - NOT_YET / GAP / enclave unavailable → wait and retry;
//! - DIVERGED / FENCED / a prologue failure → halt: nothing more is applied on this node.
//!
//! APPLIED shares from peers are verified in the enclave one by one before they are collected, and the
//! assembled certificate is verified at quorum before it is stored, so a stored certificate is one the
//! enclave accepts. The collection window is anchored to this node's OWN last stored seq.
#![allow(dead_code)]

use crate::log_client::{Applied, ApplyInput, Head, LogError, LogOutcome};
use crate::log_store::{decode_cert, decode_record, Cert, LogRecord, LogStore};
use crate::log_transport::{LogInbound, Outgoing};
use crate::log_wire::{
    parse_sig_entry, verify_fetched, Announce, CertCollector, FetchKind, FetchPlan, Parsed, Share,
    ShareOutcome,
};
use async_trait::async_trait;
use libp2p::PeerId;
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

/// What the follower needs from its enclave.
#[async_trait]
pub trait Enclave: Send + Sync {
    async fn head(&self) -> Result<Head, LogError>;
    /// This node's replicated view (seq, chain hash, state digest).
    async fn view(&self) -> Result<crate::log_client::View, LogError>;
    /// Apply one batch: this node's APPLIED share (empty when it signs none) and post-batch view.
    async fn apply(&self, input: &ApplyInput<'_>) -> Result<Applied, LogError>;
    async fn check_applied(
        &self,
        seq: u64,
        term: u64,
        chain_hash: &[u8; 32],
        view: &[u8; 32],
        bundle: &[u8],
        quorum: bool,
    ) -> Result<(), LogError>;
}

/// What the follower needs from the network.
#[async_trait]
pub trait Net: Send + Sync {
    async fn fetch(
        &self,
        kind: FetchKind,
        seq: u64,
        peer: PeerId,
    ) -> anyhow::Result<Option<Vec<u8>>>;
    async fn publish(&self, out: Outgoing);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Alarm {
    /// Every copy of `seq` fetched was refused BAD_SIG: not signed by a current member.
    NonMemberSig { seq: u64 },
    /// Every copy of `seq` fetched was unusable (malformed, wrong blob, wrong hash); `code` is the last.
    BadRecord { seq: u64, code: i64 },
    /// `seq` needs a quorum certificate and none has been found for `secs`.
    Stale { seq: u64, secs: u64 },
    /// This node can no longer follow the log.
    Halted { seq: u64, code: i64 },
    /// APPLIED shares for two different chain hashes at one seq.
    CertConflict { seq: u64 },
    /// APPLIED shares for one batch over two different post-batch views: members disagree on the state
    /// after the same batch (10d (a)). The view that reaches the quorum is certified; the other is not.
    CertViewConflict { seq: u64 },
    /// A peer's APPLIED share did not verify in the enclave.
    BadShare { seq: u64, author: PeerId },
    /// The record applied but could not be stored.
    StoreFailed { seq: u64 },
    /// The enclave head is at `seq` but the store holds only up to `stored` and cannot be repaired from
    /// peers: this node is stuck (it applies nothing wrong, but it applies nothing).
    StoreBehind { seq: u64, stored: Option<u64> },
    /// C1: a majority of members agree on the view at `seq` and THIS node does not.
    ViewMinority { seq: u64 },
    /// C1: a majority agree at `seq` and `peer` does not.
    ViewPeerOdd { seq: u64, peer: PeerId },
    /// C1: no view at `seq` can reach a majority.
    ViewSplit { seq: u64 },
    /// C1: `peer` reported two different views for `seq`.
    ViewEquivocation { seq: u64, peer: PeerId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Applied {
        seq: u64,
    },
    /// No member has the next record (yet).
    Idle,
    /// Retry the same seq later.
    Wait(&'static str),
    /// This node has stopped following (code of the refusal).
    Halted(i64),
    /// The enclave is not in log mode.
    Off,
}

#[derive(Debug, Clone)]
pub struct FollowerCfg {
    pub max_fetch_attempts: usize,
    pub stale_alarm_after: Duration,
    /// How far around this node's own position announcements and shares are kept.
    pub window: u64,
    /// Distinct APPLIED shares that make a certificate (the sealed quorum; 2 of 3).
    pub quorum_shares: usize,
}

impl Default for FollowerCfg {
    fn default() -> Self {
        Self {
            max_fetch_attempts: 4,
            stale_alarm_after: Duration::from_secs(60),
            window: 256,
            quorum_shares: 2,
        }
    }
}

pub struct Follower {
    cfg: FollowerCfg,
    members: Vec<PeerId>,
    store: LogStore,
    collector: CertCollector,
    announced: BTreeMap<u64, (Announce, PeerId)>,
    /// Quorum certificates verified in the enclave for seqs this node has not stored yet.
    pending_certs: BTreeMap<u64, Cert>,
    stale_since: Option<(u64, Instant)>,
    raised: HashSet<Alarm>,
    alarms: Vec<Alarm>,
    halted: Option<i64>,
}

impl Follower {
    /// `members`: the other member peers (this node excluded).
    pub fn new(cfg: FollowerCfg, members: Vec<PeerId>, store: LogStore) -> Self {
        let collector = CertCollector::new(cfg.quorum_shares, cfg.window);
        Self {
            cfg,
            members,
            store,
            collector,
            announced: BTreeMap::new(),
            pending_certs: BTreeMap::new(),
            stale_since: None,
            raised: HashSet::new(),
            alarms: Vec::new(),
            halted: None,
        }
    }

    /// This node's own position: the last seq it stored. Every window is anchored here, never to a
    /// seq a peer claimed.
    fn anchor(&self) -> u64 {
        self.store.last_seq().unwrap_or(0)
    }

    fn in_window(&self, seq: u64) -> bool {
        let a = self.anchor();
        seq > a.saturating_sub(self.cfg.window) && seq <= a.saturating_add(self.cfg.window)
    }

    /// Raise an alarm from outside the follower (the view compare); deduplicated like the others.
    pub fn alarm(&mut self, a: Alarm) {
        self.raise(a);
    }

    /// The view compare put this node in the minority at `seq`: its state is not the cluster's. Stop
    /// applying (sticky, as for an enclave DIVERGED) — the node re-syncs by operator action.
    pub fn halt_view_minority(&mut self, seq: u64) {
        self.raise(Alarm::ViewMinority { seq });
        self.halt(seq, crate::log_client::rc::DIVERGED);
    }

    fn raise(&mut self, a: Alarm) {
        if self.raised.insert(a.clone()) {
            tracing::error!(alarm = ?a, "replicated log alarm");
            self.alarms.push(a);
        }
    }

    /// Alarms raised since the last call.
    pub fn take_alarms(&mut self) -> Vec<Alarm> {
        std::mem::take(&mut self.alarms)
    }

    pub fn halted(&self) -> Option<i64> {
        self.halted
    }

    pub fn store(&self) -> &LogStore {
        &self.store
    }

    fn halt(&mut self, seq: u64, code: i64) -> Step {
        self.halted = Some(code);
        self.raise(Alarm::Halted { seq, code });
        Step::Halted(code)
    }

    fn prune(&mut self) {
        let floor = self.anchor().saturating_sub(self.cfg.window);
        self.announced.retain(|&q, _| q > floor);
        self.pending_certs.retain(|&q, _| q > floor);
        self.raised.retain(|a| match a {
            Alarm::NonMemberSig { seq }
            | Alarm::BadRecord { seq, .. }
            | Alarm::Stale { seq, .. }
            | Alarm::CertConflict { seq }
            | Alarm::CertViewConflict { seq }
            | Alarm::BadShare { seq, .. }
            | Alarm::StoreFailed { seq }
            | Alarm::StoreBehind { seq, .. }
            | Alarm::ViewPeerOdd { seq, .. }
            | Alarm::ViewSplit { seq }
            | Alarm::ViewEquivocation { seq, .. } => *seq > floor,
            Alarm::ViewMinority { .. } => true,
            Alarm::Halted { .. } => true,
        });
    }

    /// A message from a member peer (already gated by the transport).
    pub async fn on_inbound(&mut self, enclave: &dyn Enclave, m: LogInbound) {
        match m.msg {
            Parsed::Announce(a) => {
                if self.in_window(a.seq) && a.seq > self.anchor() {
                    self.announced.insert(a.seq, (a, m.author));
                }
            }
            // View reports are compared by the loop (log_view), not here.
            Parsed::View(_) => {}
            Parsed::Applied(s) => {
                if !self.in_window(s.seq) {
                    return;
                }
                let one = crate::quorum_bundle::build(&[(s.pubkey.to_vec(), s.der.clone())]);
                if enclave
                    .check_applied(s.seq, s.term, &s.chain_hash, &s.view, &one, false)
                    .await
                    .is_err()
                {
                    self.raise(Alarm::BadShare {
                        seq: s.seq,
                        author: m.author,
                    });
                    return;
                }
                self.collect(enclave, s).await;
            }
        }
    }

    /// Add a verified share; store the certificate once it verifies at quorum.
    async fn collect(&mut self, enclave: &dyn Enclave, s: Share) {
        let (seq, term, chain, view) = (s.seq, s.term, s.chain_hash, s.view);
        match self.collector.add(s, self.anchor()) {
            ShareOutcome::Ready(bundle) => {
                if enclave
                    .check_applied(seq, term, &chain, &view, &bundle, true)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        seq,
                        "assembled APPLIED certificate did not verify at quorum"
                    );
                    return;
                }
                let cert = Cert { view, bundle };
                if self.store.last_seq().is_some_and(|l| seq <= l)
                    && self.store.first_seq().is_some_and(|f| seq >= f)
                {
                    if let Err(e) = self.store.put_cert(seq, &cert) {
                        tracing::error!(seq, "storing a certificate: {e:#}");
                    }
                } else {
                    self.pending_certs.insert(seq, cert);
                }
            }
            ShareOutcome::Conflict { seq } => self.raise(Alarm::CertConflict { seq }),
            ShareOutcome::ViewConflict { seq } => self.raise(Alarm::CertViewConflict { seq }),
            ShareOutcome::Pending | ShareOutcome::Duplicate | ShareOutcome::OutOfWindow => {}
        }
    }

    fn announcer(&self, seq: u64) -> Option<PeerId> {
        self.announced.get(&seq).map(|(_, p)| *p)
    }

    /// Decode a fetched record: against its announcement when there is one, else on its own.
    fn check_record(&self, seq: u64, body: &[u8]) -> anyhow::Result<LogRecord> {
        let r = match self.announced.get(&seq) {
            Some((a, _)) => verify_fetched(a, body)?,
            None => decode_record(body)?,
        };
        anyhow::ensure!(
            r.seq == seq,
            "fetched record is seq {}, asked for {seq}",
            r.seq
        );
        Ok(r)
    }

    /// A quorum certificate for `seq` (signing `chain` and the view it carries), from what this node holds
    /// or from a peer — only one the enclave verifies at quorum.
    async fn find_cert(
        &mut self,
        enclave: &dyn Enclave,
        net: &dyn Net,
        seq: u64,
        term: u64,
        chain: &[u8; 32],
    ) -> Option<Cert> {
        if let Some(c) = self.pending_certs.get(&seq) {
            return Some(c.clone());
        }
        let mut plan = FetchPlan::new(
            self.announcer(seq),
            &self.members,
            self.cfg.max_fetch_attempts,
        );
        while let Some(p) = plan.next_peer() {
            let Ok(Some(file)) = net.fetch(FetchKind::Cert, seq, p).await else {
                continue;
            };
            let Ok(cert) = decode_cert(&file, seq) else {
                continue;
            };
            if enclave
                .check_applied(seq, term, chain, &cert.view, &cert.bundle, true)
                .await
                .is_ok()
            {
                return Some(cert);
            }
        }
        None
    }

    /// The head moved past the store (a crash between apply and store): fetch the head's record from a
    /// peer and store it, so the store stays contiguous and this node can serve it.
    async fn repair_tail(&mut self, net: &dyn Net, head: &Head) -> bool {
        let mut plan = FetchPlan::new(None, &self.members, self.cfg.max_fetch_attempts);
        while let Some(p) = plan.next_peer() {
            let Ok(Some(body)) = net.fetch(FetchKind::Record, head.seq, p).await else {
                continue;
            };
            let Ok(r) = decode_record(&body) else {
                continue;
            };
            if r.seq == head.seq && r.chain_hash == head.chain_hash && self.store.put(&r).is_ok() {
                return true;
            }
        }
        false
    }

    async fn finish_applied(
        &mut self,
        enclave: &dyn Enclave,
        net: &dyn Net,
        rec: LogRecord,
        out: Applied,
    ) -> Step {
        let seq = rec.seq;
        // Durable FIRST: only the proof `put` returns can be announced or signed for.
        let durable = match self.store.put(&rec) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(seq, "applied but not stored: {e:#}");
                self.raise(Alarm::StoreFailed { seq });
                return Step::Wait("store failed");
            }
        };
        net.publish(Outgoing::announce(&durable)).await;
        if let (false, Some(view)) = (out.share.is_empty(), out.view) {
            match (
                Outgoing::applied(&durable, &view, &out.share),
                parse_sig_entry(&out.share),
                durable.term(),
            ) {
                (Ok(msg), Ok((pubkey, der)), Some(term)) => {
                    net.publish(msg).await;
                    self.collect(
                        enclave,
                        Share {
                            seq,
                            term,
                            chain_hash: rec.chain_hash,
                            view,
                            pubkey,
                            der,
                        },
                    )
                    .await;
                }
                _ => tracing::error!(
                    seq,
                    "the enclave's APPLIED share is malformed; not published"
                ),
            }
        }
        if let Some(cert) = self.pending_certs.remove(&seq) {
            if let Err(e) = self.store.put_cert(seq, &cert) {
                tracing::error!(seq, "storing a certificate: {e:#}");
            }
        }
        if self.stale_since.is_some_and(|(q, _)| q == seq) {
            self.stale_since = None;
        }
        self.announced.remove(&seq);
        self.prune();
        Step::Applied { seq }
    }

    /// Try to apply the next seq once.
    pub async fn step(&mut self, enclave: &dyn Enclave, net: &dyn Net, now: Instant) -> Step {
        if let Some(code) = self.halted {
            return Step::Halted(code);
        }
        let head = match enclave.head().await {
            Ok(h) => h,
            Err(_) => return Step::Wait("enclave unavailable"),
        };
        if !head.on {
            return Step::Off;
        }
        if head.fenced {
            return self.halt(head.seq, crate::log_client::rc::FENCED);
        }
        let stored = self.store.last_seq();
        if head.seq > 0
            && stored.is_none_or(|l| l < head.seq)
            && !self.repair_tail(net, &head).await
        {
            self.raise(Alarm::StoreBehind {
                seq: head.seq,
                stored,
            });
            return Step::Wait("the store is behind the enclave head");
        }
        let Some(n) = head.seq.checked_add(1) else {
            return Step::Wait("seq exhausted");
        };

        let mut plan = FetchPlan::new(
            self.announcer(n),
            &self.members,
            self.cfg.max_fetch_attempts,
        );
        let mut any_copy = false;
        let mut only_bad_sig = true;
        let mut last_code = 0i64;
        let mut cert: Option<Cert> = self.pending_certs.get(&n).cloned();
        while let Some(p) = plan.next_peer() {
            let Ok(Some(body)) = net.fetch(FetchKind::Record, n, p).await else {
                continue;
            };
            any_copy = true;
            let rec = match self.check_record(n, &body) {
                Ok(r) => r,
                Err(_) => {
                    only_bad_sig = false;
                    last_code = crate::log_client::rc::MALFORMED;
                    continue;
                }
            };
            let mut outcome = self.apply(enclave, &rec, cert.as_ref()).await;
            if matches!(outcome, Err(LogError::Enclave(LogOutcome::Transient(c))) if c == crate::log_client::rc::STALE)
                && cert.is_none()
            {
                // A certificate is over seq ‖ term ‖ chain hash ‖ view; the term is the batch header's.
                if let Some(term) = crate::log_store::batch_term(&rec.batch) {
                    cert = self.find_cert(enclave, net, n, term, &rec.chain_hash).await;
                }
                if cert.is_some() {
                    outcome = self.apply(enclave, &rec, cert.as_ref()).await;
                }
            }
            match outcome {
                Ok(applied) => return self.finish_applied(enclave, net, rec, applied).await,
                Err(LogError::Enclave(o)) => match o {
                    LogOutcome::Ok | LogOutcome::AlreadyApplied => {
                        return Step::Wait("the head moved")
                    }
                    LogOutcome::Transient(c) if c == crate::log_client::rc::BAD_SIG => {
                        last_code = c;
                        continue;
                    }
                    LogOutcome::Transient(c) if c == crate::log_client::rc::BAD_BLOB => {
                        only_bad_sig = false;
                        last_code = c;
                        continue;
                    }
                    LogOutcome::Transient(c) if c == crate::log_client::rc::STALE => {
                        let since = match self.stale_since {
                            Some((q, t)) if q == n => t,
                            _ => {
                                self.stale_since = Some((n, now));
                                now
                            }
                        };
                        let waited = now.saturating_duration_since(since);
                        if waited >= self.cfg.stale_alarm_after {
                            self.raise(Alarm::Stale {
                                seq: n,
                                secs: self.cfg.stale_alarm_after.as_secs(),
                            });
                        }
                        return Step::Wait("stale: no quorum certificate yet");
                    }
                    LogOutcome::Transient(_) => return Step::Wait("transient"),
                    LogOutcome::Halted(c) => return self.halt(n, c),
                    LogOutcome::Off => return Step::Off,
                    LogOutcome::NotRun => return Step::Wait("enclave unavailable"),
                    LogOutcome::Refused(c) => {
                        only_bad_sig = false;
                        last_code = c;
                        continue;
                    }
                },
                Err(_) => return Step::Wait("enclave unavailable"),
            }
        }
        if !any_copy {
            return Step::Idle;
        }
        if only_bad_sig {
            self.raise(Alarm::NonMemberSig { seq: n });
        } else {
            self.raise(Alarm::BadRecord {
                seq: n,
                code: last_code,
            });
        }
        Step::Wait("no usable copy of the next record")
    }

    async fn apply(
        &self,
        enclave: &dyn Enclave,
        rec: &LogRecord,
        cert: Option<&Cert>,
    ) -> Result<Applied, LogError> {
        enclave
            .apply(&ApplyInput {
                batch: &rec.batch,
                quote_blob: &rec.quote_blob,
                market_id: rec.market_id,
                side_blobs: &rec.side_blobs,
                sig_bundle: &rec.sig_bundle,
                cert_bundle: cert.map_or(&[][..], |c| &c.bundle),
                cert_view: cert.map(|c| &c.view),
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_client::rc;
    use crate::log_store::encode_record;
    use crate::log_wire::LogGossip;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// A batch-shaped body: the enclave's header length (108), a seq marker, the term 7 at byte 100.
    const TERM: u64 = 7;
    fn batch(seq: u64) -> Vec<u8> {
        let mut b = vec![0u8; 108];
        b[0] = 1;
        b[1] = 2;
        b[2] = seq as u8;
        b[100..108].copy_from_slice(&TERM.to_be_bytes());
        b
    }

    fn rec(seq: u64) -> LogRecord {
        LogRecord {
            seq,
            chain_hash: [seq as u8; 32],
            market_id: 0,
            batch: batch(seq),
            quote_blob: vec![9; 40],
            side_blobs: vec![],
            sig_bundle: vec![7; 72],
        }
    }

    fn file(seq: u64) -> Vec<u8> {
        encode_record(&rec(seq)).unwrap()
    }

    fn share_entry(k: u8) -> Vec<u8> {
        let mut v = vec![0x02];
        v.extend([k; 32]);
        v.push(8);
        v.extend([0x30; 8]);
        v
    }

    /// The post-batch view every fake enclave reports (10d (a)).
    const VIEW: [u8; 32] = [0x5e; 32];

    fn share(seq: u64, k: u8) -> Share {
        let (pubkey, der) = parse_sig_entry(&share_entry(k)).unwrap();
        Share {
            seq,
            term: TERM,
            chain_hash: [seq as u8; 32],
            view: VIEW,
            pubkey,
            der,
        }
    }

    type ApplyRule = Box<dyn Fn(u64, &ApplyInput<'_>) -> Result<Vec<u8>, i64> + Send + Sync>;

    /// The enclave: applies record `head+1` per `rule`, advancing its head on success.
    struct FakeEnclave {
        head: Mutex<Head>,
        rule: ApplyRule,
        /// Bundles `check_applied` accepts (any `quorum`).
        good: Mutex<HashSet<Vec<u8>>>,
        applies: Mutex<Vec<(u64, Vec<u8>)>>,
        /// The certificate view handed to each apply (10d (a)).
        cert_views: Mutex<Vec<Option<[u8; 32]>>>,
        /// What apply reports as this node's view (`None`: the enclave could not compute it).
        own_view: Mutex<Option<[u8; 32]>>,
        checks: Mutex<usize>,
        /// The term of every check_applied call (D11.2: it must be the batch header's).
        terms: Mutex<Vec<u64>>,
        /// The view of every check_applied call (10d (a): the share's or the certificate's).
        views: Mutex<Vec<[u8; 32]>>,
    }

    impl FakeEnclave {
        fn new(rule: ApplyRule) -> Self {
            Self {
                head: Mutex::new(Head {
                    seq: 0,
                    t_batch: 0,
                    chain_hash: [0; 32],
                    term: 0,
                    fenced: false,
                    on: true,
                }),
                rule,
                good: Mutex::new(HashSet::new()),
                applies: Mutex::new(vec![]),
                cert_views: Mutex::new(vec![]),
                own_view: Mutex::new(Some(VIEW)),
                checks: Mutex::new(0),
                terms: Mutex::new(vec![]),
                views: Mutex::new(vec![]),
            }
        }
        fn ok() -> Self {
            Self::new(Box::new(|_, _| Ok(share_entry(0xaa))))
        }
    }

    #[async_trait]
    impl Enclave for FakeEnclave {
        async fn head(&self) -> Result<Head, LogError> {
            Ok(self.head.lock().unwrap().clone())
        }
        async fn view(&self) -> Result<crate::log_client::View, LogError> {
            let h = self.head.lock().unwrap().clone();
            Ok(crate::log_client::View {
                seq: h.seq,
                chain_hash: h.chain_hash,
                digest: [0; 32],
            })
        }
        async fn apply(&self, i: &ApplyInput<'_>) -> Result<Applied, LogError> {
            let next = self.head.lock().unwrap().seq + 1;
            self.applies
                .lock()
                .unwrap()
                .push((next, i.cert_bundle.to_vec()));
            self.cert_views.lock().unwrap().push(i.cert_view.copied());
            match (self.rule)(next, i) {
                Ok(s) => {
                    let mut h = self.head.lock().unwrap();
                    h.seq = next;
                    h.chain_hash = [next as u8; 32];
                    Ok(Applied {
                        share: s,
                        view: *self.own_view.lock().unwrap(),
                    })
                }
                Err(c) => Err(LogError::Enclave(LogOutcome::from_rc(c))),
            }
        }
        async fn check_applied(
            &self,
            _: u64,
            term: u64,
            _: &[u8; 32],
            view: &[u8; 32],
            b: &[u8],
            _: bool,
        ) -> Result<(), LogError> {
            *self.checks.lock().unwrap() += 1;
            self.terms.lock().unwrap().push(term);
            self.views.lock().unwrap().push(*view);
            if self.good.lock().unwrap().contains(b) {
                Ok(())
            } else {
                Err(LogError::Enclave(LogOutcome::from_rc(rc::BAD_SIG)))
            }
        }
    }

    /// The network: what each peer serves, every fetch made, and every publish (with whether the record
    /// was already on disk at that moment).
    struct FakeNet {
        serves: HashMap<(FetchKind, u64, PeerId), Vec<u8>>,
        fetches: Mutex<Vec<(FetchKind, u64, PeerId)>>,
        published: Mutex<Vec<(LogGossip, bool)>>,
        dir: PathBuf,
    }

    impl FakeNet {
        fn new(dir: &std::path::Path) -> Self {
            Self {
                serves: HashMap::new(),
                fetches: Mutex::new(vec![]),
                published: Mutex::new(vec![]),
                dir: dir.into(),
            }
        }
        fn serve(mut self, kind: FetchKind, seq: u64, p: PeerId, body: Vec<u8>) -> Self {
            self.serves.insert((kind, seq, p), body);
            self
        }
        fn record_fetches(&self, seq: u64) -> Vec<PeerId> {
            self.fetches
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f.0 == FetchKind::Record && f.1 == seq)
                .map(|f| f.2)
                .collect()
        }
    }

    #[async_trait]
    impl Net for FakeNet {
        async fn fetch(
            &self,
            kind: FetchKind,
            seq: u64,
            peer: PeerId,
        ) -> anyhow::Result<Option<Vec<u8>>> {
            self.fetches.lock().unwrap().push((kind, seq, peer));
            Ok(self.serves.get(&(kind, seq, peer)).cloned())
        }
        async fn publish(&self, out: Outgoing) {
            let seq = match out.gossip() {
                LogGossip::Announce { seq, .. }
                | LogGossip::Applied { seq, .. }
                | LogGossip::View { seq, .. } => *seq,
            };
            let on_disk = self.dir.join(format!("{seq:020}.rec")).exists();
            self.published
                .lock()
                .unwrap()
                .push((out.gossip().clone(), on_disk));
        }
    }

    struct Setup {
        _d: tempfile::TempDir,
        f: Follower,
        peers: Vec<PeerId>,
    }

    fn setup(cfg: FollowerCfg) -> Setup {
        let d = tempfile::tempdir().unwrap();
        let peers = vec![PeerId::random(), PeerId::random()];
        let f = Follower::new(cfg, peers.clone(), LogStore::open(d.path()).unwrap());
        Setup { _d: d, f, peers }
    }

    fn dir(s: &Setup) -> &std::path::Path {
        s._d.path()
    }

    fn announce(seq: u64) -> Parsed {
        let body = file(seq);
        Parsed::Announce(Announce {
            seq,
            chain_hash: [seq as u8; 32],
            record_len: body.len(),
            record_sha256: Sha256::digest(&body).into(),
        })
    }

    #[tokio::test]
    async fn applies_in_order_and_stores_before_it_announces_or_signs() {
        let mut s = setup(FollowerCfg::default());
        let (a, b) = (s.peers[0], s.peers[1]);
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Record, 2, b, file(2));
        let e = FakeEnclave::ok();
        let now = Instant::now();
        assert_eq!(s.f.step(&e, &net, now).await, Step::Applied { seq: 1 });
        assert_eq!(s.f.step(&e, &net, now).await, Step::Applied { seq: 2 });
        assert_eq!(s.f.step(&e, &net, now).await, Step::Idle);
        assert_eq!(s.f.store().last_seq(), Some(2));
        assert_eq!(s.f.store().get(2).unwrap().unwrap(), rec(2));
        let p = net.published.lock().unwrap();
        // Per seq: an announcement, then this node's APPLIED share; each only once the record was on disk.
        assert_eq!(p.len(), 4);
        assert!(matches!(p[0].0, LogGossip::Announce { seq: 1, .. }));
        assert!(matches!(
            &p[1].0,
            LogGossip::Applied {
                seq: 1,
                term: TERM,
                view,
                ..
            } if *view == hex::encode(VIEW)
        ));
        assert!(
            p.iter().all(|(_, on_disk)| *on_disk),
            "published before the record was durable"
        );
        assert!(s.f.take_alarms().is_empty());
    }

    #[tokio::test]
    async fn without_its_own_view_a_node_publishes_no_applied_share() {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::ok();
        *e.own_view.lock().unwrap() = None;
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Applied { seq: 1 }
        );
        let p = net.published.lock().unwrap();
        assert!(matches!(p[0].0, LogGossip::Announce { seq: 1, .. }));
        assert!(
            !p.iter()
                .any(|(g, _)| matches!(g, LogGossip::Applied { .. })),
            "a share is over a view: no view, no share"
        );
    }

    #[tokio::test]
    async fn shares_over_another_view_raise_an_alarm_and_the_agreeing_quorum_still_certifies() {
        let (mut s, e) = applied_one(true).await;
        let (a, b) = (s.peers[0], s.peers[1]);
        let mut odd = share(1, 0xa1);
        odd.view = [0xbd; 32];
        // the odd share verifies on its own (its member did sign that view), so it reaches the collector
        e.good.lock().unwrap().insert(bundle(&[0xa1], 1));
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(odd),
            },
        )
        .await;
        s.f.on_inbound(
            &e,
            LogInbound {
                author: b,
                msg: Parsed::Applied(share(1, 0xb1)),
            },
        )
        .await;
        assert_eq!(s.f.take_alarms(), vec![Alarm::CertViewConflict { seq: 1 }]);
        assert_eq!(
            s.f.store().get_cert(1).unwrap(),
            None::<Cert>,
            "two members over two views are no quorum"
        );
    }

    #[tokio::test]
    async fn a_record_that_does_not_match_its_announcement_is_not_applied() {
        let mut s = setup(FollowerCfg::default());
        let a = s.peers[0];
        let mut forged = rec(1);
        forged.batch = vec![6, 6, 6];
        let net =
            FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, a, encode_record(&forged).unwrap());
        let e = FakeEnclave::ok();
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: announce(1),
            },
        )
        .await;
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Wait("no usable copy of the next record")
        );
        assert!(
            e.applies.lock().unwrap().is_empty(),
            "the enclave saw a record its announcement does not match"
        );
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::BadRecord {
                seq: 1,
                code: rc::MALFORMED
            }]
        );
    }

    #[tokio::test]
    async fn bad_sig_from_every_peer_is_bounded_and_raises_the_non_member_alarm_once() {
        let mut s = setup(FollowerCfg::default());
        let (a, b) = (s.peers[0], s.peers[1]);
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Record, 1, b, file(1));
        let e = FakeEnclave::new(Box::new(|_, _| Err(rc::BAD_SIG)));
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(
                s.f.step(&e, &net, now).await,
                Step::Wait("no usable copy of the next record")
            );
        }
        // Each step asks each peer once — never the same peer twice in one step.
        assert_eq!(net.record_fetches(1).len(), 6);
        assert_eq!(s.f.take_alarms(), vec![Alarm::NonMemberSig { seq: 1 }]);
        assert_eq!(s.f.store().last_seq(), None);
    }

    #[tokio::test]
    async fn bad_sig_from_one_peer_falls_through_to_a_good_copy() {
        let mut s = setup(FollowerCfg::default());
        let (a, b) = (s.peers[0], s.peers[1]);
        let mut wrong = rec(1);
        wrong.sig_bundle = vec![1; 72];
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, encode_record(&wrong).unwrap())
            .serve(FetchKind::Record, 1, b, file(1));
        let e = FakeEnclave::new(Box::new(|_, i| {
            if i.sig_bundle == [7; 72] {
                Ok(vec![])
            } else {
                Err(rc::BAD_SIG)
            }
        }));
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Applied { seq: 1 }
        );
        assert!(s.f.take_alarms().is_empty());
        // No share signed (empty) → only the announcement goes out.
        assert_eq!(net.published.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_mix_of_failures_is_a_bad_record_alarm_not_a_forgery_alarm() {
        let mut s = setup(FollowerCfg::default());
        let (a, b) = (s.peers[0], s.peers[1]);
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Record, 1, b, file(1));
        let calls = Mutex::new(0);
        let e = FakeEnclave::new(Box::new(move |_, _| {
            let mut c = calls.lock().unwrap();
            *c += 1;
            Err(if *c == 1 { rc::BAD_SIG } else { rc::BAD_BLOB })
        }));
        s.f.step(&e, &net, Instant::now()).await;
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::BadRecord {
                seq: 1,
                code: rc::BAD_BLOB
            }]
        );
    }

    #[tokio::test]
    async fn stale_without_a_certificate_waits_and_alarms_only_after_the_bound() {
        let cfg = FollowerCfg {
            stale_alarm_after: Duration::from_secs(30),
            ..FollowerCfg::default()
        };
        let mut s = setup(cfg);
        let a = s.peers[0];
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, a, file(1));
        let e = FakeEnclave::new(Box::new(|_, i| {
            if i.cert_bundle.is_empty() {
                Err(rc::STALE)
            } else {
                Ok(vec![])
            }
        }));
        let t0 = Instant::now();
        assert_eq!(
            s.f.step(&e, &net, t0).await,
            Step::Wait("stale: no quorum certificate yet")
        );
        assert_eq!(
            s.f.step(&e, &net, t0 + Duration::from_secs(29)).await,
            Step::Wait("stale: no quorum certificate yet")
        );
        assert!(s.f.take_alarms().is_empty());
        s.f.step(&e, &net, t0 + Duration::from_secs(30)).await;
        assert_eq!(s.f.take_alarms(), vec![Alarm::Stale { seq: 1, secs: 30 }]);
        // Transient: the node keeps following (and so keeps serving reads), however long it waits.
        assert_eq!(s.f.halted(), None);
    }

    #[tokio::test]
    async fn stale_catches_up_with_a_peer_certificate_the_enclave_verifies() {
        let mut s = setup(FollowerCfg::default());
        let (a, b) = (s.peers[0], s.peers[1]);
        let cert = vec![0xc1; 40];
        let bogus = vec![0xc2; 40];
        // The certificate's view: what the quorum signed, not this node's own (VIEW).
        const CV: [u8; 32] = [0xc7; 32];
        let cert_file = |c: &[u8]| {
            let d = tempfile::tempdir().unwrap();
            let mut st = LogStore::open(d.path()).unwrap();
            st.put(&rec(1)).unwrap();
            st.put_cert(
                1,
                &Cert {
                    view: CV,
                    bundle: c.to_vec(),
                },
            )
            .unwrap();
            std::fs::read(d.path().join(format!("{:020}.cert", 1))).unwrap()
        };
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Cert, 1, a, cert_file(&bogus))
            .serve(FetchKind::Cert, 1, b, cert_file(&cert));
        let e = FakeEnclave::new(Box::new(|_, i| {
            if i.cert_bundle.is_empty() {
                Err(rc::STALE)
            } else {
                Ok(vec![])
            }
        }));
        e.good.lock().unwrap().insert(cert.clone());
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Applied { seq: 1 }
        );
        assert!(
            !e.terms.lock().unwrap().is_empty()
                && e.terms.lock().unwrap().iter().all(|t| *t == TERM),
            "the certificate is checked under the batch header's term"
        );
        assert!(
            e.views.lock().unwrap().iter().all(|v| *v == CV),
            "a fetched certificate is checked over the view it carries"
        );
        assert_eq!(
            e.cert_views.lock().unwrap().last().copied().flatten(),
            Some(CV),
            "the catch-up apply gets the certificate's view beside its bundle"
        );
        // Applied with the certificate that verified, never with the one that did not.
        let applies = e.applies.lock().unwrap();
        assert_eq!(applies.last().unwrap().1, cert);
        assert!(applies.iter().all(|(_, c)| c != &bogus));
    }

    #[tokio::test]
    async fn diverged_halts_and_nothing_more_is_applied() {
        let mut s = setup(FollowerCfg::default());
        let a = s.peers[0];
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Record, 2, a, file(2));
        let e = FakeEnclave::new(Box::new(|_, _| Err(rc::DIVERGED)));
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Halted(rc::DIVERGED)
        );
        assert_eq!(s.f.halted(), Some(rc::DIVERGED));
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Halted(rc::DIVERGED)
        );
        assert_eq!(e.applies.lock().unwrap().len(), 1);
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::Halted {
                seq: 1,
                code: rc::DIVERGED
            }]
        );
    }

    #[tokio::test]
    async fn a_fenced_head_halts_before_any_fetch() {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::ok();
        e.head.lock().unwrap().fenced = true;
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Halted(rc::FENCED)
        );
        assert!(net.fetches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn not_in_log_mode_is_off_and_fetches_nothing() {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::ok();
        e.head.lock().unwrap().on = false;
        assert_eq!(s.f.step(&e, &net, Instant::now()).await, Step::Off);
        assert!(net.fetches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_transient_refusal_waits_without_an_alarm() {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::new(Box::new(|_, _| Err(rc::NOT_YET)));
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Wait("transient")
        );
        assert!(s.f.take_alarms().is_empty());
    }

    #[tokio::test]
    async fn the_store_behind_the_head_is_repaired_from_a_peer_first() {
        let mut s = setup(FollowerCfg::default());
        let a = s.peers[0];
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 1, a, file(1))
            .serve(FetchKind::Record, 2, a, file(2));
        let e = FakeEnclave::ok();
        // Crashed after the enclave applied 1 and before the store wrote it.
        *e.head.lock().unwrap() = Head {
            seq: 1,
            t_batch: 0,
            chain_hash: [1; 32],
            term: 0,
            fenced: false,
            on: true,
        };
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Applied { seq: 2 }
        );
        assert_eq!(
            (s.f.store().first_seq(), s.f.store().last_seq()),
            (Some(1), Some(2))
        );
    }

    #[tokio::test]
    async fn a_repair_copy_with_another_chain_hash_is_not_stored() {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::ok();
        *e.head.lock().unwrap() = Head {
            seq: 1,
            t_batch: 0,
            chain_hash: [9; 32],
            term: 0,
            fenced: false,
            on: true,
        };
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Wait("the store is behind the enclave head")
        );
        assert_eq!(s.f.store().last_seq(), None);
        s.f.step(&e, &net, Instant::now()).await;
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::StoreBehind {
                seq: 1,
                stored: None
            }]
        );
    }

    fn bundle(ks: &[u8], seq: u64) -> Vec<u8> {
        let e: Vec<_> = ks
            .iter()
            .map(|&k| (share(seq, k).pubkey.to_vec(), share(seq, k).der))
            .collect();
        crate::quorum_bundle::build(&e)
    }

    async fn applied_one(cert_verifies: bool) -> (Setup, FakeEnclave) {
        let mut s = setup(FollowerCfg::default());
        let net = FakeNet::new(dir(&s)).serve(FetchKind::Record, 1, s.peers[0], file(1));
        let e = FakeEnclave::new(Box::new(|_, _| Ok(vec![])));
        e.good
            .lock()
            .unwrap()
            .extend([bundle(&[0xa1], 1), bundle(&[0xb1], 1)]);
        if cert_verifies {
            e.good
                .lock()
                .unwrap()
                .extend([bundle(&[0xa1, 0xb1], 1), bundle(&[0xb1, 0xa1], 1)]);
        }
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Applied { seq: 1 }
        );
        (s, e)
    }

    #[tokio::test]
    async fn a_share_the_enclave_rejects_is_an_alarm_and_is_not_counted() {
        let (mut s, e) = applied_one(true).await;
        let (a, b) = (s.peers[0], s.peers[1]);
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(1, 0xee)),
            },
        )
        .await;
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::BadShare { seq: 1, author: a }]
        );
        s.f.on_inbound(
            &e,
            LogInbound {
                author: b,
                msg: Parsed::Applied(share(1, 0xb1)),
            },
        )
        .await;
        // Had the rejected share counted, these two would have made a certificate.
        assert_eq!(s.f.store().get_cert(1).unwrap(), None::<Cert>);
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(1, 0xa1)),
            },
        )
        .await;
        assert_eq!(
            s.f.store().get_cert(1).unwrap().map(|c| c.bundle),
            Some(bundle(&[0xb1, 0xa1], 1))
        );
    }

    #[tokio::test]
    async fn a_certificate_is_stored_only_once_it_verifies_at_quorum() {
        let (mut s, e) = applied_one(true).await;
        let (a, b) = (s.peers[0], s.peers[1]);
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(1, 0xa1)),
            },
        )
        .await;
        s.f.on_inbound(
            &e,
            LogInbound {
                author: b,
                msg: Parsed::Applied(share(1, 0xb1)),
            },
        )
        .await;
        assert_eq!(
            s.f.store().get_cert(1).unwrap().map(|c| c.bundle),
            Some(bundle(&[0xa1, 0xb1], 1))
        );
        assert!(
            e.terms.lock().unwrap().iter().all(|t| *t == TERM),
            "each peer share and the assembled certificate are checked under the share's term"
        );
        assert!(
            e.views.lock().unwrap().iter().all(|v| *v == VIEW),
            "… and over the view the shares carry"
        );
        assert_eq!(
            s.f.store().get_cert(1).unwrap().map(|c| c.view),
            Some(VIEW),
            "the stored certificate keeps the view it certifies"
        );

        let (mut s, e) = applied_one(false).await;
        let (a, b) = (s.peers[0], s.peers[1]);
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(1, 0xa1)),
            },
        )
        .await;
        s.f.on_inbound(
            &e,
            LogInbound {
                author: b,
                msg: Parsed::Applied(share(1, 0xb1)),
            },
        )
        .await;
        assert_eq!(s.f.store().get_cert(1).unwrap(), None::<Cert>);
    }

    #[tokio::test]
    async fn shares_and_announcements_far_from_this_nodes_own_seq_are_ignored_unchecked() {
        let cfg = FollowerCfg {
            window: 8,
            ..FollowerCfg::default()
        };
        let mut s = setup(cfg);
        let a = s.peers[0];
        let e = FakeEnclave::ok();
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(9, 0xa1)),
            },
        )
        .await;
        assert_eq!(
            *e.checks.lock().unwrap(),
            0,
            "a share outside the window reached the enclave"
        );
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(8, 0xa1)),
            },
        )
        .await;
        assert_eq!(
            *e.checks.lock().unwrap(),
            1,
            "a share inside the window was not checked"
        );
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: announce(9),
            },
        )
        .await;
        assert!(s.f.announced.is_empty());
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: announce(8),
            },
        )
        .await;
        assert_eq!(s.f.announced.len(), 1);
        // The window stays anchored to this node's own stored seq, not to what peers announced: a share
        // just past it is still dropped unchecked after a far announcement was accepted.
        s.f.on_inbound(
            &e,
            LogInbound {
                author: a,
                msg: Parsed::Applied(share(9, 0xa1)),
            },
        )
        .await;
        assert_eq!(
            *e.checks.lock().unwrap(),
            1,
            "the window followed a peer's announcement"
        );
    }

    #[tokio::test]
    async fn prune_forgets_what_fell_behind_the_window_but_keeps_a_halt() {
        let cfg = FollowerCfg {
            window: 2,
            ..FollowerCfg::default()
        };
        let mut s = setup(cfg);
        for q in 1..=5 {
            s.f.store.put(&rec(q)).unwrap();
        }
        let p = s.peers[0];
        for q in [3, 4] {
            if let Parsed::Announce(a) = announce(q) {
                s.f.announced.insert(q, (a, p));
            }
            s.f.pending_certs.insert(
                q,
                Cert {
                    view: VIEW,
                    bundle: vec![q as u8],
                },
            );
            s.f.raise(Alarm::NonMemberSig { seq: q });
        }
        s.f.raise(Alarm::Halted {
            seq: 1,
            code: rc::DIVERGED,
        });
        s.f.prune();
        // Own seq 5, window 2: seq 3 is behind (≤ 5 − 2), seq 4 is kept.
        assert_eq!(s.f.announced.keys().copied().collect::<Vec<_>>(), vec![4]);
        assert_eq!(
            s.f.pending_certs.keys().copied().collect::<Vec<_>>(),
            vec![4]
        );
        assert!(!s.f.raised.contains(&Alarm::NonMemberSig { seq: 3 }));
        assert!(s.f.raised.contains(&Alarm::NonMemberSig { seq: 4 }));
        assert!(s.f.raised.contains(&Alarm::Halted {
            seq: 1,
            code: rc::DIVERGED
        }));
    }

    #[tokio::test]
    async fn a_store_more_than_one_behind_cannot_be_repaired_and_alarms() {
        let mut s = setup(FollowerCfg::default());
        let a = s.peers[0];
        s.f.store.put(&rec(1)).unwrap();
        // Every peer has every record, yet a store at 1 cannot take 3 (it is append-only and contiguous).
        let net = FakeNet::new(dir(&s))
            .serve(FetchKind::Record, 2, a, file(2))
            .serve(FetchKind::Record, 3, a, file(3))
            .serve(FetchKind::Record, 4, a, file(4));
        let e = FakeEnclave::ok();
        *e.head.lock().unwrap() = Head {
            seq: 3,
            t_batch: 0,
            chain_hash: [3; 32],
            term: 0,
            fenced: false,
            on: true,
        };
        assert_eq!(
            s.f.step(&e, &net, Instant::now()).await,
            Step::Wait("the store is behind the enclave head")
        );
        assert!(e.applies.lock().unwrap().is_empty());
        assert_eq!(
            s.f.take_alarms(),
            vec![Alarm::StoreBehind {
                seq: 3,
                stored: Some(1)
            }]
        );
    }
}
