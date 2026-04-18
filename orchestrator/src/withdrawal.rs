//! XRPL withdrawal flow: margin check in enclave + 2-of-3 multisig submit to XRPL.
//!
//! Uses xrpl-mithril-codec for proper XRPL binary serialization and
//! multi_signing_hash for per-signer multisig hash computation.
//!
//! Flow:
//!   1. User calls POST /v1/withdraw { user_id, amount, destination }
//!   2. Orchestrator asks the local enclave to check margin + deduct balance
//!   3. Orchestrator autofills tx (Sequence, Fee) from XRPL
//!   4. For each signer (up to quorum): compute multi_signing_hash, call
//!      the signer's enclave /v1/pool/sign, collect DER signature
//!   5. Assemble Signers[] array sorted by AccountID
//!   6. Submit via submit_multisigned RPC to XRPL

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use xrpl_mithril_codec::signing;

use crate::p2p::{SigningMessage, SigningRelay};
use crate::xrpl_signer;

// ── Types ─────────────────────────────────────────────────────────

/// Withdrawal request from user.
#[derive(Debug, Deserialize)]
pub struct WithdrawRequest {
    pub user_id: String,
    pub amount: String,
    pub destination: String,
    /// XRPL DestinationTag for the receiving account (required for exchanges)
    #[serde(default)]
    pub destination_tag: Option<u32>,
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

/// One operator's signing credentials (loaded from --signers-config).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignerConfig {
    pub name: String,
    pub enclave_url: String,
    pub address: String,          // 0x... Ethereum-style for /v1/pool/sign
    pub session_key: String,      // 0x... per-account auth token
    pub compressed_pubkey: String, // hex, 33 bytes
    pub xrpl_address: String,     // r-address
}

/// Multi-operator signing configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignersConfig {
    pub signers: Vec<SignerConfig>,
    pub quorum: usize,
    #[serde(default)]
    pub escrow_address: String,
    /// Credentials of the LOCAL enclave (the one this orchestrator talks to).
    /// Used for the margin-check-and-deduct step, which must run on the
    /// enclave that holds the user's deposit state. The signing step uses
    /// each signer's own remote enclave.
    pub local_signer: Option<SignerConfig>,
}

// ── Enclave signing helper ────────────────────────────────────────

