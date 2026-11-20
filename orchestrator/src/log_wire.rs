//! Wire pieces of the replicated-log transport, kept free of networking so each rule is testable.
//!
//! - Gossip carries only small headers: an [`LogGossip::Announce`] of a stored record (seq, chain hash,
//!   exact length, SHA-256 of the record file) and [`LogGossip::Applied`] shares. Record bodies move by
//!   request-response, which is also the catch-up path.
//! - A fetched body is checked against the announced length and hash BEFORE it is decoded, stored or
//!   given to the enclave; a mismatch means "try another peer", never "use it".
//! - Fetching is bounded: a fixed number of attempts over distinct peers.
//! - APPLIED shares are collected per (seq, chain hash) and assembled into the enclave's quorum bundle;
//!   a share for a different chain hash at the same seq is reported, never mixed in.
#![allow(dead_code)]

use crate::log_store::{decode_record, Durable, LogRecord, MAX_REC_FILE};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

/// Largest gossip header accepted (the headers are a few hundred bytes).
pub const MAX_GOSSIP_LEN: usize = 4 * 1024;
/// One member signature entry from the enclave: pubkey 33 | DER length 1 | DER ≤ 72.
pub const SIG_ENTRY_MAX: usize = 33 + 1 + 72;
/// Smallest DER ECDSA signature the format admits.
const DER_MIN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogGossip {
    /// "I have stored record `seq`; its file is `record_len` bytes with this SHA-256."
    Announce {
        seq: u64,
        chain_hash: String,
        record_len: u32,
        record_sha256: String,
    },
    /// One member's APPLIED signature entry for (seq, chain_hash).
    Applied {
        seq: u64,
        chain_hash: String,
        share: String,
    },
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32]> {
    let b = hex::decode(s).with_context(|| format!("{what} is not hex"))?;
    b.try_into()
        .map_err(|_| anyhow::anyhow!("{what} is not 32 bytes"))
}

/// A checked announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub seq: u64,
    pub chain_hash: [u8; 32],
    pub record_len: usize,
    pub record_sha256: [u8; 32],
}

/// A checked APPLIED share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub seq: u64,
    pub chain_hash: [u8; 32],
    pub pubkey: [u8; 33],
    pub der: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Announce(Announce),
    Applied(Share),
}

/// Parse one gossip message strictly (size first, unknown fields refused, every field checked).
pub fn parse_gossip(data: &[u8]) -> Result<Parsed> {
    ensure!(
        data.len() <= MAX_GOSSIP_LEN,
        "log gossip is {} bytes",
        data.len()
    );
    match serde_json::from_slice::<LogGossip>(data).context("log gossip is not well-formed")? {
        LogGossip::Announce {
            seq,
            chain_hash,
            record_len,
            record_sha256,
        } => {
            let record_len = record_len as usize;
            ensure!(
                record_len > 0 && record_len <= MAX_REC_FILE,
                "announced length {record_len} out of range"
            );
            Ok(Parsed::Announce(Announce {
                seq,
                chain_hash: hex32(&chain_hash, "chain_hash")?,
                record_len,
                record_sha256: hex32(&record_sha256, "record_sha256")?,
            }))
        }
        LogGossip::Applied {
            seq,
            chain_hash,
            share,
        } => {
            let raw = hex::decode(&share).context("share is not hex")?;
            let (pubkey, der) = parse_sig_entry(&raw)?;
            Ok(Parsed::Applied(Share {
                seq,
                chain_hash: hex32(&chain_hash, "chain_hash")?,
                pubkey,
                der,
            }))
        }
    }
}

/// The announcement of a durably stored record.
pub fn announce(d: &Durable) -> LogGossip {
    LogGossip::Announce {
        seq: d.seq(),
        chain_hash: hex::encode(d.chain_hash()),
        // A stored record file is within MAX_REC_FILE, which fits u32.
        record_len: u32::try_from(d.file_len()).expect("a stored record fits u32"),
        record_sha256: hex::encode(d.file_sha256()),
    }
}

