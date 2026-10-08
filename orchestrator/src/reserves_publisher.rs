//! #131 chunk 3d — Tier-1 reserves publisher.
//!
//! Ties the sequencer's enclave (which computes + signs the proof-of-liabilities
//! root over its sealed authoritative state — AC-R2-1) to the on-chain submit via
//! the Gnosis Safe (1-of-1 at Tier-1). This orchestrator never computes the root
//! and never holds the enclave key; it only picks the epoch, relays the enclave's
//! owner signature, and pays gas.
//!
//! All config is ENV-only (never CLI args — those are ps-visible — and never
//! committed): the RPC URL embeds the QuickNode key and the gas key is a hot EOA.
//! The publisher is opt-in (RESERVES_PUBLISH=1) and disabled otherwise.

use crate::commitment;
use crate::perp_client::PerpClient;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// The figures the enclave last committed, cached for the public attestation endpoint.
///
/// Deliberately a snapshot of what was PUBLISHED, not a fresh read: the endpoint must
/// describe the commitment that is actually on-chain, and a live re-read could disagree
/// with the published root by a whole interval of flows. A stale-but-matching figure is
/// honest; a fresh-but-unpublished one invites someone to check it against the registry
/// and find a mismatch we created ourselves.
#[derive(Debug, Clone, Copy)]
pub struct ReservesFigures {
    pub rlusd_liabilities: i64,
    pub xrp_liabilities: i64,
    pub custody_rlusd: i64,
    pub custody_xrp: i64,
    pub epoch: u64,
}

/// Shared handle: the publisher writes it once per interval, the API reads it.
pub type ReservesFiguresCache = std::sync::Arc<std::sync::Mutex<Option<ReservesFigures>>>;

#[derive(Debug, Clone)]
pub struct ReservesPublisherConfig {
    pub rpc_url: String,    // RESERVES_RPC_URL   (secret — embeds QuickNode key)
    pub gas_key: String, // RESERVES_GAS_KEY   (secret — gas-paying EOA private key, NOT the enclave key)
    pub registry: String, // RESERVES_REGISTRY  (0x… ReservesRegistry)
    pub safe: String,    // RESERVES_SAFE      (0x… Gnosis Safe, authority of the registry)
    pub chain_id: u64,   // RESERVES_CHAIN_ID  (default 84532 = Base-Sepolia)
    pub interval_secs: u64, // RESERVES_INTERVAL_SECS (default 3600)
}

impl ReservesPublisherConfig {
    /// Load from env iff `RESERVES_PUBLISH=1` and all required secrets/addresses are
    /// present; otherwise `None` (publisher disabled — the default).
    pub fn from_env() -> Option<Self> {
        use std::env::var;
        if var("RESERVES_PUBLISH").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Self {
            rpc_url: var("RESERVES_RPC_URL").ok()?,
            gas_key: var("RESERVES_GAS_KEY").ok()?,
            registry: var("RESERVES_REGISTRY").ok()?,
            safe: var("RESERVES_SAFE").ok()?,
            chain_id: var("RESERVES_CHAIN_ID")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(commitment::BASE_SEPOLIA_CHAIN_ID),
            interval_secs: var("RESERVES_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
        })
    }
}

/// Decode a 32-byte hex field (optional `0x`) out of a JSON object.
fn hex32(v: &Value, key: &str) -> Result<[u8; 32]> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string field `{key}`"))?;
    let bytes =
        hex::decode(s.trim_start_matches("0x")).with_context(|| format!("decode `{key}`"))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("`{key}` is not 32 bytes"))
}

