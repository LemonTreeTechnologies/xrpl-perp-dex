//! The Base Safe owner set as a PROJECTION of the sealed cluster membership.
//!
//! # Why this exists
//!
//! On XRPL the SignerList is a downstream projection of the sealed authority:
//! `membership_projection.rs` renders it as a pure function of the sealed set, tracks
//! `projection_confirmed_epoch`, and reports `in_sync`. The Base Safe's owner set is the
//! *same fact on a second chain* — and we never built it as a projection. It was set once
//! at genesis and left there.
//!
//! That is why a 1-of-1 Safe survived a month unnoticed while the cluster had three
//! members: on XRPL a divergence raises `in_sync = false` and the sync-before-spend gate
//! notices. On Base there was nothing to diverge *from*, because no code held an opinion
//! about what the owner set should be.
//!
//! So this module answers one question — **what owner set does the sealed membership
//! imply?** — and a second that follows from it: what is the smallest sequence of Safe
//! operations that makes the chain agree.
//!
//! # Why the derivation is verifiable rather than trusted
//!
//! The sealed set names members by XRPL AccountID (`RIPEMD160(SHA256(pubkey))`); the Safe
//! names them by EVM address (`keccak256(uncompressed_pubkey)[12..]`). Neither derives from
//! the other — both are one-way from the key. The bridge is the key itself: from a member's
//! compressed pubkey this module derives BOTH, matches the AccountID against the sealed set,
//! and cross-checks the EVM address against the one the config declares.
//!
//! Nothing here is trusted. A config entry that disagrees with its own public key is
//! refused, and every node computes the same projection from the same sealed set — so a
//! co-signer signs a hash it derived itself rather than one it was handed.
//!
//! # The secp256k1 precondition, and why it holds by failing closed
//!
//! The binding needs every member to be secp256k1: an ed25519 account has an AccountID but
//! no EVM address at all, since `keccak` of an ed25519 key is not an `ecrecover` address.
//!
//! I looked for a reject at membership-sealing time and there is none — `sealed_signer_entry_t`
//! stores only `account_id[20]`, a hash, and does not know the key type. The ed25519 rejects
//! in the enclave (`xrpl_multisig_verify.cpp:121`, `path_a.cpp:1584`) are on SIGNERS
//! presenting a key, not on membership entries.
//!
//! So the guarantee is not "it cannot be sealed" but **"it cannot be projected"**: an
//! ed25519 key is not a valid SEC1 point, `identity_from_pubkey` refuses it, no known member
//! matches that AccountID, and `project_owner_set` stops rather than deriving an address.
//! The failure direction is refusal, never a wrong owner. Tested below rather than reasoned.

use crate::safe_governance::SafeOp;
use anyhow::{bail, Context, Result};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use ripemd::Ripemd160;
use sha2::Sha256;
use sha3::{Digest, Keccak256};

/// Safe's owner linked-list sentinel. `owners[SENTINEL]` is the head, and the last owner
/// points back at it, so the predecessor of the first owner is the sentinel rather than a
/// real address.
pub const SENTINEL_OWNERS: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// One cluster member, as each chain names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberIdentity {
    pub name: String,
    /// How the SEALED membership names them.
    pub account_id: [u8; 20],
    /// How the Safe names them.
    pub evm_address: [u8; 20],
}

fn decompress(pubkey33: &[u8]) -> Result<Vec<u8>> {
    let pk = k256::PublicKey::from_sec1_bytes(pubkey33).context("not a valid secp256k1 point")?;
    Ok(pk.to_encoded_point(false).as_bytes().to_vec())
}

/// Derive both identities from the member's compressed public key, and refuse a config that
/// contradicts its own key.
///
/// The cross-check matters more than it looks: `address` in `signers_config.json` is what
/// every other part of the system uses as the Safe owner. If it ever drifted from the key —
/// a copy-paste, a stale entry after a re-key — the projection would target an address
/// nobody can sign for, and the Safe would end up with an owner that is not a cluster
/// member. Deriving it and comparing costs nothing and makes that unrepresentable.
pub fn identity_from_pubkey(
    name: &str,
    compressed_pubkey_hex: &str,
    declared_evm: &str,
) -> Result<MemberIdentity> {
    let pk = hex::decode(compressed_pubkey_hex.trim_start_matches("0x"))
        .with_context(|| format!("{name}: compressed_pubkey is not hex"))?;
    if pk.len() != 33 {
        bail!(
            "{name}: compressed_pubkey must be 33 bytes, got {}",
            pk.len()
        );
    }

    // XRPL AccountID = RIPEMD160(SHA256(compressed pubkey)).
    let account_id: [u8; 20] = Ripemd160::digest(Sha256::digest(&pk)).into();

    // EVM address = keccak256(uncompressed pubkey without the 0x04 tag)[12..].
    // The context is worded so it CANNOT be confused with the address cross-check below.
    // A test asserting only "pubkey" would pass on that other error too, because its text
    // names `compressed_pubkey` — a hollow assertion my own mutation probe caught.
    let uncompressed = decompress(&pk)
        .with_context(|| format!("{name}: not a projectable key (must be a secp256k1 point)"))?;
    let d = Keccak256::digest(&uncompressed[1..]);
    let mut evm_address = [0u8; 20];
    evm_address.copy_from_slice(&d[12..]);

    let declared = hex::decode(declared_evm.trim_start_matches("0x"))
        .with_context(|| format!("{name}: declared address is not hex"))?;
    if declared.len() != 20 || declared[..] != evm_address[..] {
        bail!(
            "{name}: signers_config address 0x{} does not match its own compressed_pubkey \
             (derives to 0x{}) — the config contradicts its key",
            hex::encode(&declared),
            hex::encode(evm_address)
        );
    }

    Ok(MemberIdentity {
        name: name.to_string(),
        account_id,
        evm_address,
    })
}

