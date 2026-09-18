//! #131 P3 — the host side of SPV-proven deposits.
//!
//! The enclave can verify a deposit; nothing assembled one. This does: watch for
//! payments into the escrow, rebuild the ledger's transaction SHAMap, read the
//! inclusion path off it, and hand the enclave an `XDEP` blob.
//!
//! **The liveness constraint that shapes this module.** Validator signatures are only
//! available from the live `validations` websocket stream — rippled does not serve them
//! historically. A deposit in ledger N is therefore provable only if we were listening
//! when N was validated. So the collector runs CONTINUOUSLY and keeps a rolling buffer,
//! rather than being started on demand when a deposit is noticed. Getting this wrong
//! does not produce a wrong credit: the enclave refuses a blob without quorum. It
//! produces an uncreditable deposit, which is a liveness failure and an operational
//! problem — the direction a failure should fall, but still one worth not walking into.
//!
//! UNTRUSTED PRODUCER throughout. Every byte is re-derived in the enclave: the tx-ID
//! from the blob, the destination against the sealed escrow, the quorum against the
//! measured-anchor validator set. Nothing here can cause a wrong credit — only a
//! refused one.

#![allow(dead_code)] // wired into the node loop with the P3 activation

use anyhow::{bail, Context, Result};
use std::collections::HashMap;

use crate::spv_proof::{build_xdep_blob, serialize_ledger_header, HEADER_LEN};
use crate::tx_shamap::{tx_id, TxShaMap};

/// How many recent ledgers' validations to keep.
///
/// Sized by what it has to survive, not by taste: a deposit becomes creditable once its
/// ledger is validated, and the driver must still hold those signatures when it gets
/// round to scanning. At roughly 4s per ledger this is about 17 minutes of slack, which
/// covers a restart of the scanner or a slow ledger fetch without covering so much that
/// a stale entry lingers past any plausible use.
pub const VALIDATION_BUFFER_LEDGERS: usize = 256;

/// Rolling store of validation blobs, keyed by the ledger hash they attest.
///
/// Keyed by HASH rather than by sequence deliberately: two different ledgers can share a
/// sequence during a fork, and the enclave checks signatures against the hash it derives
/// from the header. Keying by sequence would let a fork's validations be handed up with
/// the other fork's header, which the enclave would refuse — correctly, and confusingly.
#[derive(Default)]
pub struct ValidationBuffer {
    by_hash: HashMap<String, Vec<Vec<u8>>>,
    /// Insertion order, so the oldest can be dropped without scanning the map.
    order: Vec<String>,
}

impl ValidationBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one validation for a ledger hash. Duplicates from the same validator are
    /// the stream's business, not ours — the enclave counts DISTINCT signers, so a
    /// repeat cannot inflate the quorum.
    pub fn insert(&mut self, ledger_hash: &str, data: Vec<u8>) {
        let e = self
            .by_hash
            .entry(ledger_hash.to_string())
            .or_insert_with(|| {
                self.order.push(ledger_hash.to_string());
                Vec::new()
            });
        e.push(data);
        while self.order.len() > VALIDATION_BUFFER_LEDGERS {
            let oldest = self.order.remove(0);
            self.by_hash.remove(&oldest);
        }
    }

    pub fn get(&self, ledger_hash: &str) -> Option<&Vec<Vec<u8>>> {
        self.by_hash.get(ledger_hash)
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }
}

/// One deposit found in a ledger, with everything the enclave needs to check it.
#[derive(Debug)]
pub struct DepositProof {
    /// The XDEP blob, ready for `ecall_perp_deposit_spv`.
    pub blob: Vec<u8>,
    /// Derived here only for logging and de-duplication on the host side. The enclave
    /// derives its own and does not read this.
    pub tx_id: [u8; 32],
    pub ledger_index: u32,
}

