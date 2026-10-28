//! #131 AC-BASE-2" P2-d — orchestrator SPV proof builder.
//!
//! Ports the enclave `xrpl_spv` wire formats to the host producer side: serialize the
//! 118-byte XRPL ledger header, convert rippled's `getProofPath` wire nodes into the
//! enclave's 512-byte inner-node format, and assemble the `XSPV` transport blob that
//! `ecall_perp_reserves_verify_and_baseline` parses (`xrpl_spv_parse_transport`).
//!
//! UNTRUSTED producer: the enclave re-derives `ledger_hash`, re-verifies the validator
//! quorum, and re-hashes every SHAMap node against the validator-signed `account_hash`,
//! so a bad blob here can only FAIL the ecall, never forge a balance. This module just
//! shapes bytes; the golden test proves the shaping matches a real ledger.

#![allow(dead_code)] // consumed by the P2-d ceremony/fetch wiring, landing next.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};

pub const XSPV_MAGIC: [u8; 4] = *b"XSPV";
pub const HEADER_LEN: usize = 118;
const WIRE_ACCOUNT_STATE: u8 = 1; // leaf
const WIRE_INNER: u8 = 2; // full 16-branch inner
const WIRE_COMPRESSED_INNER: u8 = 3; // sparse inner: {hash,branch}*

const HP_LEDGER: [u8; 4] = [0x4C, 0x57, 0x52, 0x00]; // 'LWR\0'
const HP_INNER: [u8; 4] = [0x4D, 0x49, 0x4E, 0x00]; // 'MIN\0'
const HP_LEAF: [u8; 4] = [0x4D, 0x4C, 0x4E, 0x00]; // 'MLN\0'

fn sha512half(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    let full = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&full[..32]);
    out
}