/// The owner set the sealed membership implies: exactly the sealed members, as EVM
/// addresses.
///
/// Refuses if any sealed member has no known key. That refusal is the point — a projection
/// that silently dropped an unmatched member would quietly propose removing a real cluster
/// member from the Safe.
pub fn project_owner_set(
    sealed_members: &[[u8; 20]],
    known: &[MemberIdentity],
) -> Result<Vec<[u8; 20]>> {
    if sealed_members.is_empty() {
        bail!("sealed membership is empty — nothing to project");
    }
    let mut out = Vec::with_capacity(sealed_members.len());
    for m in sealed_members {
        match known.iter().find(|k| &k.account_id == m) {
            Some(k) => out.push(k.evm_address),
            None => bail!(
                "sealed member {} has no known public key — cannot project it onto the Safe",
                hex::encode(m)
            ),
        }
    }
    out.sort();
    Ok(out)
}

/// Does the chain already agree with the authority?
pub fn in_sync(
    current_owners: &[[u8; 20]],
    current_threshold: u64,
    target_owners: &[[u8; 20]],
    target_threshold: u64,
) -> bool {
    let mut a = current_owners.to_vec();
    let mut b = target_owners.to_vec();
    a.sort();
    b.sort();
    a == b && current_threshold == target_threshold
}

/// The smallest sequence of Safe operations that makes the owner set and threshold match.
///
/// `current_owners` must be in the order `getOwners()` returns, which is the linked-list
/// order — `removeOwner` and `swapOwner` need the predecessor, and the predecessor of the
/// first owner is the sentinel.
///
/// Ordering rules, both forced by Safe's own requires rather than chosen:
///   * adds first — `addOwnerWithThreshold` only grows the count, so it can never make the
///     standing threshold unsatisfiable;
///   * a LOWERED threshold moves to the front and a RAISED one to the back, so the
///     threshold is at its minimum while removals happen. `removeOwner` requires
///     `ownerCount - 1 >= _threshold`; leaving a raise until last is what keeps a shrink
///     from tripping it.
pub fn plan_reconciliation(
    current_owners: &[[u8; 20]],
    current_threshold: u64,
    target_owners: &[[u8; 20]],
    target_threshold: u64,
) -> Result<Vec<SafeOp>> {
    if target_owners.is_empty() {
        bail!("refusing to plan an empty owner set — the Safe would be unusable");
    }
    if target_threshold == 0 || target_threshold as usize > target_owners.len() {
        bail!(
            "target threshold {target_threshold} outside 1..={}",
            target_owners.len()
        );
    }

    let mut plan = Vec::new();
    // Simulated list, kept in linked-list order so each predecessor is right at the moment
    // its operation runs rather than at planning time.
    let mut list: Vec<[u8; 20]> = current_owners.to_vec();

    if target_threshold < current_threshold {
        plan.push(SafeOp::ChangeThreshold {
            threshold: target_threshold,
        });
    }
    let working_threshold = target_threshold.min(current_threshold);

    let mut to_add: Vec<[u8; 20]> = target_owners
        .iter()
        .filter(|w| !list.contains(w))
        .copied()
        .collect();
    let mut to_remove: Vec<[u8; 20]> = current_owners
        .iter()
        .filter(|h| !target_owners.contains(h))
        .copied()
        .collect();

    // Pair a removal with an addition into a SWAP wherever both exist.
    //
    // Not an optimisation. `removeOwner` requires `ownerCount - 1 >= _threshold`, so a
    // cluster running at count == threshold (three owners, threshold three) cannot remove
    // anyone at all — the plan would be impossible. `swapOwner` keeps the count constant
    // and replaces in place, which is the only way that case is expressible. The dead-code
    // check found this: I had defined SwapOwner and never emitted it, which meant the
    // planner silently could not handle a set the cluster can legitimately be in.
    while !to_add.is_empty() && !to_remove.is_empty() {
        let old = to_remove.remove(0);
        let new_owner = to_add.remove(0);
        let idx = list
            .iter()
            .position(|o| *o == old)
            .context("internal: owner vanished from the simulated list")?;
        let prev = if idx == 0 {
            SENTINEL_OWNERS
        } else {
            list[idx - 1]
        };
        plan.push(SafeOp::SwapOwner {
            prev_owner: prev,
            old_owner: old,
            new_owner,
        });
        list[idx] = new_owner; // swap is in place: position and count both unchanged
    }

    for want in &to_add {
        plan.push(SafeOp::AddOwner {
            owner: *want,
            threshold: working_threshold,
        });
        list.insert(0, *want); // Safe pushes a new owner at the head
    }

    for have in &to_remove {
        let idx = list
            .iter()
            .position(|o| o == have)
            .context("internal: owner vanished from the simulated list")?;
        let prev = if idx == 0 {
            SENTINEL_OWNERS
        } else {
            list[idx - 1]
        };
        if (list.len() as u64) - 1 < working_threshold {
            bail!(
                "removing {} would leave {} owners under threshold {working_threshold}",
                hex::encode(have),
                list.len() - 1
            );
        }
        plan.push(SafeOp::RemoveOwner {
            prev_owner: prev,
            owner: *have,
            threshold: working_threshold,
        });
        list.remove(idx);
    }

    if target_threshold > current_threshold {
        plan.push(SafeOp::ChangeThreshold {
            threshold: target_threshold,
        });
    }

    Ok(plan)
}

