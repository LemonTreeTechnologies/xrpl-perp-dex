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
        .context("enclave reserves_commit (under-custody or signing error)")?;

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
