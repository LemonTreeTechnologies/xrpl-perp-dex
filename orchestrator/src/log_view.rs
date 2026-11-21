//! C1: after each seq every member reports its replicated view (seq, chain hash, state digest); the
//! reports are compared across ALL members.
//!
//! Per seq, with `n` members and a majority of `n / 2 + 1`:
//! - every report equal → agreed;
//! - a majority agrees → each member outside it is the odd one: if that is THIS node, it stops (its state
//!   is not the cluster's); if it is a peer, an alarm names it. The majority continues;
//! - no group can still reach a majority (a three-way split) → the leader must not commit;
//! - reports disagree and no group has a majority yet → the leader holds until one does (there is no
//!   2/3 chain to extend right now); this clears when the missing report arrives.
//!
//! This is a host rule over what members say; binding the view into the quorum certificate is the
//! stronger form (a mainnet gate). A member that lies here can cause a hold or name itself odd — a
//! liveness effect, never a change to what the log accepts.

use crate::log_wire::ViewReport;
use libp2p::PeerId;
use std::collections::{BTreeMap, HashMap};

type Value = ([u8; 32], [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    /// Every member reported the same view at `seq`.
    Agreed { seq: u64 },
    /// A majority agrees at `seq` and `peer` reported something else.
    PeerOdd { seq: u64, peer: PeerId },
    /// A majority agrees at `seq` and THIS node is outside it.
    SelfOdd { seq: u64 },
    /// No view at `seq` can reach a majority any more.
    Split { seq: u64 },
    /// `peer` reported two different views for `seq`; the first one stands.
    Equivocation { seq: u64, peer: PeerId },
}

pub struct ViewBook {
    me: PeerId,
    members: Vec<PeerId>,
    window: u64,
    by_seq: BTreeMap<u64, HashMap<PeerId, Value>>,
    /// Seqs already decided (agreed / odd named / split), so each is reported once.
    decided: BTreeMap<u64, ()>,
}

impl ViewBook {
    /// `members`: every member, this node included.
    pub fn new(me: PeerId, members: Vec<PeerId>, window: u64) -> Self {
        Self {
            me,
            members,
            window,
            by_seq: BTreeMap::new(),
            decided: BTreeMap::new(),
        }
    }

    fn majority(&self) -> usize {
        self.members.len() / 2 + 1
    }

    /// Add one member's report. `anchor` is this node's own last applied seq: reports far from it are
    /// dropped, so a peer cannot fill memory with distant seqs.
    pub fn add(&mut self, from: PeerId, r: ViewReport, anchor: u64) -> Vec<ViewEvent> {
        if !self.members.contains(&from) {
            return vec![];
        }
        if r.seq <= anchor.saturating_sub(self.window) || r.seq > anchor.saturating_add(self.window)
        {
            return vec![];
        }
        let mut ev = vec![];
        let slot = self.by_seq.entry(r.seq).or_default();
        match slot.get(&from) {
            Some(v) if *v != (r.chain_hash, r.digest) => {
                ev.push(ViewEvent::Equivocation {
                    seq: r.seq,
                    peer: from,
                });
                return ev;
            }
            Some(_) => return ev,
            None => {
                slot.insert(from, (r.chain_hash, r.digest));
            }
        }
        if !self.decided.contains_key(&r.seq) {
            ev.extend(self.decide(r.seq));
        }
        self.prune(anchor);
        ev
    }

    fn decide(&mut self, seq: u64) -> Vec<ViewEvent> {
        let Some(slot) = self.by_seq.get(&seq) else {
            return vec![];
        };
        let mut groups: HashMap<Value, Vec<PeerId>> = HashMap::new();
        for (p, v) in slot {
            groups.entry(*v).or_default().push(*p);
        }
        let missing = self.members.len().saturating_sub(slot.len());
        let maj = self.majority();
        if let Some((_, winners)) = groups.iter().find(|(_, ps)| ps.len() >= maj) {
            let mut ev = vec![];
            for p in slot.keys().filter(|p| !winners.contains(p)) {
                ev.push(if *p == self.me {
                    ViewEvent::SelfOdd { seq }
                } else {
                    ViewEvent::PeerOdd { seq, peer: *p }
                });
            }
            if missing == 0 {
                self.decided.insert(seq, ());
                if ev.is_empty() {
                    ev.push(ViewEvent::Agreed { seq });
                }
            }
            return ev;
        }
        let best = groups.values().map(Vec::len).max().unwrap_or(0);
        if best + missing < maj {
            self.decided.insert(seq, ());
            return vec![ViewEvent::Split { seq }];
        }
        vec![]
    }

    /// Whether the leader must hold its next commit: some recent seq has views that disagree with no
    /// majority (yet or ever). `anchor` as in [`add`](Self::add).
    pub fn hold(&self) -> bool {
        let maj = self.majority();
        self.by_seq.values().any(|slot| {
            let mut groups: HashMap<Value, usize> = HashMap::new();
            for v in slot.values() {
                *groups.entry(*v).or_default() += 1;
            }
            groups.len() > 1 && groups.values().all(|&c| c < maj)
        })
    }

    fn prune(&mut self, anchor: u64) {
        let floor = anchor.saturating_sub(self.window);
        self.by_seq.retain(|&s, _| s > floor);
        self.decided.retain(|&s, _| s > floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(seq: u64, d: u8) -> ViewReport {
        ViewReport {
            seq,
            chain_hash: [seq as u8; 32],
            digest: [d; 32],
        }
    }

    fn three() -> (ViewBook, PeerId, PeerId, PeerId) {
        let (me, a, b) = (PeerId::random(), PeerId::random(), PeerId::random());
        (ViewBook::new(me, vec![me, a, b], 16), me, a, b)
    }

    #[test]
    fn three_equal_views_agree_once() {
        let (mut k, me, a, b) = three();
        assert!(k.add(me, v(1, 7), 1).is_empty());
        assert!(k.add(a, v(1, 7), 1).is_empty());
        assert_eq!(k.add(b, v(1, 7), 1), vec![ViewEvent::Agreed { seq: 1 }]);
        assert!(!k.hold());
    }

    #[test]
    fn two_against_one_names_the_odd_peer_and_does_not_hold() {
        let (mut k, me, a, b) = three();
        k.add(me, v(1, 7), 1);
        k.add(a, v(1, 7), 1);
        assert_eq!(
            k.add(b, v(1, 9), 1),
            vec![ViewEvent::PeerOdd { seq: 1, peer: b }]
        );
        assert!(!k.hold(), "a 2/3 chain exists: the leader continues");
    }

    #[test]
    fn this_node_in_the_minority_is_told_so() {
        let (mut k, me, a, b) = three();
        k.add(a, v(1, 7), 1);
        k.add(b, v(1, 7), 1);
        assert_eq!(k.add(me, v(1, 9), 1), vec![ViewEvent::SelfOdd { seq: 1 }]);
    }

    #[test]
    fn an_odd_report_after_the_majority_formed_is_named_at_once() {
        let (mut k, me, a, b) = three();
        k.add(a, v(1, 7), 1);
        // a majority (me + a) exists before b reports: b is odd the moment it reports differently.
        assert!(k.add(me, v(1, 7), 1).is_empty());
        assert_eq!(
            k.add(b, v(1, 8), 1),
            vec![ViewEvent::PeerOdd { seq: 1, peer: b }]
        );
    }

    #[test]
    fn three_different_views_split_and_hold() {
        let (mut k, me, a, b) = three();
        k.add(me, v(1, 1), 1);
        k.add(a, v(1, 2), 1);
        assert_eq!(k.add(b, v(1, 3), 1), vec![ViewEvent::Split { seq: 1 }]);
        assert!(k.hold());
    }

    #[test]
    fn two_disagreeing_with_one_missing_hold_until_the_third_decides() {
        let (mut k, me, a, b) = three();
        k.add(me, v(1, 1), 1);
        assert!(
            k.add(a, v(1, 2), 1).is_empty(),
            "undecided: the third report may still form a majority"
        );
        assert!(k.hold(), "no 2/3 chain to extend right now");
        assert_eq!(
            k.add(b, v(1, 1), 1),
            vec![ViewEvent::PeerOdd { seq: 1, peer: a }]
        );
        assert!(!k.hold());
    }

    #[test]
    fn a_peer_changing_its_report_is_an_equivocation_and_the_first_stands() {
        let (mut k, me, a, b) = three();
        k.add(a, v(1, 7), 1);
        assert_eq!(
            k.add(a, v(1, 9), 1),
            vec![ViewEvent::Equivocation { seq: 1, peer: a }]
        );
        k.add(me, v(1, 7), 1);
        assert_eq!(k.add(b, v(1, 7), 1), vec![ViewEvent::Agreed { seq: 1 }]);
        assert!(k.add(a, v(1, 7), 1).is_empty(), "a repeat is not news");
    }

    #[test]
    fn non_members_and_far_seqs_are_ignored() {
        let (mut k, me, a, _) = three();
        let stranger = PeerId::random();
        assert!(k.add(stranger, v(1, 1), 1).is_empty());
        k.add(me, v(1, 1), 1);
        assert!(k.add(a, v(18, 2), 1).is_empty());
        assert!(k.add(a, v(17, 2), 1).is_empty());
        assert!(!k.by_seq.contains_key(&18), "beyond the window");
        assert!(k.by_seq.contains_key(&17));
        assert!(!k.by_seq.get(&1).unwrap().contains_key(&stranger));
    }

    #[test]
    fn old_seqs_are_forgotten() {
        let (mut k, me, a, _) = three();
        k.add(me, v(1, 1), 1);
        k.add(a, v(1, 2), 1);
        assert!(k.hold());
        k.add(me, v(40, 1), 40);
        assert!(!k.by_seq.contains_key(&1));
        assert!(!k.hold(), "a hold on a seq far behind is gone with it");
    }
}