// ── Admin surface: what does the sealed membership imply, and are we in sync? ──────
//
// This is the endpoint that makes the Safe a tracked projection instead of a thing someone
// remembers to update. It answers the same question `membership_projection` answers for
// XRPL: the authority has a set, the chain has a set, do they agree — and if not, what is
// the exact sequence that makes them agree.
//
// The operator does not choose an operation here. The plan is derived.

#[derive(serde::Deserialize)]
pub struct ProjectionRequest {
    /// `getOwners()` as the chain returns it — the LINKED-LIST order, which is what makes
    /// each `prevOwner` correct. Sorting it before sending would silently corrupt removals.
    pub current_owners: Vec<String>,
    pub current_threshold: u64,
    /// Loopback URL of THIS node's enclave. The sealed membership is read from it, not
    /// taken from this request — a caller that supplied both the "sealed" set and the keys
    /// could otherwise claim any membership and converge the Safe onto it.
    pub enclave_url: String,
    /// The cluster's known keys: (name, compressed_pubkey, declared EVM address).
    pub known_members: Vec<KnownMember>,
    /// The threshold the authority wants. Today 1 (publishing is Tier-1 single-enclave);
    /// it becomes 2 when state replication makes a second signature non-blind.
    pub target_threshold: u64,
}

#[derive(serde::Deserialize)]
pub struct KnownMember {
    pub name: String,
    pub compressed_pubkey: String,
    pub address: String,
}

#[derive(serde::Serialize)]
pub struct PlannedStep {
    pub op: String,
    /// The calldata to hand to `/admin/safe/exec` — the operator relays it, never composes it.
    pub data: String,
}

#[derive(serde::Serialize)]
pub struct ProjectionResponse {
    pub in_sync: bool,
    pub target_owners: Vec<String>,
    pub target_threshold: u64,
    pub plan: Vec<PlannedStep>,
}

fn addr20(s: &str) -> Result<[u8; 20]> {
    let b = hex::decode(s.trim_start_matches("0x")).context("not hex")?;
    if b.len() != 20 {
        bail!("expected 20 bytes, got {}", b.len());
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&b);
    Ok(a)
}

/// Read the membership THIS enclave has sealed.
///
/// Provenance is the whole point of the call: the projection must be derived from what the
/// authority sealed, never from a list handed in alongside the keys. `bootstrapped: false`
/// is a soft state upstream, and it is an error here — you cannot project a membership that
/// does not exist yet, and proceeding would compute a target of nothing.
async fn fetch_sealed_members(enclave_url: &str) -> Result<Vec<String>> {
    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("{enclave_url}/v1/admin/signerlist/members"))
        .send()
        .await
        .context("ask the enclave for its sealed membership")?
        .error_for_status()
        .context("enclave refused the membership read")?
        .json()
        .await
        .context("parse the enclave's membership response")?;
    if v["bootstrapped"].as_bool() != Some(true) {
        bail!("this enclave has no sealed membership yet — nothing to project");
    }
    let arr = v["members"]
        .as_array()
        .context("membership response has no `members` array")?;
    if arr.is_empty() {
        bail!("the enclave reported an empty membership");
    }
    arr.iter()
        .map(|m| {
            m.as_str()
                .map(|s| s.to_string())
                .context("a member entry is not a string")
        })
        .collect()
}