/// Ask a remote enclave to ECDSA-sign a 32-byte hash.
/// Returns (DER signature hex uppercase, compressed pubkey hex uppercase).
async fn sign_with_enclave(
    http: &reqwest::Client,
    signer: &SignerConfig,
    hash: &[u8; 32],
) -> Result<(String, String)> {
    let hash_hex = format!("0x{}", hex::encode(hash));
    let resp: serde_json::Value = http
        .post(format!("{}/pool/sign", signer.enclave_url))
        .json(&serde_json::json!({
            "from": signer.address,
            "hash": hash_hex,
            "session_key": signer.session_key,
        }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("sign request to {} failed", signer.name))?
        .json()
        .await
        .with_context(|| format!("sign response from {} is not JSON", signer.name))?;

    if resp["status"].as_str() != Some("success") {
        anyhow::bail!(
            "{} sign failed: {}",
            signer.name,
            resp.get("message").unwrap_or(&resp)
        );
    }

    let r_hex = resp["signature"]["r"]
        .as_str()
        .context("missing r in signature")?;
    let s_hex = resp["signature"]["s"]
        .as_str()
        .context("missing s in signature")?;

    let r_bytes = hex::decode(r_hex).context("invalid r hex")?;
    let s_bytes = hex::decode(s_hex).context("invalid s hex")?;
    let der = xrpl_signer::der_encode_signature(&r_bytes, &s_bytes);
    let der_hex = hex::encode_upper(&der);

    Ok((der_hex, signer.compressed_pubkey.to_uppercase()))
}

/// Collect a signature from a remote signer via P2P relay.
async fn sign_via_p2p(
    signing_tx: &mpsc::Sender<SigningRelay>,
    signer: &SignerConfig,
    hash: &[u8; 32],
    timeout_secs: u64,
) -> Result<(String, String)> {
    let request_id = format!("{:016x}", rand::random::<u64>());
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

    signing_tx
        .send(SigningRelay {
            request_id: request_id.clone(),
            hash_hex: format!("0x{}", hex::encode(hash)),
            signer_xrpl_address: signer.xrpl_address.clone(),
            response_tx: resp_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("P2P signing channel closed"))?;

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        resp_rx,
    )
    .await
    .map_err(|_| anyhow::anyhow!("P2P signing timeout ({}s)", timeout_secs))?
    .map_err(|_| anyhow::anyhow!("P2P signing response dropped"))?;

    match response {
        SigningMessage::Response {
            der_signature: Some(der),
            compressed_pubkey: Some(pubkey),
            error: None,
            ..
        } => Ok((der, pubkey)),
        SigningMessage::Response { error: Some(e), .. } => {
            anyhow::bail!("remote signer error: {}", e)
        }
        _ => anyhow::bail!("unexpected signing response"),
    }
}

// ── Main withdrawal flow ──────────────────────────────────────────

/// Submit a multisig withdrawal: margin check in enclave + 2-of-N signing.
/// If `signing_tx` is provided, uses P2P relay for remote signatures.
/// Otherwise falls back to direct HTTP (legacy, requires SSH tunnels).
pub async fn process_withdrawal(
    perp: &crate::perp_client::PerpClient,
    xrpl_url: &str,
    escrow_address: &str,
    signers_config: &SignersConfig,
    req: &WithdrawRequest,
    signing_tx: Option<&mpsc::Sender<SigningRelay>>,
) -> Result<WithdrawResult> {
    info!(
        user = %req.user_id,
        amount = %req.amount,
        destination = %req.destination,
        "processing multisig withdrawal"
    );

    // Step 1: Margin check in local enclave — deducts balance if sufficient.
    // The local_signer is the credentials of THIS orchestrator's enclave, which
    // holds the user's deposit state. We pass a dummy hash because the actual
    // multisig signing is done per-signer below via remote enclave calls.
    let local = signers_config
        .local_signer
        .as_ref()
        .or_else(|| signers_config.signers.first())
        .context("no signers configured (need local_signer or at least one signer)")?;
    let dummy_hash = "0".repeat(64);
    let local_session_key = local.session_key.trim_start_matches("0x");
    let margin_result = perp
        .withdraw(
            &req.user_id,
            &req.amount,
            &local.address,
            local_session_key,
            &dummy_hash,
        )
        .await;

    match &margin_result {
        Ok(resp) if resp["status"].as_str() == Some("success") => {
            info!(user = %req.user_id, "margin check passed, balance deducted");
        }
        Ok(resp) => {
            let msg = resp["message"]
                .as_str()
                .unwrap_or("margin check failed")
                .to_string();
            return Ok(WithdrawResult {
                status: "error".into(),
                amount: req.amount.clone(),
                destination: req.destination.clone(),
                xrpl_tx_hash: None,
                message: msg,
            });
        }
        Err(e) => {
            return Ok(WithdrawResult {
                status: "error".into(),
                amount: req.amount.clone(),
                destination: req.destination.clone(),
                xrpl_tx_hash: None,
                message: format!("Enclave error: {}", e),
            });
        }
    }

    // Step 2: Autofill — get account sequence from XRPL
    let sequence = fetch_account_sequence(xrpl_url, escrow_address)
        .await
        .unwrap_or(1);

    // Step 3: Build unsigned Payment tx (SigningPubKey="" signals multisig)
    // Fee for multisig = base_fee * (1 + N_signers). Use generous fee.
    let fee = format!("{}", 12 * (1 + signers_config.quorum as u64));
    let mut tx_json = serde_json::json!({
        "TransactionType": "Payment",
        "Account": escrow_address,
        "Destination": req.destination,
        "Amount": format!("{}", (req.amount.parse::<f64>().unwrap_or(0.0) * 1_000_000.0) as u64),
        "Fee": fee,
        "Sequence": sequence,
        "SigningPubKey": ""
    });
    if let Some(tag) = req.destination_tag {
        tx_json["DestinationTag"] = serde_json::json!(tag);
    }
    let tx_map = tx_json.as_object().context("tx_json is not an object")?;

    info!(
        sequence,
        fee = %fee,
        signers = signers_config.signers.len(),
        quorum = signers_config.quorum,
        "built unsigned multisig Payment tx"
    );

    // Step 4: Collect signatures from quorum signers
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("failed to build HTTP client")?;

    let mut collected_signers: Vec<serde_json::Value> = Vec::new();

    for signer in &signers_config.signers {
        if collected_signers.len() >= signers_config.quorum {
            break;
        }
        let account_id = match xrpl_signer::decode_xrpl_address(&signer.xrpl_address) {
            Ok(id) => id,
            Err(e) => {
                warn!(signer = %signer.name, "failed to decode address: {}", e);
                continue;
            }
        };

        let hash = signing::multi_signing_hash(tx_map, &account_id)
            .map_err(|e| anyhow::anyhow!("multi_signing_hash for {} failed: {:?}", signer.name, e))?;

        // Use P2P relay if available, otherwise fall back to direct HTTP
        let sign_result = if let Some(stx) = signing_tx {
            sign_via_p2p(stx, signer, &hash, 30).await
        } else {
            sign_with_enclave(&http, signer, &hash).await
        };

        match sign_result {
            Ok((der_hex, pubkey_hex)) => {
                info!(
                    signer = %signer.name,
                    xrpl_addr = %signer.xrpl_address,
                    der_len = der_hex.len() / 2,
                    via = if signing_tx.is_some() { "p2p" } else { "http" },
                    "collected multisig signature"
                );
                collected_signers.push(serde_json::json!({
                    "Signer": {
                        "Account": signer.xrpl_address,
                        "SigningPubKey": pubkey_hex,
                        "TxnSignature": der_hex,
                    }
                }));
            }
            Err(e) => {
                warn!(signer = %signer.name, "signing failed: {}", e);
            }
        }
    }

    if collected_signers.len() < signers_config.quorum {
        error!(
            collected = collected_signers.len(),
            quorum = signers_config.quorum,
            "insufficient signatures for multisig withdrawal"
        );
        return Ok(WithdrawResult {
            status: "error".into(),
            amount: req.amount.clone(),
            destination: req.destination.clone(),
            xrpl_tx_hash: None,
            message: format!(
                "only {} of {} required signatures collected",
                collected_signers.len(),
                signers_config.quorum
            ),
        });
    }

    // Step 5: Sort Signers by AccountID (ascending bytes — XRPL canonical order)
    collected_signers.sort_by(|a, b| {
        let addr_a = a["Signer"]["Account"].as_str().unwrap_or("");
        let addr_b = b["Signer"]["Account"].as_str().unwrap_or("");
        let id_a = xrpl_signer::decode_xrpl_address(addr_a).unwrap_or([0xff; 20]);
        let id_b = xrpl_signer::decode_xrpl_address(addr_b).unwrap_or([0xff; 20]);
        id_a.cmp(&id_b)
    });

    // Step 6: Submit via submit_multisigned RPC
    let mut full_tx = tx_json.clone();
    full_tx["Signers"] = serde_json::Value::Array(collected_signers);

    match submit_multisigned(xrpl_url, &full_tx).await {
        Ok(xrpl_hash) => {
            info!(
                user = %req.user_id,
                xrpl_hash = %xrpl_hash,
                "multisig withdrawal submitted to XRPL"
            );
            Ok(WithdrawResult {
                status: "success".into(),
                amount: req.amount.clone(),
                destination: req.destination.clone(),
                xrpl_tx_hash: Some(xrpl_hash),
                message: "multisig withdrawal submitted to XRPL".into(),
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
                    "Signatures collected but XRPL submission failed: {}. Balance already deducted.",
                    e
                ),
            })
        }
    }
}

