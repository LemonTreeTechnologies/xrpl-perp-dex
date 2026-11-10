//! GAP B withdrawal reconcile — the orchestrator half of the reserve → finalize/rollback
//! lifecycle.
//!
//! A withdrawal is RESERVED in the enclave before it is signed: the margin is debited into a
//! durable sealed `PendingWithdrawal` row and the per-escrow high-water advances over the
//! Account Sequence so the slot can never be re-signed. The row stays reserved until the
//! enclave is SHOWN, by SPV proof, one of two terminal facts:
//!   - the Payment LANDED  -> finalize (the hold becomes a permanent custody-out), or
//!   - the Payment is DEAD  -> rollback (the hold is restored to the user).
//!
//! The enclave is the arbiter of which, and of finalize-vs-stolen-slot. The orchestrator's
//! only jobs are (1) to remember which withdrawals are in flight and (2) to carry it the
//! right SPV proof. This module owns (1) — a small durable journal — and the pure decision
//! of WHICH proof to build next; the IO (querying XRPL, assembling the proof, calling the
//! enclave) lives alongside it.
//!
//! WHY A JOURNAL AND NOT A READBACK. The enclave exposes no "list pending rows" ecall, so
//! without this note a reserved withdrawal whose orchestrator process died between reserve
//! and finalize would be invisible: its margin debited, its slot blocked by the high-water,
//! and nothing driving it to a terminal state. A readback ecall is the clean mainnet
//! replacement; this journal is the bounded-window bridge the auditor's "operator reconcile"
//! ruling permits. The enclave remains the source of truth — a journal entry only tells the
//! loop where to point an SPV proof; it can never itself move money.

use crate::deposit_spv::{build_deposit_proof, ValidationBuffer};
use crate::perp_client::{rc_from_error, PerpClient};
use crate::spv_proof::{fetch_spv_bundle, SpvFetchConfig};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// One reserved-but-not-yet-reconciled withdrawal. Written at reserve+submit, removed once
/// the enclave confirms finalize or rollback.
///
/// The authoritative figures (the fp8 amount actually held, the user's balance) live in the
/// sealed enclave row; the fields here are what the orchestrator needs to FIND the matching
/// validated transaction and build a proof, plus operator diagnostics. None of them is ever
/// asserted to the enclave — every money figure the enclave uses it re-derives from the
/// proof bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWithdrawal {
    /// escrow r-address (the Payment's `Account`).
    pub escrow: String,
    /// the reserved XRPL Account `Sequence` slot S — the unit the per-escrow high-water and
    /// every reconcile verdict is keyed on.
    pub sequence: u32,
    /// the Payment's `LastLedgerSequence` L. Once a validated ledger passes it with the slot
    /// still unconsumed, the Payment can never be included — the precondition for R-b.
    pub last_ledger_sequence: u32,
    /// drops as submitted (native XRP). Diagnostics only: the enclave re-derives fp8 from the
    /// DELIVERED amount in the proof and never trusts this number.
    pub amount_drops: u64,
    /// destination r-address (diagnostics; the enclave reads it from the proven tx).
    pub destination: String,
    /// optional DestinationTag (diagnostics).
    pub destination_tag: Option<u32>,
    /// the hash `submit_multisigned` returned — OPTIMISTIC (tes*, not validated), and a
    /// fee/`LastLedgerSequence`-bumped resubmission would carry a different one, so it is a
    /// hint for finding the tx, never the reconcile key. Absent if submission itself failed
    /// after the reserve (the sealed row still exists and must still be reconciled).
    pub submitted_hash: Option<String>,
    /// user id, for operator diagnostics.
    pub user_id: String,
    /// unix ms when the withdrawal was reserved — for staleness ordering and operator view.
    pub reserved_at_ms: u64,
    /// Follower reserve (2026-11-10): EVERY signer reserves EVERY withdrawal, so all enclaves
    /// hold the same balances. These are the
    /// signers (by `SignerConfig::name`) still owed this co-sign request; the delivery loop
    /// sends it to each, per signer strictly in Sequence order. Empty = every node has it.
    #[serde(default)]
    pub undelivered: Vec<String>,
    /// The unsigned Payment (`tx_json`) and the fp8 claim, so the request can be re-sent
    /// verbatim to a signer that missed it — before or after it landed (its signature is then
    /// useless and harmless; what matters is that its enclave reserves and settles the row).
    #[serde(default)]
    pub tx_json: Option<serde_json::Value>,
    #[serde(default)]
    pub amount_fp8: u64,
    /// This node's enclave settled the row (finalized / rolled back) but some signer is still
    /// owed the request: the entry stays for delivery only and is not reconciled again.
    #[serde(default)]
    pub settled: bool,
    /// The user's authorisation, re-sent verbatim with an owed request (scope C).
    #[serde(default)]
    pub auth: Option<crate::withdraw_auth::WithdrawAuth>,
}

/// The durable journal: the set of in-flight reserved withdrawals for this node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Journal {
    pub entries: Vec<PendingWithdrawal>,
}

