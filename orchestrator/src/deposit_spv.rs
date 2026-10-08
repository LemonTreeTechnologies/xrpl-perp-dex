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

    // FRAMED, not concatenated — the same defect the clock driver shipped with. This
    // path had never run, so nothing had ever refused it.
    let blob = build_xdep_blob(
        &header,
        validations,
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
/// something needs attention. It is also what keeps the enclave's banded SPV causes
/// (`-201..-299`) from being mistaken for anything benign.
///
/// THE -2 AMBIGUITY, now closed in the enclave and worth recording here. The enclave used
/// to return `xrpl_spv_parse_deposit_payment`'s raw code, and `XRPL_SPV_ERR_TRUNCATED` is
/// -2 — the very number this function maps to `AlreadySettled`, "expected in normal
/// operation and never worth logging as a failure". A malformed deposit payload therefore
/// read as a benign repeat and was swallowed. The enclave now bands that return, so -2
/// means only the dedup ring. Against an enclave built before 2026-09-27 the ambiguity is
/// unavoidable from here — nothing in the number distinguishes the two — which is why the
/// fix had to be on the enclave side.
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
    pub http_urls: Vec<String>,
    /// rippled websocket, for the validations stream.
    pub ws_urls: Vec<String>,
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
            // Same ruling as the clock, and for the same reason: the audit confirmed the
            // deposit path self-verifies as completely — same xrpl_spv_verify_quorum plus a
            // SHAMap inclusion proof and pinned issuer/escrow checks — so a source can only
            // WITHHOLD or censor a deposit, never forge or mis-credit one. Censorship is the
            // liveness risk multiple sources address. The previous default here was also
            // 127.0.0.1:5005, which no node runs, and it is why deposit-spv has never been
            // operationally live despite being recorded as such.
            http_urls: crate::attested_clock::split_urls(
                "XRPL_RPC_URL",
                &[
                    "https://s.altnet.rippletest.net:51234",
                    "https://clio.altnet.rippletest.net:51234",
                    "https://testnet.xrpl-labs.com",
                ],
            ),
            ws_urls: crate::attested_clock::split_urls(
                "XRPL_WS_URL",
                &[
                    "wss://s.altnet.rippletest.net:51233",
                    "wss://clio.altnet.rippletest.net:51233",
                    "wss://testnet.xrpl-labs.com",
                ],
            ),
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
/// One collector per endpoint, all writing into ONE buffer.
///
/// Merging streams is safe because a validation is self-verifying: the enclave checks each
/// signature against the pinned UNL and counts DISTINCT signers, so a duplicate from a second
/// source cannot inflate a quorum and a forged one cannot join it. What merging buys is the
/// thing the audit ruled this on — liveness. A validation is only available LIVE (rippled
/// serves no validation history), so missing the moment a ledger closed means missing it for
/// good; being subscribed in three places makes that need three simultaneous failures.
pub async fn run_validation_collectors(
    ws_urls: Vec<String>,
    buffer: std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
) {
    let mut set = Vec::new();
    for url in ws_urls {
        set.push(tokio::spawn(run_validation_collector(url, buffer.clone())));
    }
    for h in set {
        let _ = h.await;
    }
}

/// Where one websocket connection ended.
///
/// Returned rather than logged in place so a test can assert WHICH of the three happened. A
/// peer that closed, a peer that errored and a peer that went SILENT are different faults, and
/// telling the third one apart is the entire reason the idle timeout below exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpEnd {
    /// The peer sent a Close frame, or the stream simply ran out.
    Closed,
    /// The transport failed.
    Failed(String),
    /// Nothing at all arrived for the idle window.
    IdleTimeout,
}

