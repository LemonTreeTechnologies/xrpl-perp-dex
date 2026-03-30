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
    pub async fn scan_deposits(
        &self,
        last_ledger: u32,
    ) -> Result<(Vec<DepositEvent>, u32)> {
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
                result["error_message"]
                    .as_str()
                    .unwrap_or("unknown")
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

            // Extract amount — RLUSD is an issued currency (object with "value")
            let amount = if meta.get("delivered_amount").is_some()
                && !meta["delivered_amount"].is_null()
            {
                &meta["delivered_amount"]
            } else {
                &tx["Amount"]
            };

            // We only handle issued currency (object), skip XRP-only payments
            let value = match amount.as_object() {
                Some(obj) => match obj.get("value").and_then(|v| v.as_str()) {
                    Some(v) => v.to_string(),
                    None => continue,
                },
                None => continue,
            };

            let value_f64: f64 = match value.parse() {
                Ok(v) if v > 0.0 => v,
                _ => continue,
            };

            let sender = match tx["Account"].as_str() {
                Some(s) => s.to_string(),
                None => continue,
            };

            let tx_hash = match tx["hash"].as_str() {
                Some(h) => h.to_lowercase(),
                None => continue,
            };

            // Convert to FP8 string
            let fp8_amount = format!("{:.8}", value_f64);

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
