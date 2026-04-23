//! Sequencer election via heartbeat + priority-based failover.
//!
//! - Sequencer broadcasts heartbeat every 5s via gossipsub
//! - If heartbeat missed 3x (15s), highest-priority live operator takes over
//! - Priority is static from config (0 = highest)
//! - Old sequencer returning does not reclaim leadership

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

// ── Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Sequencer,
    Validator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElectionMessage {
    Heartbeat {
        peer_id: String,
        priority: u8,
        seq_num: u64,
    },
    LeaderAnnounce {
        peer_id: String,
        priority: u8,
    },
}

pub struct ElectionConfig {
    pub our_peer_id: String,
    pub our_priority: u8,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
}

// ── State machine ──────────────────────────────────────────────

pub struct ElectionState {
    config: ElectionConfig,
    role: Role,
    leader: Option<(String, u8)>, // (peer_id, priority)
    last_heartbeat: Instant,
    heartbeat_seq: u64,
    startup_grace: bool,
    startup_time: Instant,

    outbound_tx: mpsc::Sender<ElectionMessage>,
    inbound_rx: mpsc::Receiver<ElectionMessage>,
    role_tx: watch::Sender<Role>,
    // O-H3: publishes the currently-elected leader's peer_id so the
    // validator replay task can gate `batch.sequencer_id` against it
    // instead of trust-on-first-use on the first batch it observes.
    leader_tx: watch::Sender<Option<String>>,
}

impl ElectionState {
    pub fn new(
        config: ElectionConfig,
        outbound_tx: mpsc::Sender<ElectionMessage>,
        inbound_rx: mpsc::Receiver<ElectionMessage>,
        role_tx: watch::Sender<Role>,
        leader_tx: watch::Sender<Option<String>>,
    ) -> Self {
        // Priority 0 starts as sequencer, others as validator
        let role = if config.our_priority == 0 {
            Role::Sequencer
        } else {
            Role::Validator
        };

        let now = Instant::now();
        let leader = if role == Role::Sequencer {
            Some((config.our_peer_id.clone(), config.our_priority))
        } else {
            None
        };
        let _ = leader_tx.send(leader.as_ref().map(|(id, _)| id.clone()));
        ElectionState {
            leader,
            config,
            role,
            last_heartbeat: now,
            heartbeat_seq: 0,
            startup_grace: true,
            startup_time: now,
            outbound_tx,
            inbound_rx,
            role_tx,
            leader_tx,
        }
    }

    fn set_leader(&mut self, leader: Option<(String, u8)>) {
        let id = leader.as_ref().map(|(id, _)| id.clone());
        self.leader = leader;
        let _ = self.leader_tx.send(id);
    }

    pub async fn run(&mut self) {
        let mut heartbeat_tick = tokio::time::interval(self.config.heartbeat_interval);
        let mut check_tick = tokio::time::interval(Duration::from_secs(1));

        info!(
            role = ?self.role,
            priority = self.config.our_priority,
            "election started"
        );

        loop {
            tokio::select! {
                _ = heartbeat_tick.tick() => {
                    if self.role == Role::Sequencer {
                        self.send_heartbeat().await;
                    }
                }
                _ = check_tick.tick() => {
                    self.check_timeout();
                }
                Some(msg) = self.inbound_rx.recv() => {
                    self.handle_message(msg);
                }
            }
        }
    }

    async fn send_heartbeat(&mut self) {
        self.heartbeat_seq += 1;
        debug!(
            seq = self.heartbeat_seq,
            priority = self.config.our_priority,
            "sending heartbeat as sequencer"
        );
        let msg = ElectionMessage::Heartbeat {
            peer_id: self.config.our_peer_id.clone(),
            priority: self.config.our_priority,
            seq_num: self.heartbeat_seq,
        };
        let _ = self.outbound_tx.send(msg).await;
    }