/// Read THIS node's own signers config.
///
/// Not a request field, deliberately. The whole point of independent derivation is that a
/// node uses its own view — its own enclave's sealed membership and its own record of the
/// members' keys. Accepting the key map from the caller would mean every node could be
/// steered to the same wrong answer by whoever assembled the request, which is the blind
/// co-signing this exists to prevent.
fn own_known_members() -> Result<Vec<MemberIdentity>> {
    let path = std::env::var("SIGNERS_CONFIG").unwrap_or_else(|_| "signers_config.json".into());
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read this node's signers config at {path}"))?;
    let v: serde_json::Value = serde_json::from_str(&raw).context("parse signers config")?;
    let mut out = Vec::new();
    let mut push = |e: &serde_json::Value| -> Result<()> {
        let (Some(name), Some(pk), Some(addr)) = (
            e.get("name").and_then(|x| x.as_str()),
            e.get("compressed_pubkey").and_then(|x| x.as_str()),
            e.get("address").and_then(|x| x.as_str()),
        ) else {
            return Ok(()); // an entry without keys is not a projectable member
        };
        out.push(identity_from_pubkey(name, pk, addr)?);
        Ok(())
    };
    if let Some(l) = v.get("local_signer") {
        push(l)?;
    }
    if let Some(arr) = v.get("signers").and_then(|x| x.as_array()) {
        for e in arr {
            push(e)?;
        }
    }
    if out.is_empty() {
        bail!("this node's signers config lists no projectable members");
    }
    Ok(out)
}

/// The canonical digest of an owner set, as the confirmation quorum signs it.
///
/// Sorted and domain-tagged: sorted so every node hashes the same bytes regardless of the
/// order the chain or the config happened to give, tagged so this digest cannot be mistaken
/// for any other use of the shared quorum primitive.
///
/// SHA-256 to match the family the enclave's own confirmation digest uses
/// (`kBaseProjDomain`), which keeps both sides of the comparison in one hash family.
pub fn owner_set_hash(owners: &[[u8; 20]]) -> [u8; 32] {
    let mut sorted = owners.to_vec();
    sorted.sort();
    let mut h = Sha256::new();
    h.update(b"perp.base-owner-set.v1");
    for o in &sorted {
        h.update(o);
    }
    h.finalize().into()
}

/// What this node is willing to attest: that the Safe's owner set ON CHAIN matches the
/// projection of the membership THIS enclave has sealed.
///
/// This is the function that makes the confirmation quorum non-blind. `ecall_perp_record_
/// base_projection` takes `owner_set_hash` as a parameter and trusts the quorum to have
/// verified it — so every attesting node must have derived it from its own sealed
/// membership and compared it against its own read of the chain. A node that skips this and
/// signs a hash it was handed turns the in_sync gate into a correct gate on a blind quorum.
///
/// Refuses on mismatch rather than reporting it, because the only use of the result is to
/// be signed: returning "here is the hash, but the chain disagrees" invites a caller to sign
/// it anyway.
pub async fn attest_projection(
    enclave_url: &str,
    rpc_url: &str,
    safe: &str,
    target_threshold: u64,
) -> Result<(u64, [u8; 32])> {
    let sealed_hex = fetch_sealed_members(enclave_url).await?;
    let sealed: Vec<[u8; 20]> = sealed_hex
        .iter()
        .map(|s| addr20(s))
        .collect::<Result<_>>()?;
    let known = own_known_members()?;
    let target = project_owner_set(&sealed, &known)?;
    let epoch = fetch_authority_epoch(enclave_url).await?;

    let (current, current_threshold) = crate::commitment::read_safe_owners(rpc_url, safe).await?;
    if !in_sync(&current, current_threshold, &target, target_threshold) {
        let mut c = current.clone();
        c.sort();
        bail!(
            "refusing to attest: the Safe's owner set does not match this node's projection \
             of epoch {epoch}. on chain {:?} at threshold {current_threshold}; projected {:?} \
             at threshold {target_threshold}",
            c.iter().map(hex::encode).collect::<Vec<_>>(),
            target.iter().map(hex::encode).collect::<Vec<_>>()
        );
    }
    Ok((epoch, owner_set_hash(&target)))
}

/// One convergence step, derived entirely from what THIS node can see.
#[derive(Clone, Debug)]
pub struct DerivedStep {
    pub op: SafeOp,
    pub calldata: Vec<u8>,
    pub safe_tx_hash: [u8; 32],
    pub plan_len: usize,
}