impl Journal {
    /// Load the journal, or an EMPTY one if the file simply does not exist yet.
    ///
    /// A present-but-unreadable or corrupt file is an ERROR, not an empty journal: silently
    /// starting empty would strand every in-flight withdrawal (margin debited, slot blocked)
    /// with nothing left to reconcile it. The caller must surface that rather than proceed.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).with_context(|| {
                format!("parsing withdrawal reconcile journal at {}", path.display())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(e).with_context(|| {
                format!("reading withdrawal reconcile journal at {}", path.display())
            }),
        }
    }

    /// Persist ATOMICALLY: write a temp sibling then rename over the target, so a crash
    /// mid-write cannot leave a half-written (and therefore unloadable) journal that would
    /// strand every row it used to hold.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("serialising withdrawal reconcile journal")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!(
                "atomically renaming {} -> {}",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Insert, or replace an existing entry for the same (escrow, Sequence) slot. The slot is
    /// unique per escrow (the TEETH), so it is the natural identity of a reserved withdrawal.
    pub fn upsert(&mut self, e: PendingWithdrawal) {
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|x| x.escrow == e.escrow && x.sequence == e.sequence)
        {
            *slot = e;
        } else {
            self.entries.push(e);
        }
    }

    /// Drop the entry for (escrow, Sequence). Returns whether one was present.
    pub fn remove(&mut self, escrow: &str, sequence: u32) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|x| !(x.escrow == escrow && x.sequence == sequence));
        self.entries.len() != before
    }

    // Test-only conveniences. Production reads the set through `JournalHandle::snapshot` and
    // mutates it through `record`/`resolve` (which call `upsert`/`remove`); these three exist
    // only to assert journal identity in the unit tests, so they are gated out of the binary.
    #[cfg(test)]
    pub fn find(&self, escrow: &str, sequence: u32) -> Option<&PendingWithdrawal> {
        self.entries
            .iter()
            .find(|x| x.escrow == escrow && x.sequence == sequence)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What the reconcile loop should do with a reserved withdrawal, decided PURELY from three
/// XRPL facts so the whole decision table is exhaustively unit-testable with no IO:
///   - the escrow's CURRENT validated Account `Sequence`,
///   - the CURRENT validated ledger index,
///   - the entry's reserved slot S and window L.
///
/// XRPL account sequences are strictly sequential (each transaction increments by exactly
/// one), so the escrow's current Sequence relative to S tells us unambiguously whether slot S
/// is still free or has been consumed — without trusting any single tx lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Slot S was consumed by a validated transaction (escrow.Sequence has moved past S).
    /// Build the XDEP tx-inclusion proof of the tx at S and try finalize; the enclave
    /// content-matches and, on a content MISMATCH (-69), it is the R-a stolen-slot case. The
    /// orchestrator deliberately does NOT itself decide finalize-vs-rollback — the enclave is
    /// the arbiter, so a fee-bump retry (same economic content, different hash) finalizes a
    /// paid user instead of false-rolling-back and double-crediting them.
    SlotConsumed,
    /// Slot S is still free (escrow.Sequence == S) and a validated ledger has passed L. The
    /// Payment, bound to S and L, can never be included -> R-b expired rollback (XSPV
    /// AccountRoot proof that the Sequence is still S at a ledger > L).
    Expired,
    /// Slot S is still free and the window has NOT closed (validated_idx <= L): the Payment
    /// may still land. Wait and re-check.
    Pending,
    /// escrow.Sequence < S — impossible under sequential sequencing (a reorg, the wrong
    /// escrow, or a stale read). Never guess on the money path: do nothing and surface it.
    Anomaly,
}

/// The pure reconcile decision. See [`Verdict`].
pub fn classify(
    escrow_validated_sequence: u32,
    current_validated_ledger: u32,
    entry: &PendingWithdrawal,
) -> Verdict {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match escrow_validated_sequence.cmp(&entry.sequence) {
        Greater => Verdict::SlotConsumed,
        Less => Verdict::Anomaly,
        // Slot still free: dead only once a validated ledger is strictly PAST L. The strict
        // `>` matches the enclave, whose R-b refuses (-74) a proof at ledger <= L because the
        // Payment could still be included at exactly L.
        Equal => {
            if current_validated_ledger > entry.last_ledger_sequence {
                Verdict::Expired
            } else {
                Verdict::Pending
            }
        }
    }
}

// ── IO: read XRPL -> classify -> build the proof -> drive the enclave ──────────

/// The result of one reconcile pass over one journal entry. Terminal variants mean the
/// enclave confirmed a final state and the journal entry may be dropped; the others mean
/// keep it and re-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The enclave accepted a finalize, or the row was already gone (idempotent). Terminal.
    Finalized,
    /// The enclave rolled back a stolen slot (R-a). Terminal.
    RolledBackStolen,
    /// The enclave rolled back an expired withdrawal (R-b). Terminal.
    RolledBackExpired,
    /// Not yet decidable — slot free, window still open. Keep and re-check later.
    StillPending,
    /// A terminal transition is due but could not complete THIS pass (proof not yet
    /// buildable, enclave asked to retry, transient XRPL error). Keep and re-check.
    Deferred(String),
    /// A state that must never be guessed on (Sequence behind S, an unparseable stealing tx,
    /// an unexpected enclave code). Keep and surface loudly for an operator.
    Anomaly(String),
}

impl ReconcileOutcome {
    /// Whether the journal entry should be dropped (a terminal state the enclave confirmed).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finalized | Self::RolledBackStolen | Self::RolledBackExpired
        )
    }
}

/// Index into the ledger's transaction array of the tx whose id equals `target_tx_id`.
///
/// The ledger is fetched in `binary: true` mode, so each entry is `{tx_blob, meta}` with no
/// hash field — we RECOMPUTE the id from the blob (the same value the ledger committed to its
/// tx tree) rather than trust a supplied one. Pure, so the lookup is unit-tested.
pub fn find_target_index(ledger_result: &Value, target_tx_id: &[u8; 32]) -> Option<usize> {
    let txs = ledger_result["ledger"]["transactions"].as_array()?;
    for (i, t) in txs.iter().enumerate() {
        let blob_hex = match t["tx_blob"].as_str() {
            Some(s) => s,
            None => continue,
        };
        let blob = match hex::decode(blob_hex) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if &crate::tx_shamap::tx_id(&blob) == target_tx_id {
            return Some(i);
        }
    }
    None
}

fn decode_tx_id(hex_hash: &str) -> Option<[u8; 32]> {
    let v = hex::decode(hex_hash).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Some(a)
}

async fn xrpl_rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let resp: Value = http
        .post(url)
        .json(&serde_json::json!({ "method": method, "params": [params] }))
        .send()
        .await
        .with_context(|| format!("XRPL {method} request failed"))?
        .json()
        .await
        .with_context(|| format!("XRPL {method} response parse failed"))?;
    Ok(resp)
}

/// The escrow's CURRENT validated Account Sequence.
async fn escrow_validated_sequence(http: &reqwest::Client, url: &str, escrow: &str) -> Result<u32> {
    let r = xrpl_rpc(
        http,
        url,
        "account_info",
        serde_json::json!({ "account": escrow, "ledger_index": "validated" }),
    )
    .await?;
    r["result"]["account_data"]["Sequence"]
        .as_u64()
        .map(|s| s as u32)
        .context("account_info: missing validated Sequence")
}

