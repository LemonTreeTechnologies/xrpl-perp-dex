//! Leadership rule for the follower reserve (2026-11-10): only a node whose enclave holds
//! EVERY withdrawal that has landed may become the sequencer, so the leader's balances always
//! include every withdrawal already paid out.
//!
//! "Caught up" is judged from two facts neither of which is the host's word: the enclave's
//! own withdrawal high-water (the highest escrow Sequence it ever reserved) and the CHAIN —
//! the highest Sequence among the escrow's validated, successful outgoing Payments. Every
//! withdrawal is an outgoing escrow Payment, and every enclave that signed or was later
//! delivered one reserved it, so high_water >= that Sequence means nothing landed that this
//! enclave does not hold.

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};

/// Pure: may this node lead? `None` = the escrow has never sent a successful Payment.
pub fn caught_up(enclave_high_water: u32, max_landed_withdrawal_seq: Option<u32>) -> bool {
    match max_landed_withdrawal_seq {
        None => true,
        Some(s) => enclave_high_water >= s,
    }
}

/// Pure: the highest Sequence among validated, tesSUCCESS Payments SENT BY `escrow` in one
/// `account_tx` result page. Incoming Payments (deposits) and governance transactions
/// (SignerListSet, AccountSet) do not count: they are not withdrawals.
pub fn max_outgoing_payment_seq(account_tx_result: &Value, escrow: &str) -> Option<u32> {
    let txs = account_tx_result["transactions"].as_array()?;
    txs.iter()
        .filter(|t| t["validated"].as_bool() == Some(true))
        .filter_map(|t| {
            let tx = if t["tx"].is_object() {
                &t["tx"]
            } else {
                &t["tx_json"]
            };
            let ok = t["meta"]["TransactionResult"].as_str() == Some("tesSUCCESS");
            (ok && tx["TransactionType"].as_str() == Some("Payment")
                && tx["Account"].as_str() == Some(escrow))
            .then(|| tx["Sequence"].as_u64())
            .flatten()
            .map(|s| s as u32)
        })
        .max()
}

/// Walk the escrow's history newest-first until an outgoing Payment is found (the newest one
/// is the maximum) or the history ends. Bounded: a page cap keeps one check cheap; reaching it
/// without an answer is an ERROR (the caller keeps its previous verdict), never "none landed".
async fn fetch_max_landed_withdrawal_seq(
    http: &reqwest::Client,
    url: &str,
    escrow: &str,
) -> Result<Option<u32>> {
    const MAX_PAGES: usize = 20;
    let mut marker: Option<Value> = None;
    for _ in 0..MAX_PAGES {
        let mut params = serde_json::json!({
            "account": escrow,
            "ledger_index_min": -1,
            "ledger_index_max": -1,
            "binary": false,
            "forward": false,
            "limit": 200
        });
        if let Some(m) = &marker {
            params["marker"] = m.clone();
        }
        let r: Value = http
            .post(url)
            .json(&serde_json::json!({ "method": "account_tx", "params": [params] }))
            .send()
            .await
            .context("account_tx request failed")?
            .json()
            .await
            .context("account_tx response parse failed")?;
        let result = &r["result"];
        if result["status"].as_str() == Some("error") || result["error"].is_string() {
            anyhow::bail!("account_tx error: {}", result["error"]);
        }
        if let Some(s) = max_outgoing_payment_seq(result, escrow) {
            return Ok(Some(s)); // newest-first: the first page with one holds the maximum
        }
        match result.get("marker") {
            Some(m) if !m.is_null() => marker = Some(m.clone()),
            _ => return Ok(None),
        }
    }
    anyhow::bail!("account_tx: no outgoing Payment within {MAX_PAGES} pages — verdict withheld")
}

/// Background check feeding the election gate. Starts NOT caught up (fail-closed) unless the
/// enclave predates the pending listing (then the rule cannot be judged and the node behaves
/// as before — the gate is lifted, and logged once). On any read error the previous verdict
/// stands.
pub async fn run_caught_up_check(
    perp: crate::perp_client::PerpClient,
    xrpl_http_url: String,
    escrow: String,
    tx: watch::Sender<bool>,
    interval_secs: u64,
) {
    let http = reqwest::Client::new();
    let period = std::time::Duration::from_secs(interval_secs.max(1));
    let mut legacy_logged = false;
    loop {
        match perp.list_pending_withdrawals().await {
            Ok(None) => {
                if !legacy_logged {
                    warn!("enclave has no pending-withdrawal listing (pre-follower-reserve) — failover caught-up rule not enforceable, gate lifted");
                    legacy_logged = true;
                }
                let _ = tx.send(true);
            }
            Ok(Some(p)) => {
                match fetch_max_landed_withdrawal_seq(&http, &xrpl_http_url, &escrow).await {
                    Ok(max_seq) => {
                        let ok = caught_up(p.high_water, max_seq);
                        if *tx.borrow() != ok {
                            info!(
                                high_water = p.high_water,
                                ?max_seq,
                                caught_up = ok,
                                "withdrawal catch-up verdict changed"
                            );
                        }
                        let _ = tx.send(ok);
                    }
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "catch-up check: chain read failed — verdict unchanged")
                    }
                }
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "catch-up check: enclave listing failed — verdict unchanged")
            }
        }
        tokio::time::sleep(period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ESC: &str = "rEscrow";

    fn tx(kind: &str, account: &str, seq: u64, result: &str, validated: bool) -> Value {
        json!({ "validated": validated, "meta": { "TransactionResult": result },
                "tx": { "TransactionType": kind, "Account": account, "Sequence": seq } })
    }

    #[test]
    fn caught_up_needs_every_landed_withdrawal() {
        assert!(caught_up(0, None), "nothing ever landed");
        assert!(caught_up(100, Some(100)));
        assert!(caught_up(101, Some(100)));
        assert!(
            !caught_up(99, Some(100)),
            "missed the withdrawal at 100 — must not lead"
        );
        assert!(
            !caught_up(0, Some(19492293)),
            "the pre-reserve legacy withdrawal counts"
        );
    }

    #[test]
    fn only_successful_validated_outgoing_payments_count() {
        let r = json!({ "transactions": [
            tx("Payment", "rUser", 900, "tesSUCCESS", true),          // a deposit (incoming)
            tx("SignerListSet", ESC, 950, "tesSUCCESS", true),        // governance
            tx("Payment", ESC, 940, "tecUNFUNDED_PAYMENT", true),      // failed
            tx("Payment", ESC, 960, "tesSUCCESS", false),              // not validated
            tx("Payment", ESC, 930, "tesSUCCESS", true),               // the one
            tx("Payment", ESC, 920, "tesSUCCESS", true),
        ]});
        assert_eq!(max_outgoing_payment_seq(&r, ESC), Some(930));
        let none = json!({ "transactions": [ tx("Payment", "rUser", 900, "tesSUCCESS", true) ]});
        assert_eq!(max_outgoing_payment_seq(&none, ESC), None);
    }

    #[test]
    fn api_v2_tx_json_shape_is_read() {
        let r = json!({ "transactions": [
            { "validated": true, "meta": { "TransactionResult": "tesSUCCESS" },
              "tx_json": { "TransactionType": "Payment", "Account": ESC, "Sequence": 77 } } ]});
        assert_eq!(max_outgoing_payment_seq(&r, ESC), Some(77));
    }
}
