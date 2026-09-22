//! Safe owner-management: calldata, and ordering the owner signatures.
//!
//! # Why this is in the orchestrator and not the enclave
//!
//! An earlier design put this in the enclave — an operation enum, calldata builders, the
//! lot. The owner rejected it: «Enclave подписывает tx.» Contract semantics inside the TCB
//! makes every Safe version and every new contract a new MRENCLAVE and a Path-A migration.
//!
//! So everything contract-shaped lives here, untrusted, and the enclave signs a transaction
//! whose `data` it never parses. The host can build any calldata it likes; the cluster
//! quorum is what decides whether it gets signed, and `to` is pinned to the Safe itself
//! inside the enclave — not passed from here, and not expressible from here.
//!
//! # The ordering rule, which is a correctness requirement and not a style one
//!
//! `Safe.checkNSignatures` dedups by requiring **strictly ascending owner addresses**
//! (`currentOwner > lastOwner`, else GS026). A correctly-signed set in the wrong order is
//! rejected on-chain. We therefore sort by the address **recovered from each signature**
//! rather than by an address the caller asserts — a caller that mislabels which owner
//! produced which signature cannot then produce a mis-ordered blob that looks fine here
//! and fails on Base.

use anyhow::{bail, Context, Result};
use sha3::{Digest, Keccak256};

/// A Safe owner-management call. These are the four `authorized` functions — every one is
/// a self-call, which is why the enclave can pin `to` to the Safe and never need a target
/// parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafeOp {
    /// `addOwnerWithThreshold(address owner, uint256 _threshold)`
    AddOwner { owner: [u8; 20], threshold: u64 },
    /// `removeOwner(address prevOwner, address owner, uint256 _threshold)`
    ///
    /// `prev_owner` is the owner *preceding* `owner` in the Safe's linked list, or the
    /// sentinel `0x…01` when removing the head. The Safe reverts if it is wrong, so it is
    /// read from the chain rather than guessed.
    RemoveOwner {
        prev_owner: [u8; 20],
        owner: [u8; 20],
        threshold: u64,
    },
    /// `swapOwner(address prevOwner, address oldOwner, address newOwner)`
    SwapOwner {
        prev_owner: [u8; 20],
        old_owner: [u8; 20],
        new_owner: [u8; 20],
    },
    /// `changeThreshold(uint256 _threshold)`
    ChangeThreshold { threshold: u64 },
}

fn selector(sig: &str) -> [u8; 4] {
    let d = Keccak256::digest(sig.as_bytes());
    [d[0], d[1], d[2], d[3]]
}

/// ABI word for an address: left-padded to 32 bytes.
fn word_addr(a: &[u8; 20], out: &mut Vec<u8>) {
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(a);
}

/// ABI word for a uint256 that fits in u64.
fn word_u64(v: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(&v.to_be_bytes());
}

impl SafeOp {
    /// The function signature this op encodes. Kept separate so a test can check the
    /// selector against the string rather than against another copy of the bytes.
    pub fn signature(&self) -> &'static str {
        match self {
            SafeOp::AddOwner { .. } => "addOwnerWithThreshold(address,uint256)",
            SafeOp::RemoveOwner { .. } => "removeOwner(address,address,uint256)",
            SafeOp::SwapOwner { .. } => "swapOwner(address,address,address)",
            SafeOp::ChangeThreshold { .. } => "changeThreshold(uint256)",
        }
    }

    /// Encode the call. Plain ABI: selector then one 32-byte word per argument, all of
    /// which are static types here, so there is no head/tail offset to get wrong.
    pub fn calldata(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 3 * 32);
        out.extend_from_slice(&selector(self.signature()));
        match self {
            SafeOp::AddOwner { owner, threshold } => {
                word_addr(owner, &mut out);
                word_u64(*threshold, &mut out);
            }
            SafeOp::RemoveOwner {
                prev_owner,
                owner,
                threshold,
            } => {
                word_addr(prev_owner, &mut out);
                word_addr(owner, &mut out);
                word_u64(*threshold, &mut out);
            }
            SafeOp::SwapOwner {
                prev_owner,
                old_owner,
                new_owner,
            } => {
                word_addr(prev_owner, &mut out);
                word_addr(old_owner, &mut out);
                word_addr(new_owner, &mut out);
            }
            SafeOp::ChangeThreshold { threshold } => {
                word_u64(*threshold, &mut out);
            }
        }
        out
    }
}