/// The current validated ledger index.
async fn current_validated_ledger(http: &reqwest::Client, url: &str) -> Result<u32> {
    let r = xrpl_rpc(
        http,
        url,
        "ledger",
        serde_json::json!({ "ledger_index": "validated", "transactions": false }),
    )
    .await?;
    r["result"]["ledger_index"]
        .as_u64()
        .or_else(|| r["result"]["ledger"]["ledger_index"].as_u64())
        .or_else(|| {
            r["result"]["ledger"]["ledger_index"]
                .as_str()
                .and_then(|s| s.parse().ok())
        })
        .map(|s| s as u32)
        .context("ledger(validated): missing ledger_index")
}

/// Find the validated transaction that consumed the escrow's Sequence slot `s`, if visible
/// yet. Scans the escrow's recent `account_tx` (descending). Returns (tx_hash_hex,
/// ledger_index). Within the bounded `LastLedgerSequence` window the tx is very recent, so a
/// small page suffices.
async fn find_validated_tx_at_sequence(
    http: &reqwest::Client,
    url: &str,
    escrow: &str,
    s: u32,
) -> Result<Option<(String, u32)>> {
    let r = xrpl_rpc(
        http,
        url,
        "account_tx",
        serde_json::json!({
            "account": escrow,
            "ledger_index_min": -1,
            "ledger_index_max": -1,
            "binary": false,
            "forward": false,
            "limit": 50
        }),
    )
    .await?;
    let txs = match r["result"]["transactions"].as_array() {
        Some(a) => a,
        None => return Ok(None),
    };
    for t in txs {
        if t["validated"].as_bool() != Some(true) {
            continue;
        }
        // account_tx shapes vary across rippled versions: the tx under "tx" (classic) or
        // "tx_json" (API v2); the hash and ledger index at the wrapper or inside the tx.
        let txo = if t["tx"].is_object() {
            &t["tx"]
        } else {
            &t["tx_json"]
        };
        if txo["Sequence"].as_u64() != Some(s as u64) {
            continue;
        }
        let hash = t["hash"]
            .as_str()
            .or_else(|| txo["hash"].as_str())
            .map(|x| x.to_string());
        let ledger_index = t["ledger_index"]
            .as_u64()
            .or_else(|| txo["ledger_index"].as_u64())
            .or_else(|| txo["inLedger"].as_u64());
        if let (Some(h), Some(li)) = (hash, ledger_index) {
            return Ok(Some((h, li as u32)));
        }
    }
    Ok(None)
}

/// Fetch a whole ledger (every transaction, binary+expanded) — the full set the tx-tree root
/// is a hash over, as `build_deposit_proof` requires. Returns the RPC `result` object (it
/// carries both `ledger` and `ledger_hash`).
async fn fetch_full_ledger(http: &reqwest::Client, url: &str, ledger_index: u32) -> Result<Value> {
    let r = xrpl_rpc(
        http,
        url,
        "ledger",
        serde_json::json!({
            "ledger_index": ledger_index,
            "transactions": true,
            "expand": true,
            "binary": true
        }),
    )
    .await?;
    Ok(r["result"].clone())
}

/// Whether an enclave rc from a finalize / rollback refusal is RETRYABLE — a future ledger,
/// a fresher or rebuilt proof, a UNL refresh, or enclave arming could change it — versus a
/// terminal ANOMALY an operator must look at.
///
/// FAIL-LOUD DEFAULT (auditor Ruling-1(a)): this enumerates ONLY the known-retryable codes;
/// EVERY other rc — an unmapped one, and the withdrawal parse-reject family (a validly
/// included tx at Sequence S that is not a clean native-XRP withdrawal Payment: not-a-payment,
/// partial, failed, non-native, bad-amount, missing-field) whose on-chain fact is SETTLED —
/// falls through to anomaly. An unknown terminal must alarm loudly, never retry silently
/// forever (that is the silent-strand failure in slow motion). The safe direction is a false
/// alarm (row kept, operator looks), never a false retry (a stranded, debited row).
fn rc_is_retryable(rc: i64) -> bool {
    // ecall-level transients: enclave not yet armed (no SignerList / no pinned UNL / quorum
    // floor), the R-b window not yet past L (-74), the R-b UNL freshness window (-77 before
    // pin, -81 beyond window — clears on a forward re-pin), and tx-inclusion (-87: node lag
    // or a rebuildable proof).
    const RETRYABLE_ECALL: &[i64] = &[-60, -70, -71, -74, -77, -81, -87];
    if RETRYABLE_ECALL.contains(&rc) {
        return true;
    }
    // SPV-band transients (base -200): under-quorum (-8, more validations arrive),
    // wrong-ledger-hash (-9) / bad-validation (-10) / signer-not-in-pinned-UNL (-11, UNL
    // refresh) / state-path-mismatch (-12) — all clear with more validations, a UNL refresh,
    // or a rebuilt proof from a fresher ledger. The parse-reject SPV sub-codes (-14..-22,
    // and -1..-7 producer-malformed, -13 wrong-keylet, -99 internal) are NOT here: they are
    // settled or a producer bug, so they fall through to anomaly.
    let spv = rc - crate::attested_clock::SPV_HOST_RC_BASE;
    matches!(spv, -12..=-8)
}

/// One reconcile pass over one reserved withdrawal: read XRPL, decide the verdict, build the
/// matching SPV proof, drive the enclave.
///
/// The enclave is the arbiter. For a consumed slot we TRY finalize and only fall back to the
/// stolen-slot rollback on the enclave's own content-mismatch code (-69) — so a fee-bumped
/// resubmission of the SAME economic withdrawal finalizes a paid user rather than being
/// false-rolled-back into a double credit. The orchestrator never content-matches itself.
pub async fn reconcile_one(
    entry: &PendingWithdrawal,
    perp: &PerpClient,
    http: &reqwest::Client,
    xrpl_http_url: &str,
    buffer: &Arc<Mutex<ValidationBuffer>>,
    rb_fetch: &SpvFetchConfig,
) -> ReconcileOutcome {
    let escrow_seq = match escrow_validated_sequence(http, xrpl_http_url, &entry.escrow).await {
        Ok(s) => s,
        Err(e) => return ReconcileOutcome::Deferred(format!("account_info failed: {e:#}")),
    };
    let validated_idx = match current_validated_ledger(http, xrpl_http_url).await {
        Ok(l) => l,
        Err(e) => {
            return ReconcileOutcome::Deferred(format!("validated-ledger read failed: {e:#}"))
        }
    };

    match classify(escrow_seq, validated_idx, entry) {
        Verdict::Pending => ReconcileOutcome::StillPending,
        Verdict::Anomaly => ReconcileOutcome::Anomaly(format!(
            "escrow {} validated Sequence {} is BEHIND the reserved slot {} \
             (reorg / wrong escrow / stale read) — not reconciling",
            entry.escrow, escrow_seq, entry.sequence
        )),
        Verdict::SlotConsumed => {
            reconcile_consumed_slot(entry, perp, http, xrpl_http_url, buffer).await
        }
        Verdict::Expired => reconcile_expired(entry, perp, rb_fetch).await,
    }
}