/// Build the proof for the transaction at `target_index` of a ledger.
///
/// `ledger_json` is the `ledger` RPC result with `transactions: true, expand: true,
/// binary: true`; `validations` are the blobs buffered for that ledger's hash.
///
/// ALL of the ledger's transactions are required — the tx tree root is a hash over the
/// whole map, so a partial list cannot reproduce the value the validators signed. That
/// is checked here rather than left to fail in the enclave, because failing at the
/// producer names the cause.
pub fn build_deposit_proof(
    ledger_json: &serde_json::Value,
    target_index: usize,
    validations: &[Vec<u8>],
) -> Result<DepositProof> {
    let ledger = &ledger_json["ledger"];

    // The header comes from `ledger_data` when present, and only falls back to
    // re-serialising from JSON fields when it is not.
    //
    // This is not a preference. The transaction blobs require `binary: true`, and in
    // that mode rippled returns the ledger object as {closed, ledger_data, transactions}
    // — the individual header fields (total_coins, parent_hash, close_time…) are simply
    // absent. An earlier version of this function called serialize_ledger_header
    // unconditionally and could therefore never have worked on the only response shape
    // it is given. Using the bytes rippled already serialised is also strictly better
    // than rebuilding them: there is no field ordering or rounding for us to get wrong.
    let header: [u8; HEADER_LEN] = if let Some(hex) = ledger["ledger_data"].as_str() {
        let raw = unhex(hex).context("ledger_data hex")?;
        if raw.len() != HEADER_LEN {
            bail!("ledger_data is {} bytes, expected {HEADER_LEN}", raw.len());
        }
        let mut h = [0u8; HEADER_LEN];
        h.copy_from_slice(&raw);
        h
    } else {
        serialize_ledger_header(ledger).context("serialize ledger header from JSON fields")?
    };

    let txs = ledger["transactions"]
        .as_array()
        .context("ledger has no expanded transactions array")?;
    if target_index >= txs.len() {
        bail!(
            "transaction index {target_index} out of range ({} in ledger)",
            txs.len()
        );
    }

    let mut items: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(txs.len());
    for (i, t) in txs.iter().enumerate() {
        let tx_hex = t["tx_blob"]
            .as_str()
            .with_context(|| format!("transaction {i} has no tx_blob (binary:true not set?)"))?;
        let meta_hex = t["meta"]
            .as_str()
            .with_context(|| format!("transaction {i} has no meta"))?;
        items.push((unhex(tx_hex)?, unhex(meta_hex)?));
    }

    let map = TxShaMap::build(&items).context("rebuild the transaction SHAMap")?;

    // The rebuilt root must equal the transaction_hash the validators signed. Checked
    // HERE, at the producer, because at this point we can say which ledger and how many
    // transactions; in the enclave the same failure is an anonymous refusal.
    let signed_root = &header[44..76];
    if map.root_hash() != signed_root {
        bail!(
            "rebuilt tx root does not match the signed transaction_hash for ledger {} \
             ({} transactions) — the transaction list is incomplete or altered",
            ledger_json["ledger_index"],
            items.len()
        );
    }

    let (tx_blob, meta) = &items[target_index];
    let key = tx_id(tx_blob);
    let proof = map
        .inclusion_proof(&key)
        .context("read the inclusion path off the rebuilt map")?;

    let val_count = u16::try_from(validations.len())
        .context("more validations than a u16 count can express")?;
    let flat: Vec<u8> = validations.iter().flatten().copied().collect();

    let blob = build_xdep_blob(
        &header,
        val_count,
        &flat,
        tx_blob,
        meta,
        &proof.inner_root_to_leaf,
    )
    .context("assemble the XDEP blob")?;

    // The sequence is read from the HEADER BYTES, not from the JSON. Those bytes are
    // what the validators signed and what the enclave will re-derive its ledger hash
    // from, so taking the number from anywhere else invites the two to disagree — and
    // the JSON carries it at the result level in one mode and inside `ledger` in
    // another, which is exactly the kind of difference that goes unnoticed.
    let ledger_index = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);

    Ok(DepositProof {
        blob,
        tx_id: key,
        ledger_index,
    })
}

