//! Durable per-node store of replicated-log records.
//!
//! One file per sequence number, written once: temp sibling → fsync → rename → fsync the directory, so
//! a record that `put` reported as stored survives a crash or power loss, and a crash mid-write leaves
//! no half-record under a real name. The store is append-only and contiguous: after the first record,
//! only `last + 1` is accepted. Re-storing an identical record is a no-op; a DIFFERENT record at a seq
//! that is already stored is refused and never overwrites — two batches at one seq must surface, not
//! be silently replaced.
//!
//! Every file carries a SHA-256 of its contents and is length-checked before it is allocated, so a
//! damaged or oversized file is an error on read, never a record.
//!
//! The quorum certificate for a record arrives later than the record, so it is a separate write-once
//! file next to it.
#![allow(dead_code)]

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const REC_MAGIC: &[u8; 4] = b"PLR1";
const CERT_MAGIC: &[u8; 4] = b"PLC1";
const REC_EXT: &str = "rec";
const CERT_EXT: &str = "cert";
const TMP_EXT: &str = "tmp";

/// Per-field caps. They bound what one record may allocate; the enclave validates the contents.
pub const MAX_BATCH_LEN: usize = 64 * 1024;
pub const MAX_QUOTE_BLOB_LEN: usize = 64 * 1024;
pub const MAX_SIDE_BLOBS_LEN: usize = 2 * 1024 * 1024;
pub const MAX_SIG_BUNDLE_LEN: usize = 16 * 1024;
pub const MAX_CERT_LEN: usize = 16 * 1024;
/// Fixed part of a record file: magic 4 | seq 8 | chain_hash 32 | market_id 4 | 4 lengths × 4 | sha 32.
const REC_FIXED: usize = 4 + 8 + 32 + 4 + 16 + 32;
/// Largest record file the store will read.
pub const MAX_REC_FILE: usize =
    REC_FIXED + MAX_BATCH_LEN + MAX_QUOTE_BLOB_LEN + MAX_SIDE_BLOBS_LEN + MAX_SIG_BUNDLE_LEN;
/// Fixed part of a certificate file: magic 4 | seq 8 | length 4 | sha 32.
const CERT_FIXED: usize = 4 + 8 + 4 + 32;

/// One applied batch, as this node received and applied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub seq: u64,
    /// The chain hash this node's enclave reported after applying the batch.
    pub chain_hash: [u8; 32],
    pub market_id: u32,
    pub batch: Vec<u8>,
    pub quote_blob: Vec<u8>,
    pub side_blobs: Vec<u8>,
    /// The leader's member signature over the batch.
    pub sig_bundle: Vec<u8>,
}

fn field_caps(r: &LogRecord) -> Result<()> {
    ensure!(
        r.batch.len() <= MAX_BATCH_LEN,
        "batch is {} bytes",
        r.batch.len()
    );
    ensure!(
        r.quote_blob.len() <= MAX_QUOTE_BLOB_LEN,
        "quote blob is {} bytes",
        r.quote_blob.len()
    );
    ensure!(
        r.side_blobs.len() <= MAX_SIDE_BLOBS_LEN,
        "side blobs are {} bytes",
        r.side_blobs.len()
    );
    ensure!(
        r.sig_bundle.len() <= MAX_SIG_BUNDLE_LEN,
        "sig bundle is {} bytes",
        r.sig_bundle.len()
    );
    Ok(())
}

fn len32(n: usize) -> [u8; 4] {
    // Callers check the caps first; every cap fits u32.
    u32::try_from(n)
        .expect("capped length fits u32")
        .to_be_bytes()
}