/// This node's APPLIED share for a durably stored record. `entry` is the enclave's signature entry.
pub fn applied(d: &Durable, entry: &[u8]) -> Result<LogGossip> {
    parse_sig_entry(entry)?;
    Ok(LogGossip::Applied {
        seq: d.seq(),
        chain_hash: hex::encode(d.chain_hash()),
        share: hex::encode(entry),
    })
}

/// The announcement for an arbitrary record file (tests build headers for records not in a store).
#[cfg(test)]
pub fn announce_for(seq: u64, chain_hash: &[u8; 32], record_file: &[u8]) -> Result<LogGossip> {
    ensure!(
        !record_file.is_empty() && record_file.len() <= MAX_REC_FILE,
        "record file length out of range"
    );
    Ok(LogGossip::Announce {
        seq,
        chain_hash: hex::encode(chain_hash),
        record_len: u32::try_from(record_file.len()).context("record length")?,
        record_sha256: hex::encode(Sha256::digest(record_file)),
    })
}

/// Split one enclave signature entry. Strict: compressed key prefix, DER length in range, exact size.
pub fn parse_sig_entry(b: &[u8]) -> Result<([u8; 33], Vec<u8>)> {
    ensure!(
        b.len() >= 34 && b.len() <= SIG_ENTRY_MAX,
        "signature entry is {} bytes",
        b.len()
    );
    ensure!(
        b[0] == 0x02 || b[0] == 0x03,
        "not a compressed secp256k1 key"
    );
    let n = b[33] as usize;
    ensure!((DER_MIN..=72).contains(&n), "DER length {n} out of range");
    ensure!(
        b.len() == 34 + n,
        "signature entry length does not match its DER length"
    );
    Ok((b[..33].try_into().expect("33 bytes"), b[34..].to_vec()))
}

/// Check a fetched record file against its announcement, then decode it. Length and hash are checked
/// before anything is parsed.
pub fn verify_fetched(a: &Announce, file: &[u8]) -> Result<LogRecord> {
    ensure!(
        file.len() == a.record_len,
        "fetched {} bytes, announced {}",
        file.len(),
        a.record_len
    );
    ensure!(
        Sha256::digest(file).as_slice() == a.record_sha256,
        "fetched record does not match the announced hash"
    );
    let r = decode_record(file)?;
    ensure!(
        r.seq == a.seq,
        "fetched record is seq {}, announced {}",
        r.seq,
        a.seq
    );
    ensure!(
        r.chain_hash == a.chain_hash,
        "fetched record has a different chain hash than announced"
    );
    Ok(r)
}

// ── Fetch request/response framing ─────────────────────────────────────

const FETCH_MAGIC: &[u8; 4] = b"PLF1";
pub const FETCH_REQ_LEN: usize = 4 + 8;
const RESP_HAVE: u8 = 0;
const RESP_NONE: u8 = 1;

pub fn encode_fetch_request(seq: u64) -> Vec<u8> {
    let mut v = FETCH_MAGIC.to_vec();
    v.extend_from_slice(&seq.to_be_bytes());
    v
}

pub fn decode_fetch_request(b: &[u8]) -> Result<u64> {
    ensure!(
        b.len() == FETCH_REQ_LEN,
        "fetch request is {} bytes",
        b.len()
    );
    ensure!(&b[..4] == FETCH_MAGIC, "bad fetch request magic");
    Ok(u64::from_be_bytes(b[4..].try_into().expect("8 bytes")))
}

/// Response: a 5-byte header (status, u32 length) then the record file; "not present" carries length 0.
pub fn encode_fetch_response(file: Option<&[u8]>) -> Result<Vec<u8>> {
    match file {
        None => Ok(vec![RESP_NONE, 0, 0, 0, 0]),
        Some(f) => {
            ensure!(f.len() <= MAX_REC_FILE, "record file too large to serve");
            let mut v = Vec::with_capacity(5 + f.len());
            v.push(RESP_HAVE);
            v.extend_from_slice(&u32::try_from(f.len()).context("length")?.to_be_bytes());
            v.extend_from_slice(f);
            Ok(v)
        }
    }
}

/// The longest response a reader may ever accept.
pub const MAX_FETCH_RESP: usize = 1 + 4 + MAX_REC_FILE;

