//! #131 P3 — host-side reconstruction of an XRPL ledger's TRANSACTION SHAMap.
//!
//! The enclave's `xrpl_spv_verify_tx_inclusion` walks an inclusion path against the
//! validator-signed `transaction_hash` in the ledger header. Nothing produces that path
//! for us: rippled exposes `get_proof_path` for the *state* tree only, so for the tx tree
//! the host has to rebuild the map from the ledger's transactions and read the path off
//! it. That is what this module does.
//!
//! UNTRUSTED PRODUCER, same as [`crate::spv_proof`]. Every byte here is re-derived inside
//! the enclave: it re-computes the tx-ID from the blob (so the host cannot even choose
//! WHICH transaction a proof is about), re-hashes the leaf, re-walks the inner nodes and
//! compares against a `transaction_hash` it took from a quorum-attested header. A wrong or
//! malicious path can only make the ecall REFUSE. The reason to be careful here anyway is
//! availability, not safety: a subtly wrong rebuild refuses every honest deposit.
//!
//! The shapes are mirrored from `EthSignerEnclave/Enclave/xrpl_spv.cpp` and must stay in
//! lockstep with it — see the golden test, which checks the rebuild against a real
//! validated ledger rather than against our own output.

// Consumed by the P3 deposit driver, which lands with the deposit ecall — that ecall
// is waiting on the credited-amount ruling (REQ-…-p3-delivered-amount). The module is
// committed ahead of its caller because it is the half that can be proven NOW, against
// a real signed ledger, and because the enclave's interop test consumes a vector it
// emits. Drop this allow when the driver wires it up.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};

/// `'TXN\0'` — the transaction-ID prefix. The tx-ID is also the tx tree's SHAMap key.
const HP_TXN_ID: [u8; 4] = [0x54, 0x58, 0x4E, 0x00];
/// `'SND\0'` — the tx-tree leaf prefix (transaction WITH metadata).
const HP_TX_NODE: [u8; 4] = [0x53, 0x4E, 0x44, 0x00];
/// `'MIN\0'` — the inner-node prefix. Shared by BOTH trees; only the leaf prefixes differ.
const HP_INNER: [u8; 4] = [0x4D, 0x49, 0x4E, 0x00];

/// One inner node in the enclave's flat form: 16 branches x 32 bytes, absent = zero.
pub const INNER_NODE_LEN: usize = 512;
/// A SHAMap key is 32 bytes = 64 nibbles, so no path can be longer than that.
pub const MAX_DEPTH: usize = 64;

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

/// XRPL variable-length prefix. Mirrors `vl_prefix` in the enclave byte for byte —
/// the leaf hash is over the PREFIXED blobs, so an encoding difference here produces a
/// leaf hash that silently fails inclusion rather than erroring.
pub fn vl_prefix(n: usize) -> Result<Vec<u8>> {
    // Written with checked_sub and try_from throughout. The range guards make every
    // operation provably safe today, so this changes no behaviour — the point is that
    // the guarantee lives in the expression instead of in the `if` above it. Rust
    // release builds wrap silently, so an edit that moved a bound would produce a
    // wrong prefix, and a wrong prefix is a leaf hash that fails inclusion for a reason
    // nothing reports.
    let byte = |x: usize, what: &'static str| -> Result<u8> {
        u8::try_from(x).map_err(|_| anyhow::anyhow!("VL {what} byte out of range: {x}"))
    };
    if n <= 192 {
        Ok(vec![byte(n, "single-byte length")?])
    } else if n <= 12480 {
        let v = n.checked_sub(193).context("VL 2-byte base underflow")?;
        Ok(vec![byte(193 + (v >> 8), "2-byte high")?, (v & 0xFF) as u8])
    } else if n <= 918_744 {
        let v = n.checked_sub(12481).context("VL 3-byte base underflow")?;
        Ok(vec![
            byte(241 + (v >> 16), "3-byte high")?,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ])
    } else {
        bail!("VL length {n} exceeds the XRPL maximum of 918744")
    }
}

/// `txID = SHA512Half('TXN\0' || tx_blob)`. Also the tx tree's key for that transaction.
pub fn tx_id(tx_blob: &[u8]) -> [u8; 32] {
    sha512half(&[&HP_TXN_ID, tx_blob])
}

/// `leaf = SHA512Half('SND\0' || VL(tx) || tx || VL(meta) || meta || txID)`.
pub fn tx_leaf_hash(tx_blob: &[u8], meta: &[u8]) -> Result<[u8; 32]> {
    let key = tx_id(tx_blob);
    let lv = vl_prefix(tx_blob.len())?;
    let mv = vl_prefix(meta.len())?;
    Ok(sha512half(&[&HP_TX_NODE, &lv, tx_blob, &mv, meta, &key]))
}