/// The Safe EIP-712 transaction hash, derived here as well as in the enclave.
///
/// Both derivations exist ON PURPOSE. The cluster quorum signs this hash to authorise the
/// step, and that signing happens in the orchestrator — before any enclave is asked to sign
/// the transaction. The enclave then re-derives it from the same fields and verifies the
/// bundle against ITS OWN derivation, never against a hash handed in. Two independent
/// computations that must agree is what makes the quorum non-blind: a node that is fed a
/// doctored calldata computes a different hash, its bundle contribution does not match, and
/// the enclave refuses.
///
/// `value`, `operation` and every gas field are zero, exactly as the enclave pins them
/// (`perp_reserves_publish.cpp:124,126`). `operation` is the one that matters: a non-zero
/// value there is DELEGATECALL, i.e. a full Safe takeover regardless of the calldata.
pub fn safe_tx_hash(
    safe: &[u8; 20],
    chain_id: u64,
    to: &[u8; 20],
    data: &[u8],
    nonce: u64,
) -> [u8; 32] {
    fn word_addr(a: &[u8; 20]) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(a);
        w
    }
    fn word_u64(v: u64) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&v.to_be_bytes());
        w
    }

    // domainSeparator = keccak(DOMAIN_TYPEHASH ‖ chainId ‖ verifyingContract)
    let mut d = Vec::with_capacity(96);
    d.extend_from_slice(&Keccak256::digest(
        b"EIP712Domain(uint256 chainId,address verifyingContract)",
    ));
    d.extend_from_slice(&word_u64(chain_id));
    d.extend_from_slice(&word_addr(safe));
    let domain = Keccak256::digest(&d);

    // structHash = keccak(SAFE_TX_TYPEHASH ‖ to ‖ 0 ‖ keccak(data) ‖ 0 ‖ 0,0,0 ‖ 0,0 ‖ nonce)
    let mut st = Vec::with_capacity(32 * 11);
    st.extend_from_slice(&Keccak256::digest(
        b"SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)",
    ));
    st.extend_from_slice(&word_addr(to));
    st.extend_from_slice(&[0u8; 32]); // value
    st.extend_from_slice(&Keccak256::digest(data));
    st.extend_from_slice(&[0u8; 32]); // operation = CALL
    st.extend_from_slice(&[0u8; 32]); // safeTxGas
    st.extend_from_slice(&[0u8; 32]); // baseGas
    st.extend_from_slice(&[0u8; 32]); // gasPrice
    st.extend_from_slice(&[0u8; 32]); // gasToken
    st.extend_from_slice(&[0u8; 32]); // refundReceiver
    st.extend_from_slice(&word_u64(nonce));
    let struct_hash = Keccak256::digest(&st);

    let mut pre = Vec::with_capacity(2 + 64);
    pre.extend_from_slice(&[0x19, 0x01]);
    pre.extend_from_slice(&domain);
    pre.extend_from_slice(&struct_hash);
    let mut out = [0u8; 32];
    out.copy_from_slice(&Keccak256::digest(&pre));
    out
}

/// Recover the EVM address that produced `sig` over `hash`.
///
/// `sig` is Safe's 65-byte owner encoding: r ‖ s ‖ v, with v ∈ {27, 28}. (Safe also accepts
/// v ∈ {31, 32} for the `eth_sign` prefixed variant and v ∈ {0, 1} for contract and
/// approved-hash signatures; the enclave produces the plain form, so only that is handled
/// here — an unexpected v is refused rather than silently reinterpreted.)
pub fn recover_owner(hash: &[u8; 32], sig: &[u8; 65]) -> Result<[u8; 20]> {
    let v = sig[64];
    if v != 27 && v != 28 {
        bail!("unexpected signature v={v}: expected the plain ECDSA form (27 or 28)");
    }
    let rec = k256::ecdsa::RecoveryId::from_byte(v - 27).context("bad recovery id")?;
    let signature = k256::ecdsa::Signature::from_slice(&sig[..64]).context("malformed r‖s")?;
    let vk = k256::ecdsa::VerifyingKey::recover_from_prehash(hash, &signature, rec)
        .context("signature does not recover")?;
    let pt = vk.to_encoded_point(false);
    let d = Keccak256::digest(&pt.as_bytes()[1..]);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&d[12..]);
    Ok(addr)
}

