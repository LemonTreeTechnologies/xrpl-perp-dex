//! XRPL deposit monitoring via JSON-RPC.
//!
//! Rewrite of the `scan_deposits()` function from `perp_orchestrator.py`.
//! Uses raw reqwest HTTP calls to the XRPL JSON-RPC endpoint (no xrpl-rust crate).

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{info, warn};

/// A deposit event detected on the XRPL ledger.
#[derive(Debug, Clone)]
pub struct DepositEvent {
    /// Sender XRPL address (r...)
    pub sender: String,
    /// Deposit amount as FP8 string (e.g., "100.00000000")
    pub amount: String,
    /// XRPL transaction hash (lowercase hex)
    pub tx_hash: String,
    /// XRPL DestinationTag (u32) — identifies the user within the escrow account.
    /// REQ-20-impl R2: consumed by `main.rs` deposit-scan loop; routes the
    /// credit through the enclave's `deposit_bindings` map per REQ-20 §4.2.
    pub destination_tag: Option<u32>,
    /// #131 AC-R1/D-2: the XRPL validated ledger this deposit landed in. Passed to
    /// the enclave deposit ecall, which refuses a ledger below its monotonic
    /// watermark — a structural (buffer-size-independent) replay guard.
    pub ledger_index: u64,
    /// #131 AC-R1 (option-A): true if this was a NATIVE XRP payment (drops), false if
    /// an issued currency (RLUSD). The scanner routes XRP → `deposit_xrp` (credits
    /// `xrp_balance`, an XRP liability matching the XRP escrow custody) and issued
    /// currency → `deposit` (RLUSD margin). Previously ALL deposits went to the RLUSD
    /// route, mis-crediting XRP payments as RLUSD margin — the per-asset custody/
    /// liability mismatch the reserves ceremony surfaced.
    pub is_xrp: bool,
}

/// Monitors XRPL ledger for incoming deposits to an escrow account.
pub struct XrplMonitor {
    client: reqwest::Client,
    rpc_url: String,
    escrow_address: String,
    /// #131 AC-R1: XRPL senders whose Payments to the escrow are OPERATOR CAPITAL
    /// (funding the escrow's on-chain operational fees), NOT user deposits. Their
    /// payments increase custody (the escrow balance the baseline attests) but are
    /// NOT credited to any user, so they are not a liability — the honest way for the
    /// operator to keep the escrow solvent against its own SignerListSet fee spend.
    operator_capital: std::collections::HashSet<String>,
}

/// JSON-RPC request wrapper.
#[derive(Serialize)]
struct JsonRpcRequest {
    method: String,
    params: Vec<serde_json::Value>,
}