    fn handle_message(&mut self, msg: ElectionMessage) {
        match msg {
            ElectionMessage::Heartbeat {
                ref peer_id,
                priority,
                seq_num,
            } => {
                if peer_id == &self.config.our_peer_id {
                    return; // ignore own heartbeats
                }
                debug!(
                    from = %peer_id,
                    priority,
                    seq_num,
                    "received heartbeat"
                );

                // If we're in startup grace period, any heartbeat from
                // a leader means we should stay validator
                if self.startup_grace {
                    self.startup_grace = false;
                    self.set_leader(Some((peer_id.clone(), priority)));
                    self.last_heartbeat = Instant::now();
                    if self.role == Role::Sequencer && priority < self.config.our_priority {
                        // Higher priority node is already leader
                        self.switch_role(Role::Validator);
                    }
                    return;
                }

                // Update heartbeat timer if from current leader
                if let Some((ref leader_id, _)) = self.leader {
                    if peer_id == leader_id {
                        self.last_heartbeat = Instant::now();
                    }
                }

                // Accept higher-priority node as leader
                if let Some((_, leader_prio)) = self.leader {
                    if priority < leader_prio {
                        self.set_leader(Some((peer_id.clone(), priority)));
                        self.last_heartbeat = Instant::now();
                        if self.role == Role::Sequencer && priority < self.config.our_priority {
                            self.switch_role(Role::Validator);
                        }
                    }
                } else {
                    // No known leader — accept this one
                    self.set_leader(Some((peer_id.clone(), priority)));
                    self.last_heartbeat = Instant::now();
                }
            }
            ElectionMessage::LeaderAnnounce {
                ref peer_id,
                priority,
            } => {
                if peer_id == &self.config.our_peer_id {
                    return;
                }

                // Accept if they have higher priority (lower number)
                if priority < self.config.our_priority {
                    info!(
                        new_leader = %peer_id,
                        priority,
                        "accepting higher-priority leader"
                    );
                    self.set_leader(Some((peer_id.clone(), priority)));
                    self.last_heartbeat = Instant::now();
                    if self.role == Role::Sequencer {
                        self.switch_role(Role::Validator);
                    }
                } else if priority == self.config.our_priority {
                    // Tie — lower peer_id string wins
                    if *peer_id < self.config.our_peer_id {
                        self.set_leader(Some((peer_id.clone(), priority)));
                        self.last_heartbeat = Instant::now();
                        if self.role == Role::Sequencer {
                            self.switch_role(Role::Validator);
                        }
                    }
                }
                // Ignore if they have lower priority than us and we're sequencer
            }
        }
    }

    fn check_timeout(&mut self) {
        // Startup grace: don't trigger failover for first heartbeat_timeout
        if self.startup_grace {
            if self.startup_time.elapsed() > self.config.heartbeat_timeout {
                self.startup_grace = false;
                // If nobody claimed leadership during grace, take over
                if self.leader.is_none() {
                    self.promote();
                }
            }
            return;
        }

        if self.role == Role::Validator {
            let elapsed = self.last_heartbeat.elapsed();
            debug!(
                ?elapsed,
                leader = ?self.leader,
                timeout = ?self.config.heartbeat_timeout,
                "validator heartbeat tick"
            );
            if elapsed > self.config.heartbeat_timeout {
                warn!(?elapsed, "leader heartbeat timeout — attempting takeover");
                self.promote();
            }
        }
    }