/// Run one Tier-1 reserves-commit cycle:
/// 1. epoch = on-chain `latestEpoch + 1`;
/// 2. read the Safe nonce;
/// 3. the sequencer enclave computes the root + signs the SafeTxHash (refuses on
///    under-custody → this returns `Err`, never publishing an insolvent commitment);
/// 4. submit the Safe `execTransaction` (this orchestrator only relays + pays gas).
///
/// Returns the Base-Sepolia transaction hash. `account_id` is the sequencer's pool
/// EVM address ("0x…", kept as-is); `session_key` is its per-account auth token.
pub async fn run_reserves_commit_once(
    cfg: &ReservesPublisherConfig,
    perp: &PerpClient,
    account_id: &str,
    session_key: &str,
    excluded_account_ids: &[String],
    last_figures: Option<&ReservesFiguresCache>,
) -> Result<String> {
    let latest = commitment::query_latest_reserves(&cfg.rpc_url, &cfg.registry)
        .await
        .context("query latestReserves")?;
    let epoch = latest.epoch + 1; // monotonic (R-2); fresh registry epoch 0 → first publish = 1
    let safe_nonce = commitment::query_safe_nonce(&cfg.rpc_url, &cfg.safe)
        .await
        .context("query Safe nonce")?;

    let resp = perp
        .reserves_commit(
            account_id,
            session_key,
            epoch,
            &cfg.safe,
            cfg.chain_id,
            &cfg.registry,
            safe_nonce,
            excluded_account_ids,
        )
        .await
        // NO GUESS AT THE CAUSE. This said "(under-custody or signing error)", which named
        // two causes it had not checked and excluded every other one; printed alone it read
        // as a diagnosis. The enclave's rc is the diagnosis and it is already in the source
        // chain — this layer only says which call refused.
        .context("enclave reserves_commit refused")?;

    // The figures the enclave actually committed. Logged because the SPV backing gate
    // (AC-BASE-2″) replaces custody with an SPV-PROVEN balance ONE-SHOT and irreversibly:
    // deciding whether proven custody will still cover liabilities has to be a READ of the
    // live books, never an inference from the deposit mirror in Postgres.
    tracing::info!(
        rlusd_liabilities = resp
            .get("rlusd_liabilities")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        xrp_liabilities = resp
            .get("xrp_liabilities")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        custody_rlusd = resp
            .get("custody_rlusd")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        custody_xrp = resp
            .get("custody_xrp")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        leaf_count = resp.get("leaf_count").and_then(|v| v.as_u64()).unwrap_or(0),
        "reserves-commit figures (FP8)"
    );

    /* Q-BND-3 condition: the gap between proven custody and counted liabilities has to
     * be VISIBLE and QUANTIFIABLE, not merely stated in prose.
     *
     * The reason is the reconciliation path. A deposit that landed at or below the
     * baseline ledger but was never credited is permanently uncreditable through SPV —
     * the boundary refuses it, correctly, because crediting it would double-count a
     * payment already inside the proven balance. The money is real and sits in the
     * escrow. The remedy is operational, and an operator can only bound it if they can
     * see the gap: custody minus liabilities is exactly the ceiling on what such a
     * reconciliation may legitimately restore. Without the number published, "reconcile
     * no more than the observed gap" is a rule nobody can check.
     *
     * Published rather than logged for the same reason the figures themselves are: a
     * third party checking our claim should not have to take our word for the size of
     * what we have not proven. */
    if let Some(figs) = last_figures {
        let mut f = figs.lock().expect("reserves figures mutex poisoned");
        *f = Some(ReservesFigures {
            rlusd_liabilities: resp
                .get("rlusd_liabilities")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            xrp_liabilities: resp
                .get("xrp_liabilities")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            custody_rlusd: resp
                .get("custody_rlusd")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            custody_xrp: resp
                .get("custody_xrp")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            epoch,
        });
    }

    let root = hex32(&resp, "root")?;
    let snapshot = hex32(&resp, "snapshot_hash")?;
    let sig = resp
        .get("signature")
        .ok_or_else(|| anyhow!("response has no `signature`"))?;
    let r = hex32(sig, "r")?;
    let s = hex32(sig, "s")?;
    let v = sig
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("signature has no `v`"))? as u8;

    let mut owner_sig = [0u8; 65];
    owner_sig[..32].copy_from_slice(&r);
    owner_sig[32..64].copy_from_slice(&s);
    owner_sig[64] = v; // v ∈ {27,28} — the Safe's ECDSA owner-signature encoding

    commitment::submit_reserves_via_safe(
        &cfg.rpc_url,
        &cfg.gas_key,
        &cfg.safe,
        &cfg.registry,
        epoch,
        root,
        snapshot,
        owner_sig,
    )
    .await
    .context("submit Safe execTransaction")
}

