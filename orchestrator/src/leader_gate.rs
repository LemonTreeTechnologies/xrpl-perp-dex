//! O-H3 — the validator's leader-gate decision, extracted so the provenance binding is
//! provable in CI rather than living only inside the replay loop.
//!
//! A replicated batch is replayed only when its AUTHENTICATED gossip publisher (gossipsub
//! `message.source`, set and verified under `MessageAuthenticity::Signed` +
//! `ValidationMode::Strict`) IS the elected leader, AND the batch's self-declared
//! `sequencer_id` agrees. Trusting `sequencer_id` alone — a JSON field any meshed peer sets
//! freely — would let a peer forge `sequencer_id = leader` and, carrying replayed-but-real
//! client-signed orders under an honest in-window time, walk straight through the gate: the
//! per-order signature check and the time-guard would both pass, validators would fold the
//! replayed fills into derived margin, and the no-blind 2-of-3 withdrawal cosign would go
//! hollow for position-holders. Binding to the authenticated origin closes that.

/// Outcome of the leader-gate check. Everything but `Accept` means "skip this batch".
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderGate {
    /// Authenticated publisher is the elected leader and the field agrees — replay it.
    Accept,
    /// No leader elected yet.
    NoLeader,
    /// Authenticated publisher is some other peer, not the elected leader.
    NotLeader,
    /// Batch arrived without an authenticated publisher (unsigned) — origin cannot be bound.
    Unsigned,
    /// Authenticated origin is the leader, but the self-declared `sequencer_id` disagrees.
    FieldMismatch,
}

/// Decide whether a replicated batch may be replayed. `src_peer` is the AUTHENTICATED gossip
/// publisher PeerId string (`None` = unsigned); `elected` is the elected leader PeerId string
/// (`None` = no leader yet); `claimed_sequencer_id` is the batch's self-declared field.
///
/// Fail-safe: every ambiguous or non-matching case is a refusal, never a replay.
pub fn leader_gate_decision(
    src_peer: Option<&str>,
    elected: Option<&str>,
    claimed_sequencer_id: &str,
) -> LeaderGate {
    let elected_id = match elected {
        Some(e) => e,
        None => return LeaderGate::NoLeader,
    };
    match src_peer {
        Some(src) if src == elected_id => {}
        Some(_) => return LeaderGate::NotLeader,
        None => return LeaderGate::Unsigned,
    }
    if claimed_sequencer_id != elected_id {
        return LeaderGate::FieldMismatch;
    }
    LeaderGate::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEADER: &str = "12D3KooWLeaderAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER: &str = "12D3KooWOtherBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn authenticated_leader_with_matching_field_is_accepted() {
        assert_eq!(
            leader_gate_decision(Some(LEADER), Some(LEADER), LEADER),
            LeaderGate::Accept
        );
    }

    #[test]
    fn forged_field_from_a_non_leader_publisher_is_rejected() {
        // THE O-H3 attack: a meshed peer, authenticated as OTHER, publishes a batch whose JSON
        // sequencer_id CLAIMS the leader. The field-only gate passed this; binding to the
        // authenticated origin must reject it.
        assert_eq!(
            leader_gate_decision(Some(OTHER), Some(LEADER), LEADER),
            LeaderGate::NotLeader
        );
    }

    #[test]
    fn unsigned_batch_cannot_bind_origin_and_is_rejected() {
        assert_eq!(
            leader_gate_decision(None, Some(LEADER), LEADER),
            LeaderGate::Unsigned
        );
    }

    #[test]
    fn no_elected_leader_rejects() {
        assert_eq!(
            leader_gate_decision(Some(LEADER), None, LEADER),
            LeaderGate::NoLeader
        );
    }

    #[test]
    fn authenticated_leader_but_mismatched_self_declared_field_is_rejected() {
        // Origin authenticated as the leader, but the self-declared field disagrees — malformed.
        assert_eq!(
            leader_gate_decision(Some(LEADER), Some(LEADER), OTHER),
            LeaderGate::FieldMismatch
        );
    }
}