/// Read the response header and return how many body bytes follow (the caller reads exactly that many,
/// having checked it against the bound first). `None` = the peer does not have the record.
pub fn fetch_response_body_len(head: &[u8; 5]) -> Result<Option<usize>> {
    match head[0] {
        RESP_NONE => {
            ensure!(
                head[1..] == [0, 0, 0, 0],
                "a 'not present' response carries a length"
            );
            Ok(None)
        }
        RESP_HAVE => {
            let n = u32::from_be_bytes(head[1..].try_into().expect("4 bytes")) as usize;
            ensure!(
                n > 0 && n <= MAX_REC_FILE,
                "response body length {n} out of range"
            );
            Ok(Some(n))
        }
        s => anyhow::bail!("unknown fetch response status {s}"),
    }
}

// ── Bounded fetch plan ────────────────────────────────────────────────

/// Which peer to ask next for one record: each peer at most once, at most `max_attempts` in total.
#[derive(Debug)]
pub struct FetchPlan<P> {
    queue: Vec<P>,
    tried: usize,
    max_attempts: usize,
}

impl<P: Clone + PartialEq> FetchPlan<P> {
    /// `announcer` (who said they have it) is asked first; then the other members, in order.
    pub fn new(announcer: Option<P>, members: &[P], max_attempts: usize) -> Self {
        let mut queue = Vec::new();
        for p in announcer.into_iter().chain(members.iter().cloned()) {
            if !queue.contains(&p) {
                queue.push(p);
            }
        }
        Self {
            queue,
            tried: 0,
            max_attempts,
        }
    }

    pub fn next_peer(&mut self) -> Option<P> {
        if self.tried >= self.max_attempts || self.tried >= self.queue.len() {
            return None;
        }
        let p = self.queue[self.tried].clone();
        self.tried += 1;
        Some(p)
    }

    pub fn exhausted(&self) -> bool {
        self.tried >= self.max_attempts || self.tried >= self.queue.len()
    }
}

// ── APPLIED share collection ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareOutcome {
    /// Recorded; not yet enough distinct signers.
    Pending,
    /// Already had this signer's share for this seq.
    Duplicate,
    /// Enough distinct signers: the bundle to verify in the enclave and store.
    Ready(Vec<u8>),
    /// A share for another chain hash at a seq whose chain hash is already known: two different
    /// batches at one seq. Not mixed in; the caller raises the alarm.
    Conflict { seq: u64 },
    /// Outside the window the collector keeps (too old or too far ahead).
    OutOfWindow,
}

/// Collects APPLIED shares per seq. Bounded: only seqs within `window` of THIS node's own position in the
/// log (the caller's `anchor`, e.g. its last stored seq) are kept — never of the largest seq a peer
/// claimed, which one forged share could push out of reach — and a seq holds fewer than `min_shares` shares before it completes and is dropped.
/// Shares gathered so far for one seq: the chain hash they sign, and (pubkey, DER) per signer.
type Pending = ([u8; 32], Vec<([u8; 33], Vec<u8>)>);

#[derive(Debug)]
pub struct CertCollector {
    min_shares: usize,
    window: u64,
    by_seq: BTreeMap<u64, Pending>,
    done: HashSet<u64>,
}

impl CertCollector {
    pub fn new(min_shares: usize, window: u64) -> Self {
        assert!(min_shares >= 1 && window >= 1);
        Self {
            min_shares,
            window,
            by_seq: BTreeMap::new(),
            done: HashSet::new(),
        }
    }