impl XrplMonitor {
    pub fn new(rpc_url: &str, escrow_address: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            rpc_url: rpc_url.to_string(),
            escrow_address: escrow_address.to_string(),
            operator_capital: std::collections::HashSet::new(),
        }
    }

    /// #131 AC-R1: register operator-capital sender addresses (see the field doc).
    /// Payments to the escrow from these senders are NOT credited as user deposits.
    pub fn with_operator_capital(mut self, addrs: impl IntoIterator<Item = String>) -> Self {
        self.operator_capital = addrs.into_iter().collect();
        self
    }

    /// Scan for new deposits since `last_ledger`.
    ///
    /// Returns a list of deposit events and the new high-water-mark ledger index.
    ///
    /// #131 AC-R1 (RESP-commitment-scan-progress-fix pagination-straddle MUST-FIX): fetches
    /// EVERY `account_tx` page for the window (following the server `marker` to exhaustion), then
    /// applies the scan rules in the pure `scan_pages`. A single un-paginated request would see
    /// at most one page and could split a ledger across the page boundary, losing the tail — see
    /// `scan_pages` for the straddle-safety argument.
    pub async fn scan_deposits(&self, last_ledger: u32) -> Result<(Vec<DepositEvent>, u32)> {
        let pages = self.fetch_pages(last_ledger).await?;
        Ok(self.scan_pages(last_ledger, &pages))
    }

    /// Fetch every `account_tx` page for the window `(last_ledger, latest]`, following the server
    /// `marker` until it is exhausted (or `MAX_PAGES`, a cyclic-marker backstop). Returns the
    /// ordered `result` objects. A mid-walk RPC-level error stops the walk and returns the pages
    /// gathered so far — their trailing marker makes `scan_pages` cap to the complete high-water.
    /// Transport-level errors propagate.
    async fn fetch_pages(&self, last_ledger: u32) -> Result<Vec<serde_json::Value>> {
        // 50 pages * 200 txs = 10k txs/scan; a sparse escrow never approaches this. It exists only
        // to bound a pathological/cyclic marker (a misbehaving endpoint), not normal operation.
        const MAX_PAGES: usize = 50;
        const PAGE_LIMIT: u32 = 200;

        let mut pages = Vec::new();
        let mut marker: Option<serde_json::Value> = None;

        for _ in 0..MAX_PAGES {
            let mut params = serde_json::json!({
                "account": self.escrow_address,
                "ledger_index_min": last_ledger as i64 + 1,
                "ledger_index_max": -1,
                "forward": true,
                "limit": PAGE_LIMIT,
            });
            if let Some(m) = &marker {
                params["marker"] = m.clone();
            }

            let request = JsonRpcRequest {
                method: "account_tx".to_string(),
                params: vec![params],
            };

            let mut resp: serde_json::Value = self
                .client
                .post(&self.rpc_url)
                .json(&request)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .context("XRPL RPC request failed")?
                .error_for_status()
                .context("XRPL RPC returned error status")?
                .json()
                .await
                .context("XRPL RPC response not valid JSON")?;

            let result = resp["result"].take();

            // RPC-level error: stop the walk, keep the pages already gathered (their trailing
            // marker still points at the first unscanned tx, so scan_pages caps correctly).
            if result.get("error").is_some() {
                warn!(
                    "XRPL account_tx error: {}",
                    result["error_message"].as_str().unwrap_or("unknown")
                );
                break;
            }

            // Read the next marker BEFORE moving `result` into `pages`.
            let next = match result.get("marker") {
                Some(m) if !m.is_null() => Some(m.clone()),
                _ => None,
            };
            pages.push(result);

            match next {
                Some(m) => marker = Some(m),
                None => return Ok(pages), // marker exhausted → the window is fully fetched
            }
        }

        if marker.is_some() {
            warn!(
                pages = pages.len(),
                "account_tx pagination hit MAX_PAGES — deferring the remainder to the next scan"
            );
        }
        Ok(pages)
    }

    /// Pure core of the deposit scan: apply the credit rules to already-fetched `account_tx`
    /// `result` pages (in fetch order) and return the deposits plus a straddle-safe high-water.
    ///
    /// **Straddle-safety (the pagination MUST-FIX).** `account_tx` returns at most one page per
    /// request. If a page ends *inside* a ledger `L` — some of `L`'s txs on this page, the rest on
    /// the next — then naively advancing the high-water to `L` would make the next scan start at
    /// `L+1` and skip `L`'s remaining txs *forever*, losing any creditable deposit among them (and,
    /// because the high-water also seeds the enclave replay watermark, a later manual re-submission
    /// of that deposit would be refused as below-watermark). Two rules close this:
    /// 1. `fetch_pages` follows the marker to exhaustion, so in the normal case the last ledger is
    ///    fully scanned and the high-water is a true complete high-water.
    /// 2. If the walk stopped early with a still-pending marker `m` (MAX_PAGES / RPC error), the
    ///    high-water is capped at `m.ledger - 1` — the last ledger known complete, since `m` points
    ///    at the first still-unscanned tx (in ledger `m.ledger`).
    ///
    /// The per-tx credit decision (tesSUCCESS / Payment / Destination / operator-capital skip /
    /// XRP-vs-issued routing) is unchanged and independent of the high-water accounting.
    fn scan_pages(
        &self,
        last_ledger: u32,
        pages: &[serde_json::Value],
    ) -> (Vec<DepositEvent>, u32) {
        let mut deposits = Vec::new();
        let mut new_ledger = last_ledger;
        // The marker of the last processed page: `Some` ⇒ the walk stopped early (unscanned txs
        // remain, starting at `marker.ledger`) ⇒ cap the high-water; `None` ⇒ window exhausted.
        let mut pending: Option<&serde_json::Value> = None;

        for result in pages {
            let empty = Vec::new();
            let txs = result["transactions"].as_array().unwrap_or(&empty);

            for tx_entry in txs {
                let tx = &tx_entry["tx"];
                let meta = &tx_entry["meta"];

                // #131 AC-R1 scan-progress: advance the high-water for EVERY scanned tx —
                // deposit, non-deposit (e.g. SignerListSet), or operator-capital — before any
                // filter/skip below, so last_ledger never stalls on a tx that is filtered out or
                // skipped (which would re-scan it forever and block detection of any later deposit).
                if let Some(idx) = tx["ledger_index"].as_u64() {
                    new_ledger = new_ledger.max(idx as u32);
                }

                // Only successful Payment transactions to our escrow address
                if meta["TransactionResult"].as_str() != Some("tesSUCCESS") {
                    continue;
                }
                if tx["TransactionType"].as_str() != Some("Payment") {
                    continue;
                }
                if tx["Destination"].as_str() != Some(&self.escrow_address) {
                    continue;
                }

                // Extract amount — native XRP (drops) or issued currency (object with "value")
                let amount = if meta.get("delivered_amount").is_some()
                    && !meta["delivered_amount"].is_null()
                {
                    &meta["delivered_amount"]
                } else {
                    &tx["Amount"]
                };

                // Handle both native XRP (drops string/number) and issued currency (object)
                let value = if let Some(obj) = amount.as_object() {
                    // Issued currency: {"value": "100.50", "currency": "...", "issuer": "..."}
                    match obj.get("value").and_then(|v| v.as_str()) {
                        Some(v) => v.to_string(),
                        None => continue,
                    }
                } else if let Some(drops_str) = amount.as_str() {
                    // Native XRP: amount in drops as string (1 XRP = 1,000,000 drops)
                    let drops: u64 = match drops_str.parse() {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    let xrp = drops as f64 / 1_000_000.0;
                    format!("{xrp:.8}")
                } else if let Some(drops_num) = amount.as_u64() {
                    // Native XRP: amount in drops as number
                    let xrp = drops_num as f64 / 1_000_000.0;
                    format!("{xrp:.8}")
                } else {
                    continue;
                };

                // Parse amount directly as string to avoid f64 precision loss
                // XRPL amounts are already decimal strings like "100.50"
                let fp8_amount = {
                    let parts: Vec<&str> = value.split('.').collect();
                    let integer = parts[0];
                    let frac = if parts.len() > 1 { parts[1] } else { "" };
                    // Pad or truncate fraction to 8 digits
                    let frac_padded = format!("{:0<8}", &frac[..frac.len().min(8)]);
                    format!("{integer}.{frac_padded}")
                };

                // Validate it's a positive amount
                if value.starts_with('-') || value == "0" || value == "0.00000000" {
                    continue;
                }

                let sender = match tx["Account"].as_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                };

                // #131 AC-R1: an operator-capital sender's payment funds the escrow
                // (custody) but is NOT a user deposit → skip crediting. It still raised the
                // escrow balance the baseline attests, so it is over-custody, not a liability.
                if self.operator_capital.contains(&sender) {
                    info!(sender = %sender, "operator-capital payment to escrow — custody, not credited");
                    continue;
                }

                let tx_hash = match tx["hash"].as_str() {
                    Some(h) => h.to_lowercase(),
                    None => continue,
                };

                let destination_tag = tx["DestinationTag"].as_u64().map(|v| v as u32);

                info!(
                    sender = %sender,
                    amount = %fp8_amount,
                    tx_hash = &tx_hash[..16.min(tx_hash.len())],
                    destination_tag = ?destination_tag,
                    "deposit detected"
                );

                // #131 AC-R1/D-2: the deposit's own validated ledger (0 if the field is
                // absent — the enclave then refuses it once the watermark advances, which
                // is the fail-safe direction).
                let deposit_ledger = tx["ledger_index"].as_u64().unwrap_or(0);

                // #131 AC-R1 (option-A) asset routing: native XRP (drops) is a scalar;
                // issued currency (RLUSD) is an object. Route XRP → xrp_balance so the
                // liability's asset matches the XRP escrow custody.
                let is_xrp = !amount.is_object();

                deposits.push(DepositEvent {
                    sender,
                    amount: fp8_amount,
                    tx_hash: tx_hash[..64.min(tx_hash.len())].to_string(),
                    destination_tag,
                    ledger_index: deposit_ledger,
                    is_xrp,
                });
            }

            match result.get("marker") {
                Some(m) if !m.is_null() => pending = Some(m),
                _ => {
                    // This page exhausted the window (no further marker) — stop; the high-water
                    // is complete and needs no cap.
                    pending = None;
                    break;
                }
            }
        }

        let final_ledger = cap_to_complete(new_ledger, pending, last_ledger);
        (deposits, final_ledger)
    }
}