/// Which nibble of `key` selects the branch at tree depth `depth`. High nibble first —
/// depth 0 is the top 4 bits of byte 0. Must match the enclave's indexing exactly.
fn nibble(key: &[u8; 32], depth: usize) -> usize {
    if depth % 2 == 0 {
        (key[depth / 2] >> 4) as usize
    } else {
        (key[depth / 2] & 0x0F) as usize
    }
}

/// A SHAMap node. Leaves carry their already-computed hash because the tx-tree leaf hash
/// covers the transaction, its metadata AND the key — the tree itself never needs the
/// underlying blobs again.
enum Node {
    Empty,
    Leaf { key: [u8; 32], hash: [u8; 32] },
    Inner(Box<[Node; 16]>),
}

impl Node {
    fn empty_inner() -> Node {
        Node::Inner(Box::new(std::array::from_fn(|_| Node::Empty)))
    }

    fn hash(&self) -> [u8; 32] {
        match self {
            Node::Empty => [0u8; 32],
            Node::Leaf { hash, .. } => *hash,
            Node::Inner(children) => {
                let mut flat = [0u8; INNER_NODE_LEN];
                for (i, c) in children.iter().enumerate() {
                    flat[i * 32..i * 32 + 32].copy_from_slice(&c.hash());
                }
                sha512half(&[&HP_INNER, &flat])
            }
        }
    }

    /// The 512-byte child array of an inner node, in the enclave's flat form.
    fn flat_children(&self) -> Option<[u8; INNER_NODE_LEN]> {
        match self {
            Node::Inner(children) => {
                let mut flat = [0u8; INNER_NODE_LEN];
                for (i, c) in children.iter().enumerate() {
                    flat[i * 32..i * 32 + 32].copy_from_slice(&c.hash());
                }
                Some(flat)
            }
            _ => None,
        }
    }

    fn insert(&mut self, key: [u8; 32], hash: [u8; 32], depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            bail!("two transactions share a full 32-byte tx-ID — impossible without a SHA-512 collision");
        }
        match self {
            Node::Empty => {
                *self = Node::Leaf { key, hash };
                Ok(())
            }
            Node::Leaf {
                key: existing_key,
                hash: existing_hash,
            } => {
                if *existing_key == key {
                    bail!("duplicate tx-ID in one ledger");
                }
                // A SHAMap collapses: a leaf sits at the SHALLOWEST depth where its key
                // prefix is unique. Two keys meeting here push BOTH down one level, and
                // the process repeats while their nibbles keep agreeing.
                let (ek, eh) = (*existing_key, *existing_hash);
                *self = Node::empty_inner();
                self.insert(ek, eh, depth)?;
                self.insert(key, hash, depth)
            }
            Node::Inner(children) => {
                let nib = nibble(&key, depth);
                children[nib].insert(key, hash, depth + 1)
            }
        }
    }
}

/// A rebuilt transaction SHAMap.
pub struct TxShaMap {
    root: Node,
}

/// An inclusion path shaped exactly as `xrpl_spv_verify_tx_inclusion` consumes it.
pub struct TxInclusionProof {
    /// Derived from the blob; the enclave re-derives it and does not trust this copy.
    pub tx_id: [u8; 32],
    /// Inner nodes ordered ROOT -> LEAF, one 512-byte flat child array each.
    pub inner_root_to_leaf: Vec<[u8; INNER_NODE_LEN]>,
}

impl TxShaMap {
    /// Build the map from every `(tx_blob, metadata)` pair in one ledger.
    ///
    /// ALL of the ledger's transactions are required: the root is a hash over the whole
    /// map, so a single missing or extra transaction changes it and the proof stops
    /// matching the header. There is no partial rebuild.
    pub fn build(items: &[(Vec<u8>, Vec<u8>)]) -> Result<Self> {
        let mut root = Node::Empty;
        for (tx, meta) in items {
            let key = tx_id(tx);
            let leaf = tx_leaf_hash(tx, meta)?;
            root.insert(key, leaf, 0)?;
        }
        Ok(TxShaMap { root })
    }

    /// The map root. For a correctly rebuilt ledger this EQUALS the `transaction_hash`
    /// at offset 44 of the 118-byte ledger header — which is what the validators signed.
    pub fn root_hash(&self) -> [u8; 32] {
        self.root.hash()
    }