async fn reconcile_consumed_slot(
    entry: &PendingWithdrawal,
    perp: &PerpClient,
    http: &reqwest::Client,
    url: &str,
    buffer: &Arc<Mutex<ValidationBuffer>>,
) -> ReconcileOutcome {
    let (tx_hash, ledger_index) =
        match find_validated_tx_at_sequence(http, url, &entry.escrow, entry.sequence).await {
            Ok(Some(x)) => x,
            Ok(None) => {
                return ReconcileOutcome::Deferred(
                    "slot consumed but the tx at that Sequence is not yet visible in account_tx"
                        .into(),
                )
            }
            Err(e) => return ReconcileOutcome::Deferred(format!("account_tx scan failed: {e:#}")),
        };
    let target_id = match decode_tx_id(&tx_hash) {
        Some(id) => id,
        None => {
            return ReconcileOutcome::Anomaly(format!(
                "account_tx returned a non-32-byte tx hash {tx_hash}"
            ))
        }
    };
    let ledger = match fetch_full_ledger(http, url, ledger_index).await {
        Ok(l) => l,
        Err(e) => return ReconcileOutcome::Deferred(format!("ledger fetch failed: {e:#}")),
    };
    let target_index = match find_target_index(&ledger, &target_id) {
        Some(i) => i,
        None => {
            return ReconcileOutcome::Deferred(format!(
                "tx {tx_hash} not yet in fetched ledger {ledger_index} (node lag?)"
            ))
        }
    };
    let ledger_hash = match ledger["ledger_hash"].as_str() {
        Some(h) => h.to_string(),
        None => return ReconcileOutcome::Deferred("ledger result missing ledger_hash".into()),
    };
    // Lift the buffered validations out under the lock, then DROP the guard before any await
    // (std Mutex guard is not held across await).
    let validations: Vec<Vec<u8>> = {
        let guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get(&ledger_hash) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => {
                return ReconcileOutcome::Deferred(format!(
                    "no buffered validations for ledger {ledger_hash} yet; retry while in window"
                ))
            }
        }
    };
    let proof = match build_deposit_proof(&ledger, target_index, &validations) {
        Ok(p) => p,
        Err(e) => return ReconcileOutcome::Deferred(format!("XDEP proof build failed: {e:#}")),
    };

    match perp.withdraw_finalize(&proof.blob).await {
        Ok(_) => {
            info!(
                escrow = %entry.escrow, sequence = entry.sequence, tx = %tx_hash,
                "GAP B reconcile: withdrawal FINALIZED (SPV-proven landed)"
            );
            ReconcileOutcome::Finalized
        }
        Err(e) => {
            let msg = format!("{e:#}");
            match rc_from_error(&msg) {
                // -69: the validated tx at S is NOT our economic withdrawal -> a different tx
                // consumed the slot. Same XDEP blob, R-a rollback (the enclave alarms).
                Some(-69) => match perp.withdraw_rollback_slot_stolen(&proof.blob).await {
                    Ok(_) => {
                        warn!(
                            escrow = %entry.escrow, sequence = entry.sequence, tx = %tx_hash,
                            "GAP B reconcile: Sequence slot STOLEN by a different tx -> rolled back (R-a)"
                        );
                        ReconcileOutcome::RolledBackStolen
                    }
                    Err(e2) => ReconcileOutcome::Anomaly(format!(
                        "finalize reported content mismatch (-69) but R-a rollback was refused: {e2:#}"
                    )),
                },
                // FAIL-LOUD: known-retryable -> Deferred; everything else (parse-reject of a
                // settled tx at S, value-latch, user-gone, any unmapped terminal) -> ANOMALY.
                Some(rc) if rc_is_retryable(rc) => {
                    ReconcileOutcome::Deferred(format!("finalize refused rc={rc} (retryable): {msg}"))
                }
                Some(rc) => ReconcileOutcome::Anomaly(format!(
                    "finalize refused rc={rc} — a settled on-chain fact at Sequence {} or an \
                     unmapped terminal, not retryable: {msg}",
                    entry.sequence
                )),
                // No enclave rc parsed => the enclave was not reached (transport / HTTP) =>
                // transient, retry.
                None => ReconcileOutcome::Deferred(format!(
                    "finalize failed, no enclave rc (transport?): {msg}"
                )),
            }
        }
    }
}

async fn reconcile_expired(
    entry: &PendingWithdrawal,
    perp: &PerpClient,
    rb_fetch: &SpvFetchConfig,
) -> ReconcileOutcome {
    // XSPV AccountRoot proof at a fresh validated ledger. We reach here only once the
    // validated tip has already passed L, so the ledger this lands on is past L; the enclave
    // re-checks `ledger > L` and returns -74 if a race put us on one that isn't yet — retry.
    let blob = match fetch_spv_bundle(rb_fetch).await {
        Ok(b) => b,
        Err(e) => {
            return ReconcileOutcome::Deferred(format!("R-b XSPV bundle fetch failed: {e:#}"))
        }
    };
    match perp.withdraw_rollback_expired(&blob).await {
        Ok(_) => {
            info!(
                escrow = %entry.escrow, sequence = entry.sequence,
                "GAP B reconcile: withdrawal EXPIRED unconsumed -> rolled back (R-b), margin restored"
            );
            ReconcileOutcome::RolledBackExpired
        }
        Err(e) => {
            let msg = format!("{e:#}");
            match rc_from_error(&msg) {
                // FAIL-LOUD, same partition as finalize: -74 (not yet past L) and the other
                // known-retryable codes -> Deferred; everything else -> ANOMALY. In
                // particular -73 (the margin's user record is gone) is terminal, not a quiet
                // defer (auditor Ruling-1(a)); it falls through to the anomaly arm here.
                Some(rc) if rc_is_retryable(rc) => ReconcileOutcome::Deferred(format!(
                    "R-b rollback refused rc={rc} (retryable): {msg}"
                )),
                Some(rc) => ReconcileOutcome::Anomaly(format!(
                    "R-b rollback refused rc={rc} — terminal or unmapped, not retryable: {msg}"
                )),
                None => ReconcileOutcome::Deferred(format!(
                    "R-b rollback failed, no enclave rc (transport?): {msg}"
                )),
            }
        }
    }
}