pub fn encode_record(r: &LogRecord) -> Result<Vec<u8>> {
    field_caps(r)?;
    let mut out = Vec::with_capacity(
        REC_FIXED + r.batch.len() + r.quote_blob.len() + r.side_blobs.len() + r.sig_bundle.len(),
    );
    out.extend_from_slice(REC_MAGIC);
    out.extend_from_slice(&r.seq.to_be_bytes());
    out.extend_from_slice(&r.chain_hash);
    out.extend_from_slice(&r.market_id.to_be_bytes());
    for f in [&r.batch, &r.quote_blob, &r.side_blobs, &r.sig_bundle] {
        out.extend_from_slice(&len32(f.len()));
        out.extend_from_slice(f);
    }
    let sha = Sha256::digest(&out);
    out.extend_from_slice(&sha);
    Ok(out)
}

/// A bounded reader: every take is checked against what is left, never past the end.
struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(
            n <= self.b.len(),
            "truncated: need {n}, have {}",
            self.b.len()
        );
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn blob(&mut self, cap: usize, what: &str) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        ensure!(n <= cap, "{what} length {n} over its cap {cap}");
        Ok(self.take(n)?.to_vec())
    }
}

/// Split off and check the trailing SHA-256; returns the covered body.
fn checked_body<'a>(bytes: &'a [u8], magic: &[u8; 4]) -> Result<&'a [u8]> {
    ensure!(bytes.len() >= 4 + 32, "file too short");
    let (body, sha) = bytes.split_at(bytes.len() - 32);
    ensure!(Sha256::digest(body).as_slice() == sha, "checksum mismatch");
    ensure!(&body[..4] == magic, "bad magic");
    Ok(&body[4..])
}

pub fn decode_record(bytes: &[u8]) -> Result<LogRecord> {
    ensure!(
        bytes.len() <= MAX_REC_FILE,
        "record file is {} bytes",
        bytes.len()
    );
    let mut c = Cursor {
        b: checked_body(bytes, REC_MAGIC)?,
    };
    let seq = c.u64()?;
    let chain_hash: [u8; 32] = c.take(32)?.try_into().expect("32 bytes");
    let market_id = c.u32()?;
    let r = LogRecord {
        seq,
        chain_hash,
        market_id,
        batch: c.blob(MAX_BATCH_LEN, "batch")?,
        quote_blob: c.blob(MAX_QUOTE_BLOB_LEN, "quote blob")?,
        side_blobs: c.blob(MAX_SIDE_BLOBS_LEN, "side blobs")?,
        sig_bundle: c.blob(MAX_SIG_BUNDLE_LEN, "sig bundle")?,
    };
    ensure!(c.b.is_empty(), "{} trailing bytes", c.b.len());
    Ok(r)
}

fn encode_cert(seq: u64, cert: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        cert.len() <= MAX_CERT_LEN,
        "certificate is {} bytes",
        cert.len()
    );
    let mut out = Vec::with_capacity(CERT_FIXED + cert.len());
    out.extend_from_slice(CERT_MAGIC);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&len32(cert.len()));
    out.extend_from_slice(cert);
    let sha = Sha256::digest(&out);
    out.extend_from_slice(&sha);
    Ok(out)
}

/// Largest certificate file the store writes or reads.
pub const MAX_CERT_FILE: usize = CERT_FIXED + MAX_CERT_LEN;

/// Decode a certificate file (e.g. one fetched from a peer for catch-up). Strict, checksummed.
pub fn decode_cert(bytes: &[u8], want_seq: u64) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= CERT_FIXED + MAX_CERT_LEN,
        "certificate file is {} bytes",
        bytes.len()
    );
    let mut c = Cursor {
        b: checked_body(bytes, CERT_MAGIC)?,
    };
    let seq = c.u64()?;
    ensure!(
        seq == want_seq,
        "certificate file holds seq {seq}, expected {want_seq}"
    );
    let cert = c.blob(MAX_CERT_LEN, "certificate")?;
    ensure!(c.b.is_empty(), "{} trailing bytes", c.b.len());
    Ok(cert)
}

/// What a `put` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// Written and durable.
    New,
    /// The identical record was already stored.
    AlreadyStored,
}