/// Why the enclave refused to publish the liabilities root.
///
/// THIS EXISTS BECAUSE A SENTENCE CANNOT TRACK A CAUSE. The route used to answer "Reserves
/// commit failed (under-custody or signing error)" — hand-written, naming two causes nobody
/// had checked. It was right for the first 197 refusals (`-40`, custody below liabilities)
/// and then silently wrong for the next 381 (`-96`, a different condition entirely) across
/// seventeen days, because the sentence does not change when the cause does.
///
/// The codes are read from the enclave, not invented here: `ecall_perp_reserves_commit`
/// returns `-97` when no sealed SignerList authority is loaded, `-96` when
/// `base_projection_confirmed_epoch != signerlist_version`, `-41` when the excluded set is
/// over cap, `-42` when `perp_reserves_compute` could not take the shared leaf buffer, and
/// `-40` when `custody_ok` is false.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitRefusal {
    /// -40: custody is below liabilities. The refusal is the protection working.
    UnderCustody,
    /// -41: the operator-declared excluded-sender set exceeds the enclave's cap.
    ExcludedSetOverCap,
    /// -42: another caller holds the shared leaf buffer. The ONLY transient one.
    LeafBufferBusy,
    /// -96: the Base owner-set projection lags the sealed SignerList authority.
    BaseProjectionOutOfSync,
    /// -97: no sealed SignerList authority is loaded.
    NoSealedAuthority,
    /// -1: a host-side argument check in the manager, before the ecall.
    HostArgs,
    /// -1000: the ecall itself failed.
    EcallFailed,
    /// Anything else — NOT assumed benign and NOT assumed transient.
    Unknown(i32),
}

pub fn classify_commit(rc: i32) -> CommitRefusal {
    match rc {
        -40 => CommitRefusal::UnderCustody,
        -41 => CommitRefusal::ExcludedSetOverCap,
        -42 => CommitRefusal::LeafBufferBusy,
        -96 => CommitRefusal::BaseProjectionOutOfSync,
        -97 => CommitRefusal::NoSealedAuthority,
        -1 => CommitRefusal::HostArgs,
        -1000 => CommitRefusal::EcallFailed,
        other => CommitRefusal::Unknown(other),
    }
}

/// Only -42 is worth retrying. Everything else repeats until someone acts, which is why the
/// hourly driver must not describe them as transient.
pub fn commit_is_transient(r: &CommitRefusal) -> bool {
    matches!(r, CommitRefusal::LeafBufferBusy)
}

/// What the operator has to DO. A code is not an instruction; this is the difference between
/// a number in a log nobody acts on and a next step.
pub fn commit_action(r: &CommitRefusal) -> &'static str {
    match r {
        CommitRefusal::UnderCustody =>
            "custody is BELOW liabilities — the refusal is the protection working; close the gap, do not bypass",
        CommitRefusal::ExcludedSetOverCap =>
            "the declared operator-capital excluded-sender set is over the enclave's cap — shrink it",
        CommitRefusal::LeafBufferBusy =>
            "another caller holds the shared leaf buffer — transient, the next pass retries",
        CommitRefusal::BaseProjectionOutOfSync =>
            "the Base owner-set projection lags the sealed SignerList — converge the Safe owner set, then record the projection; publishing stays halted until they match",
        CommitRefusal::NoSealedAuthority =>
            "no sealed SignerList authority is loaded — this node cannot publish at all",
        CommitRefusal::HostArgs =>
            "a host-side argument check failed before the ecall — a caller bug, not cluster state",
        CommitRefusal::EcallFailed =>
            "the ecall itself failed — check the enclave process, not the reserves state",
        CommitRefusal::Unknown(_) =>
            "UNRECOGNISED code — do not assume it is benign or transient; read the enclave source for it",
    }
}

