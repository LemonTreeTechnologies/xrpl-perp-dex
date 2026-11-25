//! Whether each enclave-backed subsystem this node was configured to run is actually running.
//!
//! A subsystem that is configured here but did not come up (its startup read failed) or that came up and
//! later stopped (its task returned or panicked) leaves the node looking healthy while a security function
//! is off: the join-brain, the DKG coordinator, the Path-A recipient filter, the announcer. Each startup site
//! records what happened; a periodic check raises an ERROR alarm for every configured subsystem that is not
//! running, and /v1/system/status reports them all. A subsystem the node is not configured for is not
//! listed at all, so "absent" never reads as "broken".
//!
//! Alarm, not halt: stopping the node for a disabled join-brain would turn a degraded node into a missing
//! one. The alarm is what was missing — the node used to run degraded with one WARN line at startup.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "why", rename_all = "snake_case")]
pub enum State {
    Running,
    /// Configured, but it did not come up.
    Disabled(String),
    /// It came up and its task has since ended.
    Exited,
}

#[derive(Default)]
struct Entry {
    state: Option<State>,
    task: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct Subsystems(Mutex<BTreeMap<String, Entry>>);

impl Subsystems {
    fn with<R>(&self, f: impl FnOnce(&mut BTreeMap<String, Entry>) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// A subsystem without a long-running task of its own came up.
    pub fn running(&self, name: &str) {
        self.with(|m| m.entry(name.to_string()).or_default().state = Some(State::Running));
    }

    /// A subsystem that runs as a task came up; it is running while the task is.
    pub fn running_task(&self, name: &str, task: JoinHandle<()>) {
        self.with(|m| {
            let e = m.entry(name.to_string()).or_default();
            e.state = Some(State::Running);
            e.task = Some(task);
        });
    }

    /// A configured subsystem did not come up.
    pub fn disabled(&self, name: &str, why: impl Into<String>) {
        let why = why.into();
        self.with(|m| m.entry(name.to_string()).or_default().state = Some(State::Disabled(why)));
    }

    /// Every listed subsystem's state, with ended tasks marked Exited.
    pub fn snapshot(&self) -> BTreeMap<String, State> {
        self.with(|m| {
            for e in m.values_mut() {
                if e.state == Some(State::Running)
                    && e.task.as_ref().is_some_and(|t| t.is_finished())
                {
                    e.state = Some(State::Exited);
                }
            }
            m.iter()
                .filter_map(|(k, e)| e.state.clone().map(|s| (k.clone(), s)))
                .collect()
        })
    }

    /// The configured subsystems that are not running.
    pub fn not_running(&self) -> Vec<(String, State)> {
        self.snapshot()
            .into_iter()
            .filter(|(_, s)| *s != State::Running)
            .collect()
    }
}

/// Every `every`, an ERROR line (and the `subsystem_not_running` metric) for each configured subsystem that
/// is not running.
pub async fn watch(s: std::sync::Arc<Subsystems>, every: std::time::Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        for (name, state) in s.not_running() {
            tracing::error!(
                metric = "subsystem_not_running",
                subsystem = %name,
                state = ?state,
                "a configured subsystem of this node is not running; restart the orchestrator once the cause is fixed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_running_task_that_ends_is_reported_exited() {
        let s = Subsystems::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        s.running_task(
            "join-brain",
            tokio::spawn(async move {
                let _ = rx.await;
            }),
        );
        assert!(s.not_running().is_empty());
        tx.send(()).unwrap();
        for _ in 0..100 {
            if !s.not_running().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            s.not_running(),
            vec![("join-brain".to_string(), State::Exited)]
        );
    }

    #[test]
    fn a_disabled_subsystem_is_reported_and_an_unconfigured_one_is_not_listed() {
        let s = Subsystems::default();
        s.running("path-a-recipient-filter");
        s.disabled("dkg-coordinator", "connection refused");
        assert_eq!(
            s.not_running(),
            vec![(
                "dkg-coordinator".to_string(),
                State::Disabled("connection refused".into())
            )]
        );
        let snap = s.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(!snap.contains_key("join-brain"));
    }

    #[test]
    fn the_status_form_names_the_state_and_why() {
        let v = serde_json::to_value(State::Disabled("refused".into())).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"state": "disabled", "why": "refused"})
        );
        assert_eq!(
            serde_json::to_value(State::Running).unwrap(),
            serde_json::json!({"state": "running"})
        );
    }
}