/// Proof that a record is durably stored. Only [`LogStore::put`] makes one (the fields are private to
/// this module), and the transport announces a record or publishes an APPLIED share only from one — so
/// "persist, THEN sign/announce" holds by construction, not by call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Durable {
    seq: u64,
    chain_hash: [u8; 32],
    file_len: usize,
    file_sha256: [u8; 32],
    stored: Stored,
}

impl Durable {
    pub fn seq(&self) -> u64 {
        self.seq
    }
    pub fn chain_hash(&self) -> &[u8; 32] {
        &self.chain_hash
    }
    pub fn file_len(&self) -> usize {
        self.file_len
    }
    pub fn file_sha256(&self) -> &[u8; 32] {
        &self.file_sha256
    }
    pub fn stored(&self) -> Stored {
        self.stored
    }
}

#[derive(Debug)]
pub struct LogStore {
    dir: PathBuf,
    first: Option<u64>,
    last: Option<u64>,
}

impl LogStore {
    /// Open (creating the directory if needed). Leftover temp files from an interrupted write are
    /// removed; the stored seqs must form one contiguous run, or opening fails.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut seqs = Vec::new();
        for e in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
            let p = e?.path();
            match p.extension().and_then(|x| x.to_str()) {
                Some(TMP_EXT) => {
                    fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?
                }
                Some(REC_EXT) => seqs.push(Self::seq_of(&p)?),
                Some(CERT_EXT) => {
                    Self::seq_of(&p)?;
                }
                _ => bail!("unexpected file in the log store: {}", p.display()),
            }
        }
        seqs.sort_unstable();
        for w in seqs.windows(2) {
            ensure!(
                w[1] == w[0] + 1,
                "log store has a gap between seq {} and {}",
                w[0],
                w[1]
            );
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            first: seqs.first().copied(),
            last: seqs.last().copied(),
        })
    }

    fn seq_of(p: &Path) -> Result<u64> {
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .context("non-UTF-8 file name")?;
        ensure!(
            stem.len() == 20 && stem.bytes().all(|b| b.is_ascii_digit()),
            "bad file name {}",
            p.display()
        );
        stem.parse()
            .with_context(|| format!("bad file name {}", p.display()))
    }

    fn path(&self, seq: u64, ext: &str) -> PathBuf {
        self.dir.join(format!("{seq:020}.{ext}"))
    }

    pub fn first_seq(&self) -> Option<u64> {
        self.first
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.last
    }

    /// Write `bytes` at `path` durably: temp sibling, fsync, rename, fsync the directory.
    fn write_durable(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let tmp = path.with_extension(TMP_EXT);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("syncing {}", tmp.display()))?;
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing directory {}", self.dir.display()))?;
        Ok(())
    }

    fn read_bounded(path: &Path, cap: usize) -> Result<Option<Vec<u8>>> {
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        };
        ensure!(
            meta.len() <= cap as u64,
            "{} is {} bytes, over {cap}",
            path.display(),
            meta.len()
        );
        Ok(Some(
            fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        ))
    }

    /// Store a record durably. Returns only after the record is on disk, with the proof of it.
    pub fn put(&mut self, r: &LogRecord) -> Result<Durable> {
        let bytes = encode_record(r)?;
        let proof = |stored| Durable {
            seq: r.seq,
            chain_hash: r.chain_hash,
            file_len: bytes.len(),
            file_sha256: Sha256::digest(&bytes).into(),
            stored,
        };
        if let (Some(first), Some(last)) = (self.first, self.last) {
            if r.seq >= first && r.seq <= last {
                let have = self.get(r.seq)?.context("stored record vanished")?;
                ensure!(
                    have == *r,
                    "a different record is already stored at seq {}",
                    r.seq
                );
                return Ok(proof(Stored::AlreadyStored));
            }
            let next = last.checked_add(1).context("seq overflow")?;
            ensure!(
                r.seq == next,
                "log store is append-only: next is {next}, got {}",
                r.seq
            );
        }
        self.write_durable(&self.path(r.seq, REC_EXT), &bytes)?;
        self.first.get_or_insert(r.seq);
        self.last = Some(r.seq);
        Ok(proof(Stored::New))
    }

    pub fn get(&self, seq: u64) -> Result<Option<LogRecord>> {
        let Some(bytes) = Self::read_bounded(&self.path(seq, REC_EXT), MAX_REC_FILE)? else {
            return Ok(None);
        };
        let r = decode_record(&bytes).with_context(|| format!("record {seq}"))?;
        ensure!(r.seq == seq, "file for seq {seq} holds seq {}", r.seq);
        Ok(Some(r))
    }

    /// Store the quorum certificate for a stored record. Write-once: the first certificate is kept
    /// (any valid one finalises the batch); returns whether this call wrote it.
    pub fn put_cert(&mut self, seq: u64, cert: &[u8]) -> Result<bool> {
        ensure!(
            self.first.is_some_and(|f| seq >= f) && self.last.is_some_and(|l| seq <= l),
            "no stored record at seq {seq} for a certificate"
        );
        let bytes = encode_cert(seq, cert)?;
        let path = self.path(seq, CERT_EXT);
        if self.get_cert(seq)?.is_some() {
            return Ok(false);
        }
        self.write_durable(&path, &bytes)?;
        Ok(true)
    }

    pub fn get_cert(&self, seq: u64) -> Result<Option<Vec<u8>>> {
        let Some(bytes) = Self::read_bounded(&self.path(seq, CERT_EXT), CERT_FIXED + MAX_CERT_LEN)?
        else {
            return Ok(None);
        };
        Ok(Some(
            decode_cert(&bytes, seq).with_context(|| format!("certificate {seq}"))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64) -> LogRecord {
        LogRecord {
            seq,
            chain_hash: [seq as u8; 32],
            market_id: 0,
            batch: vec![1, 2, 3, seq as u8],
            quote_blob: vec![9; 40],
            side_blobs: vec![],
            sig_bundle: vec![7; 72],
        }
    }

    #[test]
    fn records_round_trip_and_survive_reopen() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        assert_eq!(s.last_seq(), None);
        for q in 5..=7 {
            assert_eq!(s.put(&rec(q)).unwrap().stored(), Stored::New);
        }
        let s = LogStore::open(d.path()).unwrap();
        assert_eq!((s.first_seq(), s.last_seq()), (Some(5), Some(7)));
        assert_eq!(s.get(6).unwrap().unwrap(), rec(6));
        assert_eq!(s.get(8).unwrap(), None);
    }

    #[test]
    fn append_only_and_a_conflicting_record_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        s.put(&rec(1)).unwrap();
        s.put(&rec(2)).unwrap();
        assert!(s.put(&rec(4)).is_err(), "a gap is refused");
        assert_eq!(
            s.put(&rec(1)).unwrap().stored(),
            Stored::AlreadyStored,
            "identical is a no-op"
        );
        let mut other = rec(2);
        other.batch.push(0xff);
        let e = s.put(&other).unwrap_err().to_string();
        assert!(e.contains("different record"), "{e}");
        assert_eq!(
            s.get(2).unwrap().unwrap(),
            rec(2),
            "the first record is kept"
        );
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_record() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        s.put(&rec(1)).unwrap();
        let p = d.path().join(format!("{:020}.rec", 1));
        let mut b = fs::read(&p).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 1;
        fs::write(&p, &b).unwrap();
        assert!(s.get(1).is_err());
        // A file that names one seq but holds another.
        let good = encode_record(&rec(3)).unwrap();
        fs::write(&p, &good).unwrap();
        assert!(s.get(1).unwrap_err().to_string().contains("holds seq 3"));
    }

    #[test]
    fn decoding_is_strict() {
        let good = encode_record(&rec(1)).unwrap();
        assert_eq!(decode_record(&good).unwrap(), rec(1));
        // A payload byte flipped (the last byte of the sig bundle, just before the checksum): only the
        // checksum can see this.
        let mut flipped = good.clone();
        let at = flipped.len() - 33;
        flipped[at] ^= 1;
        assert!(decode_record(&flipped)
            .unwrap_err()
            .to_string()
            .contains("checksum"));
        // A well-formed certificate file is not a record: the magic differs, the checksum is valid.
        let cert = encode_cert(1, &good[..10]).unwrap();
        assert!(decode_record(&cert)
            .unwrap_err()
            .to_string()
            .contains("magic"));
        let mut remagic = good[..good.len() - 32].to_vec();
        remagic[..4].copy_from_slice(b"PLR2");
        let sha = Sha256::digest(&remagic);
        remagic.extend_from_slice(&sha);
        assert!(decode_record(&remagic)
            .unwrap_err()
            .to_string()
            .contains("magic"));
        assert!(decode_record(&good[..good.len() - 1]).is_err(), "truncated");
        let mut extra = good[..good.len() - 32].to_vec();
        extra.push(0);
        let sha = Sha256::digest(&extra);
        extra.extend_from_slice(&sha);
        assert!(decode_record(&extra)
            .unwrap_err()
            .to_string()
            .contains("trailing"));
        // A length field that claims more than its cap, with a valid checksum.
        let mut big = good[..REC_FIXED - 32 - 16].to_vec();
        big.extend_from_slice(&len32(MAX_BATCH_LEN + 1));
        let sha = Sha256::digest(&big);
        big.extend_from_slice(&sha);
        assert!(decode_record(&big)
            .unwrap_err()
            .to_string()
            .contains("over its cap"));
        let mut wrong = rec(1);
        wrong.side_blobs = vec![0; MAX_SIDE_BLOBS_LEN + 1];
        assert!(
            encode_record(&wrong).is_err(),
            "an over-cap field is not stored"
        );
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        s.put(&rec(1)).unwrap();
        let p = d.path().join(format!("{:020}.rec", 1));
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(MAX_REC_FILE as u64 + 1).unwrap();
        assert!(s.get(1).unwrap_err().to_string().contains("over"));
    }

    #[test]
    fn opening_removes_temp_files_and_refuses_gaps_and_strangers() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        s.put(&rec(1)).unwrap();
        s.put(&rec(2)).unwrap();
        let tmp = d.path().join(format!("{:020}.tmp", 3));
        fs::write(&tmp, b"half").unwrap();
        let s = LogStore::open(d.path()).unwrap();
        assert!(!tmp.exists());
        assert_eq!(s.last_seq(), Some(2));
        fs::remove_file(d.path().join(format!("{:020}.rec", 1))).unwrap();
        fs::write(
            d.path().join(format!("{:020}.rec", 4)),
            encode_record(&rec(4)).unwrap(),
        )
        .unwrap();
        assert!(LogStore::open(d.path())
            .unwrap_err()
            .to_string()
            .contains("gap"));
        let d2 = tempfile::tempdir().unwrap();
        fs::write(d2.path().join("notes.txt"), b"x").unwrap();
        assert!(LogStore::open(d2.path()).is_err());
    }

    #[test]
    fn certificates_are_write_once_and_only_for_stored_records() {
        let d = tempfile::tempdir().unwrap();
        let mut s = LogStore::open(d.path()).unwrap();
        assert!(s.put_cert(1, &[1]).is_err(), "no record yet");
        s.put(&rec(1)).unwrap();
        assert!(s.put_cert(2, &[1]).is_err(), "past the last record");
        assert!(s.put_cert(1, &[0xaa; 10]).unwrap());
        assert!(
            !s.put_cert(1, &[0xbb; 10]).unwrap(),
            "the first certificate is kept"
        );
        let s = LogStore::open(d.path()).unwrap();
        assert_eq!(s.get_cert(1).unwrap().unwrap(), vec![0xaa; 10]);
        assert!(s.get_cert(2).unwrap().is_none());
        let p = d.path().join(format!("{:020}.cert", 1));
        fs::write(&p, encode_cert(5, &[1]).unwrap()).unwrap();
        assert!(
            s.get_cert(1).is_err(),
            "a certificate file for another seq is refused"
        );
    }
}
