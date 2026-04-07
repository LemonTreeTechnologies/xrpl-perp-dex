//! XRPL withdrawal flow: margin check in enclave + submit Payment to XRPL.
//!
//! MVP: single operator, single enclave signature.
//! Production: 2-of-3 multisig via SignerListSet (see doc 04).
//!
//! Flow:
//!   1. User calls POST /v1/withdraw { user_id, amount, destination }
//!   2. Orchestrator builds XRPL Payment tx hash
//!   3. Enclave checks margin + ECDSA signs hash
//!   4. Orchestrator submits signed tx to XRPL
//!   5. Returns tx hash to user

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use tracing::{error, info};

/// Withdrawal request from user.
#[derive(Debug, Deserialize)]
pub struct WithdrawRequest {
    pub user_id: String,
    pub amount: String,
    pub destination: String, // user's XRPL r-address to receive funds
}

/// Withdrawal result.
#[derive(Debug, Serialize)]
pub struct WithdrawResult {
    pub status: String,
    pub amount: String,
    pub destination: String,
    pub xrpl_tx_hash: Option<String>,
    pub message: String,
}

/// Submit a withdrawal: enclave signs, orchestrator submits to XRPL.
pub async fn process_withdrawal(
    perp: &crate::perp_client::PerpClient,
    xrpl_url: &str,
    escrow_address: &str,
    escrow_account_id: &str,
    session_key: &str,
    req: &WithdrawRequest,
) -> Result<WithdrawResult> {
    // Step 1: Build a mock tx hash for the enclave to sign
    // In production, this would be the SHA-512Half of the serialized XRPL tx
    // For MVP, we use a deterministic hash of the withdrawal parameters
    let tx_data = format!(
        "Payment:{}:{}:{}:{}",
        escrow_address, req.destination, req.amount,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let tx_hash = sha512_half(tx_data.as_bytes());
    let tx_hash_hex = hex::encode(&tx_hash);

    info!(
        user = %req.user_id,
        amount = %req.amount,
        destination = %req.destination,
        "processing withdrawal"
    );

    // Step 2: Call enclave — margin check + sign
    let result = perp
        .withdraw(
            &req.user_id,
            &req.amount,
            escrow_account_id,
            session_key,
            &tx_hash_hex,
        )
        .await;

    match result {
        Ok(resp) => {
            let status = resp["status"].as_str().unwrap_or("unknown");
            if status != "success" {
                let msg = resp["message"]
                    .as_str()
                    .unwrap_or("enclave rejected withdrawal")
                    .to_string();
                return Ok(WithdrawResult {
                    status: "error".into(),
                    amount: req.amount.clone(),
                    destination: req.destination.clone(),
                    xrpl_tx_hash: None,
                    message: msg,
                });
            }

            let signature_hex = resp["signature"]
                .as_str()
                .unwrap_or("")
                .to_string();

            info!(
                user = %req.user_id,
                sig_len = signature_hex.len(),
                "enclave signed withdrawal"
            );

            // Step 3: Submit to XRPL
            // For MVP: build and submit Payment tx
            // In production: collect 2 signatures, build multisig Signers array
            match submit_xrpl_payment(
                xrpl_url,
                escrow_address,
                &req.destination,
                &req.amount,
                &signature_hex,
            )
            .await
            {
                Ok(xrpl_hash) => {
                    info!(
                        user = %req.user_id,
                        xrpl_hash = %xrpl_hash,
                        "withdrawal submitted to XRPL"
                    );
                    Ok(WithdrawResult {
                        status: "success".into(),
                        amount: req.amount.clone(),
                        destination: req.destination.clone(),
                        xrpl_tx_hash: Some(xrpl_hash),
                        message: "withdrawal submitted to XRPL".into(),
                    })
                }
                Err(e) => {
                    error!(user = %req.user_id, "XRPL submission failed: {}", e);
                    Ok(WithdrawResult {
                        status: "signed_but_not_submitted".into(),
                        amount: req.amount.clone(),
                        destination: req.destination.clone(),
                        xrpl_tx_hash: None,
                        message: format!(
                            "Enclave signed but XRPL submission failed: {}. Balance already deducted — retry submission.",
                            e
                        ),
                    })
                }
            }
        }
        Err(e) => Ok(WithdrawResult {
            status: "error".into(),
            amount: req.amount.clone(),
            destination: req.destination.clone(),
            xrpl_tx_hash: None,
            message: format!("Enclave error: {}", e),
        }),
    }
}

/// Submit Payment to XRPL via JSON-RPC.
/// MVP: simplified — uses the `submit` method with pre-signed blob.
/// Production: would use xrpl binary codec for proper serialization.
async fn submit_xrpl_payment(
    xrpl_url: &str,
    escrow_address: &str,
    destination: &str,
    amount: &str,
    _signature_hex: &str,
) -> Result<String> {
    let client = reqwest::Client::new();

    // For MVP: submit a Payment tx JSON via the sign-and-submit flow
    // This is a placeholder — full implementation needs XRPL binary codec
    // to serialize the tx, inject the signature, and submit the blob.
    //
    // The real implementation is in Python (sgx_signer.py + e2e_multisig_withdrawal.py)
    // where we use xrpl-py's binary codec.
    //
    // TODO: integrate xrpl-rs crate or port binary codec to Rust

    let tx_json = serde_json::json!({
        "method": "submit",
        "params": [{
            "tx_json": {
                "TransactionType": "Payment",
                "Account": escrow_address,
                "Destination": destination,
                "Amount": {
                    "currency": "USD",
                    "issuer": escrow_address,
                    "value": amount.trim_end_matches('0').trim_end_matches('.')
                },
                "Fee": "36"
            }
        }]
    });

    let resp: serde_json::Value = client
        .post(xrpl_url)
        .json(&tx_json)
        .send()
        .await
        .context("XRPL RPC request failed")?
        .json()
        .await
        .context("XRPL RPC response parse failed")?;

    let engine_result = resp["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");

    if engine_result == "tesSUCCESS" || engine_result.starts_with("tes") {
        let hash = resp["result"]["tx_json"]["hash"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();
        Ok(hash)
    } else {
        anyhow::bail!(
            "XRPL engine_result: {} — {}",
            engine_result,
            resp["result"]["engine_result_message"]
                .as_str()
                .unwrap_or("")
        )
    }
}

/// SHA-512Half: first 32 bytes of SHA-512 (XRPL's signing hash).
fn sha512_half(data: &[u8]) -> [u8; 32] {
    let full = Sha512::digest(data);
    let mut result = [0u8; 32];
    result.copy_from_slice(&full[..32]);
    result
}