fn hex32(v: &serde_json::Value, k: &str) -> Result<[u8; 32]> {
    let s = v[k].as_str().with_context(|| format!("{k} not a string"))?;
    let b = hex::decode(s).with_context(|| format!("{k} not hex"))?;
    if b.len() != 32 {
        bail!("{k} must be 32 bytes, got {}", b.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    Ok(out)
}

/// `ledger_index` in a `ledger` RPC result may be a string or a number.
fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Serialize the canonical 118-byte XRPL ledger header from a `ledger` RPC result's
/// `ledger` object: seq(u32) ‖ drops(u64) ‖ parent_hash(32) ‖ transaction_hash(32) ‖
/// account_hash(32) ‖ parent_close_time(u32) ‖ close_time(u32) ‖
/// close_time_resolution(u8) ‖ close_flags(u8). All big-endian (matches rippled + the
/// enclave `xrpl_spv_ledger_hash`).
pub fn serialize_ledger_header(ledger: &serde_json::Value) -> Result<[u8; HEADER_LEN]> {
    // Every field below is narrowed with try_from, not `as`. This JSON comes from a
    // rippled node we do not trust; a value past the field width means the response is
    // not what we think it is, and truncating it would silently serialise a DIFFERENT
    // header than the one the validators signed — which then fails quorum for a reason
    // no log line explains. Fail here, where the cause is visible.
    let seq = u32::try_from(as_u64(&ledger["ledger_index"]).context("ledger_index")?)
        .context("ledger_index does not fit the header's u32 sequence field")?;
    let drops: u64 = ledger["total_coins"]
        .as_str()
        .context("total_coins")?
        .parse()
        .context("total_coins parse")?;
    let parent = hex32(ledger, "parent_hash")?;
    let txh = hex32(ledger, "transaction_hash")?;
    let acct = hex32(ledger, "account_hash")?;
    let pct = u32::try_from(as_u64(&ledger["parent_close_time"]).context("parent_close_time")?)
        .context("parent_close_time does not fit the header's u32 field")?;
    let ct = u32::try_from(as_u64(&ledger["close_time"]).context("close_time")?)
        .context("close_time does not fit the header's u32 field")?;
    let ctr =
        u8::try_from(as_u64(&ledger["close_time_resolution"]).context("close_time_resolution")?)
            .context("close_time_resolution does not fit the header's u8 field")?;
    let cf = u8::try_from(as_u64(&ledger["close_flags"]).context("close_flags")?)
        .context("close_flags does not fit the header's u8 field")?;

    let mut out = [0u8; HEADER_LEN];
    let mut p = 0;
    let mut put = |b: &[u8], p: &mut usize| {
        out[*p..*p + b.len()].copy_from_slice(b);
        *p += b.len();
    };
    put(&seq.to_be_bytes(), &mut p);
    put(&drops.to_be_bytes(), &mut p);
    put(&parent, &mut p);
    put(&txh, &mut p);
    put(&acct, &mut p);
    put(&pct.to_be_bytes(), &mut p);
    put(&ct.to_be_bytes(), &mut p);
    put(&[ctr], &mut p);
    put(&[cf], &mut p);
    debug_assert_eq!(p, HEADER_LEN);
    Ok(out)
}

/// SHA-512Half('LWR\0' ‖ header) — the ledger_hash (for producer-side sanity checks).
pub fn ledger_hash(header: &[u8; HEADER_LEN]) -> [u8; 32] {
    sha512half(&[&HP_LEDGER, header])
}

/// A reserves proof shaped for the XSPV blob (one AccountRoot / RippleState).
pub struct XspvProof {
    pub kind: u8, // 0 = AccountRoot (XRP), 1 = RippleState (RLUSD)
    pub leaf_index: [u8; 32],
    pub leaf_data: Vec<u8>,
    pub inner_root_to_leaf: Vec<[u8; 512]>,
}

/// Expand a rippled wire inner node into the enclave's flat 512-byte (16×32) form,
/// zero-filling absent branches for the compressed form.
fn wire_inner_to_512(wire: &[u8]) -> Result<[u8; 512]> {
    if wire.is_empty() {
        bail!("empty inner wire node");
    }
    let tag = wire[wire.len() - 1];
    let body = &wire[..wire.len() - 1];
    let mut out = [0u8; 512];
    match tag {
        WIRE_INNER => {
            if body.len() != 512 {
                bail!("full inner must be 512+1 bytes, got {}", wire.len());
            }
            out.copy_from_slice(body);
        }
        WIRE_COMPRESSED_INNER => {
            if body.len() % 33 != 0 {
                bail!(
                    "compressed inner body must be a multiple of 33, got {}",
                    body.len()
                );
            }
            for chunk in body.chunks(33) {
                let branch = chunk[32] as usize;
                if branch >= 16 {
                    bail!("compressed inner branch {branch} out of range");
                }
                out[branch * 32..branch * 32 + 32].copy_from_slice(&chunk[..32]);
            }
        }
        _ => bail!("not an inner wire node (tag {tag})"),
    }
    Ok(out)
}

/// Split a leaf (account-state) wire node into (item_data, key). Layout:
/// item ‖ key(32) ‖ tag(=1).
fn wire_leaf_split(wire: &[u8]) -> Result<(Vec<u8>, [u8; 32])> {
    if wire.len() < 33 || wire[wire.len() - 1] != WIRE_ACCOUNT_STATE {
        bail!("not an account-state leaf wire node");
    }
    let n = wire.len();
    let mut key = [0u8; 32];
    key.copy_from_slice(&wire[n - 33..n - 1]);
    Ok((wire[..n - 33].to_vec(), key))
}

/// Convert a rippled `getProofPath` result (leaf-first, root-last) into an `XspvProof`
/// with inner nodes ordered root→leaf (the enclave's `verify_state_inclusion` order).
pub fn proof_from_getproofpath(proof_path: &[Vec<u8>], kind: u8) -> Result<XspvProof> {
    if proof_path.len() < 2 {
        bail!("proof_path too short ({})", proof_path.len());
    }
    let (leaf_data, leaf_index) = wire_leaf_split(&proof_path[0]).context("leaf node")?;
    let mut inner_root_to_leaf = Vec::with_capacity(proof_path.len() - 1);
    for w in proof_path[1..].iter().rev() {
        inner_root_to_leaf.push(wire_inner_to_512(w).context("inner node")?);
    }
    Ok(XspvProof {
        kind,
        leaf_index,
        leaf_data,
        inner_root_to_leaf,
    })
}

/// Independent SHAMap verify (host-side sanity check before shipping): the leaf hashes
/// up through the root-to-leaf inner path to `account_hash`. Mirrors the enclave's
/// `xrpl_spv_verify_state_inclusion` exactly.
pub fn verify_inclusion(proof: &XspvProof, account_hash: &[u8; 32]) -> bool {
    let mut running = sha512half(&[&HP_LEAF, &proof.leaf_data, &proof.leaf_index]);
    let depth = proof.inner_root_to_leaf.len();
    for d in (0..depth).rev() {
        let node = &proof.inner_root_to_leaf[d];
        let nib = if d % 2 == 0 {
            proof.leaf_index[d / 2] >> 4
        } else {
            proof.leaf_index[d / 2] & 0x0F
        } as usize;
        if node[nib * 32..nib * 32 + 32] != running {
            return false;
        }
        running = sha512half(&[&HP_INNER, node]);
    }
    &running == account_hash
}

/// Decode a 32-byte hex string into a key/hash.
fn hex32_str(s: &str) -> Result<[u8; 32]> {
    let b = hex::decode(s).context("not hex")?;
    if b.len() != 32 {
        bail!("expected 32 bytes, got {}", b.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    Ok(out)
}

/// Nibble selector into a 32-byte SHAMap key at `depth` (high nibble of byte depth/2 at
/// even depth, low at odd) — the SAME indexing `verify_inclusion` walks.
fn state_nibble(key: &[u8; 32], depth: usize) -> usize {
    if depth % 2 == 0 {
        (key[depth / 2] >> 4) as usize
    } else {
        (key[depth / 2] & 0x0F) as usize
    }
}

/// A node of the ACCOUNT-STATE SHAMap, rebuilt host-side from a full `ledger_data` dump.
/// Mirrors `tx_shamap::Node`, but a leaf carries its serialized object `data` (needed for
/// the XSPV `leaf_data`) and the leaf hash is the account-state hash
/// `SHA512Half('MLN\0' ‖ data ‖ index)`. Inner hashing (`'MIN\0'` over the 512-byte child
/// array) is identical to the tx tree. See `verify_inclusion` for the reading half.
enum StateNode {
    Empty,
    Leaf { key: [u8; 32], data: Vec<u8> },
    Inner(Box<[StateNode; 16]>),
}

impl StateNode {
    fn empty_inner() -> StateNode {
        StateNode::Inner(Box::new(std::array::from_fn(|_| StateNode::Empty)))
    }
    fn hash(&self) -> [u8; 32] {
        match self {
            StateNode::Empty => [0u8; 32],
            StateNode::Leaf { key, data } => sha512half(&[&HP_LEAF, data, key]),
            StateNode::Inner(_) => {
                let flat = self.flat_children().expect("inner node has a flat form");
                sha512half(&[&HP_INNER, &flat])
            }
        }
    }
    fn flat_children(&self) -> Option<[u8; 512]> {
        match self {
            StateNode::Inner(children) => {
                let mut flat = [0u8; 512];
                for (i, c) in children.iter().enumerate() {
                    flat[i * 32..i * 32 + 32].copy_from_slice(&c.hash());
                }
                Some(flat)
            }
            _ => None,
        }
    }
    fn insert(&mut self, key: [u8; 32], data: Vec<u8>, depth: usize) -> Result<()> {
        if depth >= 64 {
            bail!("two state entries share a full 32-byte index — impossible without a SHA-512 collision");
        }
        match self {
            StateNode::Empty => {
                *self = StateNode::Leaf { key, data };
                Ok(())
            }
            StateNode::Leaf {
                key: ek,
                data: ed,
            } => {
                if *ek == key {
                    // Same index twice (paginated-dump overlap) — keep the first, idempotent.
                    return Ok(());
                }
                // SHAMap collapse: two keys meeting here push BOTH down while their nibbles agree.
                let (ek, ed) = (*ek, std::mem::take(ed));
                *self = StateNode::empty_inner();
                self.insert(ek, ed, depth)?;
                self.insert(key, data, depth)
            }
            StateNode::Inner(children) => {
                let nib = state_nibble(&key, depth);
                children[nib].insert(key, data, depth + 1)
            }
        }
    }
}

/// An account-state SHAMap rebuilt from a full `ledger_data` state dump. The proof producer
/// is UNTRUSTED: the enclave re-verifies, and the host `verify_inclusion` self-check against
/// the signed `account_hash` catches any reconstruction error before a blob ships.
pub struct StateShaMap {
    root: StateNode,
}

impl StateShaMap {
    /// Build from EVERY state object `(index, serialized_data)` in one ledger. All are
    /// required — the root hashes the whole map, so a missing/extra object changes it and
    /// the proof stops matching the signed `account_hash`. The root is an inner node for a
    /// non-empty map (rippled convention), matching the tx-tree regression lesson.
    pub fn build(items: Vec<([u8; 32], Vec<u8>)>) -> Result<Self> {
        let mut root = if items.is_empty() {
            StateNode::Empty
        } else {
            StateNode::empty_inner()
        };
        for (key, data) in items {
            root.insert(key, data, 0)?;
        }
        Ok(StateShaMap { root })
    }
    /// Root hash — EQUALS the signed `account_hash` (header offset 76) for a correct rebuild.
    pub fn root_hash(&self) -> [u8; 32] {
        self.root.hash()
    }
    /// Read one AccountRoot's inclusion path, shaped as the `XspvProof` the enclave consumes
    /// (inner nodes root→leaf; `verify_inclusion` is the matching verifier).
    pub fn state_proof(&self, target: &[u8; 32]) -> Result<XspvProof> {
        let mut inner_root_to_leaf = Vec::new();
        let mut node = &self.root;
        let mut depth = 0usize;
        loop {
            match node {
                StateNode::Inner(children) => {
                    inner_root_to_leaf.push(node.flat_children().expect("inner node"));
                    node = &children[state_nibble(target, depth)];
                    depth += 1;
                }
                StateNode::Leaf { key, data } => {
                    if key != target {
                        bail!("state entry not in this ledger (a different index sits at its slot)");
                    }
                    return Ok(XspvProof {
                        kind: 0,
                        leaf_index: *target,
                        leaf_data: data.clone(),
                        inner_root_to_leaf,
                    });
                }
                StateNode::Empty => {
                    bail!("state entry not in this ledger (empty slot at depth {depth})")
                }
            }
        }
    }
}

/// Paginate `ledger_data {binary:true}` to collect EVERY account-state object `(index, data)`
/// at `seq` from a STANDARD node — the input to a host-side state-tree rebuild (no patch,
/// no dedicated node). Uses the `marker` cursor until the state is exhausted.
async fn fetch_state_objects(http_url: &str, seq: u64) -> Result<Vec<([u8; 32], Vec<u8>)>> {
    let mut out: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    let mut marker = serde_json::Value::Null;
    loop {
        let mut params = serde_json::json!({"ledger_index": seq, "binary": true, "limit": 2048});
        if !marker.is_null() {
            params["marker"] = marker.clone();
        }
        let r = http_rpc(http_url, "ledger_data", params)
            .await
            .context("ledger_data fetch")?;
        let state = r["state"]
            .as_array()
            .context("ledger_data response has no state array")?;
        for o in state {
            let idx = hex32_str(o["index"].as_str().context("state object index")?)
                .context("state object index")?;
            let data = hex::decode(o["data"].as_str().context("state object data")?)
                .context("state object data hex")?;
            out.push((idx, data));
        }
        match r.get("marker") {
            Some(m) if !m.is_null() => marker = m.clone(),
            _ => break,
        }
    }
    Ok(out)
}

/// Host-side escrow AccountRoot proof from a STANDARD node's `ledger_data` — NO getProofPath,
/// NO patched node. Fetches the escrow's SHAMap index, rebuilds the full state tree, and
/// reads the inclusion path off it. The caller self-checks the result with `verify_inclusion`
/// against the signed `account_hash` (and the enclave re-verifies independently).
async fn reconstruct_state_proof(http_url: &str, seq: u64, escrow: &str) -> Result<XspvProof> {
    // standard ledger_entry (no `proof` flag) returns the AccountRoot's SHAMap key (`index`).
    let le = http_rpc(
        http_url,
        "ledger_entry",
        serde_json::json!({"account_root": escrow, "ledger_index": seq, "binary": true}),
    )
    .await
    .context("ledger_entry (index) fetch")?;
    let index = hex32_str(
        le["index"]
            .as_str()
            .context("ledger_entry has no index for the escrow AccountRoot")?,
    )?;
    let objects = fetch_state_objects(http_url, seq).await?;
    let map = StateShaMap::build(objects)?;
    map.state_proof(&index)
}

fn push_be16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// Minimal bounds-checked STObject field walker. From `b[pos..]`, returns
/// (type, field, id_start, value_start, next_pos) for the next field, or None on
/// truncation / an unhandled type. Only the types an STValidation carries are handled.
fn st_next(b: &[u8], pos: usize) -> Option<(u16, u16, usize, usize, usize)> {
    let id_start = pos;
    let mut i = pos;
    let h = *b.get(i)?;
    i += 1;
    let mut tc = (h >> 4) as u16;
    let mut fc = (h & 0x0F) as u16;
    if tc == 0 {
        tc = *b.get(i)? as u16;
        i += 1;
    }
    if fc == 0 {
        fc = *b.get(i)? as u16;
        i += 1;
    }
    let vstart;
    let vlen;
    match tc {
        1 => {
            vstart = i;
            vlen = 2;
        }
        2 => {
            vstart = i;
            vlen = 4;
        }
        3 => {
            vstart = i;
            vlen = 8;
        }
        4 => {
            vstart = i;
            vlen = 16;
        }
        5 => {
            vstart = i;
            vlen = 32;
        }
        16 => {
            vstart = i;
            vlen = 1;
        }
        6 => {
            vstart = i;
            vlen = if b.get(i)? & 0x80 != 0 { 48 } else { 8 };
        }
        7 | 8 | 19 => {
            // VL-length-prefixed (1/2/3-byte length)
            let l0 = *b.get(i)? as usize;
            i += 1;
            // The subtractions are guarded by the branch conditions, but written with
            // checked_sub so the guarantee lives in the expression rather than in the
            // `if` above it — a later edit that moves a bound cannot turn this into a
            // silent wrap (release builds do not panic).
            let l = if l0 <= 192 {
                l0
            } else if l0 <= 240 {
                let hi = l0.checked_sub(193)?;
                i += 1;
                193 + (hi << 8) + *b.get(i - 1)? as usize
            } else {
                let a = *b.get(i)? as usize;
                let bb = *b.get(i + 1)? as usize;
                i += 2;
                let hi = l0.checked_sub(241)?;
                12481 + (hi << 16) + (a << 8) + bb
            };
            vstart = i;
            vlen = l;
        }
        _ => return None,
    }
    let next = vstart.checked_add(vlen)?;
    if next > b.len() {
        return None;
    }
    Some((tc, fc, id_start, vstart, next))
}

/// From a raw serialized STValidation (`data` from the validations stream), extract the
/// XSPV validation entry the enclave's `verify_quorum` consumes: the signing pubkey (33),
/// the DER signature, and the vbody = the validation WITHOUT its `sfSignature` field
/// (the signed content: SHA-512Half('VAL\0' ‖ vbody) is what the validator signed).
pub fn validation_entry(data: &[u8]) -> Result<([u8; 33], Vec<u8>, Vec<u8>)> {
    let mut pos = 0;
    let mut pubkey: Option<[u8; 33]> = None;
    let mut sig: Option<Vec<u8>> = None;
    let mut sig_field: Option<(usize, usize)> = None; // (id_start, next) of sfSignature
    while pos < data.len() {
        let (tc, fc, idst, vs, next) = st_next(data, pos).context("STValidation walk")?;
        if tc == 7 && fc == 3 {
            // sfSigningPubKey
            if next - vs != 33 {
                bail!("sfSigningPubKey not 33 bytes");
            }
            let mut pk = [0u8; 33];
            pk.copy_from_slice(&data[vs..next]);
            pubkey = Some(pk);
        } else if tc == 7 && fc == 6 {
            // sfSignature
            sig = Some(data[vs..next].to_vec());
            sig_field = Some((idst, next));
        }
        pos = next;
    }
    let pubkey = pubkey.context("no sfSigningPubKey")?;
    let sig = sig.context("no sfSignature")?;
    let (sf_start, sf_end) = sig_field.unwrap();
    let mut vbody = Vec::with_capacity(data.len() - (sf_end - sf_start));
    vbody.extend_from_slice(&data[..sf_start]);
    vbody.extend_from_slice(&data[sf_end..]);
    Ok((pubkey, sig, vbody))
}

/// Build the XSPV validations section (`val_count`, concatenated entries) from the raw
/// `data` blobs collected for one ledger. Each entry: pubkey33 ‖ sig_len u8 ‖ sig ‖
/// vbody_len u16 ‖ vbody. The enclave dedups + checks each against the pinned UNL.
fn build_validations_section(datas: &[Vec<u8>]) -> Result<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut count: u16 = 0;
    for d in datas {
        let (pk, sig, vbody) = validation_entry(d)?;
        if sig.len() > 72 || vbody.len() > u16::MAX as usize {
            bail!("validation sig/vbody too large");
        }
        out.extend_from_slice(&pk);
        out.push(sig.len() as u8);
        out.extend_from_slice(&sig);
        push_be16(&mut out, vbody.len() as u16);
        out.extend_from_slice(&vbody);
        count += 1;
    }
    Ok((count, out))
}

/// Assemble the `XSPV` transport blob the enclave parses. `validations` is the
/// concatenated validation entries (pubkey33|sig_len|sig|vbody_len|vbody)*.
pub fn build_xspv_blob(
    header: &[u8; HEADER_LEN],
    validations: &[Vec<u8>],
    proofs: &[XspvProof],
) -> Result<Vec<u8>> {
    let (val_count, vals) = build_validations_section(validations)?;
    build_xspv_blob_framed(header, val_count, &vals, proofs)
}

/// The same bytes from an ALREADY-FRAMED section plus a separately-supplied count.
///
/// Private, and it is the shape of this signature that caused a live defect rather than
/// anything in its body: taking `&[u8]` and a `u16` let two of the three callers pass
/// the collector's RAW `data` blobs with a count of however many there were. The bytes
/// were well-formed as a concatenation and meaningless as a validations section, so the
/// enclave refused every one of them with "the bundle does not parse" — the attested
/// clock refused 100% of ledgers live on 2026-09-27, and the deposit path would have
/// done the same the moment it was switched on. Only the reserves path framed properly,
/// and it is the only one that had ever run.
///
/// The public entry points take `&[Vec<u8>]` and derive the count from what they framed,
/// so the count cannot disagree with the bytes and the framing cannot be skipped. This
/// one stays for the cross-language vectors, whose filler entries are not real
/// validations and so cannot be produced by framing anything.
fn build_xspv_blob_framed(
    header: &[u8; HEADER_LEN],
    val_count: u16,
    validations: &[u8],
    proofs: &[XspvProof],
) -> Result<Vec<u8>> {
    // Same defect class as build_xdep_blob — see the note there. `proofs.len() as u8`
    // and `leaf_data.len() as u16` would each silently serialise a wrong value rather
    // than fail.
    let proof_count = u8::try_from(proofs.len()).context("too many proofs for a u8 count")?;
    let mut b = Vec::new();
    b.extend_from_slice(&XSPV_MAGIC);
    b.push(1); // version
    push_be16(&mut b, HEADER_LEN as u16); // const 118, cannot narrow
    b.extend_from_slice(header);
    push_be16(&mut b, val_count);
    b.extend_from_slice(validations);
    b.push(proof_count);
    for pr in proofs {
        let leaf_len =
            u16::try_from(pr.leaf_data.len()).context("ledger entry too large for a u16 length")?;
        let depth = u8::try_from(pr.inner_root_to_leaf.len())
            .context("inclusion path deeper than a u8 can express")?;
        b.push(pr.kind);
        b.extend_from_slice(&pr.leaf_index);
        push_be16(&mut b, leaf_len);
        b.extend_from_slice(&pr.leaf_data);
        b.push(depth);
        for node in &pr.inner_root_to_leaf {
            b.extend_from_slice(node);
        }
    }
    Ok(b)
}

/// #131 P3 — the deposit transport (`XDEP`).
///
/// Same attested-ledger prefix as [`build_xspv_blob`] — the header the validators
/// signed and their signatures over it — then one transaction, its metadata, and the
/// transaction-tree inclusion path from [`crate::tx_shamap`].
///
/// Deliberately ONE deposit per blob. Batching would make a partial failure ambiguous
/// (which of N was refused, and did the others credit?), and a deposit credit is not a
/// place to be ambiguous.
///
/// Note what is NOT in the blob: no tx-ID, no leaf index, no amount, no sender. The
/// enclave derives the SHAMap key from the transaction bytes itself, so the host cannot
/// say which transaction a proof is about — only which bytes it ships.
///
/// Lengths are u32, unlike XSPV's u16 leaf_len: that one sizes a ledger entry, which
/// cannot approach 64 KiB, whereas real transaction metadata on a busy ledger can.
pub fn build_xdep_blob(
    header: &[u8; HEADER_LEN],
    validations: &[Vec<u8>],
    tx_blob: &[u8],
    meta: &[u8],
    inner_root_to_leaf: &[[u8; 512]],
) -> Result<Vec<u8>> {
    let (val_count, vals) = build_validations_section(validations)?;
    build_xdep_blob_framed(header, val_count, &vals, tx_blob, meta, inner_root_to_leaf)
}

/// Framed-section form — see [`build_xspv_blob_framed`] for why it is private.
fn build_xdep_blob_framed(
    header: &[u8; HEADER_LEN],
    val_count: u16,
    validations: &[u8],
    tx_blob: &[u8],
    meta: &[u8],
    inner_root_to_leaf: &[[u8; 512]],
) -> Result<Vec<u8>> {
    // DEFECT FIXED, not merely hardening (audit-claude, 2026-09-17): the original
    // `inner_root_to_leaf.len() as u8` SERIALISED A WRONG WIRE VALUE — a 256-deep path
    // became a 0 depth byte, emitting a malformed blob with no error at the producer.
    // The enclave refuses such a blob, so nothing unsafe reaches the TCB, but "the
    // receiver catches it" does not make a producer that silently writes the wrong
    // number correct. Recorded as a defect so the trail shows one, not a style note.
    let tx_len = u32::try_from(tx_blob.len()).context("transaction too large for a u32 length")?;
    let meta_len = u32::try_from(meta.len()).context("metadata too large for a u32 length")?;
    let depth = u8::try_from(inner_root_to_leaf.len())
        .context("inclusion path deeper than a u8 can express")?;

    let mut b = Vec::new();
    b.extend_from_slice(b"XDEP");
    b.push(1); // version
    push_be16(&mut b, HEADER_LEN as u16); // const 118, cannot narrow
    b.extend_from_slice(header);
    push_be16(&mut b, val_count);
    b.extend_from_slice(validations);
    b.extend_from_slice(&tx_len.to_be_bytes());
    b.extend_from_slice(tx_blob);
    b.extend_from_slice(&meta_len.to_be_bytes());
    b.extend_from_slice(meta);
    b.push(depth);
    for node in inner_root_to_leaf {
        b.extend_from_slice(node);
    }
    Ok(b)
}

/// Trusted-price — the clock transport (`XCLK`).
///
/// The attested prefix and **nothing after it**: magic, version, the 118-byte header the
/// validators signed, and their signatures over it.
///
/// A third sibling format rather than `XSPV` with `proof_count = 0`, and the distinction is
/// load-bearing on the receiving side: the enclave's parser refuses trailing bytes, so a
/// custody proof cannot ride in on a call whose only job is learning what time it is. This
/// producer therefore appends nothing, and there is no parameter through which it could.
///
/// See `xrpl_spv_parse_clock_transport` in the enclave for the reading half.
pub fn build_xclk_blob(header: &[u8; HEADER_LEN], validations: &[Vec<u8>]) -> Result<Vec<u8>> {
    let (val_count, vals) = build_validations_section(validations)?;
    build_xclk_blob_framed(header, val_count, &vals)
}

/// Framed-section form — see [`build_xspv_blob_framed`] for why it is private.
fn build_xclk_blob_framed(
    header: &[u8; HEADER_LEN],
    val_count: u16,
    validations: &[u8],
) -> Result<Vec<u8>> {
    let mut b = Vec::with_capacity(9 + HEADER_LEN + validations.len());
    b.extend_from_slice(b"XCLK");
    b.push(1); // version
    push_be16(&mut b, HEADER_LEN as u16); // const 118, cannot narrow
    b.extend_from_slice(header);
    push_be16(&mut b, val_count);
    b.extend_from_slice(validations);
    Ok(b)
}

/// A REAL full validation `data` blob, testnet ledger 1124A120… (signing key 027F285B…),
/// whose signature is checked against its own reconstructed vbody by
/// `validation_entry_vbody_verifies`. Crate-visible so the XCLK and XDEP vectors can be
/// generated by FRAMING it, instead of carrying filler entries that are already in wire
/// form — a fixture shaped like the output cannot catch a producer that never framed.
#[cfg(test)]
pub(crate) const REAL_VALIDATION_HEX: &str = "228000000126013949042932300C863A2D270CF98511C639511124A12059FA1D708D4CB718767B5C2C1971E6D644F7731BD8D60B8666538532501700000000000000000000000000000000000000000000000000000000000000005019696E5F5288E3EE08676023DB69CFA2E333692B0F7E97D87553CB0312D9C5C0767321027F285B8BB33F0E8B025BF955C29A7CFA8A0995831EE4AD93A9BD572A7C8EEDCD76463044022018975AFA77A0F6D273D8E36F5FDCB4876491FF3CD2719CE5C30D3A464F6CD33B0220383E44ECCAAB37930DBE272AA2586B1CBD287C872E31FEF587D5218142E44554";

#[cfg(test)]
mod xclk_producer_tests {
    use super::*;

    fn fixture() -> ([u8; HEADER_LEN], Vec<Vec<u8>>) {
        let mut h = [0u8; HEADER_LEN];
        for (i, b) in h.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7);
        }
        (h, vec![vec![0xAA; 40], vec![0xBB; 33]])
    }

    #[test]
    fn the_blob_is_the_attested_prefix_and_nothing_after_it() {
        let (h, vals) = fixture();
        let flat: Vec<u8> = vals.iter().flatten().copied().collect();
        let blob = build_xclk_blob_framed(&h, vals.len() as u16, &flat).unwrap();

        assert_eq!(&blob[0..4], b"XCLK", "magic");
        assert_eq!(blob[4], 1, "version");
        assert_eq!(u16::from_be_bytes([blob[5], blob[6]]), HEADER_LEN as u16);
        assert_eq!(&blob[7..7 + HEADER_LEN], &h[..], "header verbatim");
        let vc_at = 7 + HEADER_LEN;
        assert_eq!(u16::from_be_bytes([blob[vc_at], blob[vc_at + 1]]), 2);
        assert_eq!(&blob[vc_at + 2..], &flat[..], "validations verbatim");

        // The property the format exists for: the length is fully accounted for, so
        // nothing can be appended without the enclave's parser seeing trailing bytes.
        assert_eq!(
            blob.len(),
            9 + HEADER_LEN + flat.len(),
            "no slack in the blob"
        );
    }

    #[test]
    fn it_is_not_the_deposit_transport() {
        // A reader that only checked "starts with X" would take one for the other; the
        // magic is the thing keeping a custody proof off the clock path.
        let (h, vals) = fixture();
        let flat: Vec<u8> = vals.iter().flatten().copied().collect();
        let clk = build_xclk_blob_framed(&h, vals.len() as u16, &flat).unwrap();
        let dep =
            build_xdep_blob_framed(&h, vals.len() as u16, &flat, &[1, 2, 3], &[4, 5], &[]).unwrap();
        assert_ne!(&clk[0..4], &dep[0..4]);
        assert!(
            dep.len() > clk.len(),
            "the deposit transport carries payload the clock transport has no room for"
        );
    }

    /// CROSS-LANGUAGE VECTOR. The same hex is parsed by a C++ test in the enclave repo
    /// (`test_xrpl_spv.cpp`, "the producer's bytes"), so the two sides are pinned to one
    /// byte string rather than to two independent readings of a comment. A layout both
    /// sides agreed on because both were written from the same paragraph is exactly the
    /// co-deluded fixture this guards against: if either changes, one of the two tests
    /// goes red instead of both staying green.
    ///
    /// BOTH HALVES ARE REAL. The header is from our own rippled 3.1.3, testnet ledger
    /// 21038626, close_time 843653980 (Ripple epoch) = 2026-09-25T12:19:40Z. The single
    /// validation entry is produced by FRAMING a real full validation
    /// ([`REAL_VALIDATION_HEX`]) through the shipping code path.
    ///
    /// It used to carry structural fillers already in wire form, and that made the producer
    /// test below tautological: it sliced those bytes back out of this constant, fed them to
    /// the builder and asserted the builder returned them — proving only that the builder
    /// copies bytes. It could not see a producer that never framed at all, which is exactly
    /// the defect that refused 100% of ledgers live on 2026-09-27
    /// (RESP-wire-format-seam-class-ruling-2026-09-27 Q3).
    ///
    /// Regenerate with `cargo test --locked emit_xclk_vector -- --ignored --nocapture`.
    /// The signature is real but is NOT expected to satisfy quorum here: this vector pins
    /// the TRANSPORT, and quorum verification has its own real-manifest tests.
    const XCLK_VECTOR: &str = "58434C4B010076014106220163455E948617B432C5B74631AB7C120172A918C5A23CA5B32DE27202D3EA62E3A795500EE4A5EB3F6570C1F9CEA1889D00F8C4462F31820A87A723198F38EF301AE8B243E3FF074286408431A7AA5268500679DF31767F7832DC9775E4E15CB2566699B7583AF6324923543249235C0A000001027F285B8BB33F0E8B025BF955C29A7CFA8A0995831EE4AD93A9BD572A7C8EEDCD463044022018975AFA77A0F6D273D8E36F5FDCB4876491FF3CD2719CE5C30D3A464F6CD33B0220383E44ECCAAB37930DBE272AA2586B1CBD287C872E31FEF587D5218142E4455400A0228000000126013949042932300C863A2D270CF98511C639511124A12059FA1D708D4CB718767B5C2C1971E6D644F7731BD8D60B8666538532501700000000000000000000000000000000000000000000000000000000000000005019696E5F5288E3EE08676023DB69CFA2E333692B0F7E97D87553CB0312D9C5C0767321027F285B8BB33F0E8B025BF955C29A7CFA8A0995831EE4AD93A9BD572A7C8EEDCD";

    /// Emit the XCLK vector from a framed REAL validation. Ignored by default.
    ///
    ///   cargo test --locked emit_xclk_vector -- --ignored --nocapture
    #[test]
    #[ignore = "vector generator, not an assertion"]
    fn emit_xclk_vector() {
        let hdr_hex = &XCLK_VECTOR[14..14 + HEADER_LEN * 2];
        let mut header = [0u8; HEADER_LEN];
        for (i, b) in header.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hdr_hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let data = hex::decode(REAL_VALIDATION_HEX).unwrap();
        let blob = build_xclk_blob(&header, std::slice::from_ref(&data)).unwrap();
        println!("{}", hex::encode_upper(&blob));
    }

    #[test]
    fn the_producer_emits_the_vector_the_enclave_parser_is_tested_against() {
        // The header comes from the vector; the VALIDATION does not. It is framed here from
        // real validation bytes through the public entry point, so this reproduces the
        // vector only if the framing is still what the enclave reads. Slicing the entry
        // back out of the vector — what this test used to do — could not fail that way.
        let hdr_hex = &XCLK_VECTOR[14..14 + HEADER_LEN * 2];
        let mut header = [0u8; HEADER_LEN];
        for (i, b) in header.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hdr_hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let data = hex::decode(REAL_VALIDATION_HEX).unwrap();
        let produced = build_xclk_blob(&header, std::slice::from_ref(&data)).unwrap();
        assert_eq!(
            hex::encode_upper(&produced),
            XCLK_VECTOR,
            "the producer no longer emits the bytes the enclave test parses"
        );
    }

    #[test]
    fn an_empty_validation_set_still_produces_a_well_formed_blob() {
        // Refusing is the ENCLAVE's job (it fails quorum), not the producer's: a producer
        // that silently dropped the call would hide a node that is not receiving
        // validations behind "the clock just isn't advancing".
        let (h, _) = fixture();
        let blob = build_xclk_blob(&h, &[]).unwrap();
        assert_eq!(blob.len(), 9 + HEADER_LEN);
        assert_eq!(
            u16::from_be_bytes([blob[7 + HEADER_LEN], blob[8 + HEADER_LEN]]),
            0
        );
    }
}

#[cfg(test)]
mod xspv_producer_tests {
    use super::*;
    use crate::spv_proof_vector as v;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn real_proof() -> XspvProof {
        let mut leaf_index = [0u8; 32];
        leaf_index.copy_from_slice(&unhex(v::ESCROW_KEYLET));
        let inner: Vec<[u8; 512]> = v::INNER_ROOT_TO_LEAF
            .iter()
            .map(|h| {
                let mut n = [0u8; 512];
                n.copy_from_slice(&unhex(h));
                n
            })
            .collect();
        XspvProof {
            kind: 0,
            leaf_index,
            leaf_data: unhex(v::ESCROW_LEAF),
            inner_root_to_leaf: inner,
        }
    }

    /// The gap this closes: `build_xspv_blob` assembles the blob for the RESERVES path —
    /// the one live in production since 2026-09-08 — and had no test at all. The enclave
    /// suite tests its parser against a blob the enclave's own test builds by hand in
    /// C++, so the Rust producer and the C++ consumer had never met. Both sides are
    /// mine, written from one reading of the format; it works in production, which is
    /// real evidence, but nothing would have caught a drift.
    ///
    /// Found by taking the auditor's question about the deposit module seriously and
    /// asking it of the others: which module has tests on its edges and none on its
    /// centre?
    #[test]
    fn the_blob_verifies_against_the_root_the_validators_signed() {
        let p = real_proof();
        let mut account_hash = [0u8; 32];
        account_hash.copy_from_slice(&unhex(v::ACCOUNT_HASH));
        assert!(
            verify_inclusion(&p, &account_hash),
            "the real escrow proof must reach the signed account_hash"
        );
    }

    #[test]
    fn the_producer_emits_what_the_parser_expects() {
        // Mirrors the enclave parser's layout expectations byte for byte, so a change to
        // either side that the other does not follow shows up here rather than as a
        // refused ceremony.
        let header = [0x11u8; HEADER_LEN];
        let blob = build_xspv_blob(&header, &[], &[real_proof()]).expect("blob builds");

        assert_eq!(&blob[0..4], b"XSPV", "magic");
        assert_eq!(blob[4], 1, "version");
        assert_eq!(&blob[5..7], &[0, 118], "header length, big-endian");
        assert_eq!(&blob[7..7 + HEADER_LEN], &header[..], "header verbatim");
        let p = 7 + HEADER_LEN;
        assert_eq!(&blob[p..p + 2], &[0, 0], "validation count");
        assert_eq!(blob[p + 2], 1, "proof count");
        assert_eq!(blob[p + 3], 0, "kind = AccountRoot");
        assert_eq!(
            &blob[p + 4..p + 36],
            &unhex(v::ESCROW_KEYLET)[..],
            "leaf index"
        );

        let leaf = unhex(v::ESCROW_LEAF);
        let ll = u16::from_be_bytes([blob[p + 36], blob[p + 37]]) as usize;
        assert_eq!(ll, leaf.len(), "leaf length");
        assert_eq!(&blob[p + 38..p + 38 + ll], &leaf[..], "leaf verbatim");
        assert_eq!(blob[p + 38 + ll], 7, "depth");
        assert_eq!(
            blob.len(),
            p + 39 + ll + 7 * 512,
            "no trailing bytes — the enclave refuses a blob with any"
        );
    }

    /// A depth past what the wire format can express must refuse, not truncate. The
    /// enclave caps depth at 64 anyway, but a producer that silently wrote a wrong byte
    /// would emit a blob that fails inclusion for a reason nothing reports.
    #[test]
    fn an_inexpressible_depth_is_refused() {
        let mut p = real_proof();
        p.inner_root_to_leaf = vec![[0u8; 512]; 300];
        assert!(build_xspv_blob(&[0u8; HEADER_LEN], &[], &[p]).is_err());
    }
}

// ── async fetch (ws validations + HTTP header/proof → XSPV blob) ───────────────
use std::collections::HashMap;
use std::time::Duration;

/// Config for the leader's one-shot SPV proof fetch.
pub struct SpvFetchConfig {
    pub http_url: String, // http://127.0.0.1:5005
    pub ws_url: String,   // ws://127.0.0.1:6006
    pub escrow_account: String,
    pub quorum: usize,     // stop once one ledger has this many full validations
    pub collect_secs: u64, // ws collect window
    /// Prefer the patched node's getProofPath (one tiny inclusion path) over a full
    /// `ledger_data` state dump. Set true for a dedicated/patched proof node (the reserves
    /// baseline against our own xrpld+getProofPath); leave false for a standard public node
    /// that only serves `ledger_data`. Either producer is UNTRUSTED — only the account_hash
    /// match makes a proof good — so this is a performance/availability choice, not trust.
    pub prefer_getproofpath: bool,
}

async fn http_rpc(url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let body = serde_json::json!({"method": method, "params": [params]});
    let resp: serde_json::Value = http.post(url).json(&body).send().await?.json().await?;
    Ok(resp["result"].clone())
}

/// Fetch header + ≥quorum validations + the escrow AccountRoot proof for ONE validated
/// ledger and assemble the XSPV blob the enclave verifies. Host-side sanity-checks the
/// chaining (header→ledger_hash the validations attest, proof→account_hash) before
/// returning — a producer bug fails here loudly rather than shipping an unverifiable blob.
/// Escrow AccountRoot proof from a PATCHED node's getProofPath (one tiny path). RETRY: the
/// node can lag a few seconds behind the attested ledger before it can walk the state map,
/// so `ledger_entry` may lack `proof_path` momentarily.
async fn proof_via_getproofpath(
    cfg: &SpvFetchConfig,
    seq: u64,
    account_hash: &[u8; 32],
) -> Result<XspvProof> {
    let mut le = serde_json::Value::Null;
    for attempt in 0..10u32 {
        let r = http_rpc(
            &cfg.http_url,
            "ledger_entry",
            serde_json::json!({"account_root": cfg.escrow_account, "ledger_index": seq, "binary": true, "proof": true}),
        )
        .await
        .context("ledger_entry proof fetch")?;
        if r.get("proof_path").and_then(|p| p.as_array()).is_some() {
            le = r;
            break;
        }
        tracing::debug!(attempt, seq, "#131 SPV: no proof_path yet — retrying");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let path: Vec<Vec<u8>> = le["proof_path"]
        .as_array()
        .context("node served no getProofPath (not a patched node)")?
        .iter()
        .map(|v| hex::decode(v.as_str().unwrap_or_default()).context("proof node hex"))
        .collect::<Result<_>>()?;
    let p = proof_from_getproofpath(&path, 0)?;
    if !verify_inclusion(&p, account_hash) {
        bail!("getProofPath proof does not hash to the ledger's account_hash");
    }
    Ok(p)
}

/// Escrow AccountRoot proof rebuilt host-side from a STANDARD node's full `ledger_data`
/// state dump (no patch needed). The rebuilt path is self-checked against `account_hash`,
/// so a reconstruction bug fails here rather than shipping an unverifiable blob.
async fn proof_via_ledger_data(
    cfg: &SpvFetchConfig,
    seq: u64,
    account_hash: &[u8; 32],
) -> Result<XspvProof> {
    let p = reconstruct_state_proof(&cfg.http_url, seq, &cfg.escrow_account).await?;
    if !verify_inclusion(&p, account_hash) {
        bail!("ledger_data reconstruction did not hash to the ledger's account_hash");
    }
    Ok(p)
}

pub async fn fetch_spv_bundle(cfg: &SpvFetchConfig) -> Result<Vec<u8>> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // 1. collect validations by ledger_hash until one ledger reaches quorum.
    let (mut ws, _) = tokio_tungstenite::connect_async(&cfg.ws_url)
        .await
        .context("ws connect")?;
    ws.send(Message::text(
        r#"{"command":"subscribe","streams":["validations"]}"#,
    ))
    .await
    .context("ws subscribe")?;
    let mut by_ledger: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.collect_secs);
    let (lh_hex, datas) = loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out collecting validations"))?
            .context("ws stream closed")?
            .context("ws frame")?;
        let Message::Text(t) = msg else { continue };
        let j: serde_json::Value = match serde_json::from_str(&t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if j["type"] == "validationReceived" && j["full"] == true {
            if let Some(d) = j["data"].as_str() {
                let lh = j["ledger_hash"].as_str().unwrap_or_default().to_string();
                let entry = by_ledger.entry(lh.clone()).or_default();
                if let Ok(bytes) = hex::decode(d) {
                    entry.push(bytes);
                }
                if entry.len() >= cfg.quorum {
                    break (lh, entry.clone());
                }
            }
        }
    };
    let _ = ws.close(None).await;

    // 2. header for that validated ledger + sanity-check it hashes to lh_hex.
    //
    // RETRY: validations for ledger N reach us a few seconds BEFORE our own node has
    // validated and stored N, so an immediate fetch races and comes back without a
    // `ledger` object. Observed live on the first real ceremony. Poll briefly rather than
    // failing the whole ceremony over a few seconds of lag.
    let mut lr = serde_json::Value::Null;
    for attempt in 0..10u32 {
        let r = http_rpc(
            &cfg.http_url,
            "ledger",
            serde_json::json!({"ledger_hash": lh_hex, "transactions": false}),
        )
        .await
        .context("ledger fetch")?;
        if r.get("ledger").is_some() {
            lr = r;
            break;
        }
        tracing::debug!(
            attempt,
            ledger_hash = %lh_hex,
            "#131 SPV: our node has not stored the attested ledger yet — retrying"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if lr.get("ledger").is_none() {
        bail!(
            "our node never produced ledger {lh_hex} within ~20s — it attested a ledger it \
             does not have stored (is the proof node lagging or pruning?)"
        );
    }
    let ledger = &lr["ledger"];
    let header = serialize_ledger_header(ledger)?;
    if hex::encode_upper(ledger_hash(&header)) != lh_hex.to_uppercase() {
        bail!("serialized header does not hash to the validated ledger_hash");
    }
    let seq = as_u64(&ledger["ledger_index"]).context("ledger_index")?;

    // 3. escrow AccountRoot proof — host-side reconstruction from a STANDARD node's
    //    `ledger_data` (node-free, patch-free): rebuild the full state SHAMap and read the
    //    inclusion path off it. The enclave re-verifies independently, and `verify_inclusion`
    //    below self-checks the rebuilt path against the validator-signed `account_hash`, so a
    //    reconstruction bug FAILS here, never forges. If a node will not serve the full state,
    //    fall back to a patched node's getProofPath — both producers are UNTRUSTED; only the
    //    account_hash match makes a proof good.
    let ah = hex32(ledger, "account_hash")?;
    // Two UNTRUSTED producers; only the account_hash match makes a proof good. Prefer
    // getProofPath (one tiny path) for our patched proof node, else rebuild from a full
    // `ledger_data` dump for a standard node. A miss on the preferred producer falls through
    // to the other, so a mis-set flag costs latency, never correctness.
    let proof = if cfg.prefer_getproofpath {
        match proof_via_getproofpath(cfg, seq, &ah).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(seq, err = %format!("{e:#}"), "#131 SPV: getProofPath failed — trying ledger_data");
                proof_via_ledger_data(cfg, seq, &ah).await?
            }
        }
    } else {
        match proof_via_ledger_data(cfg, seq, &ah).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(seq, err = %format!("{e:#}"), "#131 SPV: ledger_data failed — trying getProofPath");
                proof_via_getproofpath(cfg, seq, &ah).await?
            }
        }
    };

    // 4. assemble the XSPV blob.
    build_xspv_blob(&header, &datas, &[proof])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real proof captured from the patched rippled node (testnet ledger 20508252,
    // escrow rfYnJDSA…). getProofPath wire nodes, leaf-first. Frozen so the converter
    // can't drift from what the node produces.
    const LEDGER_HASH_HEX: &str =
        "F8839E1BB526F365E3EAE31D1C8E5DED174C610DBF855EC3EFD7663B08DB5D25";
    const ACCOUNT_HASH_HEX: &str =
        "E3EACD09D4E2DA5B25CB24D74897B39E31FE95F7D39BBD3C054BF69CA94FF317";
    const ESCROW_INDEX_HEX: &str =
        "CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE6";
    const WIRE: [&str; 8] = [
        "11006122001000002401296DC325013454F92D00000001551CEBE3779C38D3B00DCAD78530796B345BAD1CC8A5B38A9DFA56CDF2D6FC927062400000000613A9E8811447B13C7EC1823651F3C4B2CCA52CBAB6773A98C8CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE601",
        "DABA4103AA37CA2CFD43DE4D9562A1B4D0C6538CF40D133B2D02DB40122EEDAA08EDADF2A450A06DC215FA0B265DF2D3DEED8800C3D8998D4EADE5300CCCF72E970903",
        "00000000000000000000000000000000000000000000000000000000000000006BBC4F2C0120A3B49AFE99548A2443A02F79C06523176BB6B7A4C91C8627DBB9CAB252DF8CF8FD8C81AAE15043828E1EEA6D0FEC2B5B53DE9C614DC9767B204BABFAFB6D6BEB2D8E4A9F30E82D25AC7AC5C2568C7AE72A34E68F11C31D60DE8720BD9F44CEA922BBEE28C3ACD3BBCC63FB64D9CEADC170B70F586544BB1F5C5B2AB65CCCA6D6F793853620AC10381A7921D32ED7EFCA0099C7766347373CF29B00000000000000000000000000000000000000000000000000000000000000004A7F60470DFBC2719408834F5DBC6B6AC746FFA1E83986230A138A1DC681CE1BC4704EE750D4DB765D973EC809213CF2CE8987FF161F25A16A69369E8399EE7C000000000000000000000000000000000000000000000000000000000000000090770D99CDC356488FE0C594CCFDBDFE30518F9858C438ECFA1E5E1E38456B7BE45017D05C5A9266567D0B1C2241D4E950002A627447E8930E614B7DC44EF3248DEBD33E5F692FE18149F21012CFE019838A9EACC376894D27835F1C7230AB9EC62726D730601960B0653CA08F90EA105757081F71E4FE080EFA1EE5E584FA1C4B7AD1F5ED861C1478291DCA0166AD4BF0F80E898932B79046131BAA53F57F9C8FC28E1B53340EA19DD5923D0B0C2F38AAD02B47F756A807690B38124BAA7E1E02",
        "816D38B105AF841D7B6EF1C5EC8405A01846C1942A5BD8AE09C57AB71EE27830F5007BD95624EEEE5653A4286BE201221F72B941F6E1261D0B0FAF646E72517D5B1754B51E6E155B006271C92010708DDD85B96712876175FAE62ED6CA32B620C722285DD580A20892B9FCA866E5EBDA1C16DD42AB12C661179BF7D896F76539ABD603FBBB15D0C4B8428C2C7147F25909A766240571F89818E6DDB25AD517DE67EB6CE5719FB8E7A1321F9AB6697E6AE34728B5DC627B36F831D6FC8DA64CBB15F33060762E107DD16BA3D5776C4BDC606B8225CCE1E856819112184ACC4F5762F3E1C2B181455AF9900DE5EA778109D9D87C74B924D02596C7BC5DF3BC8A1BD9CEB7A5E4B24F4E0ED12024CF5A0CABFB4D6B51C9DD961D7D1DDAB69DF0F351329A0063E0657A2F68529163622B493214DAB1E7C0D22B3C072AB1BCFEC575F447EABA237780DF6ACB56AC8AF1B6127B6BA6A32EB0A763C308C5DB868CD21F277801A6306003A957DC211831E66EFDBB683E78A2F4F0D9F3EE5350240F47505D6CF3763BC551AE8CD8DC7311706A105D86E1D1571F2544BD5F2AC0E0AB831414806EB48B849BD10982DF0496031E812A3057362B365D8262AE7D88DE5807D30DDDDE37CE9F01B3CE813FEB2506096135684D343C7A98BBD0BF3EF0B4B1E3CCE71D2907F4DF72A7F917C8BD6E84BC19C11C75052FCFAC218B677C2A0E1A60A56702",
        "F914922958CD846E8FE36911C1F53219ED50A6785F6B6037305F0104C10AE6281337A7D315138998E428F76BFB46D212796308865AA7D745C39CBD64B4F3DF5406DFAF6DCE3B31E5BC441236FD16623F1E4D8E166BA94E66AF041D952BD9B26E9E3938E43F793DC8CC0F38D32A7A893D0D4824F861C76CF654F11904AB568C1983EA30152E6FF427994FF6CECDD84991E6B820F9551D315EFDABECBC8337C6208D58927440DF5B89623C20F14864BB764C2B0F5FF6A952C7FDAD95BA7A6C293314488A7F0EEF771DACFD5D7B17A89719C12DF33A2DD046D7968E5C6B004407258CF6EDFE746D08A132F2FD03585759AF396EDB94322697DD1FA9D89907C473AF8485A4797B7C3763BA822A47E198E69D2829304F7A186E7E00B2B70E07770BBBEE64BAD1D8DBEB3BB53D44E59717809EF9A0341A43E88765F6DEDEADFBE596F06773E55AA2B969853FA30807D24D83A6DC33C5A70F762EEFA62BC9BC4B75A27BB90DDBD6184A5132FB133E1F22F97A7D7118BC536CD7BA6027C95E89F4ABEFFAD163989868EADBACCDB494E920A5CB4AC0D785EE2DDEF647CDF9FA531BB393B24DAD7167F9832A0637000A61682BFD92F5F20E3406B6380E2490F00530C0924B2761A6B8A542F211E00E28E96E937725006A6CB5C7DC7D85D3FD2033F8A50CDAA891A4DCA50CF4CB3DF46285454AD816339E6DD639F097E6EF07ECC08C5830C002",
        "37FE1DC406E3DDE71848B8D8FFE81815FC6CA6730EEF8470EF1512B1004B0E412B5C9DA62792FC261EEDFE35798FFC457544BFC22F2DA96A15DA1737AF6B5A916D0755783A3CF8B6CB6BEB11F99A64BEC6FB41B6B65C1DB9BD37CD3721E2FBC54F542E77987317C77D11897FC781BEAEB3D32BA6ADEE1A0644CB6FD49D9A3796E8516AA90B10946E25B97F13D537C6D7751757B636621D9383AFBA8BDF60CDB37473A8C1710BB5919DC806CAB36327740E6C9B86A05B3F39466D1FADEAF1CC8DF12AE43C4607A70A4474C2BD08F6D0F50B9866345550B302780DDFA454F4BD251022232C20C4947B97767130DDA3D2AD0CB64FCA705344B5546ADA8352EE51087889A9FA9DADCEC0C9ABCC963C6F23BFD66DC21D705C570105B43F0551DA61D481147C84ABE2D44C1C904ED9F3B1A84CDED73B707B4BE1ACC90B53BEEE69C8262F14BA3DBE051D52CEEABDC8C17FFF94FDD67089C92D16410C4050DDF7E14300FDFAEB90B8F82EFD4DD613B167F77DE07A082FA6BDE7E7BBED27B3B0162E3CBBDA8FED1B7F50BD8F1F99AA6B7282A25524DD6E5428E9F40CF3EA7FDA9BCE78364C0F2542911840E384D6679E570EEA27061DE251E3C5B6B335B4F6F007769811AEF56C741B3B046FBD9F733476B99AA8937907D3482722F0668F6F2DC0C58035CF74187058519E477328D7FC205A68152A00437E30632EB70178669015A9AC8502",
        "3CA67843D005C3AFD07D0BDB256F1C43B4359354D2627DAD1B6567B1738E8DEF12B33E7E1A078675C9613C873F50A84AF3F025D86F5AE06E01E87D75B19FDE3E688A5F33F7C65CA534A5344F267B437CBC1D76B11CE1C2C80E9979342A490091FB1B698D5D0793754D6343CFF4911F36DF19272217BBB39C29A08C9A691E98EBDF44692A54E14B91EF73C873206A0C03B4A596FC6EB9281195844327CABD86FDA791A35182C78CFD2FEFEEB763EDA6341FED4F6618D434D7F83EF45A8B6A66D377B0E4035F917E12D856AA6F0E6BA550409D8F9A9618F2A31FB38F3457F7D00BB414D2F67E4039A5F3C0442B49A7166C2506C6C18E457B9E2D8ECED674BBF01B35CEFD7A09CB72D6ABEA54C131EC4B913EBCED47CDB8BB02F49F1A7D120580B08C0CCD2015A40E5EF772CFFEC1DD723890183706919C1B0111F528FBE53BEE767D90919C143D62F85B2EE64E41B1B1AB03D7110E6BB822436D5E76705573E6E0734AD44AC0722395B3BC4B4DA78DD315A8BCE8A25DECD92B8FAD7253C09E12CCFBCA85F25687B200508C2943210E4105538607E4741ECDFD40DCCABEE4CB119034060D9C508B9D604F7FB9FDBAC74AAD8D375E280AD192EE0EA2E721CD971CA452EE033B5C98A69E8C9F6447A9B7A35F3E08780D5582DD3BDBE888DD4F2AD5F2BF16C8E4D118566E10069162D7AC1E127B1124AB96111DEEB3EC9B334155E83F02",
        "1C870B833E9A16F0CE0E931B03491481AE80FDB5DEBA03C20C4FDCBC1527B23710DC230DCEA016944B7C6631C22A7257063C497445BDA07392D1E39E6F1054899988D8870F41D0C94FD4270A702A11AB4BDD5711580098475561AD39F9216BA1250F1991676B6E4C700FD1CE62F915EFCDEDC9B334B570668918093EFE79A10D6DE15CFC13E9496D6D764BA0FE355886B0A3050FF8F2E66CC245186A764531BD16F6579190B90F702A44425F40953B4931C0F3F2AA74929D59681ABE4D6CC4971F16D9AA26622743C3985E184E795E5E73EB9467064C3A1B05E3726769604AC9F30EEDA92D015BCE9E6FE47340231B71D6DEC10A68A0864B0A18FA14B8F2CCCEFA843310AF29338DA52AF2CD10A50AEA942AAE80CAD7FD3EA34AFBEB8CE57503FC3FB42DB62C40DF69D2F687CA91601C263DC1A208F774DBC2F6DF5146297FDB3AD75BC54A687A2951B9A16FB62D45C9E7B2C89C6CCFECBCC64275B56BDFDC1E0FA1E67B0C3D91313A16EF4214F9DBDCE744C3619166CD6C84E5A5DC3B6BEDECDF60E168E33956F9A1273B6E82B3CB55B2B8E808DEDE71117D16578467E34820C128B8B52C2D7EDCCA50F69728BD88F771AD47FB1323A45095852B975DB3C2344308694EFE20E038218A5131BE02E8C94857E79991DC31950BB18A395A27FF926112518046F6573B3758DCAAE66B80A5E35564C2FE585AD5E240075FCC86C70302",
    ];

    #[test]
    fn header_serializes_to_real_ledger_hash() {
        let ledger = serde_json::json!({
            "ledger_index": 20508252u64,
            "total_coins": "99999891703128011",
            "parent_hash": "DEDD2B66D03349C63F9D15373A49A9CB2DDFE9014F3A7C7A512C93969868FF00",
            "transaction_hash": "C823386B6ADC7FE83868E226CDCE2BFBFF44C15B480FBCC8A508CD9B4FD29125",
            "account_hash": ACCOUNT_HASH_HEX,
            "parent_close_time": 841936330u64,
            "close_time": 841936331u64,
            "close_time_resolution": 10u64,
            "close_flags": 0u64,
        });
        let hdr = serialize_ledger_header(&ledger).unwrap();
        assert_eq!(
            hex::encode_upper(ledger_hash(&hdr)),
            LEDGER_HASH_HEX,
            "header must hash to the node's ledger_hash"
        );
    }

    #[test]
    fn getproofpath_converts_and_verifies_to_account_hash() {
        let path: Vec<Vec<u8>> = WIRE.iter().map(|w| hex::decode(w).unwrap()).collect();
        let proof = proof_from_getproofpath(&path, 0).unwrap();
        assert_eq!(proof.inner_root_to_leaf.len(), 7, "7 inner levels");
        assert_eq!(
            hex::encode_upper(proof.leaf_index),
            ESCROW_INDEX_HEX,
            "leaf key == escrow keylet"
        );
        let mut ah = [0u8; 32];
        ah.copy_from_slice(&hex::decode(ACCOUNT_HASH_HEX).unwrap());
        assert!(
            verify_inclusion(&proof, &ah),
            "converted proof must hash to the validator-signed account_hash"
        );
        // the escrow leaf carries balance 613A9E8 drops (101951976) — the E-1 custody figure
        assert!(proof
            .leaf_data
            .windows(4)
            .any(|w| w == [0x06, 0x13, 0xA9, 0xE8]));
    }

    // LIVE capture from OUR OWN patched xrpld 3.4.1 proof-node (Hetzner ~/perp-rippled),
    // ledger seq 21873212 (validated/closed). Firsthand confirmation on real 3.4.1 data that
    // the getProofPath path reconstructs byte-exact to the VALIDATED account_hash, AND that
    // every tamper direction is refused ("ship on reconstructs AND tampered-refused").
    const LIVE_ACCOUNT_HASH: &str =
        "E9C8C2121F1E8C27FC70825F30A314755E594DFE3BE33FC9B3703BA493CDB838";
    const LIVE_LEAF_INDEX: &str =
        "CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE6";
    const LIVE_PROOF_PATH: &[&str] = &[
        "11006122001000002401296DC325014B33C82D00000001558FB8A92D1D9FD6FA5EF53FC24AA603A09EE181039ECDA47FCCFD47C9B679EB83624000000018419828811447B13C7EC1823651F3C4B2CCA52CBAB6773A98C8CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE601",
        "0579768BC6A13EB44A267B3DABC325F0292325FA85428C636B5BA5F0786B3F6208EDADF2A450A06DC215FA0B265DF2D3DEED8800C3D8998D4EADE5300CCCF72E970903",
        "00000000000000000000000000000000000000000000000000000000000000006BBC4F2C0120A3B49AFE99548A2443A02F79C06523176BB6B7A4C91C8627DBB9CAB252DF8CF8FD8C81AAE15043828E1EEA6D0FEC2B5B53DE9C614DC9767B204BABFAFB6D6BEB2D8E4A9F30E82D25AC7AC5C2568C7AE72A34E68F11C31D60DE87203DC1416929E58E4EFE36D5167B102CDDE8A90F3DFE7003AF25BE62481D66A32AB65CCCA6D6F793853620AC10381A7921D32ED7EFCA0099C7766347373CF29B00000000000000000000000000000000000000000000000000000000000000004A7F60470DFBC2719408834F5DBC6B6AC746FFA1E83986230A138A1DC681CE1BC4704EE750D4DB765D973EC809213CF2CE8987FF161F25A16A69369E8399EE7C000000000000000000000000000000000000000000000000000000000000000090770D99CDC356488FE0C594CCFDBDFE30518F9858C438ECFA1E5E1E38456B7BE45017D05C5A9266567D0B1C2241D4E950002A627447E8930E614B7DC44EF3248DEBD33E5F692FE18149F21012CFE019838A9EACC376894D27835F1C7230AB9EC62726D730601960B0653CA08F90EA105757081F71E4FE080EFA1EE5E584FA1CEF3034BED2A9F72EB93EFD505963057A6AD6C1439FD2F7953AB8C4D1568FB3948FC28E1B53340EA19DD5923D0B0C2F38AAD02B47F756A807690B38124BAA7E1E02",
        "816D38B105AF841D7B6EF1C5EC8405A01846C1942A5BD8AE09C57AB71EE27830F5007BD95624EEEE5653A4286BE201221F72B941F6E1261D0B0FAF646E72517D97D78A06C9BC7BFA1EA48AAEF1F42BE299ED581BCDC2D197FB8E07D6CFBA518E8E27C7BB72928D379FC0446933FEDE39EAB4FF17F2280890077BCA4FB4F3850AC611BC5730FAD77B2CE9B7AC8F9290C5747EC8ACA1CB5E7BD55401A17B791A38DD3C1CE2DF262ECCE392271F0951CD1CA795DA35621DF22E937BD8FEE1E3814C7307EC81C796AAD87D03DE333C777922F741FECB6F24E4756B79A151F41877B662F3E1C2B181455AF9900DE5EA778109D9D87C74B924D02596C7BC5DF3BC8A1BFAD791F788A61CAC832F232FB7A37A69A5816475766070E54B5EFEE0F76819DA329A0063E0657A2F68529163622B493214DAB1E7C0D22B3C072AB1BCFEC575F48B01219D562F71EC61836B8823E27A028F72CE902D455D1E6A9323AFBBC583D47801A6306003A957DC211831E66EFDBB683E78A2F4F0D9F3EE5350240F47505D6CF3763BC551AE8CD8DC7311706A105D86E1D1571F2544BD5F2AC0E0AB8314143D0224125E728EE8E7D9F0490325232664BC1D1B0CF0D12E45D59E628FB7FA51DDDE37CE9F01B3CE813FEB2506096135684D343C7A98BBD0BF3EF0B4B1E3CCE71D2907F4DF72A7F917C8BD6E84BC19C11C75052FCFAC218B677C2A0E1A60A56702",
        "EBB1D9128549677B49EF44C05A7C55CD1A3E6D34E6B6947FB4A3C847FDAD45949740B97906639C44345A2ABEE65890F59131F27934CC03C9A97A55FDE09959C724106146966A2B0411E0933E5D795E04D0BEB60FDBD9E882D7074CF5586610F7D1AEE34BC3D5E4368F71E40F02740CEF6FA90DDA84CEBF2C4900437043D1A2377152DC141C1355F9D2444D723BD2AE24A83893114B8FB392466E24E50618D224AFFA39BDBD0DADB9B0E6316578A68FC75F504370E3070201B0441CD35293B1F55462DC3AD3E2C23024200A6CB39ACECE088645C243B305323B670666A8DD3E14DB4310E5BF145B62E56942A3EB37125D704D6A7DB3005C29EB910B6FAB41FC473B0EBF0FFDFD8F85BC16AD34372AAF921F544C3488D6042D047E615C1CC7E343B9B59F1B52BFE4E92D3836DB8A7CC3799EA430251812D8257E96CD80BF7A0B16328F77C8325E2DAA885DFEF0CA82E5024BAAC6FE96C5450DFBD07E6D23B94693A5D455849DDF124331B14DF794F36AFEA0F6C507371EF9EF2FA3EA3AA7A39468CD153E05D65564002E93B9C058C794FE2A3FA43DCFA841DAE9E82C433408A6A920D7C6FB764660A55F38FE9C6D09C0335A0E3145C7BE21487ED1D61982E2A3DC65822A9AF4E7C69FE2797BF6FD51449DAB5A2C33120C3DC4EF127956684A3A00166A1C68861DF636CA5052BD81502B44FB82DB5607DFC787ABAEC544E290F09102",
        "49F3CA61A35A3271701B1276C083764FDD7B4B613037EE42C77E0518C362502EFED5F11D5A3C1716978A485ECEA44FE15371DE3D625D66C04C0B6C62F39A2C79B955D3E9B14E33DBE64C3BD5C37979E453F1B2F50F22F2063EC8EFE06F21535E6D42FD264840602288D0728EBA03D986C7A47A1FEAE37AF982C6DDF102BF93795DDF981448F982DAD117EDC7EAB921999736DBF7788AF89C0AAB82DB4874354E383D6332085A095E98BA03E701CA4F23D46FF6F427E264DFEF48E810E54BB929121263D884183ED8FEF67F4B26946F695BBD02F6FC1C1C0E456922D254FE7E2F87A63B24FFEA7C3CA4BA9F862422BD2ECA6FFD01177597BA23987340B2693A374F006C46A0DBA46D2B42570C3B2653C54084B638CF05783E64C2F22231A7C16A38897727D1DFFE06CE33DE80828049EAD6C658A921CF431B687065CD14BC32379877EE89C512DBE79CCC672FBA8C1F35768CEA91CACC5E91821DDF96A4E17B78E45942E24A42E75FDFE24F36866E77E6FD514E5760EF8045A047D27BC39630D2336B298EB7B50C0A547011AF96B7F48B0E1E4230EAFF7E9934E6B566AA5460AC50FD1BD7323862A66830A3A7D7D4E8F11100E737DDFF7DE83FFDC6637AE44B82B128D6116D5153D07F0F22B20A8EFC39D0B47256ABC4CA045F4C0B6000242A3D836A55B2D9A5523D79CA6960A7308043B771299CE53928A4BFDD0963355D895902",
        "A19983B7F06A0CA8A4AB9A3B6037F62D2B5F49303E1EB2C1BB7BE152E9F535E7514859412E463F259CBD55F7EEAA090DFC3B1268382CF5AF3B4ABB21290CE8EBAD5FF73ED19D4753A49CE37AE4D4B471AE538DCC1E0B0AE33A11BC7975CF8CDE94F78635B204D18DB188E6D3C5C2520742CD443FFAD6A08B26CF8320843B49F1367DF7B773D95EEC1F38F9FBF4ACBD7401DA1F3C8BF216B61CCC4A3E7DB9A2C7A442A6821E1D547E9D138E37118911EB934C20419E4C42FD6713C3243CC1208F036A41382246ADE4E316440FFF9316F2647179B2D2BAF861FA92E4869251AF1A58107E78084E70C6CA598795459559C867ABBF056A4A25558D8EC1FB42DF7C72AF2F0F056DD8AAA157476ED466B98E43BE733788C8CBA1299D38D7E45347DBF15118078DFD4B8FF57ED8AABC30156372A315E7E2E35AA7C80C4542BEC03D4ECA9815055E6B09103E399D5E4B090CADE5659F77AB87CE90B264D11FB589254488649AA1E51748D5B6C9337E113B193C27A26AF5472DD2870711BA8F2293813ADD55BA9116908C74AB6829B320244F78743690C4D69E9056A6A53A809772899E679EB13491D4B3FC38CCFEE07DA88F1A35A3EC4CA77E5439D3C8BE6836844DE7509EE615CA0F22113B3DE669340304EABE8A18A1EA323B07FD65C06BECD2495709E5C2065F32668FC877823991C30F1CEA5044FB48C3EE1B1C78A88EFC6820427102",
        "3E13F8D3162E0F3B3CAC114E1D6DA0E9E3D11C8119B9007FF30029372B57990DAE31188FE143BDE3E87FAB0841BA9894F6C83EC0D9D86B235BEFB912C75B9E1008730CCC025E34C6B0AC0705184CE6807073813D59BD08A0B618BC9530499C019863FB2F01E4D56863BECB60063F7F9B77C8A8B08B1476411C69569299691A601C61C8B8AE808EAFED0D548672F17F8206B40BBBB6C3DD9F90ABD606997943ECB5ABEC2FB9106618E885305AB824CDE280019C70C26099CF1CA4ED27C9502181D499EFAA988609B6E7E65361664886F02FBA776C885165BE19FECB6ABB92526CC25CE97C8A6F303B7087D90BB88D55ED9BC3B29C9BAAF2C807951598CB97D287E365DFF5B18E43136D922508AA27AE1AD93978B1EA2EE84327062E782A46F2D3682F17EDC8766F5DE23B277B25080466DCBAA413106FE2B858D91536CB78845CB55FBD8C999A44317D11ACD661FA523100AABFC85C9BE6D816BC5CA9A5D1D1AD49D5E7C61028BDD92E796D11C6DA56A5F93F338280BBC83286FC4ED4C689A7901903181FCFEF98F51188DD421218AEE9D1100511778C819061B8FB946FF673F918428C890125851CDA3528B6DE1F142F4B686984BC68541B88B95013024FE596BA2DC9E4E787CC7DE250108D2E482D5E852AE4077C6CF93AAD374211A68902F1C2A536B46637F43B8B8C68304FAE7415FD7DFB61419FC8D0D5F95B8B091AF51702",
    ];

    #[test]
    fn live_escrow_proof_selfcheck_and_breaktest() {
        let path: Vec<Vec<u8>> =
            LIVE_PROOF_PATH.iter().map(|w| hex::decode(w).unwrap()).collect();
        let proof = proof_from_getproofpath(&path, 0).expect("convert live getProofPath");
        assert_eq!(
            hex::encode_upper(proof.leaf_index),
            LIVE_LEAF_INDEX,
            "leaf key == escrow keylet"
        );
        let mut ah = [0u8; 32];
        ah.copy_from_slice(&hex::decode(LIVE_ACCOUNT_HASH).unwrap());

        // SELF-CHECK (invariant ii/iii firsthand): the live path reconstructs byte-exact to
        // the VALIDATED account_hash. This is what the enclave re-does; equal => offsets +
        // MLN/MIN hashing are correct on real 3.4.1 data.
        assert!(
            verify_inclusion(&proof, &ah),
            "SELF-CHECK: live path must hash to the validated account_hash"
        );

        // BREAK-TEST A: flip one byte in an inner node -> REFUSE
        let mut a = proof_from_getproofpath(&path, 0).unwrap();
        a.inner_root_to_leaf[0][0] ^= 0x01;
        assert!(!verify_inclusion(&a, &ah), "tampered inner node must be refused");

        // BREAK-TEST B: wrong account_hash (one bit) -> REFUSE
        let mut ah_bad = ah;
        ah_bad[0] ^= 0x01;
        assert!(
            !verify_inclusion(&proof, &ah_bad),
            "wrong account_hash must be refused"
        );

        // BREAK-TEST C: tamper the leaf data (e.g. a balance byte) -> REFUSE
        let mut c = proof_from_getproofpath(&path, 0).unwrap();
        c.leaf_data[0] ^= 0x01;
        assert!(!verify_inclusion(&c, &ah), "tampered leaf must be refused");

        // BREAK-TEST D: wrong leaf index (a different account's key) -> REFUSE
        let mut d = proof_from_getproofpath(&path, 0).unwrap();
        d.leaf_index[0] ^= 0x01;
        assert!(!verify_inclusion(&d, &ah), "wrong leaf index must be refused");
    }

    use super::REAL_VALIDATION_HEX as VAL_DATA;

    /// Walk a validations section exactly as the enclave's `parse_attested_prefix` does,
    /// returning how many bytes it consumed. Deliberately a re-implementation and not a
    /// call into shared code: the point is to hold the producer against the CONSUMER's
    /// reading of the format, and shared code would let one drift while both stayed green.
    fn walk_framed(section: &[u8], count: u16) -> Result<usize> {
        let mut p = 0usize;
        for _ in 0..count {
            let rem = section
                .len()
                .checked_sub(p)
                .context("walked past the end")?;
            if rem < 34 {
                bail!("truncated at pubkey+sig_len");
            }
            p += 33;
            let sig_len = section[p] as usize;
            p += 1;
            let rem = section
                .len()
                .checked_sub(p)
                .context("walked past the end")?;
            if sig_len > 72 || rem < sig_len + 2 {
                bail!("truncated at signature");
            }
            p += sig_len;
            let vbody_len = u16::from_be_bytes([section[p], section[p + 1]]) as usize;
            p += 2;
            let rem = section
                .len()
                .checked_sub(p)
                .context("walked past the end")?;
            if rem < vbody_len {
                bail!("truncated at vbody");
            }
            p += vbody_len;
        }
        Ok(p)
    }

    /// THE SEAM, which nothing tested and which cost a live outage.
    ///
    /// The collector buffers raw STValidation `data` blobs; the enclave parses a FRAMED
    /// section (pubkey33 | sig_len | sig | vbody_len | vbody). The step between them lived
    /// in `build_validations_section`, and two of the three producers never called it —
    /// they concatenated the raw blobs and passed a count of however many there were. On
    /// 2026-09-27 the attested clock refused 100% of ledgers with -72 for exactly this,
    /// and the deposit path would have done the same the moment it was switched on.
    ///
    /// Why the existing tests all stayed green: each side was tested against the format as
    /// its own author read it, and the cross-language vector could not help, because its
    /// entries are fillers already in wire form rather than anything framed from real
    /// data. Feeding the vector back through the builder proves the builder copies bytes.
    ///
    /// So this starts from a REAL validation and requires the produced section to parse
    /// under the consumer's own loop and to end EXACTLY at the blob's end — the enclave
    /// returns TRAILING_GARBAGE for anything else.
    #[test]
    fn the_validations_section_is_framed_not_the_raw_data() {
        let data = hex::decode(VAL_DATA).unwrap();
        let header = [0x11u8; HEADER_LEN];
        let blob = build_xclk_blob(&header, std::slice::from_ref(&data)).unwrap();

        let count = u16::from_be_bytes([blob[7 + HEADER_LEN], blob[8 + HEADER_LEN]]);
        assert_eq!(
            count, 1,
            "the builder derives the count from what it framed"
        );
        let at = 9 + HEADER_LEN;
        let consumed = walk_framed(&blob[at..], count).expect("the framed section must parse");
        assert_eq!(
            at + consumed,
            blob.len(),
            "the section must end exactly at the blob's end"
        );

        // and the framed entry must carry the REAL content, not merely parse
        let (pk, sig, vbody) = validation_entry(&data).unwrap();
        assert_eq!(&blob[at..at + 33], &pk[..], "signing pubkey");
        assert_eq!(blob[at + 33] as usize, sig.len(), "signature length");
        assert_eq!(&blob[at + 34..at + 34 + sig.len()], &sig[..], "signature");
        let vb_at = at + 34 + sig.len();
        assert_eq!(
            u16::from_be_bytes([blob[vb_at], blob[vb_at + 1]]) as usize,
            vbody.len(),
            "vbody length"
        );
    }

    /// PROBE for the test above: the shipped bug must actually fail the same walk.
    ///
    /// Without this, `the_validations_section_is_framed_not_the_raw_data` could be passing
    /// because the walk accepts almost anything. Raw STValidation bytes are a valid blob
    /// and an invalid section, and this is what the enclave saw for every ledger.
    #[test]
    fn the_raw_concatenation_the_clock_shipped_would_not_parse() {
        let data = hex::decode(VAL_DATA).unwrap();
        let end = walk_framed(&data, 1);
        match end {
            Err(_) => {}
            Ok(n) => assert_ne!(
                n,
                data.len(),
                "raw `data` must NOT walk cleanly to its end, or the framing check above \
                 cannot distinguish the bug from the fix"
            ),
        }
    }

    #[test]
    fn validation_entry_vbody_verifies() {
        use k256::ecdsa::signature::hazmat::PrehashVerifier;
        use k256::ecdsa::{Signature, VerifyingKey};
        let data = hex::decode(VAL_DATA).unwrap();
        let (pk, sig, vbody) = validation_entry(&data).unwrap();
        assert_eq!(pk[0] & 0xFE, 0x02, "compressed secp256k1 signing key");
        // the validator signed SHA-512Half('VAL\0' || vbody); the reconstructed vbody
        // (data minus sfSignature) must verify — proving the field-strip is exact.
        let h = sha512half(&[&[0x56, 0x41, 0x4C, 0x00], &vbody]);
        let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
        let s = Signature::from_der(&sig).unwrap();
        assert!(
            vk.verify_prehash(&h, &s).is_ok(),
            "reconstructed vbody must verify under the signing key"
        );
        // and the vbody must carry the ledger_hash (sfLedgerHash) the enclave checks
        assert!(vbody
            .windows(32)
            .any(|w| w == hex::decode(LEDGER_HASH_VAL).unwrap().as_slice()));
    }
    const LEDGER_HASH_VAL: &str =
        "1124A12059FA1D708D4CB718767B5C2C1971E6D644F7731BD8D60B8666538532";

    /// End-to-end fetch against a LIVE patched node reachable at 127.0.0.1:5005/6006
    /// (SSH-forward from Hetzner). #[ignore] so CI (no node) skips it. The fetch itself
    /// verifies chaining (header→ledger_hash the validations attest, proof→account_hash),
    /// so a returned blob is already self-consistent; we re-parse to double-check shape.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_spv_bundle() {
        let cfg = SpvFetchConfig {
            http_url: "http://127.0.0.1:5005".into(),
            ws_url: "ws://127.0.0.1:6006".into(),
            escrow_account: "rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd".into(),
            quorum: 4,
            collect_secs: 45,
            prefer_getproofpath: true,
        };
        let blob = fetch_spv_bundle(&cfg).await.expect("fetch_spv_bundle");
        assert_eq!(&blob[..4], b"XSPV", "magic");
        assert_eq!(blob[4], 1, "version");
        eprintln!(
            "XSPV blob: {} bytes (self-verified header+proof+validations)",
            blob.len()
        );
    }

    /// #131 AC-BASE-2″ P2-e: EMIT fresh C++ cross-ledger-splice vectors from the LIVE
    /// patched node. Fetches two DISTINCT validated ledgers' headers + escrow proofs,
    /// converts the proofs with the frozen-tested `proof_from_getproofpath` (no
    /// reimplementation → no drift), asserts the splice actually rejects on real data,
    /// and prints C++ hex constants for test_xrpl_spv.cpp. #[ignore]: run on Hetzner.
    #[tokio::test]
    #[ignore]
    async fn emit_cross_splice_vectors() {
        let http = "http://127.0.0.1:5005";
        let escrow = "rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd";
        async fn grab(
            http: &str,
            escrow: &str,
            idx: serde_json::Value,
        ) -> (String, [u8; 32], u64, XspvProof) {
            let lr = http_rpc(
                http,
                "ledger",
                serde_json::json!({"ledger_index": idx, "transactions": false}),
            )
            .await
            .unwrap();
            let ledger = &lr["ledger"];
            let header = serialize_ledger_header(ledger).unwrap();
            assert_eq!(
                hex::encode_upper(ledger_hash(&header)),
                ledger["ledger_hash"].as_str().unwrap().to_uppercase(),
                "LWR mismatch"
            );
            let seq = as_u64(&ledger["ledger_index"]).unwrap();
            let ah = hex32(ledger, "account_hash").unwrap();
            assert_eq!(&header[76..108], &ah[..], "account_hash != header+76");
            let le = http_rpc(
                http,
                "ledger_entry",
                serde_json::json!({"account_root": escrow, "ledger_index": seq, "binary": true, "proof": true}),
            )
            .await
            .unwrap();
            let path: Vec<Vec<u8>> = le["proof_path"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| hex::decode(v.as_str().unwrap()).unwrap())
                .collect();
            let proof = proof_from_getproofpath(&path, 0).unwrap();
            assert!(
                verify_inclusion(&proof, &ah),
                "proof must verify to account_hash"
            );
            (hex::encode_upper(header), ah, seq, proof)
        }
        let (hdr_a, ah_a, seq_a, pa) = grab(http, escrow, serde_json::json!("validated")).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let si = http_rpc(http, "server_info", serde_json::json!({}))
                .await
                .unwrap();
            if as_u64(&si["info"]["validated_ledger"]["seq"]).unwrap() > seq_a + 1 {
                break;
            }
        }
        let (hdr_b, ah_b, seq_b, pb) = grab(http, escrow, serde_json::json!("validated")).await;
        let emit = |name: &str, hdr: &str, seq: u64, p: &XspvProof| {
            eprintln!("// ledger {name} (seq {seq})");
            eprintln!("static const char* HDR_{name} = \"{hdr}\";");
            eprintln!(
                "static const char* KEYLET_{name} = \"{}\";",
                hex::encode_upper(p.leaf_index)
            );
            eprintln!(
                "static const char* LEAF_{name} = \"{}\";",
                hex::encode_upper(&p.leaf_data)
            );
            eprintln!("#define DEPTH_{name} {}", p.inner_root_to_leaf.len());
            eprintln!(
                "static const char* INNER_{name}[{}] = {{",
                p.inner_root_to_leaf.len()
            );
            for node in &p.inner_root_to_leaf {
                eprintln!("  \"{}\",", hex::encode_upper(node));
            }
            eprintln!("}};");
        };
        emit("A", &hdr_a, seq_a, &pa);
        emit("B", &hdr_b, seq_b, &pb);
        assert_ne!(ah_a, ah_b, "need two DISTINCT ledgers for a cross-splice");
        assert!(
            !verify_inclusion(&pb, &ah_a),
            "SPLICE must reject: proof B vs root A"
        );
        assert!(
            !verify_inclusion(&pa, &ah_b),
            "SPLICE must reject: proof A vs root B"
        );
        eprintln!("// cross-splice rejection CONFIRMED on real two-ledger data");
    }

    #[test]
    fn wrong_account_hash_rejected() {
        let path: Vec<Vec<u8>> = WIRE.iter().map(|w| hex::decode(w).unwrap()).collect();
        let proof = proof_from_getproofpath(&path, 0).unwrap();
        assert!(
            !verify_inclusion(&proof, &[0xEE; 32]),
            "a wrong root must be rejected"
        );
    }

    /// NODE-FREE state-tree reconstruction against a real recent testnet ledger from a
    /// STANDARD public node (s.altnet) — no getProofPath, no patched/dedicated node. The
    /// correctness pin: the FULL rebuilt state-SHAMap root EQUALS the validator-signed
    /// `account_hash` (read from the header, not computed), and the escrow AccountRoot
    /// inclusion path verifies to it. #[ignore]: live network fetch (dumps full state).
    #[tokio::test]
    #[ignore = "live testnet fetch (full ledger_data state dump)"]
    async fn state_reconstruction_root_equals_account_hash() {
        const URL: &str = "https://s.altnet.rippletest.net:51234";
        const ESCROW: &str = "rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd";
        let val = http_rpc(
            URL,
            "ledger",
            serde_json::json!({"ledger_index": "validated", "transactions": false}),
        )
        .await
        .unwrap();
        let seq = as_u64(&val["ledger"]["ledger_index"]).unwrap();
        let ah = hex32(&val["ledger"], "account_hash").unwrap();
        let objects = fetch_state_objects(URL, seq).await.unwrap();
        eprintln!("// ledger {seq}: {} state objects dumped", objects.len());
        let map = StateShaMap::build(objects).unwrap();
        assert_eq!(
            map.root_hash(),
            ah,
            "rebuilt state-tree root must EQUAL the signed account_hash (ledger {seq})"
        );
        let proof = reconstruct_state_proof(URL, seq, ESCROW).await.unwrap();
        assert!(
            verify_inclusion(&proof, &ah),
            "escrow AccountRoot proof must verify to the signed account_hash"
        );
        eprintln!(
            "// node-free state reconstruction CONFIRMED: root==account_hash + escrow proof verifies (ledger {seq})"
        );
    }
}