/// How long a validations stream may stay SILENT before we treat it as dead.
///
/// It has to be a read timeout, not a connect timeout: the fault it exists for is a half-open
/// socket, where the connection succeeded and the peer has since vanished without a FIN. An
/// unbounded `ws.next()` then waits forever, the collector never reconnects, and NOTHING is
/// logged — one of the three sources is silently gone. Liveness is the only thing the
/// multi-source set buys us (a validation is self-verifying, so merging streams cannot inflate
/// a quorum), so a source that dies quietly is precisely the failure this design exists to
/// prevent.
///
/// Testnet closes a ledger roughly every 4s and each close yields several validations, so a
/// minute of silence is two orders of magnitude past normal.
const VALIDATION_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read one connection's frames into the buffer until that connection ends.
///
/// Split out of the reconnect loop so a test can reach it, and takes the idle window as an
/// argument so a test can use a short one. `run_validation_collector` passes
/// `VALIDATION_IDLE_TIMEOUT`.
pub async fn pump_validations<S, E>(
    ws: &mut S,
    buffer: &std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
    idle: std::time::Duration,
) -> PumpEnd
where
    S: futures_util::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    loop {
        let msg = match tokio::time::timeout(idle, ws.next()).await {
            Err(_elapsed) => return PumpEnd::IdleTimeout,
            Ok(None) => return PumpEnd::Closed,
            Ok(Some(Err(e))) => return PumpEnd::Failed(e.to_string()),
            Ok(Some(Ok(m))) => m,
        };

        let txt = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return PumpEnd::Closed,
            // A KEEPALIVE IS NOT A REASON TO TEAR DOWN THE CONNECTION. tungstenite answers a
            // Ping with a Pong itself and ALSO hands the Ping to us (tungstenite-0.24
            // protocol/mod.rs:611, asserted by its own tests at :829), so the previous
            // `let Ok(Message::Text(txt)) = msg else { break }` dropped the whole stream on
            // any server keepalive.
            //
            // WHAT IS MEASURED, and it is not what I first claimed: our client took 163 text
            // frames and ZERO pings from clio in 90s with the stream still open
            // (`frame_kinds_a_real_endpoint_sends_us`), while a python client on the bastion
            // saw a server ping every ~5s on the SAME endpoint. So ping delivery is
            // connection- or path-dependent — Cloudflare fronts that host and is what 418s
            // us — and pings are NOT the cause of the ~60s reconnect cycle the cluster shows
            // (1322 stream-closes in 24h on node-1). That cause is still unidentified; the
            // per-source field and the three distinct endings below are what will name it.
            // This arm is correct either way: a reader that ends its stream on a frame kind
            // it merely does not consume is wrong whether or not the frame has arrived yet.
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                continue
            }
        };

        let Ok(j) = serde_json::from_str::<serde_json::Value>(&txt) else {
            continue;
        };
        // `full: true` only. A partial validation is not a signature over the ledger the
        // enclave will check, so buffering one would inflate the apparent count and produce a
        // blob that fails quorum for a reason nothing reports.
        if j["type"] != "validationReceived" || j["full"] != true {
            continue;
        }
        let (Some(lh), Some(data_hex)) = (j["ledger_hash"].as_str(), j["data"].as_str()) else {
            continue;
        };
        if let Ok(data) = unhex(data_hex) {
            if let Ok(mut b) = buffer.lock() {
                b.insert(lh, data);
            }
        }
    }
}