/// Concatenate owner signatures in the order `checkNSignatures` demands.
///
/// Returns the bytes to hand to `execTransaction`. Rejects a duplicate signer rather than
/// letting Base reject it: the same owner twice is how a naive implementation reaches
/// threshold with one key, and catching it here names the problem instead of returning
/// GS026 from a transaction that already cost gas.
pub fn order_signatures(hash: &[u8; 32], sigs: &[[u8; 65]]) -> Result<Vec<u8>> {
    if sigs.is_empty() {
        bail!("no owner signatures to order");
    }
    let mut with_owner: Vec<([u8; 20], &[u8; 65])> = Vec::with_capacity(sigs.len());
    for s in sigs {
        with_owner.push((recover_owner(hash, s)?, s));
    }
    with_owner.sort_by(|a, b| a.0.cmp(&b.0));
    if with_owner.windows(2).any(|w| w[0].0 == w[1].0) {
        bail!("the same owner signed twice — Safe counts distinct owners, not signatures");
    }
    let mut out = Vec::with_capacity(sigs.len() * 65);
    for (_, s) in &with_owner {
        out.extend_from_slice(*s);
    }
    Ok(out)
}

// ── Admin surface ───────────────────────────────────────────────────────────────
//
// Loopback-only, off by default, alongside the FROST round probe. This is the operator's
// tool for performing a governance action once the cluster has produced the owner
// signatures: it builds the calldata, orders the signatures the way `checkNSignatures`
// demands, and submits the self-call.
//
// It deliberately does NOT collect the quorum bundle or drive the enclaves — that is the
// ceremony, and it belongs with the operators rather than behind one node's admin port.

#[derive(serde::Deserialize)]
pub struct SafeExecRequest {
    /// The SafeTxHash the enclaves signed. Used to recover each signer, so the ordering
    /// cannot be faked by mislabelling.
    pub safe_tx_hash: String,
    /// 65-byte owner signatures, hex, in any order.
    pub signatures: Vec<String>,
    /// The calldata to execute, hex.
    ///
    /// Deliberately NOT an operation the caller picks. It comes from
    /// `/admin/safe/projection`, which derives the plan from the sealed membership — an
    /// operator choosing "add this owner" would be governing the Safe on the side, which
    /// is the whole thing this design exists to stop. The quorum bundle the enclaves
    /// required is over the hash of exactly these bytes, so a calldata that did not come
    /// from the plan cannot have been signed.
    pub data: String,
}

#[derive(serde::Serialize)]
pub struct SafeExecResponse {
    pub status: &'static str,
    pub tx_hash: String,
    pub owners_in_order: Vec<String>,
}

