//! The sync gate: a node feeds its enclave log entries (and, in log mode, the leader produces deposit
//! entries) only while its own XRPL node is in sync. Liveness, not safety: the enclave verifies what it
//! is given against its own trust; a node out of sync just waits.
//!
//! In sync means: `server_state` at least `full` (full, validating or proposing), at least `min_peers`
//! peers, and a validated ledger no older than a bound DERIVED from the close interval this node has
//! measured: `closes × interval + pad`. The measured interval is clamped, so a network that stops
//! closing ledgers does not stretch the bound — its validated ledger simply ages out of it.

use anyhow::{Context, Result};
use serde::Serialize;
use std::time::{Duration, Instant};

/// What the gate reads from `server_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub server_state: String,
    pub peers: u64,
    pub validated_seq: u64,
    /// Seconds since the validated ledger closed, as the node reports it.
    pub validated_age_s: u64,
}

impl ServerInfo {
    pub fn from_result(info: &serde_json::Value) -> Result<Self> {
        let i = &info["info"];
        Ok(Self {
            server_state: i["server_state"]
                .as_str()
                .context("no server_state")?
                .to_string(),
            peers: i["peers"].as_u64().context("no peers")?,
            validated_seq: i["validated_ledger"]["seq"]
                .as_u64()
                .context("no validated_ledger.seq")?,
            validated_age_s: i["validated_ledger"]["age"]
                .as_u64()
                .context("no validated_ledger.age")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct SyncCfg {
    pub min_peers: u64,
    /// The bound is this many measured close intervals…
    pub closes: u32,
    /// …plus this pad.
    pub pad: Duration,
    /// The measured interval is clamped to this range.
    pub interval_min: Duration,
    pub interval_max: Duration,
    /// Until an interval has been measured, this one is assumed.
    pub interval_initial: Duration,
}

impl Default for SyncCfg {
    fn default() -> Self {
        Self {
            min_peers: 3,
            closes: 5,
            pad: Duration::from_secs(5),
            interval_min: Duration::from_secs(2),
            interval_max: Duration::from_secs(10),
            interval_initial: Duration::from_secs(4),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SyncVerdict {
    InSync,
    /// Not in sync, with the reason.
    Wait {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncStatus {
    pub verdict: Option<SyncVerdict>,
    pub server_state: String,
    pub peers: u64,
    pub validated_seq: u64,
    pub validated_age_s: u64,
    pub interval_ms: u64,
    pub bound_s: u64,
}

pub struct SyncGate {
    cfg: SyncCfg,
    /// The first sample of the current measurement and the latest one: (validated seq, when seen).
    first: Option<(u64, Instant)>,
    interval: Duration,
}

/// Only states at or above `full` are in sync (rippled's order: … tracking < full < validating < proposing).
fn synced_state(s: &str) -> bool {
    matches!(s, "full" | "validating" | "proposing")
}

impl SyncGate {
    pub fn new(cfg: SyncCfg) -> Self {
        let interval = cfg
            .interval_initial
            .clamp(cfg.interval_min, cfg.interval_max);
        Self {
            cfg,
            first: None,
            interval,
        }
    }

    pub fn bound(&self) -> Duration {
        self.interval
            .saturating_mul(self.cfg.closes)
            .saturating_add(self.cfg.pad)
    }

    /// Feed one sample; get the verdict. The interval is measured from the first sample to the latest
    /// one that advanced the validated seq (wall time per ledger, averaged over the whole run).
    pub fn evaluate(&mut self, si: &ServerInfo, now: Instant) -> SyncVerdict {
        match self.first {
            None => self.first = Some((si.validated_seq, now)),
            Some((s0, t0)) if si.validated_seq > s0 => {
                let ledgers = si.validated_seq - s0;
                let per =
                    now.saturating_duration_since(t0) / u32::try_from(ledgers).unwrap_or(u32::MAX);
                self.interval = per.clamp(self.cfg.interval_min, self.cfg.interval_max);
            }
            // A seq that went backwards (node resynced from scratch): start measuring again.
            Some((s0, _)) if si.validated_seq < s0 => self.first = Some((si.validated_seq, now)),
            Some(_) => {}
        }
        if !synced_state(&si.server_state) {
            return SyncVerdict::Wait {
                reason: format!("server_state {}", si.server_state),
            };
        }
        if si.peers < self.cfg.min_peers {
            return SyncVerdict::Wait {
                reason: format!("{} peers < {}", si.peers, self.cfg.min_peers),
            };
        }
        let bound = self.bound();
        if Duration::from_secs(si.validated_age_s) > bound {
            return SyncVerdict::Wait {
                reason: format!(
                    "validated ledger {}s old > {}s",
                    si.validated_age_s,
                    bound.as_secs()
                ),
            };
        }
        SyncVerdict::InSync
    }

    pub fn status(&self, si: &ServerInfo, v: &SyncVerdict) -> SyncStatus {
        SyncStatus {
            verdict: Some(v.clone()),
            server_state: si.server_state.clone(),
            peers: si.peers,
            validated_seq: si.validated_seq,
            validated_age_s: si.validated_age_s,
            interval_ms: u64::try_from(self.interval.as_millis()).unwrap_or(u64::MAX),
            bound_s: self.bound().as_secs(),
        }
    }
}

/// Read `server_info` from this node's own XRPL endpoint.
pub async fn server_info(http: &reqwest::Client, xrpl_url: &str) -> Result<ServerInfo> {
    let v: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({"method": "server_info", "params": [{}]}))
        .send()
        .await
        .context("server_info request")?
        .json()
        .await
        .context("server_info response")?;
    ServerInfo::from_result(&v["result"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn si(state: &str, peers: u64, seq: u64, age: u64) -> ServerInfo {
        ServerInfo {
            server_state: state.into(),
            peers,
            validated_seq: seq,
            validated_age_s: age,
        }
    }

    #[test]
    fn in_sync_needs_a_synced_state_enough_peers_and_a_fresh_ledger() {
        let mut g = SyncGate::new(SyncCfg::default());
        let t = Instant::now();
        assert_eq!(g.evaluate(&si("full", 10, 100, 2), t), SyncVerdict::InSync);
        for s in ["validating", "proposing"] {
            assert_eq!(
                g.evaluate(&si(s, 10, 100, 2), t),
                SyncVerdict::InSync,
                "{s}"
            );
        }
        for s in ["tracking", "syncing", "connected", "disconnected", ""] {
            assert!(
                matches!(g.evaluate(&si(s, 10, 100, 2), t), SyncVerdict::Wait { reason } if reason.contains("server_state")),
                "{s}"
            );
        }
        assert!(
            matches!(g.evaluate(&si("full", 2, 100, 2), t), SyncVerdict::Wait { reason } if reason.contains("peers"))
        );
        assert_eq!(g.evaluate(&si("full", 3, 100, 2), t), SyncVerdict::InSync);
    }

    #[test]
    fn the_age_bound_is_derived_from_the_measured_close_interval() {
        let mut g = SyncGate::new(SyncCfg::default());
        // Initial assumption 4s: bound 5 × 4 + 5 = 25s.
        assert_eq!(g.bound(), Duration::from_secs(25));
        let t = Instant::now();
        g.evaluate(&si("full", 10, 100, 0), t);
        // 10 ledgers in 30s: 3s per ledger → 5 × 3 + 5 = 20s.
        g.evaluate(&si("full", 10, 110, 0), t + Duration::from_secs(30));
        assert_eq!(g.bound(), Duration::from_secs(20));
        assert_eq!(
            g.evaluate(&si("full", 10, 110, 20), t + Duration::from_secs(30)),
            SyncVerdict::InSync
        );
        assert!(matches!(
            g.evaluate(&si("full", 10, 110, 21), t + Duration::from_secs(30)),
            SyncVerdict::Wait { reason } if reason.contains("21s old > 20s")
        ));
    }

    #[test]
    fn a_network_that_stops_closing_does_not_stretch_the_bound() {
        let mut g = SyncGate::new(SyncCfg::default());
        let t = Instant::now();
        g.evaluate(&si("full", 10, 100, 0), t);
        // One ledger in 10 minutes: the interval is clamped at 10s → bound 55s, not 50 minutes.
        g.evaluate(&si("full", 10, 101, 0), t + Duration::from_secs(600));
        assert_eq!(g.bound(), Duration::from_secs(55));
        // And while nothing closes, the interval is not re-estimated from the stall.
        assert!(matches!(
            g.evaluate(&si("full", 10, 101, 600), t + Duration::from_secs(1200)),
            SyncVerdict::Wait { .. }
        ));
        // Too fast is clamped too (2s floor → 15s).
        let mut f = SyncGate::new(SyncCfg::default());
        f.evaluate(&si("full", 10, 100, 0), t);
        f.evaluate(&si("full", 10, 200, 0), t + Duration::from_secs(10));
        assert_eq!(f.bound(), Duration::from_secs(15));
    }

    #[test]
    fn a_seq_that_goes_backwards_restarts_the_measurement() {
        let mut g = SyncGate::new(SyncCfg::default());
        let t = Instant::now();
        g.evaluate(&si("full", 10, 1000, 0), t);
        g.evaluate(&si("full", 10, 50, 0), t + Duration::from_secs(1));
        // Measured from 50 at t+1: 5 ledgers in 15s = 3s → 20s.
        g.evaluate(&si("full", 10, 55, 0), t + Duration::from_secs(16));
        assert_eq!(g.bound(), Duration::from_secs(20));
    }

    #[test]
    fn server_info_is_read_strictly() {
        let ok = serde_json::json!({"info": {"server_state": "full", "peers": 7,
            "validated_ledger": {"seq": 5, "age": 2}}});
        assert_eq!(ServerInfo::from_result(&ok).unwrap(), si("full", 7, 5, 2));
        for missing in ["server_state", "peers"] {
            let mut v = ok.clone();
            v["info"].as_object_mut().unwrap().remove(missing);
            assert!(ServerInfo::from_result(&v).is_err(), "{missing}");
        }
        let mut v = ok.clone();
        v["info"]["validated_ledger"]
            .as_object_mut()
            .unwrap()
            .remove("age");
        assert!(ServerInfo::from_result(&v).is_err());
    }
}