/// Indices of the transactions in this ledger that pay `escrow_classic`.
///
/// A filter, not a gate. Everything it decides is re-decided in the enclave against the
/// SEALED escrow address, so a wrong answer here costs a wasted call or a missed
/// deposit — never a wrong credit. It reads the expanded JSON rather than parsing the
/// binary, because being approximate is fine for a filter and parsing is not free.
pub fn find_escrow_payments(ledger_json: &serde_json::Value, escrow_classic: &str) -> Vec<usize> {
    let Some(txs) = ledger_json["ledger"]["transactions"].as_array() else {
        return Vec::new();
    };
    txs.iter()
        .enumerate()
        .filter(|(_, t)| t["TransactionType"] == "Payment" && t["Destination"] == escrow_classic)
        .map(|(i, _)| i)
        .collect()
}

/// Outcome of offering one proven deposit to the enclave.
///
/// The distinction that matters operationally is PERMANENT vs TRANSIENT. A driver that
/// retries everything hammers a refusal that will never change; a driver that retries
/// nothing drops a deposit over a momentary failure. The enclave's codes carry that
/// distinction and this preserves it rather than collapsing everything to an error.
#[derive(Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Credited.
    Credited { user_id: String, amount_fp8: i64 },
    /// Already in the dedup ring (-2), or below the watermark/boundary (-86). Expected
    /// in normal operation — a rescan sees deposits it has already submitted — and never
    /// worth retrying or logging as a failure.
    AlreadySettled,
    /// The enclave refused for a reason that will not change: not a Payment, not to our
    /// escrow, failed, partial, non-native, not in the tree. Retrying is pointless; the
    /// deposit needs a human if it was expected to credit.
    PermanentRefusal(i32),
    /// The boundary is not armed (-85). Not a deposit problem at all — the operator has
    /// not run the activation — so every deposit will refuse until they do. Called out
    /// separately because it is the one refusal that says "stop and do something".
    BoundaryNotArmed,
    /// Transport or an unrecognised code. Worth retrying.
    Transient(String),
}

/// Classify what the enclave said. Pure, so the mapping is testable without a cluster.
///
/// Unknown NEGATIVE codes are treated as PERMANENT, not transient. That is the
/// conservative direction here: a new refusal we do not recognise is far more likely to
/// be a new gate than a hiccup, and retrying it forever would bury the message that
/// something needs attention.
pub fn classify_submit(rc: i32) -> SubmitOutcome {
    match rc {
        -2 | -86 => SubmitOutcome::AlreadySettled,
        -85 => SubmitOutcome::BoundaryNotArmed,
        _ => SubmitOutcome::PermanentRefusal(rc),
    }
}

/// Pull the enclave's `rc=` out of the error text the HTTP layer surfaces.
///
/// The handler reports refusals as a 400 with the code in the message, so this is how a
/// structured outcome is recovered from an unstructured error. If the code cannot be
/// found the failure is TRANSIENT, not permanent: an unparseable error is more likely a
/// transport problem than a verdict, and mistaking one for a permanent refusal would
/// silently drop a creditable deposit.
pub fn outcome_from_error(msg: &str) -> SubmitOutcome {
    if let Some(i) = msg.find("rc=") {
        let rest = &msg[i + 3..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len());
        if let Ok(rc) = rest[..end].parse::<i32>() {
            return classify_submit(rc);
        }
    }
    SubmitOutcome::Transient(msg.to_string())
}

/// Config for the deposit driver.
pub struct DepositDriverConfig {
    /// rippled HTTP RPC, e.g. `http://127.0.0.1:5005`.
    pub http_url: String,
    /// rippled websocket, for the validations stream.
    pub ws_url: String,
    /// The escrow, as a classic address — the same form rippled renders `Destination` in.
    pub escrow_classic: String,
    /// Seconds between scans. A ledger closes roughly every 4s, so anything under that
    /// just re-reads the same ledger.
    pub scan_interval_secs: u64,
}

