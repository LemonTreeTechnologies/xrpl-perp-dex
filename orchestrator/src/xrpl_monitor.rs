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
    pub async fn scan_deposits(&self, last_ledger: u32) -> Result<(Vec<DepositEvent>, u32)> {
        let params = serde_json::json!({
            "account": self.escrow_address,
            "ledger_index_min": last_ledger as i64 + 1,
            "ledger_index_max": -1,
            "forward": true,
        });

        let request = JsonRpcRequest {
            method: "account_tx".to_string(),
            params: vec![params],
        };

        let resp: serde_json::Value = self
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

        let result = &resp["result"];

        // Check for RPC-level errors
        if result.get("error").is_some() {
            warn!(
                "XRPL account_tx error: {}",
                result["error_message"].as_str().unwrap_or("unknown")
            );
            return Ok((vec![], last_ledger));
        }

        let txs = result["transactions"]
            .as_array()
            .unwrap_or(&Vec::new())
            .clone();

        let mut deposits = Vec::new();
        let mut new_ledger = last_ledger;

        for tx_entry in &txs {
            let tx = &tx_entry["tx"];
            let meta = &tx_entry["meta"];

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
            let amount =
                if meta.get("delivered_amount").is_some() && !meta["delivered_amount"].is_null() {
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

            // Track highest ledger index
            if deposit_ledger > 0 {
                new_ledger = new_ledger.max(deposit_ledger as u32);
            }
        }

        Ok((deposits, new_ledger))
    }
}