// ── XRPL RPC helpers ──────────────────────────────────────────────

/// Fetch account Sequence number from XRPL.
async fn fetch_account_sequence(xrpl_url: &str, account: &str) -> Result<u32> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "account_info",
            "params": [{"account": account}]
        }))
        .send()
        .await?
        .json()
        .await?;
    let seq = resp["result"]["account_data"]["Sequence"]
        .as_u64()
        .context("missing Sequence in account_info")?;
    Ok(seq as u32)
}

/// Submit a multisigned transaction via submit_multisigned RPC.
async fn submit_multisigned(xrpl_url: &str, tx_json: &serde_json::Value) -> Result<String> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "submit_multisigned",
            "params": [{"tx_json": tx_json}]
        }))
        .send()
        .await
        .context("XRPL submit_multisigned request failed")?
        .json()
        .await
        .context("XRPL submit_multisigned response parse failed")?;

    let engine_result = resp["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");

    if engine_result == "tesSUCCESS" || engine_result.starts_with("tes") {
        let hash = resp["result"]["tx_json"]["hash"]
            .as_str()
            .or_else(|| resp["result"]["hash"].as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(hash)
    } else {
        anyhow::bail!(
            "XRPL: {} — {}",
            engine_result,
            resp["result"]["engine_result_message"]
                .as_str()
                .unwrap_or("")
        )
    }
}