/// `POST /admin/safe/exec` — order the owner signatures and submit the self-call.
pub async fn handle_safe_exec(
    axum::Json(req): axum::Json<SafeExecRequest>,
) -> std::result::Result<axum::Json<SafeExecResponse>, (axum::http::StatusCode, String)> {
    let bad = |e: anyhow::Error| (axum::http::StatusCode::BAD_REQUEST, e.to_string());

    let h = hex::decode(req.safe_tx_hash.trim_start_matches("0x"))
        .map_err(|e| bad(anyhow::anyhow!("safe_tx_hash is not hex: {e}")))?;
    let hash: [u8; 32] = h
        .try_into()
        .map_err(|_| bad(anyhow::anyhow!("safe_tx_hash must be 32 bytes")))?;

    let data = hex::decode(req.data.trim_start_matches("0x"))
        .map_err(|e| bad(anyhow::anyhow!("data is not hex: {e}")))?;
    if data.is_empty() {
        return Err(bad(anyhow::anyhow!("data is empty")));
    }

    let mut sigs = Vec::with_capacity(req.signatures.len());
    for s in &req.signatures {
        let b = hex::decode(s.trim_start_matches("0x"))
            .map_err(|e| bad(anyhow::anyhow!("signature is not hex: {e}")))?;
        let one: [u8; 65] = b
            .try_into()
            .map_err(|_| bad(anyhow::anyhow!("each signature must be 65 bytes")))?;
        sigs.push(one);
    }

    let ordered = order_signatures(&hash, &sigs).map_err(bad)?;
    let owners_in_order = ordered
        .chunks_exact(65)
        .map(|c| {
            let mut one = [0u8; 65];
            one.copy_from_slice(c);
            recover_owner(&hash, &one).map(|a| format!("0x{}", hex::encode(a)))
        })
        .collect::<Result<Vec<_>>>()
        .map_err(bad)?;

    // Same configuration the publisher reads; there is no second way to hold these.
    let rpc = std::env::var("RESERVES_RPC_URL")
        .map_err(|_| bad(anyhow::anyhow!("RESERVES_RPC_URL not set")))?;
    let gas_key = std::env::var("RESERVES_GAS_KEY")
        .map_err(|_| bad(anyhow::anyhow!("RESERVES_GAS_KEY not set")))?;
    let safe = std::env::var("RESERVES_SAFE")
        .map_err(|_| bad(anyhow::anyhow!("RESERVES_SAFE not set")))?;

    let tx_hash = crate::commitment::submit_safe_selfcall(&rpc, &gas_key, &safe, data, ordered)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(axum::Json(SafeExecResponse {
        status: "submitted",
        tx_hash,
        owners_in_order,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};

    /// The four selectors, as literals, checked against the keccak of the signature string.
    ///
    /// Both halves are independent: the literal is what I believe Safe's ABI is, the
    /// computation is what our encoder will actually emit. If they disagree, one of the two
    /// is wrong and the test says so — which is the point. Asserting only the computation
    /// would re-derive our own bug; asserting only the literal would not test the encoder.
    #[test]
    fn selectors_match_the_known_safe_abi() {
        let cases: [(&str, [u8; 4]); 4] = [
            (
                "addOwnerWithThreshold(address,uint256)",
                [0x0d, 0x58, 0x2f, 0x13],
            ),
            (
                "removeOwner(address,address,uint256)",
                [0xf8, 0xdc, 0x5d, 0xd9],
            ),
            (
                "swapOwner(address,address,address)",
                [0xe3, 0x18, 0xb5, 0x2b],
            ),
            ("changeThreshold(uint256)", [0x69, 0x4e, 0x80, 0xc3]),
        ];
        for (sig, want) in cases {
            assert_eq!(selector(sig), want, "selector drift for {sig}");
        }
    }

    /// Safe's two EIP-712 typehashes, as published constants, checked against the keccak
    /// our derivation actually computes.
    ///
    /// Two independent halves again: the literals are what Safe's own source documents,
    /// the keccak is what we will hash with. Asserting only the computation would re-derive
    /// our own mistake; asserting only the literal would not test the derivation. If they
    /// disagree, one of the two is wrong and the test says which pair.
    #[test]
    fn the_eip712_typehashes_match_safes_published_constants() {
        let domain = Keccak256::digest(b"EIP712Domain(uint256 chainId,address verifyingContract)");
        assert_eq!(
            hex::encode(domain),
            "47e79534a245952e8b16893a336b85a3d9ea9fa8c573f3d803afb92a79469218",
            "DOMAIN_TYPEHASH"
        );
        let safetx = Keccak256::digest(
            b"SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)",
        );
        assert_eq!(
            hex::encode(safetx),
            "bb8310d486368db6bd6f849402fdd73ad53d316b5a4b2644ad6efe0f941286d8",
            "SAFE_TX_TYPEHASH"
        );
    }

    #[test]
    fn every_field_of_the_safe_tx_hash_actually_binds() {
        // A hash that ignored any of these would let a bundle authorising one transaction
        // be replayed for another — a different Safe, a different chain, a different call,
        // or the same call twice.
        let safe = [0x11u8; 20];
        let to = [0x22u8; 20];
        let base = safe_tx_hash(&safe, 84532, &to, b"\x01\x02", 7);

        let mut other_safe = safe;
        other_safe[0] ^= 1;
        assert_ne!(
            base,
            safe_tx_hash(&other_safe, 84532, &to, b"\x01\x02", 7),
            "safe"
        );
        assert_ne!(
            base,
            safe_tx_hash(&safe, 1, &to, b"\x01\x02", 7),
            "chain_id"
        );
        let mut other_to = to;
        other_to[0] ^= 1;
        assert_ne!(
            base,
            safe_tx_hash(&safe, 84532, &other_to, b"\x01\x02", 7),
            "to"
        );
        assert_ne!(
            base,
            safe_tx_hash(&safe, 84532, &to, b"\x01\x03", 7),
            "data"
        );
        assert_ne!(
            base,
            safe_tx_hash(&safe, 84532, &to, b"\x01\x02", 8),
            "nonce"
        );
    }

    #[test]
    fn calldata_is_selector_plus_one_word_per_argument() {
        let owner = [0xAAu8; 20];
        let d = SafeOp::AddOwner {
            owner,
            threshold: 2,
        }
        .calldata();
        assert_eq!(d.len(), 4 + 64, "selector + 2 words");
        assert_eq!(&d[0..4], &[0x0d, 0x58, 0x2f, 0x13]);
        assert_eq!(&d[4..16], &[0u8; 12], "address is left-padded");
        assert_eq!(&d[16..36], &owner);
        assert_eq!(d[67], 2, "threshold in the low byte of the second word");

        assert_eq!(
            SafeOp::ChangeThreshold { threshold: 2 }.calldata().len(),
            4 + 32
        );
        assert_eq!(
            SafeOp::SwapOwner {
                prev_owner: [1; 20],
                old_owner: [2; 20],
                new_owner: [3; 20]
            }
            .calldata()
            .len(),
            4 + 96
        );
    }

    fn sign(key: &SigningKey, hash: &[u8; 32]) -> [u8; 65] {
        let (sig, rec): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) =
            key.sign_prehash(hash).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = 27 + rec.to_byte();
        out
    }

    fn addr_of(key: &SigningKey) -> [u8; 20] {
        let pt = key.verifying_key().to_encoded_point(false);
        let d = Keccak256::digest(&pt.as_bytes()[1..]);
        let mut a = [0u8; 20];
        a.copy_from_slice(&d[12..]);
        a
    }

    #[test]
    fn recover_owner_returns_the_signing_address() {
        let key = SigningKey::from_slice(&[0x11; 32]).unwrap();
        let hash = [0x42u8; 32];
        let sig = sign(&key, &hash);
        assert_eq!(recover_owner(&hash, &sig).unwrap(), addr_of(&key));
    }

    #[test]
    fn recover_owner_refuses_an_unexpected_v() {
        // Safe also accepts v of 0, 1, 31 and 32 for other signature kinds. The enclave
        // emits only the plain form, so anything else is refused rather than reinterpreted
        // as a contract or approved-hash signature.
        let key = SigningKey::from_slice(&[0x11; 32]).unwrap();
        let hash = [0x42u8; 32];
        let mut sig = sign(&key, &hash);
        for bad_v in [0u8, 1, 26, 29, 31, 32] {
            sig[64] = bad_v;
            assert!(
                recover_owner(&hash, &sig).is_err(),
                "v={bad_v} must be refused"
            );
        }
    }

    #[test]
    fn signatures_come_out_in_ascending_owner_order_whatever_order_they_went_in() {
        // This is the test that would have caught shipping the unsorted concatenation:
        // every signature is individually valid, so nothing else notices.
        let keys: Vec<SigningKey> = (1u8..=3)
            .map(|i| SigningKey::from_slice(&[i; 32]).unwrap())
            .collect();
        let hash = [0x7au8; 32];
        let sigs: Vec<[u8; 65]> = keys.iter().map(|k| sign(k, &hash)).collect();

        // Feed them in the worst order for the property: descending by address.
        let mut worst: Vec<([u8; 20], [u8; 65])> = keys
            .iter()
            .zip(sigs.iter())
            .map(|(k, s)| (addr_of(k), *s))
            .collect();
        worst.sort_by(|a, b| b.0.cmp(&a.0));
        let input: Vec<[u8; 65]> = worst.iter().map(|(_, s)| *s).collect();

        let blob = order_signatures(&hash, &input).unwrap();
        assert_eq!(blob.len(), 3 * 65);

        let mut last = [0u8; 20];
        for i in 0..3 {
            let mut one = [0u8; 65];
            one.copy_from_slice(&blob[i * 65..(i + 1) * 65]);
            let owner = recover_owner(&hash, &one).unwrap();
            assert!(
                owner > last,
                "checkNSignatures requires strictly ascending owners; got {owner:?} after {last:?}"
            );
            last = owner;
        }
    }

    #[test]
    fn the_same_owner_twice_is_refused() {
        // Safe counts distinct owners, not signatures. Catching it here names the problem
        // instead of paying gas for a GS026 revert.
        let key = SigningKey::from_slice(&[0x11; 32]).unwrap();
        let hash = [0x42u8; 32];
        let sig = sign(&key, &hash);
        let err = order_signatures(&hash, &[sig, sig]).unwrap_err();
        assert!(err.to_string().contains("signed twice"), "got: {err}");
    }

    #[test]
    fn an_empty_signature_set_is_refused() {
        assert!(order_signatures(&[0u8; 32], &[]).is_err());
    }
}