// ── shared journal handle (producer side) + the loop (consumer side) ───────────

/// A handle to the shared durable journal: the in-memory set plus where it persists. Cloned
/// into the withdrawal HTTP handler (which RECORDS a reserved withdrawal) and into the
/// reconcile loop (which RESOLVES entries the enclave confirms terminal).
#[derive(Clone)]
pub struct JournalHandle {
    journal: Arc<Mutex<Journal>>,
    path: PathBuf,
}

impl JournalHandle {
    /// Open the journal at `path`, loading any existing entries. A corrupt file is an error
    /// (see [`Journal::load`]).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let journal = Journal::load(&path)?;
        Ok(Self {
            journal: Arc::new(Mutex::new(journal)),
            path,
        })
    }

    fn with_journal<R>(&self, f: impl FnOnce(&mut Journal) -> R) -> R {
        let mut g = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    /// Record (or refresh) a reserved withdrawal and persist. Persistence is best-effort: a
    /// write failure is logged, not propagated — the in-memory set still drives reconcile for
    /// this run, and a lost file degrades liveness (a stuck row), never safety (the enclave
    /// high-water still blocks a re-sign).
    pub fn record(&self, e: PendingWithdrawal) {
        self.with_journal(|j| {
            j.upsert(e);
            if let Err(err) = j.save(&self.path) {
                warn!(error = %format!("{err:#}"),
                    "withdrawal reconcile journal: persist failed (in-memory set retained)");
            }
        });
    }

    /// A snapshot of the in-flight entries to iterate without holding the lock across awaits.
    pub fn snapshot(&self) -> Vec<PendingWithdrawal> {
        self.with_journal(|j| j.entries.clone())
    }

    /// This node's enclave settled the row. Drop the entry if every signer has the request;
    /// otherwise keep it, marked settled, for the delivery loop only.
    pub fn settle(&self, escrow: &str, sequence: u32) {
        self.with_journal(|j| {
            let keep = j
                .entries
                .iter_mut()
                .find(|x| x.escrow == escrow && x.sequence == sequence)
                .map(|e| {
                    e.settled = true;
                    !e.undelivered.is_empty()
                })
                .unwrap_or(false);
            if !keep {
                j.remove(escrow, sequence);
            }
            if let Err(err) = j.save(&self.path) {
                warn!(error = %format!("{err:#}"),
                    "withdrawal reconcile journal: persist after settle failed");
            }
        });
    }

    /// `signer` now holds the request at (escrow, sequence). Drops the entry once it is both
    /// settled here and delivered everywhere.
    pub fn delivered(&self, escrow: &str, sequence: u32, signer: &str) {
        self.with_journal(|j| {
            let mut drop = false;
            if let Some(e) = j
                .entries
                .iter_mut()
                .find(|x| x.escrow == escrow && x.sequence == sequence)
            {
                e.undelivered.retain(|s| s != signer);
                drop = e.settled && e.undelivered.is_empty();
            }
            if drop {
                j.remove(escrow, sequence);
            }
            if let Err(err) = j.save(&self.path) {
                warn!(error = %format!("{err:#}"),
                    "withdrawal reconcile journal: persist after delivery failed");
            }
        });
    }
}

/// Strict per-signer order (auditor ruling 2026-11-10): a signer that has NOT yet received an
/// earlier withdrawal must not be sent a later one, or its enclave's monotonic high-water
/// would refuse the earlier one forever (-59). True when `signer` is owed some request at a
/// Sequence other than `except_sequence`.
pub fn signer_owed_earlier(
    entries: &[PendingWithdrawal],
    signer: &str,
    except_sequence: u32,
) -> bool {
    entries
        .iter()
        .any(|e| e.sequence != except_sequence && e.undelivered.iter().any(|s| s == signer))
}

/// The next request owed to `signer`: the LOWEST Sequence it is still missing.
pub fn next_owed(entries: &[PendingWithdrawal], signer: &str) -> Option<PendingWithdrawal> {
    entries
        .iter()
        .filter(|e| e.undelivered.iter().any(|s| s == signer))
        .min_by_key(|e| e.sequence)
        .cloned()
}

/// Config for the background reconcile loop + boot sweep.
pub struct ReconcileLoopConfig {
    pub xrpl_http_url: String,
    /// Params for the R-b XSPV AccountRoot fetch (escrow + a validator quorum ledger).
    pub rb_fetch: SpvFetchConfig,
    pub interval_secs: u64,
}