/// Given the high-water reached so far and the marker of the last-processed `account_tx` page,
/// return the highest ledger we can safely treat as fully scanned. A pending marker's `ledger`
/// field is the ledger of the next tx to be returned, so every ledger strictly below it is
/// complete and `marker.ledger - 1` is the safe high-water. Never advances below `floor`
/// (`last_ledger`) or above the reached `new_ledger`. `None` ⇒ the window was exhausted and
/// `new_ledger` is already complete.
fn cap_to_complete(new_ledger: u32, pending: Option<&serde_json::Value>, floor: u32) -> u32 {
    match pending {
        None => new_ledger,
        Some(m) => match m["ledger"].as_u64() {
            Some(l) => {
                let complete = l.saturating_sub(1).min(u32::MAX as u64) as u32;
                new_ledger.min(complete).max(floor)
            }
            // A pending marker without a usable `ledger` field: do not advance past the floor,
            // so the whole window is re-scanned next cycle (safe, deposits dedup in the enclave).
            None => floor,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ESCROW: &str = "rEscrowTestAccount";

    fn monitor() -> XrplMonitor {
        XrplMonitor::new("http://unused.test", ESCROW)
    }

    /// A successful native-XRP Payment `tx_entry` to the escrow.
    fn xrp_payment(sender: &str, drops: &str, ledger: u64, hash: &str) -> Value {
        json!({
            "tx": {
                "TransactionType": "Payment",
                "Account": sender,
                "Destination": ESCROW,
                "Amount": drops,
                "hash": hash,
                "ledger_index": ledger,
            },
            "meta": { "TransactionResult": "tesSUCCESS", "delivered_amount": drops },
        })
    }

    /// A non-deposit (SignerListSet) `tx_entry` — the escrow paying its own multisig fee.
    fn signer_list_set(ledger: u64, hash: &str) -> Value {
        json!({
            "tx": {
                "TransactionType": "SignerListSet",
                "Account": ESCROW,
                "hash": hash,
                "ledger_index": ledger,
            },
            "meta": { "TransactionResult": "tesSUCCESS" },
        })
    }

    /// An `account_tx` `result` page. `marker` = `Some((ledger, seq))` ⇒ more pages follow.
    fn page(txs: Vec<Value>, marker: Option<(u64, u64)>) -> Value {
        let mut p = json!({ "transactions": txs });
        if let Some((ledger, seq)) = marker {
            p["marker"] = json!({ "ledger": ledger, "seq": seq });
        }
        p
    }

    // The pagination-straddle regression: a page boundary falls inside ledger 100 — page 1 carries
    // only the escrow's SignerListSet at 100, page 2 carries ledger 100's actual deposit plus a
    // ledger-101 deposit. A single-page scan would advance to 100 and lose the ledger-100 deposit
    // forever; scanning both pages credits it.
    #[test]
    fn straddle_across_pages_does_not_drop_the_tail_deposit() {
        let m = monitor();
        let pages = vec![
            page(vec![signer_list_set(100, "H0")], Some((100, 5))),
            page(
                vec![
                    xrp_payment("rAlice", "1000000", 100, "H1"),
                    xrp_payment("rBob", "2000000", 101, "H2"),
                ],
                None,
            ),
        ];
        let (deposits, new_ledger) = m.scan_pages(99, &pages);

        assert_eq!(deposits.len(), 2, "both page-2 deposits must be credited");
        assert_eq!(deposits[0].sender, "rAlice");
        assert_eq!(deposits[0].ledger_index, 100);
        assert!(deposits[0].is_xrp);
        assert_eq!(deposits[0].amount, "1.00000000");
        assert_eq!(deposits[1].sender, "rBob");
        assert_eq!(deposits[1].ledger_index, 101);
        assert_eq!(
            new_ledger, 101,
            "high-water is the exhausted window's max ledger"
        );
    }

    // Early stop with a still-pending marker (MAX_PAGES / RPC error): a tx at ledger 100 was seen
    // but ledger 100 is incomplete (marker points back into it), so the high-water must be capped
    // at 99 — ledger 100 is re-scanned next cycle rather than being skipped.
    #[test]
    fn pending_marker_caps_high_water_to_last_complete_ledger() {
        let m = monitor();
        let pages = vec![page(
            vec![
                xrp_payment("rAlice", "1000000", 99, "H1"),
                signer_list_set(100, "H0"),
            ],
            Some((100, 3)),
        )];
        let (deposits, new_ledger) = m.scan_pages(98, &pages);

        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].ledger_index, 99);
        assert_eq!(
            new_ledger, 99,
            "capped to marker.ledger-1, not the partially-scanned ledger 100"
        );
    }

    // The original stall fix: an operator-capital (skipped) tx must still advance the high-water,
    // so the scan does not re-process it forever.
    #[test]
    fn operator_capital_tx_is_skipped_but_still_advances_high_water() {
        let m = monitor().with_operator_capital(["rOperator".to_string()]);
        let pages = vec![page(
            vec![xrp_payment("rOperator", "5000000", 100, "H1")],
            None,
        )];
        let (deposits, new_ledger) = m.scan_pages(99, &pages);

        assert!(
            deposits.is_empty(),
            "operator-capital payment is not credited"
        );
        assert_eq!(new_ledger, 100, "but the high-water advances past it");
    }

    // Sanity: the ordinary single-page exhausted case still credits and advances.
    #[test]
    fn single_page_exhausted_credits_and_advances() {
        let m = monitor();
        let pages = vec![page(
            vec![xrp_payment("rAlice", "1000000", 100, "H1")],
            None,
        )];
        let (deposits, new_ledger) = m.scan_pages(99, &pages);

        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].sender, "rAlice");
        assert_eq!(new_ledger, 100);
    }

    // No pages fetched at all (page-0 RPC error) ⇒ no deposits, high-water unchanged.
    #[test]
    fn empty_pages_leave_high_water_unchanged() {
        let m = monitor();
        let (deposits, new_ledger) = m.scan_pages(42, &[]);
        assert!(deposits.is_empty());
        assert_eq!(new_ledger, 42);
    }
}