    pub fn add(&mut self, s: Share, anchor: u64) -> ShareOutcome {
        self.prune(anchor);
        let too_old = s.seq.saturating_add(self.window) <= anchor;
        let too_new = s.seq > anchor.saturating_add(self.window);
        if too_old || too_new {
            return ShareOutcome::OutOfWindow;
        }
        if self.done.contains(&s.seq) {
            return ShareOutcome::Duplicate;
        }
        let entry = self
            .by_seq
            .entry(s.seq)
            .or_insert_with(|| (s.chain_hash, Vec::new()));
        if entry.0 != s.chain_hash {
            return ShareOutcome::Conflict { seq: s.seq };
        }
        if entry.1.iter().any(|(pk, _)| *pk == s.pubkey) {
            return ShareOutcome::Duplicate;
        }
        entry.1.push((s.pubkey, s.der));
        let ready = entry.1.len() >= self.min_shares;
        let bundle = ready.then(|| {
            let pairs: Vec<(Vec<u8>, Vec<u8>)> = entry
                .1
                .iter()
                .map(|(pk, d)| (pk.to_vec(), d.clone()))
                .collect();
            crate::quorum_bundle::build(&pairs)
        });
        match bundle {
            Some(b) => {
                self.by_seq.remove(&s.seq);
                self.done.insert(s.seq);
                ShareOutcome::Ready(b)
            }
            None => ShareOutcome::Pending,
        }
    }

    fn prune(&mut self, anchor: u64) {
        let floor = anchor.saturating_sub(self.window);
        self.by_seq.retain(|&q, _| q > floor);
        self.done.retain(|&q| q > floor);
    }