pub async fn run_validation_collector(
    ws_url: String,
    buffer: std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
) {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    // A RECONNECT counter, not a health counter. It increments once per successful connect, so
    // a high value means the stream keeps dying. Logged because reading the bare
    // "collecting validations" count across three sources as 193/91/1 looks like "the first
    // two are busiest" when it means "the first two are flapping and the third is stable" —
    // which is how I read it and got it backwards.
    let mut opens: u64 = 0;

    loop {
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                if let Err(e) = ws
                    .send(Message::text(
                        r#"{"command":"subscribe","streams":["validations"]}"#,
                    ))
                    .await
                {
                    // EVERY branch names the source. Only the success line did, so 1086
                    // failures in 24h could not be attributed to one of three endpoints —
                    // and a per-source fault is exactly what a multi-source set must be able
                    // to see.
                    tracing::warn!(source = %ws_url, error = %e,
                        "deposit-spv: validations subscribe failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                opens += 1;
                tracing::info!(source = %ws_url, opens, "collecting validations");
                match pump_validations(&mut ws, &buffer, VALIDATION_IDLE_TIMEOUT).await {
                    PumpEnd::Closed => tracing::warn!(source = %ws_url,
                        "deposit-spv: validations stream closed, reconnecting"),
                    PumpEnd::Failed(e) => tracing::warn!(source = %ws_url, error = %e,
                        "deposit-spv: validations stream failed, reconnecting"),
                    PumpEnd::IdleTimeout => tracing::warn!(source = %ws_url,
                        idle_secs = VALIDATION_IDLE_TIMEOUT.as_secs(),
                        "deposit-spv: validations stream went SILENT, reconnecting"),
                }
            }
            Err(e) => tracing::warn!(source = %ws_url, error = %e,
                "deposit-spv: ws connect failed"),
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

        // First endpoint that answers. Unlike the clock there is no "newest" to prefer here:
        // the scanner walks forward from where it left off, so a lagging endpoint costs a
        // round rather than stalling us, and the next tick asks again.
        let Some(primary) = first_reachable(&http, &cfg.http_urls).await else {
            tracing::warn!(
                tried = cfg.http_urls.len(),
                "deposit-spv: no XRPL endpoint answered; retrying"
            );
            tokio::time::sleep(std::time::Duration::from_secs(cfg.scan_interval_secs)).await;
            continue;
        };
        let validated = match fetch_validated_index(&http, &primary).await {
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
        match scan_one_ledger(&http, &primary, &cfg, &perp, &buffer, start).await {
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

/// The first endpoint that answers a trivial query. Tried in order; the list is short and a
/// tick has time to spare, so a sequential walk keeps a dead endpoint to a skipped entry.
async fn first_reachable(http: &reqwest::Client, urls: &[String]) -> Option<String> {
    for u in urls {
        if fetch_validated_index(http, u).await.is_ok() {
            return Some(u.clone());
        }
        tracing::debug!(url = %u, "deposit-spv: endpoint did not answer");
    }
    None
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
    rpc_url: &str,
    cfg: &DepositDriverConfig,
    perp: &crate::perp_client::PerpClient,
    buffer: &std::sync::Arc<std::sync::Mutex<ValidationBuffer>>,
    index: u64,
) -> Result<()> {
    // Expanded JSON first, to find the payments cheaply without parsing binary.
    let j: serde_json::Value = http
        .post(rpc_url)
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
        .post(rpc_url)
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
                other => {
                    // Name the cause when the enclave banded it. A bare "-202" in the log
                    // is the -72 problem again, one path over.
                    let cause = match &other {
                        SubmitOutcome::PermanentRefusal(rc) => {
                            crate::attested_clock::spv_cause(*rc as i64)
                        }
                        _ => None,
                    };
                    tracing::warn!(ledger = index, tx = idx, outcome = ?other,
                                   cause = cause.unwrap_or("(no banded cause)"),
                                   "deposit-spv: refused")
                }
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
    /// Proves OUR CLIENT can reach the public validations stream — not that the endpoint
    /// serves it.
    ///
    /// THE DISTINCTION IS THE WHOLE POINT, and it cost a deploy on 2026-10-07. The endpoints
    /// were verified by hand with a python TLS client, and the ports were verified reachable
    /// from every node, so "the source works" was established. What was never checked was
    /// whether `tokio_tungstenite::connect_async` could speak TLS at all — the crate was built
    /// with `default-features = false` and no TLS backend, because the previous default URL was
    /// `ws://127.0.0.1:6006` and plaintext never needed one. Moving the defaults to `wss://`
    /// turned every connection into "URL error: TLS support not compiled in", and the clock sat
    /// at advances=0 refusals=0 for three minutes looking inconclusive.
    ///
    /// `#[ignore]` because it needs the network and CI must stay hermetic. Run it by hand after
    /// touching the TLS features or the endpoint defaults:
    ///
    ///     cargo test --locked ws_tls -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs outbound network; run by hand after touching TLS features or endpoints"]
    async fn ws_tls_really_connects_and_validations_arrive() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        // The same call main makes. Without it this test panics inside rustls rather than
        // failing on the network, which is a different bug wearing the same red.
        crate::attested_clock::install_tls_provider();
        let url = "wss://s.altnet.rippletest.net:51233";
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "connect_async({url}) failed: {e} \
                 — if this says 'TLS support not compiled in', the tokio-tungstenite \
                 features in Cargo.toml lost their rustls backend"
                )
            });
        ws.send(Message::text(
            r#"{"command":"subscribe","streams":["validations"]}"#,
        ))
        .await
        .expect("subscribe");

        // One real validation frame is enough: it proves TLS, the handshake, the subscription
        // and the stream, which is every link the collector depends on.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut saw = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await {
                Ok(Some(Ok(Message::Text(txt)))) => {
                    if txt.contains("validationReceived") && txt.contains("ledger_hash") {
                        saw = true;
                        break;
                    }
                }
                Ok(Some(Ok(_))) => continue,
                _ => break,
            }
        }
        assert!(saw, "no validationReceived frame in 30s from {url}");
    }

    use super::*;

    /// One REAL frame off `wss://clio.altnet.rippletest.net:51233`, captured 2026-10-08.
    ///
    /// Every key and value is as the endpoint sent it. Only `data` is shortened — the capture
    /// script truncated it — and that is safe here because the pump only unhexes it; nothing in
    /// this file inspects the validation blob. Written out rather than invented because a
    /// fixture built from my idea of the shape passes tautologically: the live frame carries
    /// thirteen keys, including `cookie` and `network_id`, which I would not have guessed.
    const REAL_VALIDATION_FRAME: &str = r#"{"cookie":"6079102537327064379","data":"2280000001260146177129325A06CA3A1234567890ABCDEF","flags":2147483649,"full":true,"ledger_hash":"451C6E1D2A1B5BFA776CB54B3CBC35F1B77F06F6D27C1229384EF279369BFC05","ledger_index":"21370737","master_key":"nHUCAdca6VoWWYVdBH1bwCUQggEX2e5acQSqxM3DwyuhsFknxmh3","network_id":1,"signature":"304402204B2CA83844D82E8EC6199E904D7AA6DFAD3FE131BB1376AF8354D49ABDB1FFF302205507A77ACC2A76FA68D8345135AEA80FA729A0AAD1D62C156A25D4D4CF2548A3","signing_time":844760778,"type":"validationReceived","validated_hash":"E890251C88D063C4DB4D507EB19F5E04D035506C9BFE4CAA0E8DB7A72DFB5D5F","validation_public_key":"n9KWVA64rMeqkAvcQ4DNCa2eDXTzprCtK1HLC8H5PEyUVwSSyL5X"}"#;

    const REAL_LEDGER_HASH: &str =
        "451C6E1D2A1B5BFA776CB54B3CBC35F1B77F06F6D27C1229384EF279369BFC05";

    /// Serve ONE websocket connection on loopback and run `script` against it.
    ///
    /// Loopback is the right harness here and not a shortcut past a cluster run: the seam under
    /// test is OUR READER against frames a peer sends, so the far side only has to produce those
    /// frames. What a real endpoint adds — TLS, the subscription, the public internet — is
    /// covered by the `#[ignore]`d network tests below, which is where it belongs.
    async fn serve_once<F, Fut>(script: F) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let ws = tokio_tungstenite::accept_async(sock)
                .await
                .expect("server handshake");
            script(ws).await;
        });
        format!("ws://{addr}")
    }

    async fn connect(
        url: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        tokio_tungstenite::connect_async(url)
            .await
            .expect("client handshake")
            .0
    }

    fn empty_buffer() -> std::sync::Arc<std::sync::Mutex<ValidationBuffer>> {
        std::sync::Arc::new(std::sync::Mutex::new(ValidationBuffer::new()))
    }

    /// A SILENT stream must end the pump instead of wedging it forever.
    ///
    /// This is the half-open socket: the handshake completed and the peer then vanished without
    /// a FIN, so the kernel has nothing to report and `ws.next()` never returns. Before the
    /// timeout, that collector was gone for the life of the process with NOT ONE log line —
    /// one of the three liveness sources silently consumed, which is the single failure mode a
    /// multi-source set exists to prevent.
    #[tokio::test]
    async fn a_silent_stream_times_out_instead_of_wedging_forever() {
        let url = serve_once(|ws| async move {
            // Hold the connection open and send NOTHING, ever.
            let _held = ws;
            futures_util::future::pending::<()>().await;
        })
        .await;

        let mut ws = connect(&url).await;
        let buffer = empty_buffer();
        let end = pump_validations(&mut ws, &buffer, std::time::Duration::from_millis(300)).await;

        assert_eq!(end, PumpEnd::IdleTimeout, "a silent peer must time out");
    }

    /// A KEEPALIVE PING MUST NOT END THE STREAM, and the validation after it must still land.
    ///
    /// This is the probe for the defect, and the buffer assertion is the part that discriminates:
    /// `PumpEnd` alone cannot tell the two implementations apart, because the old code ended at
    /// the ping and returned the same "closed" it returns here. Only the validation that arrives
    /// AFTER the ping separates them — on the old reader the buffer comes back empty.
    #[tokio::test]
    async fn a_keepalive_ping_does_not_end_the_stream_and_the_next_validation_still_lands() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let url = serve_once(|mut ws| async move {
            ws.send(Message::Ping(vec![])).await.expect("ping");
            ws.send(Message::text(REAL_VALIDATION_FRAME))
                .await
                .expect("validation");
            ws.send(Message::Close(None)).await.expect("close");
        })
        .await;

        let mut ws = connect(&url).await;
        let buffer = empty_buffer();
        let end = pump_validations(&mut ws, &buffer, std::time::Duration::from_secs(10)).await;

        assert_eq!(
            end,
            PumpEnd::Closed,
            "the stream must end on the CLOSE, not on the ping"
        );
        let b = buffer.lock().expect("buffer");
        assert_eq!(
            b.len(),
            1,
            "the validation sent AFTER the ping was dropped — the reader ended at the keepalive"
        );
        assert!(
            b.get(REAL_LEDGER_HASH).is_some_and(|v| v.len() == 1),
            "the validation landed under the wrong ledger hash"
        );
    }

    /// A partial validation must be ignored even though it is well-formed JSON.
    ///
    /// Buffering one would inflate the apparent count and produce a blob that fails quorum for
    /// a reason nothing reports. Probed by flipping `full` on an otherwise REAL frame, so the
    /// only difference between this test and the one above is the field under test.
    #[tokio::test]
    async fn a_partial_validation_is_not_buffered() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let partial = REAL_VALIDATION_FRAME.replace(r#""full":true"#, r#""full":false"#);
        assert_ne!(
            partial, REAL_VALIDATION_FRAME,
            "the fixture edit did not land"
        );

        let url = serve_once(move |mut ws| async move {
            ws.send(Message::text(partial)).await.expect("partial");
            ws.send(Message::Close(None)).await.expect("close");
        })
        .await;

        let mut ws = connect(&url).await;
        let buffer = empty_buffer();
        let end = pump_validations(&mut ws, &buffer, std::time::Duration::from_secs(10)).await;

        assert_eq!(end, PumpEnd::Closed);
        assert_eq!(buffer.lock().expect("buffer").len(), 0);
    }

    /// WHAT FRAME KINDS DOES A REAL ENDPOINT ACTUALLY SEND US?
    ///
    /// Not a pass/fail test — a measurement, kept because the question it answers was settled
    /// twice by inference and both times wrongly. A python client saw a server PING every ~5s
    /// on clio, while the live collector's connections lasted ~60s; those two cannot both
    /// describe a reader that tears down on a ping, so the frame kinds OUR client receives had
    /// to be measured rather than derived from the dependency's source.
    ///
    /// RESULT 2026-10-08 from the laptop: `text=163 ping=0 pong=0 binary=0 other=0`, ended
    /// `still open at the deadline`. Re-run it from a NODE before concluding anything about
    /// the cluster: the nodes are the hosts getting 418'd, and this ran from somewhere else.
    ///
    ///     cargo test --locked frame_kinds -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs outbound network; a measurement, not an assertion"]
    async fn frame_kinds_a_real_endpoint_sends_us() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        crate::attested_clock::install_tls_provider();
        let url = "wss://clio.altnet.rippletest.net:51233";
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect");
        ws.send(Message::text(
            r#"{"command":"subscribe","streams":["validations"]}"#,
        ))
        .await
        .expect("subscribe");

        let (mut text, mut ping, mut pong, mut binary, mut other) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        let mut ended = "still open at the deadline";
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(20), ws.next()).await {
                Err(_) => {
                    ended = "20s of silence";
                    break;
                }
                Ok(None) => {
                    ended = "stream ended (None)";
                    break;
                }
                Ok(Some(Err(e))) => {
                    println!("  transport error: {e}");
                    ended = "transport error";
                    break;
                }
                Ok(Some(Ok(Message::Text(_)))) => text += 1,
                Ok(Some(Ok(Message::Ping(_)))) => ping += 1,
                Ok(Some(Ok(Message::Pong(_)))) => pong += 1,
                Ok(Some(Ok(Message::Binary(_)))) => binary += 1,
                Ok(Some(Ok(Message::Close(c)))) => {
                    println!("  close frame: {c:?}");
                    ended = "close frame";
                    break;
                }
                Ok(Some(Ok(_))) => other += 1,
            }
        }
        println!(
            "  {url}\n  ended={ended}\n  text={text} ping={ping} pong={pong} binary={binary} other={other}"
        );
    }

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

        // The banded SPV causes must NOT land anywhere benign. -202 is a truncated blob;
        // before the enclave banded it, it arrived as -2 and this function called it
        // AlreadySettled — a structural refusal read as a normal repeat.
        for e in [2i32, 17, 19, 20, 22, 99] {
            let rc = -200 - e;
            assert_eq!(
                classify_submit(rc),
                SubmitOutcome::PermanentRefusal(rc),
                "banded cause {rc} must be a refusal, never AlreadySettled"
            );
            assert!(
                crate::attested_clock::spv_cause(rc as i64).is_some(),
                "banded cause {rc} must be nameable in the log"
            );
        }
        assert_ne!(
            classify_submit(-202),
            SubmitOutcome::AlreadySettled,
            "a truncated blob is not a settled deposit"
        );
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