impl DepositDriverConfig {
    /// Absent `PERP_DEPOSIT_SPV=1`, the driver does not run.
    ///
    /// Opt-in rather than opt-out on purpose: until an operator has armed the boundary,
    /// every submission refuses with -85, and a driver running by default would fill the
    /// log with refusals on every node that upgraded. P3 starts when someone turns it on.
    pub fn from_env(escrow_classic: &str) -> Option<Self> {
        if std::env::var("PERP_DEPOSIT_SPV").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Self {
            http_url: std::env::var("XRPL_RPC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:5005".to_string()),
            ws_url: std::env::var("XRPL_WS_URL")
                .unwrap_or_else(|_| "ws://127.0.0.1:6006".to_string()),
            escrow_classic: escrow_classic.to_string(),
            scan_interval_secs: std::env::var("PERP_DEPOSIT_SPV_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
        })
    }
}

/// Which ledger to resume scanning from, given the enclave's watermark.
///
/// The enclave will refuse anything at or below `max(last_credited, boundary)`, so
/// rescanning below that is wasted work — but it is not HARMFUL, and that asymmetry is
/// why this errs low. Starting too high silently skips a deposit forever; starting too
/// low costs a few refusals that classify as AlreadySettled. Given the choice, lose time.
pub fn resume_from(last_credited: u64, boundary: u64, safety_margin: u64) -> u64 {
    let floor = last_credited.max(boundary);
    floor.saturating_sub(safety_margin)
}

/// Continuously collect validations into the shared buffer.
///
/// Runs for the life of the process and reconnects on failure. It must NOT be started
/// on demand: rippled serves no validation history, so a signature missed while we were
/// not listening is gone, and the deposit it would have proven becomes uncreditable
/// until someone reconciles it by hand.
///
/// Errors are logged and retried rather than propagated. A collector that exits on a
/// dropped websocket is a collector that silently stops proving deposits, and the
/// symptom would appear hours later as "deposits stopped crediting".
pub async fn run_validation_collector(
    ws_url: String,
    buffer: std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    loop {
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                if let Err(e) = ws
                    .send(Message::text(
                        r#"{"command":"subscribe","streams":["validations"]}"#,
                    ))
                    .await
                {
                    tracing::warn!(error = %e, "deposit-spv: validations subscribe failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                tracing::info!("deposit-spv: collecting validations");
                while let Some(msg) = ws.next().await {
                    let Ok(Message::Text(txt)) = msg else { break };
                    let Ok(j) = serde_json::from_str::<serde_json::Value>(&txt) else {
                        continue;
                    };
                    // `full: true` only. A partial validation is not a signature over the
                    // ledger the enclave will check, so buffering one would inflate the
                    // apparent count and produce a blob that fails quorum for a reason
                    // nothing reports.
                    if j["type"] != "validationReceived" || j["full"] != true {
                        continue;
                    }
                    let (Some(lh), Some(data_hex)) =
                        (j["ledger_hash"].as_str(), j["data"].as_str())
                    else {
                        continue;
                    };
                    if let Ok(data) = unhex(data_hex) {
                        if let Ok(mut b) = buffer.lock() {
                            b.insert(lh, data);
                        }
                    }
                }
                tracing::warn!("deposit-spv: validations stream closed, reconnecting");
            }
            Err(e) => tracing::warn!(error = %e, "deposit-spv: ws connect failed"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Scan validated ledgers for escrow payments and submit proofs.
///
/// Sequencer-only, like the reserves publisher: only that node holds the authoritative
/// state, so a follower submitting deposits would be crediting a book it does not own.
/// The flag is re-read every pass rather than captured, because the role changes at
/// runtime and a driver that captured it would keep running after a demotion.
pub async fn run_deposit_scanner(
    cfg: DepositDriverConfig,
    perp: crate::perp_client::PerpClient,
    buffer: std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
    is_sequencer: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let http = reqwest::Client::new();
    // In-memory, deliberately. The enclave's watermark is the durable record of what has
    // been credited, and it refuses anything at or below it — so a restart that rescans
    // costs a handful of AlreadySettled refusals and nothing else. Persisting a cursor
    // here would add a second source of truth that can disagree with the first.
    let mut next_ledger: Option<u64> = None;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(cfg.scan_interval_secs)).await;
        if !is_sequencer.load(Ordering::Relaxed) {
            continue;
        }

        let validated = match fetch_validated_index(&http, &cfg.http_url).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "deposit-spv: cannot read the validated index");
                continue;
            }
        };
        let start = *next_ledger.get_or_insert(validated);
        if start > validated {
            continue;
        }

        // One ledger per pass. Deliberately unhurried: a backlog is not urgent (the
        // deposits are already on XRPL and the watermark keeps them creditable), and
        // sprinting through hundreds of ledgers would hold the enclave busy against the
        // hourly reserves commit, which IS time-sensitive.
        match scan_one_ledger(&http, &cfg, &perp, &buffer, start).await {
            Ok(()) => next_ledger = Some(start + 1),
            Err(e) => {
                // Do NOT advance. A ledger that failed for a transient reason must be
                // retried, and one that failed permanently will keep failing loudly
                // rather than being skipped silently — a skipped ledger is a deposit
                // nobody will ever notice was lost.
                tracing::warn!(ledger = start, error = %format!("{e:#}"),
                               "deposit-spv: ledger scan failed, will retry");
            }
        }
    }
}