    fn promote(&mut self) {
        info!(
            priority = self.config.our_priority,
            "promoting self to sequencer"
        );
        self.set_leader(Some((
            self.config.our_peer_id.clone(),
            self.config.our_priority,
        )));
        self.switch_role(Role::Sequencer);

        // Announce leadership (fire-and-forget)
        let msg = ElectionMessage::LeaderAnnounce {
            peer_id: self.config.our_peer_id.clone(),
            priority: self.config.our_priority,
        };
        let tx = self.outbound_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(msg).await;
        });
    }

    fn switch_role(&mut self, new_role: Role) {
        if self.role != new_role {
            info!(from = ?self.role, to = ?new_role, "role change");
            self.role = new_role;
            let _ = self.role_tx.send(new_role);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn priority_0_starts_as_sequencer() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, role_rx) = watch::channel(Role::Validator);

        let config = ElectionConfig {
            our_peer_id: "A".into(),
            our_priority: 0,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        assert_eq!(state.role, Role::Sequencer);
        drop(in_tx); // suppress warning
        drop(role_rx);
    }

    #[tokio::test]
    async fn priority_1_starts_as_validator() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Validator);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        assert_eq!(state.role, Role::Validator);
    }

    #[tokio::test]
    async fn steps_down_on_higher_priority_announce() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, mut _role_rx) = watch::channel(Role::Sequencer);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.role = Role::Sequencer;
        state.startup_grace = false;

        state.handle_message(ElectionMessage::LeaderAnnounce {
            peer_id: "A".into(),
            priority: 0,
        });

        assert_eq!(state.role, Role::Validator);
    }

    #[tokio::test]
    async fn ignores_lower_priority_announce() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Sequencer);

        let config = ElectionConfig {
            our_peer_id: "A".into(),
            our_priority: 0,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.startup_grace = false;

        // Lower priority (higher number) tries to claim leadership
        state.handle_message(ElectionMessage::LeaderAnnounce {
            peer_id: "C".into(),
            priority: 2,
        });

        // Should remain sequencer
        assert_eq!(state.role, Role::Sequencer);
    }

    #[tokio::test]
    async fn ignores_own_heartbeat() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Validator);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.startup_grace = false;

        // Own heartbeat should be ignored
        state.handle_message(ElectionMessage::Heartbeat {
            peer_id: "B".into(),
            priority: 1,
            seq_num: 1,
        });

        // Leader should still be None (not self)
        assert!(state.leader.is_none() || state.leader.as_ref().unwrap().0 != "B");
    }

    #[tokio::test]
    async fn heartbeat_resets_timeout() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Validator);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.startup_grace = false;
        state.leader = Some(("A".into(), 0));
        // Simulate old heartbeat
        state.last_heartbeat = Instant::now() - Duration::from_secs(10);

        // Receive heartbeat from leader
        state.handle_message(ElectionMessage::Heartbeat {
            peer_id: "A".into(),
            priority: 0,
            seq_num: 5,
        });

        // Heartbeat should be recent now
        assert!(state.last_heartbeat.elapsed() < Duration::from_secs(1));
        assert_eq!(state.role, Role::Validator);
    }

    #[tokio::test]
    async fn timeout_promotes_to_sequencer() {
        let (out_tx, mut out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Validator);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_millis(50), // short for test
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.startup_grace = false;
        state.leader = Some(("A".into(), 0));
        // Set last heartbeat to long ago
        state.last_heartbeat = Instant::now() - Duration::from_secs(1);

        state.check_timeout();

        assert_eq!(state.role, Role::Sequencer);
        assert_eq!(state.leader.as_ref().unwrap().0, "B");

        // Should have sent LeaderAnnounce
        tokio::time::sleep(Duration::from_millis(10)).await;
        let msg = out_rx.try_recv();
        assert!(msg.is_ok());
    }

    #[tokio::test]
    async fn role_change_sent_via_watch() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, role_rx) = watch::channel(Role::Sequencer);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.role = Role::Sequencer;
        state.startup_grace = false;

        // Trigger role change
        state.handle_message(ElectionMessage::LeaderAnnounce {
            peer_id: "A".into(),
            priority: 0,
        });

        assert_eq!(*role_rx.borrow(), Role::Validator);
    }

    #[tokio::test]
    async fn tie_breaking_by_peer_id() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Sequencer);

        let config = ElectionConfig {
            our_peer_id: "Z_high_peer_id".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let (leader_tx, _leader_rx) = watch::channel::<Option<String>>(None);
        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        state.role = Role::Sequencer;
        state.startup_grace = false;

        // Same priority but lower peer_id → they win
        state.handle_message(ElectionMessage::LeaderAnnounce {
            peer_id: "A_low_peer_id".into(),
            priority: 1,
        });

        assert_eq!(state.role, Role::Validator);
        assert_eq!(state.leader.as_ref().unwrap().0, "A_low_peer_id");
    }

    #[tokio::test]
    async fn leader_channel_publishes_on_election_change() {
        // O-H3: the watch channel must reflect the currently-elected
        // leader so the validator replay loop can gate batch sources.
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, _role_rx) = watch::channel(Role::Validator);
        let (leader_tx, leader_rx) = watch::channel::<Option<String>>(None);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx, leader_tx);
        // Priority-1 validator starts with no leader.
        assert_eq!(*leader_rx.borrow(), None);

        // A heartbeat from a higher-priority node establishes leadership.
        state.handle_message(ElectionMessage::Heartbeat {
            peer_id: "A".into(),
            priority: 0,
            seq_num: 1,
        });
        assert_eq!(leader_rx.borrow().as_deref(), Some("A"));

        // Self-promotion (e.g. on leader timeout) publishes our own id.
        state.promote();
        assert_eq!(leader_rx.borrow().as_deref(), Some("B"));
    }

    #[test]
    fn election_message_serialization() {
        let msg = ElectionMessage::Heartbeat {
            peer_id: "A".into(),
            priority: 0,
            seq_num: 42,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"heartbeat\""));
        let decoded: ElectionMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            ElectionMessage::Heartbeat { seq_num, .. } => assert_eq!(seq_num, 42),
            _ => panic!("wrong variant"),
        }
    }
}