/// Derive convergence step `step_index` from this node's own enclave and its own read of
/// the chain.
///
/// Nothing about the step's CONTENT is an input: not the calldata, not the hash, not the
/// owner set. The caller says only WHICH step of the plan it wants. Every node handed the
/// same (epoch, step_index, nonce) computes the same bytes, or it computes different bytes
/// and its quorum contribution simply does not match — which is the entire mechanism that
/// makes an opaque-hash quorum non-blind.
#[allow(clippy::too_many_arguments)]
pub async fn derive_step(
    enclave_url: &str,
    rpc_url: &str,
    safe: &str,
    chain_id: u64,
    expected_epoch: u64,
    target_threshold: u64,
    step_index: usize,
    safe_nonce: u64,
) -> Result<DerivedStep> {
    let sealed_hex = fetch_sealed_members(enclave_url).await?;
    let sealed: Vec<[u8; 20]> = sealed_hex
        .iter()
        .map(|s| addr20(s))
        .collect::<Result<_>>()?;
    let known = own_known_members()?;
    let target = project_owner_set(&sealed, &known)?;

    let (current, current_threshold) = crate::commitment::read_safe_owners(rpc_url, safe).await?;
    let plan = plan_reconciliation(&current, current_threshold, &target, target_threshold)?;
    if plan.is_empty() {
        bail!("already in sync — nothing to converge");
    }
    let op = plan
        .get(step_index)
        .with_context(|| {
            format!(
                "step {step_index} is past the end of a {}-step plan",
                plan.len()
            )
        })?
        .clone();

    // The epoch the caller believes it is converging to must be the one this node has
    // sealed. A mismatch means the cluster is not agreeing about WHICH membership this
    // step serves, and signing anyway would attach a quorum to the wrong question.
    let epoch = fetch_authority_epoch(enclave_url).await?;
    if epoch != expected_epoch {
        bail!("this node's sealed authority epoch is {epoch}, caller expected {expected_epoch}");
    }

    let safe_bytes = addr20(safe)?;
    let calldata = op.calldata();
    // `to` is the Safe itself: owner management is a self-call, and the enclave derives its
    // hash the same way with no `to` parameter at all.
    let safe_tx_hash = crate::safe_governance::safe_tx_hash(
        &safe_bytes,
        chain_id,
        &safe_bytes,
        &calldata,
        safe_nonce,
    );

    Ok(DerivedStep {
        op,
        calldata,
        safe_tx_hash,
        plan_len: plan.len(),
    })
}

/// The authority epoch this node has sealed.
async fn fetch_authority_epoch(enclave_url: &str) -> Result<u64> {
    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("{enclave_url}/v1/admin/signerlist/members"))
        .send()
        .await
        .context("ask the enclave for its authority epoch")?
        .error_for_status()
        .context("enclave refused the membership read")?
        .json()
        .await
        .context("parse the enclave's membership response")?;
    v["authority_epoch"]
        .as_u64()
        .context("membership response has no authority_epoch")
}

/// `POST /admin/safe/projection`
pub async fn handle_projection(
    axum::Json(req): axum::Json<ProjectionRequest>,
) -> std::result::Result<axum::Json<ProjectionResponse>, (axum::http::StatusCode, String)> {
    let bad = |e: anyhow::Error| (axum::http::StatusCode::BAD_REQUEST, e.to_string());

    let current: Vec<[u8; 20]> = req
        .current_owners
        .iter()
        .map(|s| addr20(s))
        .collect::<Result<_>>()
        .map_err(bad)?;
    // The enclave surface is loopback-only by construction; refuse to be aimed elsewhere.
    if !(req.enclave_url.contains("127.0.0.1") || req.enclave_url.contains("localhost")) {
        return Err(bad(anyhow::anyhow!("enclave_url must be loopback")));
    }
    let sealed_hex = fetch_sealed_members(&req.enclave_url)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e.to_string()))?;
    let sealed: Vec<[u8; 20]> = sealed_hex
        .iter()
        .map(|s| addr20(s))
        .collect::<Result<_>>()
        .map_err(bad)?;

    let known: Vec<MemberIdentity> = req
        .known_members
        .iter()
        .map(|m| identity_from_pubkey(&m.name, &m.compressed_pubkey, &m.address))
        .collect::<Result<_>>()
        .map_err(bad)?;

    let target = project_owner_set(&sealed, &known).map_err(bad)?;
    let synced = in_sync(
        &current,
        req.current_threshold,
        &target,
        req.target_threshold,
    );
    let plan = plan_reconciliation(
        &current,
        req.current_threshold,
        &target,
        req.target_threshold,
    )
    .map_err(bad)?;

    Ok(axum::Json(ProjectionResponse {
        in_sync: synced,
        target_owners: target
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect(),
        target_threshold: req.target_threshold,
        plan: plan
            .iter()
            .map(|op| PlannedStep {
                op: op.signature().to_string(),
                data: format!("0x{}", hex::encode(op.calldata())),
            })
            .collect(),
    }))
}

// ── The two endpoints that make derivation independent ──────────────────────────
//
// Note what neither request carries: no calldata, no SafeTxHash, no owner set. The caller
// says WHICH step or WHICH epoch; every node computes the content itself. `deny_unknown_
// fields` is there so a caller who believes it is supplying content gets an error instead of
// having it silently ignored — a silent drop would look like it worked.

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeriveStepRequest {
    pub enclave_url: String,
    pub rpc_url: String,
    pub safe: String,
    pub chain_id: u64,
    pub epoch: u64,
    pub target_threshold: u64,
    pub step_index: usize,
    pub safe_nonce: u64,
}

#[derive(serde::Serialize)]
pub struct DeriveStepResponse {
    pub op: String,
    pub calldata: String,
    pub safe_tx_hash: String,
    pub plan_len: usize,
}

