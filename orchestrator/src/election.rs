//! Sequencer election via heartbeat + priority-based failover.
//!
//! - Sequencer broadcasts heartbeat every 5s via gossipsub
//! - If heartbeat missed 3x (15s), highest-priority live operator takes over
//! - Priority is static from config (0 = highest)
//! - Old sequencer returning does not reclaim leadership

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

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
}

impl ElectionState {
    pub fn new(
        config: ElectionConfig,
        outbound_tx: mpsc::Sender<ElectionMessage>,
        inbound_rx: mpsc::Receiver<ElectionMessage>,
        role_tx: watch::Sender<Role>,
    ) -> Self {
        // Priority 0 starts as sequencer, others as validator
        let role = if config.our_priority == 0 {
            Role::Sequencer
        } else {
            Role::Validator
        };

        let now = Instant::now();
        ElectionState {
            leader: if role == Role::Sequencer {
                Some((config.our_peer_id.clone(), config.our_priority))
            } else {
                None
            },
            config,
            role,
            last_heartbeat: now,
            heartbeat_seq: 0,
            startup_grace: true,
            startup_time: now,
            outbound_tx,
            inbound_rx,
            role_tx,
        }
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
        let msg = ElectionMessage::Heartbeat {
            peer_id: self.config.our_peer_id.clone(),
            priority: self.config.our_priority,
            seq_num: self.heartbeat_seq,
        };
        let _ = self.outbound_tx.send(msg).await;
    }

    fn handle_message(&mut self, msg: ElectionMessage) {
        match msg {
            ElectionMessage::Heartbeat { ref peer_id, priority, .. } => {
                if peer_id == &self.config.our_peer_id {
                    return; // ignore own heartbeats
                }

                // If we're in startup grace period, any heartbeat from
                // a leader means we should stay validator
                if self.startup_grace {
                    self.startup_grace = false;
                    self.leader = Some((peer_id.clone(), priority));
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
                        self.leader = Some((peer_id.clone(), priority));
                        self.last_heartbeat = Instant::now();
                        if self.role == Role::Sequencer && priority < self.config.our_priority {
                            self.switch_role(Role::Validator);
                        }
                    }
                } else {
                    // No known leader — accept this one
                    self.leader = Some((peer_id.clone(), priority));
                    self.last_heartbeat = Instant::now();
                }
            }
            ElectionMessage::LeaderAnnounce { ref peer_id, priority } => {
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
                    self.leader = Some((peer_id.clone(), priority));
                    self.last_heartbeat = Instant::now();
                    if self.role == Role::Sequencer {
                        self.switch_role(Role::Validator);
                    }
                } else if priority == self.config.our_priority {
                    // Tie — lower peer_id string wins
                    if *peer_id < self.config.our_peer_id {
                        self.leader = Some((peer_id.clone(), priority));
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

        if self.role == Role::Validator
            && self.last_heartbeat.elapsed() > self.config.heartbeat_timeout
        {
            warn!(
                elapsed = ?self.last_heartbeat.elapsed(),
                "leader heartbeat timeout — attempting takeover"
            );
            self.promote();
        }
    }

    fn promote(&mut self) {
        info!(
            priority = self.config.our_priority,
            "promoting self to sequencer"
        );
        self.leader = Some((
            self.config.our_peer_id.clone(),
            self.config.our_priority,
        ));
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

        let state = ElectionState::new(config, out_tx, in_rx, role_tx);
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

        let state = ElectionState::new(config, out_tx, in_rx, role_tx);
        assert_eq!(state.role, Role::Validator);
    }

    #[tokio::test]
    async fn steps_down_on_higher_priority_announce() {
        let (out_tx, _out_rx) = mpsc::channel(10);
        let (_in_tx, in_rx) = mpsc::channel(10);
        let (role_tx, mut role_rx) = watch::channel(Role::Sequencer);

        let config = ElectionConfig {
            our_peer_id: "B".into(),
            our_priority: 1,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(15),
        };

        let mut state = ElectionState::new(config, out_tx, in_rx, role_tx);
        // Force to sequencer for test
        state.role = Role::Sequencer;
        state.startup_grace = false;

        // Receive LeaderAnnounce from higher-priority node
        state.handle_message(ElectionMessage::LeaderAnnounce {
            peer_id: "A".into(),
            priority: 0,
        });

        assert_eq!(state.role, Role::Validator);
    }
}
