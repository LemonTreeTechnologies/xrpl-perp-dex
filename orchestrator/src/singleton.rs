//! Singleton-runner: run a daemon on exactly one cluster node (the sequencer).
//!
//! Watches `role_rx`; spawns the work future on Sequencer promotion,
//! aborts it on demotion. Re-creates afresh on re-promotion.
//!
//! Brief overlap during failover (within heartbeat_timeout) is accepted
//! for idempotent daemons (price feed, deposit scanner, liquidation scan).
//! Adapted from Phoenix PM's singleton.rs.

use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::election::Role;

pub struct SingletonHandle {
    #[allow(dead_code)] // kept for debug logs if Drop ever needs to identify the handle
    name: &'static str,
    monitor: JoinHandle<()>,
}

impl Drop for SingletonHandle {
    fn drop(&mut self) {
        self.monitor.abort();
    }
}

pub fn spawn<F, Fut>(
    name: &'static str,
    mut role_rx: watch::Receiver<Role>,
    factory: F,
) -> SingletonHandle
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let factory = Arc::new(factory);

    let monitor = tokio::spawn(async move {
        info!(name, "singleton monitor started");
        let mut active: Option<JoinHandle<()>> = None;
        let mut last_role = *role_rx.borrow();

        if last_role == Role::Sequencer {
            info!(name, "starting (role=Sequencer at startup)");
            active = Some(tokio::spawn(factory()));
        }

        while role_rx.changed().await.is_ok() {
            let new_role = *role_rx.borrow();
            if new_role == last_role {
                continue;
            }
            match new_role {
                Role::Sequencer => {
                    info!(name, "promoted — starting singleton");
                    if let Some(prev) = active.take() {
                        warn!(name, "stale handle — aborting before restart");
                        prev.abort();
                    }
                    active = Some(tokio::spawn(factory()));
                }
                Role::Validator => {
                    info!(name, "demoted — stopping singleton");
                    if let Some(prev) = active.take() {
                        prev.abort();
                    }
                }
            }
            last_role = new_role;
        }
        if let Some(prev) = active.take() {
            prev.abort();
        }
    });

    SingletonHandle { name, monitor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn starts_on_promote_stops_on_demote() {
        let (role_tx, role_rx) = watch::channel(Role::Validator);
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let c = counter.clone();
        let _handle = spawn("test", role_rx, move || {
            let c = c.clone();
            async move {
                loop {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);

        let _ = role_tx.send(Role::Sequencer);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(counter.load(std::sync::atomic::Ordering::SeqCst) >= 3);

        let _ = role_tx.send(Role::Validator);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after_demote = counter.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let final_count = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            final_count - after_demote <= 1,
            "singleton must stop after demotion"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_runs_on_re_promotion() {
        let (role_tx, role_rx) = watch::channel(Role::Validator);
        let starts = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let s = starts.clone();
        let _handle = spawn("test", role_rx, move || {
            s.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { tokio::time::sleep(Duration::from_secs(60)).await }
        });

        let _ = role_tx.send(Role::Sequencer);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = role_tx.send(Role::Validator);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = role_tx.send(Role::Sequencer);
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