    /// Read the inclusion path for one transaction off the rebuilt map.
    pub fn inclusion_proof(&self, target_tx_id: &[u8; 32]) -> Result<TxInclusionProof> {
        let mut inner_root_to_leaf = Vec::new();
        let mut node = &self.root;
        let mut depth = 0usize;
        loop {
            match node {
                Node::Inner(children) => {
                    let flat = node
                        .flat_children()
                        .expect("inner node always has a flat form");
                    inner_root_to_leaf.push(flat);
                    let nib = nibble(target_tx_id, depth);
                    node = &children[nib];
                    depth += 1;
                }
                Node::Leaf { key, .. } => {
                    if key != target_tx_id {
                        bail!("transaction is not in this ledger's tx tree (a different key sits at its slot)");
                    }
                    return Ok(TxInclusionProof {
                        tx_id: *target_tx_id,
                        inner_root_to_leaf,
                    });
                }
                Node::Empty => {
                    bail!(
                        "transaction is not in this ledger's tx tree (empty slot at depth {depth})"
                    )
                }
            }
        }
    }
}

/// Independent host-side replay of the enclave's walk, for use BEFORE shipping a blob.
/// Deliberately written from the enclave's algorithm rather than from `TxShaMap`'s
/// internals, so a bug in the tree builder cannot validate itself.
pub fn verify_tx_inclusion(
    transaction_hash: &[u8; 32],
    tx_blob: &[u8],
    meta: &[u8],
    inner_root_to_leaf: &[[u8; INNER_NODE_LEN]],
) -> Result<[u8; 32]> {
    let key = tx_id(tx_blob);
    let mut running = tx_leaf_hash(tx_blob, meta)?;
    for d in (0..inner_root_to_leaf.len()).rev() {
        let node = &inner_root_to_leaf[d];
        let nib = nibble(&key, d);
        if node[nib * 32..nib * 32 + 32] != running {
            bail!("inclusion failed at depth {d}: branch {nib} does not hold the running hash");
        }
        running = sha512half(&[&HP_INNER, node]);
    }
    if &running != transaction_hash {
        bail!("inclusion failed at the root: rebuilt path does not reach transaction_hash");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_shamap_vector as v;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn ledger_items() -> Vec<(Vec<u8>, Vec<u8>)> {
        v::TXS
            .iter()
            .map(|(tx, meta)| (unhex(tx), unhex(meta)))
            .collect()
    }

    /// The expected root is READ OUT OF THE SIGNED HEADER, never stored separately —
    /// so this cannot pass by agreeing with itself.
    fn expected_root() -> [u8; 32] {
        let hdr = unhex(v::LEDGER_HEADER);
        assert_eq!(hdr.len(), 118, "ledger header must be 118 bytes");
        let mut root = [0u8; 32];
        root.copy_from_slice(&hdr[44..76]);
        root
    }

    #[test]
    fn rebuilt_root_equals_the_signed_transaction_hash() {
        let map = TxShaMap::build(&ledger_items()).unwrap();
        assert_eq!(
            map.root_hash(),
            expected_root(),
            "rebuilt tx-tree root must equal transaction_hash at header offset 44 \
             (ledger {})",
            v::LEDGER_INDEX
        );
    }

    #[test]
    fn every_transaction_in_the_ledger_proves_inclusion() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let root = expected_root();
        for (i, (tx, meta)) in items.iter().enumerate() {
            let key = tx_id(tx);
            let proof = map.inclusion_proof(&key).unwrap();
            let derived = verify_tx_inclusion(&root, tx, meta, &proof.inner_root_to_leaf)
                .unwrap_or_else(|e| panic!("tx[{i}] must prove inclusion: {e}"));
            assert_eq!(derived, key, "tx[{i}] derived key must match");
        }
    }

    /// The chosen ledger collapses unevenly — two leaves at depth 1, two at depth 2.
    /// Asserted explicitly so a future fixture swap that loses this property is loud
    /// rather than silently weakening every other test in this file.
    #[test]
    fn the_fixture_actually_exercises_shamap_collapse() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let depths: Vec<usize> = items
            .iter()
            .map(|(tx, _)| {
                map.inclusion_proof(&tx_id(tx))
                    .unwrap()
                    .inner_root_to_leaf
                    .len()
            })
            .collect();
        let min = depths.iter().min().copied().unwrap();
        let max = depths.iter().max().copied().unwrap();
        assert!(
            min < max,
            "fixture must contain leaves at DIFFERENT depths or it does not test \
             collapse at all; got {depths:?}"
        );
    }

    #[test]
    fn a_transaction_from_another_ledger_is_not_included() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        // Any key not in the map: flip a bit of a real one.
        let mut foreign = tx_id(&items[0].0);
        foreign[31] ^= 0x01;
        assert!(
            map.inclusion_proof(&foreign).is_err(),
            "a key that is not in the tree must not yield a proof"
        );
    }

    /// Dropping ONE transaction changes the root. This is the property that makes the
    /// rebuild safe to trust operationally: a host that omits a transaction cannot
    /// produce a path that still reaches the signed hash.
    #[test]
    fn an_incomplete_ledger_cannot_reproduce_the_root() {
        let mut items = ledger_items();
        items.pop();
        let partial = TxShaMap::build(&items).unwrap();
        assert_ne!(
            partial.root_hash(),
            expected_root(),
            "a ledger missing a transaction must NOT reach the signed root"
        );
    }

    /// Tampering with metadata alone must break inclusion: the leaf hash covers the
    /// metadata, so a host cannot rewrite a payment's outcome and keep the path valid.
    #[test]
    fn tampered_metadata_breaks_inclusion() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let root = expected_root();
        let (tx, meta) = &items[0];
        let proof = map.inclusion_proof(&tx_id(tx)).unwrap();
        let mut bad_meta = meta.clone();
        let last = bad_meta.len() - 1;
        bad_meta[last] ^= 0xFF;
        assert!(
            verify_tx_inclusion(&root, tx, &bad_meta, &proof.inner_root_to_leaf).is_err(),
            "tampered metadata must fail inclusion"
        );
    }

    /// A path of the WRONG DEPTH must not validate a transaction.
    ///
    /// Note what this test deliberately does NOT claim. Borrowing a same-depth sibling's
    /// path is not a forgery: every leaf at depth 1 has the root, and only the root, as
    /// its entire path, so that path legitimately proves all of them — the branch is
    /// chosen by the key the enclave derives from the blob, not by the path. The real
    /// property is that a path which does not lead to the transaction's own slot fails,
    /// and depth is the way to exhibit that in this fixture. (An earlier version of this
    /// test asserted the naive claim and failed, correctly.)
    #[test]
    fn a_path_of_the_wrong_depth_does_not_validate() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let root = expected_root();

        let depth_of = |i: usize| {
            map.inclusion_proof(&tx_id(&items[i].0))
                .unwrap()
                .inner_root_to_leaf
                .len()
        };
        let shallow = (0..items.len()).min_by_key(|&i| depth_of(i)).unwrap();
        let deep = (0..items.len()).max_by_key(|&i| depth_of(i)).unwrap();
        assert!(
            depth_of(shallow) < depth_of(deep),
            "fixture must have leaves at different depths"
        );

        let deep_path = map.inclusion_proof(&tx_id(&items[deep].0)).unwrap();
        let shallow_path = map.inclusion_proof(&tx_id(&items[shallow].0)).unwrap();

        let (s_tx, s_meta) = &items[shallow];
        assert!(
            verify_tx_inclusion(&root, s_tx, s_meta, &deep_path.inner_root_to_leaf).is_err(),
            "a deeper transaction's path must not prove a shallow one"
        );
        let (d_tx, d_meta) = &items[deep];
        assert!(
            verify_tx_inclusion(&root, d_tx, d_meta, &shallow_path.inner_root_to_leaf).is_err(),
            "a shallow transaction's path must not prove a deeper one"
        );
    }

    /// Emit the C golden vector that the ENCLAVE test consumes, so the two independent
    /// implementations are checked against each other on real ledger bytes rather than
    /// each against its own idea of the format. Ignored by default (it only prints).
    ///
    ///   cargo test --locked emit_enclave_vector -- --ignored --nocapture
    ///
    /// Paste the output into `EthSignerEnclave/tests/tx_tree_vector.h`.
    #[test]
    #[ignore = "vector generator, not an assertion"]
    fn emit_enclave_vector() {
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let root = expected_root();
        // Use the DEEPEST transaction: its path has more than one inner node, so the
        // enclave's loop is exercised rather than a single-node special case.
        let deep = (0..items.len())
            .max_by_key(|&i| {
                map.inclusion_proof(&tx_id(&items[i].0))
                    .unwrap()
                    .inner_root_to_leaf
                    .len()
            })
            .unwrap();
        let (tx, meta) = &items[deep];
        let proof = map.inclusion_proof(&tx_id(tx)).unwrap();

        let c_bytes = |name: &str, b: &[u8]| {
            let mut out = format!("static const uint8_t {name}[] = {{\n");
            for (i, x) in b.iter().enumerate() {
                if i % 12 == 0 {
                    out.push_str("    ");
                }
                out.push_str(&format!("0x{x:02x},"));
                out.push(if i % 12 == 11 { '\n' } else { ' ' });
            }
            out.push_str("\n};\n");
            out
        };

        let mut flat_inner = Vec::new();
        for n in &proof.inner_root_to_leaf {
            flat_inner.extend_from_slice(n);
        }
        println!(
            "/* ledger {} tx[{}] — emitted by orchestrator tx_shamap */",
            v::LEDGER_INDEX,
            deep
        );
        print!("{}", c_bytes("kTx2Root", &root));
        print!("{}", c_bytes("kTx2Blob", tx));
        print!("{}", c_bytes("kTx2Meta", meta));
        print!("{}", c_bytes("kTx2Inner", &flat_inner));
        print!("{}", c_bytes("kTx2IdExpect", &proof.tx_id));
        println!("#define kTx2Depth {}", proof.inner_root_to_leaf.len());
    }

    /// Emit an XDEP transport blob for the enclave's parser test. Ignored by default.
    ///
    ///   cargo test --locked emit_xdep_vector -- --ignored --nocapture
    ///
    /// `val_count = 0`: this vector exercises the TRANSPORT and the inclusion path, not
    /// the quorum — validator signatures are verified by machinery that already has its
    /// own real-manifest tests, and stapling six signatures in here would make the
    /// vector huge without testing anything the other suite does not.
    #[test]
    #[ignore = "vector generator, not an assertion"]
    fn emit_xdep_vector() {
        use crate::spv_proof::build_xdep_blob;
        let items = ledger_items();
        let map = TxShaMap::build(&items).unwrap();
        let hdr_v = unhex(v::LEDGER_HEADER);
        let mut header = [0u8; 118];
        header.copy_from_slice(&hdr_v);
        let deep = (0..items.len())
            .max_by_key(|&i| {
                map.inclusion_proof(&tx_id(&items[i].0))
                    .unwrap()
                    .inner_root_to_leaf
                    .len()
            })
            .unwrap();
        let (tx, meta) = &items[deep];
        let proof = map.inclusion_proof(&tx_id(tx)).unwrap();
        let blob = build_xdep_blob(&header, 0, &[], tx, meta, &proof.inner_root_to_leaf).unwrap();

        println!(
            "/* XDEP blob, ledger {} tx[{}], emitted by orchestrator build_xdep_blob */",
            v::LEDGER_INDEX,
            deep
        );
        println!("static const uint8_t kXdepBlob[] = {{");
        for (i, x) in blob.iter().enumerate() {
            if i % 12 == 0 {
                print!("    ");
            }
            print!("0x{x:02x},");
            if i % 12 == 11 {
                println!();
            } else {
                print!(" ");
            }
        }
        println!("\n}};");
        println!("#define kXdepTxLen {}", tx.len());
        println!("#define kXdepMetaLen {}", meta.len());
        println!("#define kXdepDepth {}", proof.inner_root_to_leaf.len());
    }

    /// VL prefix boundaries, mirrored from the enclave's `vl_prefix`. These are the
    /// exact lengths where the encoding changes width; an off-by-one here shifts every
    /// leaf hash for transactions of that size.
    #[test]
    fn vl_prefix_matches_the_enclave_encoding() {
        assert_eq!(vl_prefix(0).unwrap(), vec![0]);
        assert_eq!(vl_prefix(192).unwrap(), vec![192]);
        assert_eq!(vl_prefix(193).unwrap(), vec![193, 0]);
        assert_eq!(vl_prefix(12480).unwrap(), vec![240, 255]);
        assert_eq!(vl_prefix(12481).unwrap(), vec![241, 0, 0]);
        assert_eq!(vl_prefix(918_744).unwrap(), vec![254, 212, 23]);
        assert!(vl_prefix(918_745).is_err());
    }

    /// The real metadata in this fixture is 1662 bytes — past the 192-byte boundary —
    /// so the two-byte VL form is genuinely exercised by the root check above, not just
    /// by the unit assertions.
    #[test]
    fn the_fixture_exercises_the_multi_byte_vl_form() {
        let items = ledger_items();
        assert!(
            items.iter().any(|(_, m)| m.len() > 192),
            "fixture must contain metadata past the 1-byte VL boundary"
        );
    }
}