/// `POST /admin/safe/derive-step`
pub async fn handle_derive_step(
    axum::Json(req): axum::Json<DeriveStepRequest>,
) -> std::result::Result<axum::Json<DeriveStepResponse>, (axum::http::StatusCode, String)> {
    if !(req.enclave_url.contains("127.0.0.1") || req.enclave_url.contains("localhost")) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "enclave_url must be loopback".to_string(),
        ));
    }
    let d = derive_step(
        &req.enclave_url,
        &req.rpc_url,
        &req.safe,
        req.chain_id,
        req.epoch,
        req.target_threshold,
        req.step_index,
        req.safe_nonce,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(axum::Json(DeriveStepResponse {
        op: d.op.signature().to_string(),
        calldata: format!("0x{}", hex::encode(&d.calldata)),
        safe_tx_hash: format!("0x{}", hex::encode(d.safe_tx_hash)),
        plan_len: d.plan_len,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttestRequest {
    pub enclave_url: String,
    pub rpc_url: String,
    pub safe: String,
    pub target_threshold: u64,
}

#[derive(serde::Serialize)]
pub struct AttestResponse {
    pub epoch: u64,
    pub owner_set_hash: String,
}

/// `POST /admin/safe/attest-projection` — what this node is willing to sign into the
/// confirmation bundle, after checking the chain against its own projection.
pub async fn handle_attest(
    axum::Json(req): axum::Json<AttestRequest>,
) -> std::result::Result<axum::Json<AttestResponse>, (axum::http::StatusCode, String)> {
    if !(req.enclave_url.contains("127.0.0.1") || req.enclave_url.contains("localhost")) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "enclave_url must be loopback".to_string(),
        ));
    }
    let (epoch, h) = attest_projection(
        &req.enclave_url,
        &req.rpc_url,
        &req.safe,
        req.target_threshold,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(axum::Json(AttestResponse {
        epoch,
        owner_set_hash: format!("0x{}", hex::encode(h)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> [u8; 20] {
        let mut x = [0u8; 20];
        x[19] = n;
        x
    }

    // ── the derivation is verifiable ────────────────────────────────────

    /// A real secp256k1 key, so both derivations run on a genuine point rather than a
    /// hand-made byte string that could not exist on the curve.
    fn real_key() -> (String, String) {
        let sk = k256::SecretKey::from_slice(&[0x11; 32]).unwrap();
        let comp = hex::encode(sk.public_key().to_sec1_bytes());
        let unc = sk.public_key().to_encoded_point(false);
        let d = Keccak256::digest(&unc.as_bytes()[1..]);
        (comp, format!("0x{}", hex::encode(&d[12..])))
    }

    #[test]
    fn identity_derives_both_names_from_one_key() {
        let (comp, evm) = real_key();
        let id = identity_from_pubkey("node-1", &comp, &evm).unwrap();
        assert_eq!(format!("0x{}", hex::encode(id.evm_address)), evm);
        // The AccountID must be the XRPL construction, not anything else 20 bytes long.
        let pk = hex::decode(&comp).unwrap();
        let want: [u8; 20] = Ripemd160::digest(Sha256::digest(&pk)).into();
        assert_eq!(id.account_id, want);
    }

    #[test]
    fn a_config_that_contradicts_its_own_key_is_refused() {
        // The failure this prevents: a stale or mistyped `address` would make the
        // projection target an address nobody in the cluster can sign for.
        let (comp, evm) = real_key();
        let mut wrong = hex::decode(evm.trim_start_matches("0x")).unwrap();
        wrong[0] ^= 0x01;
        let err = identity_from_pubkey("node-1", &comp, &format!("0x{}", hex::encode(&wrong)))
            .unwrap_err();
        assert!(
            err.to_string().contains("contradicts its key"),
            "got: {err}"
        );
    }

    #[test]
    fn an_ed25519_key_cannot_be_turned_into_a_safe_owner() {
        // The precondition the whole no-schema-change binding rests on. An ed25519 account
        // has an AccountID but no EVM address, so if one could be projected we would derive
        // a meaningless owner. It cannot: 0xED is not a SEC1 prefix, so the point parse
        // fails and the member is refused.
        let mut ed = vec![0xEDu8];
        ed.extend_from_slice(&[0x11u8; 32]); // 33 bytes, XRPL's ed25519 wire form
        let err = identity_from_pubkey(
            "node-ed",
            &hex::encode(&ed),
            "0x0000000000000000000000000000000000000000",
        )
        .unwrap_err();
        // Assert WHERE it failed, not just that it did: the context must name the pubkey
        // stage. Failing on the address cross-check instead would mean the key had been
        // accepted and some address derived from it — the outcome this test exists to
        // exclude. (k256 words its own error "crypto error"; the stage is ours.)
        assert!(
            err.to_string().contains("not a projectable key"),
            "must fail deriving from the KEY, not later on the address: {err:#}"
        );
    }

    #[test]
    fn an_ed25519_member_refuses_the_projection_rather_than_deriving_a_wrong_owner() {
        // End to end: a sealed member whose key is ed25519 has no projectable address, so
        // the projection STOPS. The failure direction is what matters — never a wrong owner.
        let (comp, evm) = real_key();
        let good = identity_from_pubkey("node-1", &comp, &evm).unwrap();
        let mut ed = vec![0xEDu8];
        ed.extend_from_slice(&[0x22u8; 32]);
        let ed_account: [u8; 20] = Ripemd160::digest(Sha256::digest(&ed)).into();
        let err = project_owner_set(&[good.account_id, ed_account], std::slice::from_ref(&good))
            .unwrap_err();
        assert!(
            err.to_string().contains("no known public key"),
            "got: {err}"
        );
    }

    #[test]
    fn a_sealed_member_with_no_known_key_stops_the_projection() {
        // Silently dropping it would propose REMOVING a real cluster member from the Safe.
        let (comp, evm) = real_key();
        let id = identity_from_pubkey("node-1", &comp, &evm).unwrap();
        let err = project_owner_set(&[id.account_id, a(9)], std::slice::from_ref(&id)).unwrap_err();
        assert!(
            err.to_string().contains("no known public key"),
            "got: {err}"
        );
    }

    // ── in_sync ─────────────────────────────────────────────────────────

    #[test]
    fn the_owner_set_digest_is_order_independent_but_content_sensitive() {
        // Every node must hash the same bytes whatever order its chain read or its config
        // produced, or the confirmation quorum can never assemble even when all nodes agree.
        assert_eq!(
            owner_set_hash(&[a(1), a(2), a(3)]),
            owner_set_hash(&[a(3), a(1), a(2)])
        );
        assert_ne!(
            owner_set_hash(&[a(1), a(2)]),
            owner_set_hash(&[a(1), a(2), a(3)])
        );
        assert_ne!(owner_set_hash(&[a(1), a(2)]), owner_set_hash(&[a(1), a(4)]));

        // Domain-separated: the digest must NOT be a bare hash of the concatenated owners.
        // The tag is what stops a value minted for this purpose from colliding with any
        // other use of the shared quorum primitive — a property neither of the assertions
        // above touches, which a mutation probe showed by deleting the tag and staying
        // green.
        let untagged: [u8; 32] = {
            let mut h = sha2::Sha256::new();
            for o in [a(1), a(2)] {
                sha3::Digest::update(&mut h, o);
            }
            sha3::Digest::finalize(h).into()
        };
        assert_ne!(
            owner_set_hash(&[a(1), a(2)]),
            untagged,
            "the owner-set digest must be domain-tagged, not a bare concatenation hash"
        );
    }

    #[test]
    fn the_derivation_requests_refuse_caller_supplied_content() {
        // The security property is structural: a caller cannot hand a node the calldata, the
        // hash, or the owner set. `deny_unknown_fields` makes an attempt an ERROR rather
        // than a silent drop — a silently ignored field looks like it was honoured.
        let with_calldata = r#"{"enclave_url":"http://127.0.0.1:9088","rpc_url":"x",
            "safe":"0x00","chain_id":1,"epoch":1,"target_threshold":1,"step_index":0,
            "safe_nonce":0,"calldata":"0xdeadbeef"}"#;
        assert!(
            serde_json::from_str::<DeriveStepRequest>(with_calldata).is_err(),
            "supplying calldata must be refused, not ignored"
        );
        let with_hash = r#"{"enclave_url":"http://127.0.0.1:9088","rpc_url":"x",
            "safe":"0x00","target_threshold":1,"owner_set_hash":"0xabcd"}"#;
        assert!(
            serde_json::from_str::<AttestRequest>(with_hash).is_err(),
            "supplying the owner-set hash must be refused, not ignored"
        );
        // The legitimate shapes still parse.
        let ok = r#"{"enclave_url":"http://127.0.0.1:9088","rpc_url":"x","safe":"0x00",
            "chain_id":1,"epoch":1,"target_threshold":1,"step_index":0,"safe_nonce":0}"#;
        assert!(serde_json::from_str::<DeriveStepRequest>(ok).is_ok());
    }

    #[test]
    fn in_sync_ignores_order_but_not_content_or_threshold() {
        assert!(in_sync(&[a(2), a(1)], 1, &[a(1), a(2)], 1));
        assert!(!in_sync(&[a(1)], 1, &[a(1), a(2)], 1), "missing owner");
        assert!(!in_sync(&[a(1), a(2)], 1, &[a(1), a(2)], 2), "threshold");
    }

    // ── the planner ─────────────────────────────────────────────────────

    #[test]
    fn todays_actual_case_is_two_adds_and_no_threshold_change() {
        // The live Safe: one owner, threshold 1. The cluster: three members. This is the
        // drift that went unnoticed for a month.
        let plan = plan_reconciliation(&[a(1)], 1, &[a(1), a(2), a(3)], 1).unwrap();
        assert_eq!(plan.len(), 2, "two adds, nothing else: {plan:?}");
        for op in &plan {
            match op {
                SafeOp::AddOwner { threshold, .. } => assert_eq!(*threshold, 1),
                other => panic!("unexpected op {other:?}"),
            }
        }
    }

    #[test]
    fn a_no_op_plan_is_empty() {
        assert!(plan_reconciliation(&[a(1), a(2)], 2, &[a(2), a(1)], 2)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_raised_threshold_goes_last_so_a_shrink_cannot_trip_safes_require() {
        // Safe's removeOwner requires ownerCount - 1 >= _threshold. Raising first would
        // make the removal below revert on-chain; the plan must not be orderable that way.
        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 1, &[a(1), a(2)], 2).unwrap();
        assert!(
            matches!(plan.last(), Some(SafeOp::ChangeThreshold { threshold: 2 })),
            "raise must be last: {plan:?}"
        );
        assert!(
            matches!(plan.first(), Some(SafeOp::RemoveOwner { .. })),
            "removal first: {plan:?}"
        );
    }

    #[test]
    fn a_lowered_threshold_goes_first_so_the_removal_is_legal() {
        // 3 owners at threshold 3 shrinking to 2 at threshold 2: removing before lowering
        // would need 2 >= 3 and revert.
        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 3, &[a(1), a(2)], 2).unwrap();
        assert!(
            matches!(plan.first(), Some(SafeOp::ChangeThreshold { threshold: 2 })),
            "lower must be first: {plan:?}"
        );
    }

    #[test]
    fn removal_names_the_sentinel_for_the_head_and_the_real_predecessor_otherwise() {
        // getOwners order IS the linked-list order, and Safe reverts on a wrong prevOwner.
        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 1, &[a(2), a(3)], 1).unwrap();
        match &plan[0] {
            SafeOp::RemoveOwner { prev_owner, .. } => {
                assert_eq!(*prev_owner, SENTINEL_OWNERS, "head's predecessor")
            }
            other => panic!("expected a removal, got {other:?}"),
        }

        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 1, &[a(1), a(3)], 1).unwrap();
        match &plan[0] {
            SafeOp::RemoveOwner { prev_owner, .. } => assert_eq!(*prev_owner, a(1)),
            other => panic!("expected a removal, got {other:?}"),
        }
    }

    #[test]
    fn predecessors_stay_correct_across_several_removals() {
        // The trap: computing every prevOwner against the ORIGINAL list. After the first
        // removal the list has changed, and the second op would name a predecessor that no
        // longer precedes anything.
        let plan = plan_reconciliation(&[a(1), a(2), a(3), a(4)], 1, &[a(1), a(4)], 1).unwrap();
        assert_eq!(plan.len(), 2);
        match (&plan[0], &plan[1]) {
            (
                SafeOp::RemoveOwner {
                    prev_owner: p1,
                    owner: o1,
                    ..
                },
                SafeOp::RemoveOwner {
                    prev_owner: p2,
                    owner: o2,
                    ..
                },
            ) => {
                assert_eq!((*o1, *p1), (a(2), a(1)));
                // a(2) is gone by now, so a(3)'s predecessor is a(1), NOT a(2).
                assert_eq!(
                    (*o2, *p2),
                    (a(3), a(1)),
                    "predecessor must follow the removal"
                );
            }
            other => panic!("expected two removals, got {other:?}"),
        }
    }

    #[test]
    fn replacing_an_owner_at_count_equals_threshold_uses_a_swap() {
        // The case the old planner could not express at all. Three owners, threshold
        // three, replacing one: `removeOwner` requires ownerCount - 1 >= threshold, i.e.
        // 2 >= 3, so any remove-then-add plan reverts on-chain. Only an in-place swap
        // keeps the count constant. Found by the dead-code check, not by review.
        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 3, &[a(1), a(2), a(4)], 3).unwrap();
        assert_eq!(plan.len(), 1, "one swap, nothing else: {plan:?}");
        match &plan[0] {
            SafeOp::SwapOwner {
                prev_owner,
                old_owner,
                new_owner,
            } => {
                assert_eq!(*old_owner, a(3));
                assert_eq!(*new_owner, a(4));
                assert_eq!(*prev_owner, a(2), "predecessor in linked-list order");
            }
            other => panic!("expected a swap, got {other:?}"),
        }
    }

    #[test]
    fn a_swap_leaves_the_successor_predecessors_intact() {
        // A swap replaces in place, so an owner AFTER the swapped one still has the same
        // predecessor position. Getting this wrong would name a stale predecessor in a
        // later op and revert.
        let plan = plan_reconciliation(&[a(1), a(2), a(3)], 2, &[a(9), a(2)], 2).unwrap();
        // a(1) -> a(9) as a swap (head), then a(3) removed with predecessor a(2).
        match (&plan[0], &plan[1]) {
            (
                SafeOp::SwapOwner {
                    prev_owner,
                    old_owner,
                    new_owner,
                },
                SafeOp::RemoveOwner {
                    prev_owner: p2,
                    owner,
                    ..
                },
            ) => {
                assert_eq!((*old_owner, *new_owner), (a(1), a(9)));
                assert_eq!(*prev_owner, SENTINEL_OWNERS, "a(1) was the head");
                assert_eq!((*owner, *p2), (a(3), a(2)), "swap did not move a(3)");
            }
            other => panic!("expected swap then remove, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_target_or_impossible_threshold_is_refused() {
        assert!(plan_reconciliation(&[a(1)], 1, &[], 1).is_err());
        assert!(plan_reconciliation(&[a(1)], 1, &[a(1)], 0).is_err());
        assert!(plan_reconciliation(&[a(1)], 1, &[a(1)], 2).is_err());
    }
}