    pub fn tracked(&self) -> usize {
        self.by_seq.len() + self.done.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_store::encode_record;

    fn entry(prefix: u8, der_len: usize) -> Vec<u8> {
        let mut e = vec![0x02];
        e.extend([prefix; 32]);
        e.push(der_len as u8);
        e.extend(vec![0x30; der_len]);
        e
    }

    fn rec(seq: u64) -> LogRecord {
        LogRecord {
            seq,
            chain_hash: [7; 32],
            market_id: 0,
            batch: vec![1, 2, 3],
            quote_blob: vec![4; 10],
            side_blobs: vec![],
            sig_bundle: entry(1, 70),
        }
    }

    #[test]
    fn signature_entries_parse_strictly() {
        let e = entry(9, 70);
        let (pk, der) = parse_sig_entry(&e).unwrap();
        assert_eq!((pk[0], pk[1], der.len()), (0x02, 9, 70));
        let mut bad_prefix = e.clone();
        bad_prefix[0] = 0x04;
        assert!(parse_sig_entry(&bad_prefix).is_err());
        let mut short = e.clone();
        short.pop();
        assert!(
            parse_sig_entry(&short).is_err(),
            "length must match the DER length byte"
        );
        assert!(
            parse_sig_entry(&entry(9, 7)).is_err(),
            "DER below the minimum"
        );
        assert!(parse_sig_entry(&entry(9, 73)).is_err(), "DER above 72");
        assert!(parse_sig_entry(&e[..20]).is_err());
    }

    #[test]
    fn gossip_round_trips_and_refuses_bad_shapes() {
        let file = encode_record(&rec(5)).unwrap();
        let g = announce_for(5, &[7; 32], &file).unwrap();
        let bytes = serde_json::to_vec(&g).unwrap();
        let Parsed::Announce(a) = parse_gossip(&bytes).unwrap() else {
            panic!()
        };
        assert_eq!(
            (a.seq, a.record_len, a.chain_hash),
            (5, file.len(), [7; 32])
        );
        let share = serde_json::to_vec(&LogGossip::Applied {
            seq: 5,
            chain_hash: hex::encode([7; 32]),
            share: hex::encode(entry(3, 70)),
        })
        .unwrap();
        assert!(
            matches!(parse_gossip(&share).unwrap(), Parsed::Applied(s) if s.seq == 5 && s.pubkey[1] == 3)
        );
        let zero = format!(
            r#"{{"type":"announce","seq":1,"chain_hash":"{h}","record_len":0,"record_sha256":"{h}"}}"#,
            h = "00".repeat(32)
        );
        assert!(parse_gossip(zero.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("out of range"));
        assert!(
            announce_for(1, &[0; 32], &[]).is_err(),
            "nothing to announce"
        );
        assert!(
            announce_for(1, &[0; 32], &vec![0; MAX_REC_FILE + 1]).is_err(),
            "over the cap"
        );
        let huge = format!(
            r#"{{"type":"announce","seq":1,"chain_hash":"{h}","record_len":{n},"record_sha256":"{h}"}}"#,
            h = "00".repeat(32),
            n = MAX_REC_FILE + 1
        );
        assert!(parse_gossip(huge.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("out of range"));
        let extra = format!(
            r#"{{"type":"announce","seq":1,"chain_hash":"{h}","record_len":5,"record_sha256":"{h}","x":1}}"#,
            h = "00".repeat(32)
        );
        assert!(
            parse_gossip(extra.as_bytes()).is_err(),
            "unknown fields are refused"
        );
        let short_hash = format!(
            r#"{{"type":"announce","seq":1,"chain_hash":"{h}","record_len":5,"record_sha256":"00"}}"#,
            h = "00".repeat(32)
        );
        assert!(parse_gossip(short_hash.as_bytes()).is_err());
        assert!(parse_gossip(&vec![b' '; MAX_GOSSIP_LEN + 1])
            .unwrap_err()
            .to_string()
            .contains("bytes"));
    }

    #[test]
    fn announcements_and_shares_are_made_from_the_store_proof() {
        let d = tempfile::tempdir().unwrap();
        let mut s = crate::log_store::LogStore::open(d.path()).unwrap();
        let proof = s.put(&rec(5)).unwrap();
        let file = std::fs::read(d.path().join(format!("{:020}.rec", 5))).unwrap();
        assert_eq!(
            announce(&proof),
            announce_for(5, &[7; 32], &file).unwrap(),
            "the header describes the file on disk"
        );
        let again = s.put(&rec(5)).unwrap();
        assert_eq!(
            announce(&again),
            announce(&proof),
            "re-storing proves the same file"
        );
        let Parsed::Applied(sh) =
            parse_gossip(&serde_json::to_vec(&applied(&proof, &entry(3, 70)).unwrap()).unwrap())
                .unwrap()
        else {
            panic!()
        };
        assert_eq!((sh.seq, sh.chain_hash, sh.pubkey[1]), (5, [7; 32], 3));
        assert!(
            applied(&proof, &entry(3, 7)).is_err(),
            "a malformed entry is not published"
        );
    }

    #[test]
    fn a_fetched_body_must_match_its_announcement_before_it_is_decoded() {
        let file = encode_record(&rec(5)).unwrap();
        let Parsed::Announce(a) =
            parse_gossip(&serde_json::to_vec(&announce_for(5, &[7; 32], &file).unwrap()).unwrap())
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(verify_fetched(&a, &file).unwrap(), rec(5));
        // A different, internally valid record of the same length: only the hash tells them apart.
        let mut other = rec(5);
        other.batch = vec![9, 9, 9];
        let other_file = encode_record(&other).unwrap();
        assert_eq!(other_file.len(), file.len());
        assert!(verify_fetched(&a, &other_file)
            .unwrap_err()
            .to_string()
            .contains("hash"));
        // A short body is refused on its length, before the hash is computed.
        assert!(verify_fetched(&a, &file[..file.len() - 1])
            .unwrap_err()
            .to_string()
            .contains("bytes, announced"));
        // Announced seq / chain hash must match what the record itself says.
        let mut wrong_seq = a;
        wrong_seq.seq = 6;
        assert!(verify_fetched(&wrong_seq, &file)
            .unwrap_err()
            .to_string()
            .contains("seq"));
        let mut wrong_chain = a;
        wrong_chain.chain_hash = [8; 32];
        assert!(verify_fetched(&wrong_chain, &file)
            .unwrap_err()
            .to_string()
            .contains("chain hash"));
    }

    #[test]
    fn fetch_framing_is_exact_and_bounded() {
        assert_eq!(decode_fetch_request(&encode_fetch_request(42)).unwrap(), 42);
        assert!(decode_fetch_request(b"PLF1\0\0\0").is_err());
        assert!(decode_fetch_request(b"XXXX\0\0\0\0\0\0\0\x01").is_err());
        let r = encode_fetch_response(Some(b"abc")).unwrap();
        let head: [u8; 5] = r[..5].try_into().unwrap();
        assert_eq!(fetch_response_body_len(&head).unwrap(), Some(3));
        assert_eq!(&r[5..], b"abc");
        assert_eq!(fetch_response_body_len(&[1, 0, 0, 0, 0]).unwrap(), None);
        assert_eq!(
            encode_fetch_response(None).unwrap(),
            vec![1, 0, 0, 0, 0],
            "the header is always 5 bytes"
        );
        assert!(
            fetch_response_body_len(&[1, 0, 0, 0, 3]).is_err(),
            "'not present' with a length"
        );
        let over = u32::try_from(MAX_REC_FILE + 1).unwrap().to_be_bytes();
        assert!(fetch_response_body_len(&[0, over[0], over[1], over[2], over[3]]).is_err());
        assert!(
            fetch_response_body_len(&[0, 0, 0, 0, 0]).is_err(),
            "an empty body is not a record"
        );
        assert!(fetch_response_body_len(&[2, 0, 0, 0, 1]).is_err());
        assert!(encode_fetch_response(Some(&vec![0; MAX_REC_FILE + 1])).is_err());
    }

    #[test]
    fn the_fetch_plan_is_bounded_and_asks_each_peer_once() {
        let mut p = FetchPlan::new(Some("b"), &["a", "b", "c", "d"], 3);
        assert_eq!(
            (p.next_peer(), p.next_peer(), p.next_peer(), p.next_peer()),
            (Some("b"), Some("a"), Some("c"), None)
        );
        assert!(p.exhausted());
        let mut q = FetchPlan::new(None, &["a"], 5);
        assert_eq!((q.next_peer(), q.next_peer()), (Some("a"), None));
    }

    fn share(seq: u64, signer: u8, chain: u8) -> Share {
        Share {
            seq,
            chain_hash: [chain; 32],
            pubkey: [signer; 33],
            der: vec![0x30; 70],
        }
    }

    #[test]
    fn shares_assemble_into_the_enclave_bundle_at_the_threshold() {
        let mut c = CertCollector::new(2, 16);
        assert_eq!(c.add(share(1, 2, 7), 1), ShareOutcome::Pending);
        assert_eq!(
            c.add(share(1, 2, 7), 1),
            ShareOutcome::Duplicate,
            "same signer twice"
        );
        let ShareOutcome::Ready(b) = c.add(share(1, 3, 7), 1) else {
            panic!()
        };
        let expected = crate::quorum_bundle::build(&[
            (vec![2; 33], vec![0x30; 70]),
            (vec![3; 33], vec![0x30; 70]),
        ]);
        assert_eq!(b, expected);
        assert_eq!(
            c.add(share(1, 4, 7), 1),
            ShareOutcome::Duplicate,
            "a seq is finished once"
        );
    }

    #[test]
    fn a_share_for_another_chain_at_the_same_seq_is_a_conflict_not_a_vote() {
        let mut c = CertCollector::new(2, 16);
        assert_eq!(c.add(share(4, 2, 7), 1), ShareOutcome::Pending);
        assert_eq!(c.add(share(4, 3, 8), 1), ShareOutcome::Conflict { seq: 4 });
        assert_eq!(
            c.add(share(4, 3, 7), 1),
            ShareOutcome::Ready(crate::quorum_bundle::build(&[
                (vec![2; 33], vec![0x30; 70]),
                (vec![3; 33], vec![0x30; 70]),
            ]))
        );
    }

    #[test]
    fn the_collector_stays_bounded() {
        let mut c = CertCollector::new(3, 4);
        for q in 0..100 {
            c.add(share(q, 2, 7), q);
        }
        assert!(c.tracked() <= 5, "kept {}", c.tracked());
        assert_eq!(
            c.add(share(10, 3, 7), 99),
            ShareOutcome::OutOfWindow,
            "too old"
        );
        assert_eq!(
            c.add(share(1000, 3, 7), 99),
            ShareOutcome::OutOfWindow,
            "too far ahead"
        );
        // A forged far-ahead share does not move the window: real shares near our position still count.
        let mut f = CertCollector::new(2, 4);
        assert_eq!(f.add(share(u64::MAX, 9, 7), 10), ShareOutcome::OutOfWindow);
        assert_eq!(f.add(share(11, 2, 7), 10), ShareOutcome::Pending);
        assert!(matches!(f.add(share(11, 3, 7), 10), ShareOutcome::Ready(_)));
    }
}