impl ReconcileLoopConfig {
    /// Build from the environment, gated on `PERP_WITHDRAWAL_RECONCILE=1`. Shares the XRPL
    /// endpoint lists with the deposit/clock drivers (`XRPL_RPC_URL` / `XRPL_WS_URL`), so one
    /// node points all SPV paths at the same rippled.
    pub fn from_env(escrow: &str) -> Option<Self> {
        if std::env::var("PERP_WITHDRAWAL_RECONCILE").ok().as_deref() != Some("1") {
            return None;
        }
        let http = crate::attested_clock::split_urls(
            "XRPL_RPC_URL",
            &[
                "https://s.altnet.rippletest.net:51234",
                "https://clio.altnet.rippletest.net:51234",
                "https://testnet.xrpl-labs.com",
            ],
        );
        let ws = crate::attested_clock::split_urls(
            "XRPL_WS_URL",
            &[
                "wss://s.altnet.rippletest.net:51233",
                "wss://clio.altnet.rippletest.net:51233",
                "wss://testnet.xrpl-labs.com",
            ],
        );
        let http_url = http.first()?.clone();
        let ws_url = ws.first()?.clone();
        let quorum = std::env::var("PERP_WITHDRAWAL_RECONCILE_QUORUM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5); // testnet pinned-UNL floor (5-of-6); the enclave re-verifies quorum.
        let collect_secs = std::env::var("PERP_WITHDRAWAL_RECONCILE_COLLECT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15);
        let interval_secs = std::env::var("PERP_WITHDRAWAL_RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        // R-b rollback SPV-proves the escrow AccountRoot's Sequence (a STATE-tree inclusion).
        // A dedicated patched getProofPath node (our own xrpld 3.4.1) serves that as one tiny
        // path; PERP_WITHDRAWAL_RECONCILE_PROOF_URL points the rb_fetch at it and flips the
        // producer to getProofPath. Unset → keep the public-node ledger_data reconstruction.
        let rb_proof_url = std::env::var("PERP_WITHDRAWAL_RECONCILE_PROOF_URL").ok();
        let rb_prefer_gpp = rb_proof_url.is_some();
        let rb_http_url = rb_proof_url.unwrap_or_else(|| http_url.clone());
        Some(Self {
            xrpl_http_url: http_url.clone(),
            rb_fetch: SpvFetchConfig {
                http_url: rb_http_url,
                ws_url,
                escrow_account: escrow.to_string(),
                quorum,
                collect_secs,
                prefer_getproofpath: rb_prefer_gpp,
            },
            interval_secs,
        })
    }

    /// The single WS endpoint the R-b fetch subscribes to — so main can add it to the shared
    /// validations-collector union.
    pub fn ws_url(&self) -> &str {
        &self.rb_fetch.ws_url
    }
}

/// Counts from one sweep, for a single summary log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepSummary {
    pub finalized: usize,
    pub rolled_back: usize,
    pub pending: usize,
    pub deferred: usize,
    pub anomaly: usize,
}

/// One full pass over the journal — also the BOOT sweep (the loop runs it immediately on
/// start). Resolves (drops) every entry the enclave confirms terminal; leaves the rest.
pub async fn reconcile_sweep(
    cfg: &ReconcileLoopConfig,
    perp: &PerpClient,
    http: &reqwest::Client,
    journal: &JournalHandle,
    buffer: &Arc<Mutex<ValidationBuffer>>,
) -> SweepSummary {
    let mut summary = SweepSummary::default();
    let journal_rows = journal.snapshot();
    // Follower reserve: this node's ENCLAVE is the arbiter of what it holds in flight. Rows it
    // reserved as a CO-SIGNER were never written to this node's journal (only the node that
    // took the request writes one), so they are settled from the enclave's own listing. An
    // enclave that predates the listing returns None: journal-only, exactly as before.
    let enclave_only: Vec<PendingWithdrawal> = match perp.list_pending_withdrawals().await {
        Ok(Some(p)) => {
            enclave_rows_missing_from_journal(&p.rows, &journal_rows, &cfg.rb_fetch.escrow_account)
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "GAP B reconcile: enclave pending listing failed — journal rows only this pass");
            Vec::new()
        }
    };
    let todo = journal_rows
        .into_iter()
        .filter(|r| !r.settled)
        .map(|r| (r, true))
        .chain(enclave_only.into_iter().map(|r| (r, false)));
    for (row, in_journal) in todo {
        let outcome =
            reconcile_one(&row, perp, http, &cfg.xrpl_http_url, buffer, &cfg.rb_fetch).await;
        match &outcome {
            ReconcileOutcome::Finalized => summary.finalized += 1,
            ReconcileOutcome::RolledBackStolen | ReconcileOutcome::RolledBackExpired => {
                summary.rolled_back += 1
            }
            ReconcileOutcome::StillPending => summary.pending += 1,
            ReconcileOutcome::Deferred(reason) => {
                summary.deferred += 1;
                info!(escrow = %row.escrow, sequence = row.sequence, reason = %reason,
                    "GAP B reconcile: deferred");
            }
            ReconcileOutcome::Anomaly(reason) => {
                summary.anomaly += 1;
                warn!(escrow = %row.escrow, sequence = row.sequence, reason = %reason,
                    "GAP B reconcile: ANOMALY (left in journal for an operator)");
            }
        }
        if outcome.is_terminal() && in_journal {
            // Settled HERE; kept only if some signer is still owed the request.
            journal.settle(&row.escrow, row.sequence);
        }
    }
    summary
}

/// Enclave rows this node holds that its journal does not know (a co-signer's rows), as
/// reconcile entries. Only S and L drive the reconcile; the rest is diagnostics, and the
/// enclave re-derives every figure it acts on from the proof.
pub fn enclave_rows_missing_from_journal(
    rows: &[crate::perp_client::EnclavePendingRow],
    journal: &[PendingWithdrawal],
    escrow: &str,
) -> Vec<PendingWithdrawal> {
    rows.iter()
        .filter(|r| {
            !journal
                .iter()
                .any(|j| j.escrow == escrow && j.sequence == r.sequence && !j.settled)
        })
        .map(|r| PendingWithdrawal {
            escrow: escrow.to_string(),
            sequence: r.sequence,
            last_ledger_sequence: r.last_ledger_sequence,
            amount_drops: r.amount_drops,
            destination: r.destination_hex.clone(),
            destination_tag: r.destination_tag,
            user_id: r.user_id.clone(),
            ..Default::default()
        })
        .collect()
}

/// The long-running reconcile loop: a boot sweep immediately, then one sweep per interval.
pub async fn run_withdrawal_reconcile(
    cfg: ReconcileLoopConfig,
    perp: PerpClient,
    journal: JournalHandle,
    buffer: Arc<Mutex<ValidationBuffer>>,
) {
    let http = reqwest::Client::new();
    let period = std::time::Duration::from_secs(cfg.interval_secs.max(1));
    info!(
        interval_secs = cfg.interval_secs,
        "withdrawal-reconcile loop started (boot sweep first)"
    );
    loop {
        let s = reconcile_sweep(&cfg, &perp, &http, &journal, &buffer).await;
        if s != SweepSummary::default() {
            info!(
                finalized = s.finalized,
                rolled_back = s.rolled_back,
                pending = s.pending,
                deferred = s.deferred,
                anomaly = s.anomaly,
                "GAP B reconcile sweep complete"
            );
        }
        tokio::time::sleep(period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sequence: u32, last_ledger_sequence: u32) -> PendingWithdrawal {
        PendingWithdrawal {
            escrow: "rEscrowAccountForTesting000000000".to_string(),
            sequence,
            last_ledger_sequence,
            amount_drops: 1_000_000,
            destination: "rDestinationForTesting0000000000".to_string(),
            destination_tag: Some(42),
            submitted_hash: Some("ABCD".to_string()),
            user_id: "user-1".to_string(),
            reserved_at_ms: 1_700_000_000_000,
            ..Default::default()
        }
    }

    // ── classify: exhaustive decision table ────────────────────────────────

    #[test]
    fn slot_consumed_when_sequence_moved_past_s() {
        let e = entry(100, 140);
        // escrow.Sequence past S -> consumed, regardless of the ledger/window.
        assert_eq!(classify(101, 50, &e), Verdict::SlotConsumed);
        assert_eq!(classify(101, 200, &e), Verdict::SlotConsumed);
        assert_eq!(classify(999, 141, &e), Verdict::SlotConsumed);
    }

    #[test]
    fn expired_only_when_free_and_strictly_past_l() {
        let e = entry(100, 140);
        assert_eq!(classify(100, 141, &e), Verdict::Expired);
        assert_eq!(classify(100, 10_000, &e), Verdict::Expired);
    }

    #[test]
    fn pending_when_free_and_window_open_including_exactly_l() {
        let e = entry(100, 140);
        assert_eq!(classify(100, 139, &e), Verdict::Pending);
        // Boundary: exactly L is still PENDING — the Payment can validate at L. This is the
        // mirror of the enclave's R-b `ledger <= L -> -74`; a `>=` here would false-expire a
        // withdrawal that then lands, which the enclave would (correctly) refuse to roll back.
        assert_eq!(classify(100, 140, &e), Verdict::Pending);
    }

    #[test]
    fn anomaly_when_sequence_is_behind_s() {
        let e = entry(100, 140);
        assert_eq!(classify(99, 200, &e), Verdict::Anomaly);
        assert_eq!(classify(0, 200, &e), Verdict::Anomaly);
    }

    // ── journal: persistence + identity ────────────────────────────────────

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wr-journal-missing-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let j = Journal::load(&path).expect("missing file -> empty journal");
        assert!(j.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wr-journal-rt-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut j = Journal::default();
        j.upsert(entry(100, 140));
        j.upsert(entry(101, 141));
        j.save(&path).expect("save");
        let back = Journal::load(&path).expect("load");
        assert_eq!(back.entries, j.entries);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_empty() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wr-journal-corrupt-{}.json", std::process::id()));
        std::fs::write(&path, b"{ not json ]").unwrap();
        assert!(
            Journal::load(&path).is_err(),
            "a corrupt journal must error, never silently strand in-flight rows"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_replaces_same_slot_and_remove_drops_it() {
        let mut j = Journal::default();
        j.upsert(entry(100, 140));
        j.upsert(entry(100, 999)); // same (escrow, sequence) -> replace, not duplicate
        assert_eq!(j.len(), 1);
        assert_eq!(
            j.find("rEscrowAccountForTesting000000000", 100)
                .unwrap()
                .last_ledger_sequence,
            999
        );
        assert!(j.remove("rEscrowAccountForTesting000000000", 100));
        assert!(!j.remove("rEscrowAccountForTesting000000000", 100));
        assert!(j.is_empty());
    }

    #[test]
    fn distinct_slots_coexist() {
        let mut j = Journal::default();
        j.upsert(entry(100, 140));
        j.upsert(entry(101, 140));
        assert_eq!(j.len(), 2);
    }

    // ── find_target_index: lookup by RECOMPUTED tx id ──────────────────────

    #[test]
    fn find_target_index_matches_by_recomputed_tx_id() {
        // Arbitrary blobs: find_target_index recomputes the id from the bytes, so the lookup
        // is exercised without a real signed tx (tx_shamap's own suite covers the hash).
        let b0 = hex::encode([0xAAu8; 40]);
        let b1 = hex::encode([0xBBu8; 40]);
        let b2 = hex::encode([0xCCu8; 40]);
        let ledger = serde_json::json!({
            "ledger": { "transactions": [
                { "tx_blob": b0, "meta": "00" },
                { "tx_blob": b1, "meta": "00" },
                { "tx_blob": b2, "meta": "00" },
            ]}
        });
        let target = crate::tx_shamap::tx_id(&[0xBBu8; 40]);
        assert_eq!(find_target_index(&ledger, &target), Some(1));
        let absent = crate::tx_shamap::tx_id(&[0xDDu8; 40]);
        assert_eq!(find_target_index(&ledger, &absent), None);
    }

    #[test]
    fn find_target_index_handles_missing_and_bad_blobs() {
        let good = hex::encode([0x11u8; 40]);
        let ledger = serde_json::json!({
            "ledger": { "transactions": [
                { "meta": "00" },                 // no tx_blob -> skip
                { "tx_blob": "zz", "meta": "00" },// bad hex -> skip
                { "tx_blob": good, "meta": "00" },
            ]}
        });
        let target = crate::tx_shamap::tx_id(&[0x11u8; 40]);
        assert_eq!(find_target_index(&ledger, &target), Some(2));
        // no transactions array at all
        assert_eq!(
            find_target_index(&serde_json::json!({ "ledger": {} }), &target),
            None
        );
    }

    #[test]
    fn reconcile_outcome_terminality() {
        assert!(ReconcileOutcome::Finalized.is_terminal());
        assert!(ReconcileOutcome::RolledBackStolen.is_terminal());
        assert!(ReconcileOutcome::RolledBackExpired.is_terminal());
        assert!(!ReconcileOutcome::StillPending.is_terminal());
        assert!(!ReconcileOutcome::Deferred("x".into()).is_terminal());
        assert!(!ReconcileOutcome::Anomaly("x".into()).is_terminal());
    }

    #[test]
    fn decode_tx_id_requires_32_bytes() {
        assert!(decode_tx_id(&"AB".repeat(32)).is_some());
        assert!(decode_tx_id(&"AB".repeat(31)).is_none()); // 31 bytes
        assert!(decode_tx_id("nothex").is_none());
    }

    // ── rc partition (auditor Ruling-1(a) fail-loud) ───────────────────────

    const B: i64 = crate::attested_clock::SPV_HOST_RC_BASE; // -200

    #[test]
    fn rc_partition_retryable_set() {
        // ecall transients
        for rc in [-60, -70, -71, -74, -77, -81, -87] {
            assert!(rc_is_retryable(rc), "ecall rc {rc} must be retryable");
        }
        // SPV-band transients: under-quorum(-8)/hash(-9)/badsig(-10)/not-in-UNL(-11)/path(-12)
        for off in [-8, -9, -10, -11, -12] {
            assert!(rc_is_retryable(B + off), "spv {off} must be retryable");
        }
    }

    #[test]
    fn rc_partition_anomaly_is_fail_loud_default() {
        // enclave terminals
        for rc in [-17, -72, -73] {
            assert!(
                !rc_is_retryable(rc),
                "enclave rc {rc} must be a terminal anomaly"
            );
        }
        // SPV parse-reject / producer-bug / internal family -> anomaly
        for off in [-1, -7, -13, -14, -17, -19, -20, -21, -22, -99] {
            assert!(
                !rc_is_retryable(B + off),
                "spv {off} (settled/producer) must be a terminal anomaly"
            );
        }
        // unmapped codes default to anomaly (fail-loud), never silent retry
        for rc in [-5000, -1, -100, 0, 42] {
            assert!(
                !rc_is_retryable(rc),
                "unmapped rc {rc} must default to anomaly"
            );
        }
    }

    // ── Follower reserve: delivery bookkeeping ─────────────────────────────

    fn owed(seq: u32, signers: &[&str], settled: bool) -> PendingWithdrawal {
        PendingWithdrawal {
            escrow: "rEsc".into(),
            sequence: seq,
            undelivered: signers.iter().map(|s| s.to_string()).collect(),
            settled,
            ..Default::default()
        }
    }

    #[test]
    fn strict_order_a_signer_owed_an_earlier_request_is_not_asked_a_later_one() {
        let j = vec![owed(100, &["node-3"], false), owed(101, &[], false)];
        assert!(
            signer_owed_earlier(&j, "node-3", 102),
            "node-3 still owes 100 — 102 must queue behind it"
        );
        assert!(!signer_owed_earlier(&j, "node-2", 102));
        assert!(
            !signer_owed_earlier(&j, "node-3", 100),
            "its own entry is not 'earlier'"
        );
    }

    #[test]
    fn next_owed_is_the_lowest_sequence_for_that_signer() {
        let j = vec![
            owed(105, &["node-3"], false),
            owed(101, &["node-3", "node-2"], true),
            owed(103, &["node-2"], false),
        ];
        assert_eq!(next_owed(&j, "node-3").map(|e| e.sequence), Some(101));
        assert_eq!(next_owed(&j, "node-2").map(|e| e.sequence), Some(101));
        assert_eq!(next_owed(&j, "node-1"), None);
    }

    fn tmp_journal(name: &str) -> (JournalHandle, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("wd-journal-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        (JournalHandle::open(&p).unwrap(), p)
    }

    #[test]
    fn settled_entry_is_kept_until_every_signer_has_it() {
        let (j, p) = tmp_journal("settle");
        j.record(owed(100, &["node-3"], false));
        j.settle("rEsc", 100);
        let s = j.snapshot();
        assert_eq!(
            s.len(),
            1,
            "settled here but node-3 is still owed it — kept"
        );
        assert!(s[0].settled);
        j.delivered("rEsc", 100, "node-3");
        assert!(
            j.snapshot().is_empty(),
            "settled AND delivered everywhere — dropped"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn delivered_before_settled_keeps_it_for_reconcile() {
        let (j, p) = tmp_journal("deliver");
        j.record(owed(100, &["node-3"], false));
        j.delivered("rEsc", 100, "node-3");
        let s = j.snapshot();
        assert_eq!(
            s.len(),
            1,
            "everyone has it but this node has not settled it yet"
        );
        assert!(s[0].undelivered.is_empty() && !s[0].settled);
        j.settle("rEsc", 100);
        assert!(j.snapshot().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_fully_delivered_entry_settles_away_immediately() {
        let (j, p) = tmp_journal("plain");
        j.record(owed(100, &[], false));
        j.settle("rEsc", 100);
        assert!(
            j.snapshot().is_empty(),
            "nobody owed — settled = gone, as before"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn journal_with_the_old_shape_still_loads() {
        let old = r#"{"entries":[{"escrow":"rEsc","sequence":7,"last_ledger_sequence":9,"amount_drops":1,
            "destination":"rD","destination_tag":null,"submitted_hash":null,"user_id":"u","reserved_at_ms":0}]}"#;
        let j: Journal = serde_json::from_str(old).unwrap();
        assert_eq!(j.entries[0].sequence, 7);
        assert!(
            j.entries[0].undelivered.is_empty()
                && !j.entries[0].settled
                && j.entries[0].tx_json.is_none()
        );
    }

    #[test]
    fn enclave_rows_unknown_to_the_journal_are_reconciled_too() {
        let row = |s: u32| crate::perp_client::EnclavePendingRow {
            sequence: s,
            last_ledger_sequence: s + 20,
            amount_drops: 5,
            destination_hex: "aa".into(),
            destination_tag: None,
            user_id: "u".into(),
        };
        let journal = vec![owed(100, &[], false), owed(101, &[], true)];
        let got =
            enclave_rows_missing_from_journal(&[row(100), row(101), row(102)], &journal, "rEsc");
        let seqs: Vec<u32> = got.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![101, 102], "100 is reconciled from the journal; a settled journal entry or an unknown row comes from the enclave");
        assert_eq!(got[1].last_ledger_sequence, 122);
        assert_eq!(got[1].escrow, "rEsc");
    }
}
