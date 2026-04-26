//! Per-key sliding-window rate limiter.
//!
//! Used by the orchestrator's authenticated REST endpoints to bound
//! enumeration / brute-force probes — see
//! `SECURITY-REAUDIT-4-FIXPLAN.md` O-L2 (the zero-balance fallback
//! masks user existence and is intentional, but the timing channel
//! across many probes still leaks; this caps the probe rate so the
//! channel cannot be drained at scale).
//!
//! Keyed on a string (typically the authenticated XRPL address); per
//! key we keep a `VecDeque<Instant>` of recent admit-times and prune
//! entries older than `window`. Admit if the post-prune queue length
//! is below `max_per_window`.
//!
//! The same shape mirrors `p2p::P2PNode::check_signing_rate` from the
//! X-C1 fix; if a third instance shows up it should consolidate here.
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    window: Duration,
    max_per_window: usize,
    inner: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new(window: Duration, max_per_window: usize) -> Self {
        Self {
            window,
            max_per_window,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether `key` has remaining budget; if yes, record a hit
    /// and return `true`. If no, return `false` without recording.
    pub fn check_and_record(&self, key: &str) -> bool {
        self.check_and_record_at(key, Instant::now())
    }

    /// Same as `check_and_record` but accepts an explicit "now" — used
    /// only by tests to avoid sleep-based checks.
    pub fn check_and_record_at(&self, key: &str, now: Instant) -> bool {
        let mut guard = self.inner.lock().expect("rate-limiter mutex poisoned");
        let q = match guard.entry(key.to_string()) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => v.insert(VecDeque::new()),
        };
        // Prune entries that fell out of the trailing window.
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.max_per_window {
            return false;
        }
        q.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_up_to_max_then_rejects() {
        let rl = RateLimiter::new(Duration::from_secs(60), 30);
        let now = Instant::now();
        for i in 0..30 {
            assert!(
                rl.check_and_record_at("alice", now),
                "request {i} must be admitted"
            );
        }
        assert!(
            !rl.check_and_record_at("alice", now),
            "31st request in same instant must be rate-limited"
        );
    }

    #[test]
    fn separate_keys_have_separate_buckets() {
        let rl = RateLimiter::new(Duration::from_secs(60), 5);
        let now = Instant::now();
        for _ in 0..5 {
            assert!(rl.check_and_record_at("alice", now));
        }
        // Alice exhausted; Bob should still be admitted.
        assert!(!rl.check_and_record_at("alice", now));
        for _ in 0..5 {
            assert!(rl.check_and_record_at("bob", now));
        }
        assert!(!rl.check_and_record_at("bob", now));
    }

    #[test]
    fn entries_outside_window_are_pruned() {
        let rl = RateLimiter::new(Duration::from_secs(60), 5);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(rl.check_and_record_at("alice", t0));
        }
        // 5/5 used at t0; 31s later still in window — rejected.
        let t1 = t0 + Duration::from_secs(31);
        assert!(!rl.check_and_record_at("alice", t1));
        // 61s later all 5 are outside the window — admitted.
        let t2 = t0 + Duration::from_secs(61);
        assert!(rl.check_and_record_at("alice", t2));
    }

    #[test]
    fn refusal_does_not_record() {
        // Subtle invariant: a refused request must not extend the
        // bucket's window, otherwise an attacker can keep the limiter
        // stuck open forever by hammering. Verify by exhausting at
        // t0, refusing at t0+30, and checking that one more slot
        // becomes available exactly window-after-t0 (not t0+30).
        let rl = RateLimiter::new(Duration::from_secs(60), 1);
        let t0 = Instant::now();
        assert!(rl.check_and_record_at("alice", t0));
        let t30 = t0 + Duration::from_secs(30);
        assert!(!rl.check_and_record_at("alice", t30)); // refused
        let t61 = t0 + Duration::from_secs(61);
        assert!(
            rl.check_and_record_at("alice", t61),
            "single original entry must have aged out by t0+61, regardless of the t0+30 refusal"
        );
    }
}
