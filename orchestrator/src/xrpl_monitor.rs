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
}

/// Monitors XRPL ledger for incoming deposits to an escrow account.
pub struct XrplMonitor {
    client: reqwest::Client,
    rpc_url: String,
    escrow_address: String,
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
        }
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
                format!("{:.8}", xrp)
            } else if let Some(drops_num) = amount.as_u64() {
                // Native XRP: amount in drops as number
                let xrp = drops_num as f64 / 1_000_000.0;
                format!("{:.8}", xrp)
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
                format!("{}.{}", integer, frac_padded)
            };

            // Validate it's a positive amount
            if value.starts_with('-') || value == "0" || value == "0.00000000" {
                continue;
            }

            let sender = match tx["Account"].as_str() {
                Some(s) => s.to_string(),
                None => continue,
            };

            let tx_hash = match tx["hash"].as_str() {
                Some(h) => h.to_lowercase(),
                None => continue,
            };

            info!(
                sender = %sender,
                amount = %fp8_amount,
                tx_hash = &tx_hash[..16.min(tx_hash.len())],
                "deposit detected"
            );

            deposits.push(DepositEvent {
                sender,
                amount: fp8_amount,
                tx_hash: tx_hash[..64.min(tx_hash.len())].to_string(),
            });

            // Track highest ledger index
            if let Some(idx) = tx["ledger_index"].as_u64() {
                new_ledger = new_ledger.max(idx as u32);
            }
        }

        Ok((deposits, new_ledger))
    }
}