async fn fetch_validated_index(http: &reqwest::Client, url: &str) -> Result<u64> {
    let body = serde_json::json!({"method": "ledger",
        "params": [{"ledger_index": "validated", "transactions": false}]});
    let v: serde_json::Value = http.post(url).json(&body).send().await?.json().await?;
    v["result"]["ledger_index"]
        .as_u64()
        .context("no validated ledger_index in the response")
}

async fn scan_one_ledger(
    http: &reqwest::Client,
    cfg: &DepositDriverConfig,
    perp: &crate::perp_client::PerpClient,
    buffer: &std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
    index: u64,
) -> Result<()> {
    // Expanded JSON first, to find the payments cheaply without parsing binary.
    let j: serde_json::Value = http
        .post(&cfg.http_url)
        .json(&serde_json::json!({"method": "ledger", "params": [
            {"ledger_index": index, "transactions": true, "expand": true}]}))
        .send()
        .await?
        .json()
        .await?;
    let result = &j["result"];
    let hits = find_escrow_payments(result, &cfg.escrow_classic);
    if hits.is_empty() {
        return Ok(());
    }

    let ledger_hash = result["ledger_hash"]
        .as_str()
        .context("ledger response has no ledger_hash")?
        .to_string();
    let validations = {
        let b = buffer
            .lock()
            .map_err(|_| anyhow::anyhow!("buffer mutex poisoned"))?;
        b.get(&ledger_hash).cloned()
    };
    let Some(validations) = validations else {
        // We were not listening when this ledger was validated. Said plainly, because
        // the deposit is now uncreditable through SPV and needs a human — silently
        // moving on would lose it.
        bail!(
            "no buffered validations for ledger {index} ({ledger_hash}) —              {} escrow payment(s) in it cannot be proven and need reconciliation",
            hits.len()
        );
    };

    // Binary for the transaction bytes: the tree is rebuilt from what the validators
    // signed, not from a JSON rendering of it.
    let jb: serde_json::Value = http
        .post(&cfg.http_url)
        .json(&serde_json::json!({"method": "ledger", "params": [
            {"ledger_index": index, "transactions": true, "expand": true, "binary": true}]}))
        .send()
        .await?
        .json()
        .await?;

    for idx in hits {
        let proof = build_deposit_proof(&jb["result"], idx, &validations)
            .with_context(|| format!("build proof for tx {idx} of ledger {index}"))?;
        match perp.deposit_spv(&proof.blob).await {
            Ok(v) => tracing::info!(
                ledger = index,
                user = %v["credited_user_id"].as_str().unwrap_or("?"),
                amount_fp8 = v["amount_fp8"].as_i64().unwrap_or(0),
                "deposit-spv: credited"
            ),
            Err(e) => match outcome_from_error(&format!("{e:#}")) {
                SubmitOutcome::AlreadySettled => {}
                SubmitOutcome::BoundaryNotArmed => {
                    bail!("the SPV-deposit boundary is not armed — no deposit can credit until an operator arms it")
                }
                other => tracing::warn!(ledger = index, tx = idx, outcome = ?other,
                                        "deposit-spv: refused"),
            },
        }
    }
    Ok(())
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("odd-length hex ({} chars)", s.len());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow::anyhow!("bad hex at {i}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing: a REAL `ledger` response (binary:true) for validated testnet
    /// ledger 20808565, verbatim. Proves what the unit tests above cannot — that the
    /// rebuilt root reaches the transaction_hash the validators signed, and that the
    /// blob this function emits is the shape the enclave parses.
    ///
    /// This function had no test until the fixture was written, and writing it found
    /// that the function could never have worked: it re-serialised the header from JSON
    /// fields that `binary: true` does not return. A green module around an untested
    /// centre.
    const REAL_LEDGER: &str = include_str!("deposit_spv_vector.json");

    fn real_ledger() -> serde_json::Value {
        serde_json::from_str(REAL_LEDGER).expect("fixture parses")
    }

    #[test]
    fn builds_a_proof_whose_root_matches_the_signed_header() {
        let j = real_ledger();
        // No validations: this test is about the tx-tree half. The quorum is verified by
        // machinery with its own real-manifest suite, and stapling signatures here would
        // make the fixture huge without testing anything that suite does not.
        let p = build_deposit_proof(&j, 3, &[]).expect("proof builds");
        assert_eq!(
            p.ledger_index, 20_808_565,
            "sequence read from the header bytes"
        );
        assert!(
            p.blob.starts_with(b"XDEP"),
            "blob carries the deposit magic"
        );
        assert_ne!(p.tx_id, [0u8; 32]);
    }

    #[test]
    fn every_transaction_in_the_ledger_can_be_proved() {
        let j = real_ledger();
        let n = j["ledger"]["transactions"].as_array().unwrap().len();
        assert_eq!(n, 4, "fixture shape");
        for i in 0..n {
            build_deposit_proof(&j, i, &[])
                .unwrap_or_else(|e| panic!("transaction {i} must be provable: {e:#}"));
        }
    }

    /// Dropping one transaction changes the tree root, so the rebuild can no longer
    /// reach the signed hash. Caught HERE with a message naming the ledger and the
    /// count, rather than as an anonymous refusal inside the enclave.
    #[test]
    fn an_incomplete_transaction_list_is_refused_at_the_producer() {
        let mut j = real_ledger();
        j["ledger"]["transactions"].as_array_mut().unwrap().pop();
        let err = build_deposit_proof(&j, 0, &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not match the signed transaction_hash"),
            "must name the cause, got: {msg}"
        );
    }

    #[test]
    fn an_out_of_range_index_is_refused() {
        let j = real_ledger();
        assert!(build_deposit_proof(&j, 99, &[]).is_err());
    }

    /// A ledger whose `ledger_data` is not a 118-byte header must be refused rather
    /// than truncated into one.
    #[test]
    fn a_malformed_header_is_refused() {
        let mut j = real_ledger();
        j["ledger"]["ledger_data"] = serde_json::json!("00112233");
        assert!(build_deposit_proof(&j, 0, &[]).is_err());
    }

    #[test]
    fn resume_errs_low_because_skipping_is_worse_than_repeating() {
        // Below the floor the enclave refuses as AlreadySettled — cheap. Above it, a
        // deposit is skipped forever — not cheap. So the margin is subtracted.
        assert_eq!(resume_from(1000, 900, 10), 990);
        assert_eq!(
            resume_from(900, 1000, 10),
            990,
            "the higher of the two is the floor"
        );
    }

    #[test]
    fn resume_never_underflows_at_genesis() {
        // A fresh enclave has both at 0; saturating_sub keeps this from wrapping to
        // u64::MAX, which would skip every deposit that will ever exist.
        assert_eq!(resume_from(0, 0, 50), 0);
        assert_eq!(resume_from(5, 0, 50), 0);
    }

    #[test]
    fn already_settled_is_not_a_failure() {
        // A rescan re-offering a credited deposit is NORMAL. Treating -2 as an error
        // would make routine operation look broken.
        assert_eq!(classify_submit(-2), SubmitOutcome::AlreadySettled);
        assert_eq!(classify_submit(-86), SubmitOutcome::AlreadySettled);
    }

    #[test]
    fn an_unarmed_boundary_is_called_out_separately() {
        // Every deposit refuses until an operator arms it, so this must not look like
        // one bad deposit among many.
        assert_eq!(classify_submit(-85), SubmitOutcome::BoundaryNotArmed);
    }

    #[test]
    fn unknown_codes_are_permanent_not_transient() {
        // The conservative direction: an unrecognised refusal is more likely a new gate
        // than a hiccup, and retrying forever would bury it.
        assert_eq!(classify_submit(-19), SubmitOutcome::PermanentRefusal(-19));
        assert_eq!(
            classify_submit(-12345),
            SubmitOutcome::PermanentRefusal(-12345)
        );
    }

    #[test]
    fn codes_are_recovered_from_the_error_text() {
        assert_eq!(
            outcome_from_error("SPV deposit refused (rc=-85)"),
            SubmitOutcome::BoundaryNotArmed
        );
        assert_eq!(
            outcome_from_error("SPV deposit refused (rc=-2)"),
            SubmitOutcome::AlreadySettled
        );
    }

    #[test]
    fn an_unparseable_error_is_transient_not_permanent() {
        // Mistaking a transport failure for a verdict would silently drop a creditable
        // deposit, so the ambiguous case must fall to the retryable side.
        match outcome_from_error("connection reset by peer") {
            SubmitOutcome::Transient(_) => {}
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn buffer_evicts_oldest_and_keeps_the_rest() {
        let mut b = ValidationBuffer::new();
        for i in 0..(VALIDATION_BUFFER_LEDGERS + 10) {
            b.insert(&format!("hash{i}"), vec![i as u8]);
        }
        assert_eq!(b.len(), VALIDATION_BUFFER_LEDGERS);
        assert!(
            b.get("hash0").is_none(),
            "the oldest must have been evicted"
        );
        assert!(
            b.get(&format!("hash{}", VALIDATION_BUFFER_LEDGERS + 9))
                .is_some(),
            "the newest must be retained"
        );
    }

    #[test]
    fn buffer_accumulates_validations_per_ledger() {
        let mut b = ValidationBuffer::new();
        b.insert("L", vec![1]);
        b.insert("L", vec![2]);
        b.insert("L", vec![3]);
        assert_eq!(b.get("L").unwrap().len(), 3, "one entry per validator");
        assert_eq!(b.len(), 1, "still a single ledger");
    }

    /// Keying by hash rather than sequence: a fork gives two ledgers the same sequence,
    /// and mixing their validations would produce a blob the enclave refuses.
    #[test]
    fn two_ledgers_at_the_same_sequence_do_not_collide() {
        let mut b = ValidationBuffer::new();
        b.insert("fork_a", vec![1]);
        b.insert("fork_b", vec![2]);
        assert_eq!(b.get("fork_a").unwrap(), &vec![vec![1u8]]);
        assert_eq!(b.get("fork_b").unwrap(), &vec![vec![2u8]]);
    }

    #[test]
    fn finds_only_payments_to_the_escrow() {
        let j = serde_json::json!({"ledger": {"transactions": [
            {"TransactionType": "Payment",   "Destination": "rESCROW"},
            {"TransactionType": "Payment",   "Destination": "rOTHER"},
            {"TransactionType": "OfferCreate", "Destination": "rESCROW"},
            {"TransactionType": "Payment",   "Destination": "rESCROW"},
        ]}});
        assert_eq!(find_escrow_payments(&j, "rESCROW"), vec![0, 3]);
    }

    #[test]
    fn a_ledger_without_transactions_yields_nothing() {
        let j = serde_json::json!({"ledger": {}});
        assert!(find_escrow_payments(&j, "rESCROW").is_empty());
    }
}