/// Classify from the error chain the publisher actually produces.
///
/// Reuses `deposit_spv::rc_from_error` rather than carrying a second copy of the parse — a
/// second copy is the thing that drifts from the first.
pub fn classify_commit_from_error(msg: &str) -> Option<CommitRefusal> {
    crate::deposit_spv::rc_from_error(msg).map(classify_commit)
}

#[cfg(test)]
mod commit_refusal_tests {
    use super::*;

    /// The REAL chain, copied from node-1's journal at 2026-10-08T10:00:00Z, with only the
    /// server message changed — that message is what the enclave-side fix alters, and
    /// everything around it (the `.context`, the URL, civetweb's own `Error 500:` page, the
    /// JSON body on its own line) is exactly what the publisher produced.
    ///
    /// Written out rather than invented because the shape is not guessable: the body arrives
    /// AFTER civetweb's generated status page, on a second line, and a fixture that omitted
    /// that would pass while the real chain parsed to nothing.
    const REAL_CHAIN: &str = concat!(
        "enclave reserves_commit refused: https://localhost:9088/v1/perp/reserves-commit",
        " -> HTTP 500 Internal Server Error: Error 500: Internal Server Error\n",
        r#"{"message":"Reserves commit refused (rc=-96)","status":"error"}"#
    );

    #[test]
    fn the_real_error_chain_yields_the_base_projection_refusal() {
        assert_eq!(
            classify_commit_from_error(REAL_CHAIN),
            Some(CommitRefusal::BaseProjectionOutOfSync)
        );
    }

    /// The measured history: 197 refusals at -40, then 381 at -96. The two demand DIFFERENT
    /// operator actions, which is the whole reason a single sentence was not enough.
    #[test]
    fn the_two_phases_of_the_live_halt_classify_differently() {
        assert_eq!(classify_commit(-40), CommitRefusal::UnderCustody);
        assert_eq!(classify_commit(-96), CommitRefusal::BaseProjectionOutOfSync);
        assert_ne!(
            commit_action(&classify_commit(-40)),
            commit_action(&classify_commit(-96))
        );
        assert!(commit_action(&CommitRefusal::UnderCustody).contains("custody"));
        assert!(commit_action(&CommitRefusal::BaseProjectionOutOfSync).contains("Safe owner set"));
    }

    /// Only the leaf-buffer contention is worth retrying. Calling anything else transient is
    /// what makes an hourly driver look like it is making progress while nothing is.
    #[test]
    fn only_the_leaf_buffer_refusal_is_transient() {
        assert!(commit_is_transient(&CommitRefusal::LeafBufferBusy));
        for r in [
            CommitRefusal::UnderCustody,
            CommitRefusal::ExcludedSetOverCap,
            CommitRefusal::BaseProjectionOutOfSync,
            CommitRefusal::NoSealedAuthority,
            CommitRefusal::HostArgs,
            CommitRefusal::EcallFailed,
            CommitRefusal::Unknown(-7),
        ] {
            assert!(
                !commit_is_transient(&r),
                "{r:?} must not be called transient"
            );
        }
    }

    #[test]
    fn an_unknown_code_is_not_assumed_benign() {
        assert_eq!(classify_commit(-5), CommitRefusal::Unknown(-5));
        assert!(commit_action(&CommitRefusal::Unknown(-5)).contains("UNRECOGNISED"));
    }

    /// A chain with no rc must come back None, so the caller SAYS it has no code rather than
    /// classifying one. The old line's whole defect was answering without knowing.
    #[test]
    fn a_chain_without_a_code_classifies_to_nothing() {
        assert_eq!(
            classify_commit_from_error("enclave reserves_commit refused: connection refused"),
            None
        );
    }
}
