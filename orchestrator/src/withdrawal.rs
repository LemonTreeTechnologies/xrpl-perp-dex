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
    /// The account's own authorisation of exactly this withdrawal
    /// (`docs/withdrawal-authorization-v1.md`): user_pubkey, user_sig, auth_nonce, max_ledger.
    /// Every enclave that signs the Payment verifies it; without it none will.
    #[serde(flatten, default)]
    pub auth: Option<crate::withdraw_auth::WithdrawAuth>,
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
    pub address: String,           // 0x... Ethereum-style for /v1/pool/sign
    pub session_key: String,       // 0x... per-account auth token
    pub compressed_pubkey: String, // hex, 33 bytes
    pub xrpl_address: String,      // r-address
    /// 33-byte ECDH identity pubkey from `node-bootstrap`. Optional for
    /// backward compatibility with pre-Phase-2.1c entry files. Used by
    /// `dkg_coordinate.rs` to look up peer ECDH pubkeys for DKG share
    /// envelope encryption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecdh_pubkey: Option<String>,
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
/// β4 Thread A (AC-β4-A2): sends the for-signing BLOB, never a digest — the
/// enclave re-derives the hash itself and refuses anything that is not a
/// `Payment`. (The bare hash-signing oracle refuses the escrow-role key
/// outright, so a digest here would simply be rejected.)
async fn sign_with_enclave(
    http: &reqwest::Client,
    signer: &SignerConfig,
    tx_map: &serde_json::Map<String, serde_json::Value>,
    user_id: &str,
    amount_fp8: u64,
    auth: &crate::withdraw_auth::WithdrawAuth,
) -> Result<(String, String)> {
    let mut blob = Vec::new();
    xrpl_mithril_codec::serializer::serialize_json_object(tx_map, &mut blob, true)
        .map_err(|e| anyhow::anyhow!("serialise for signing failed: {e:?}"))?;

    let resp: serde_json::Value = http
        .post(format!(
            "{}/pool/sign/withdrawal-payment",
            signer.enclave_url
        ))
        .json(&serde_json::json!({
            "from": signer.address,
            "session_key": signer.session_key,
            // The co-signer checks BOTH against its own sealed state before signing.
            // Without them it would be signing a payment it cannot dispute, which is
            // exactly what this path used to do (audit High/Critical, 2026-09-25).
            "user_id": user_id,
            "amount_fp8": amount_fp8.to_string(),
            "tx_blob": hex::encode(&blob),
            "user_pubkey": auth.user_pubkey,
            "user_sig": auth.user_sig,
            "auth_nonce": auth.auth_nonce,
            "max_ledger": auth.max_ledger,
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
///
/// X-C1: we publish the full unsigned tx plus the signer's account_id
/// so receivers can re-derive the multi_signing_hash themselves. The
/// caller-computed hash is no longer sent on the wire — it would be
/// trusted blindly by the receiver enclave and let any gossipsub peer
/// demand signatures on arbitrary hashes.
#[allow(clippy::too_many_arguments)]
async fn sign_via_p2p(
    signing_tx: &mpsc::Sender<SigningRelay>,
    signer: &SignerConfig,
    unsigned_tx: &serde_json::Value,
    account_id: &[u8; 20],
    timeout_secs: u64,
    user_id: &str,
    amount_fp8: u64,
    auth: &crate::withdraw_auth::WithdrawAuth,
) -> Result<(String, String)> {
    let request_id = format!("{:016x}", rand::random::<u64>());
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

    signing_tx
        .send(SigningRelay {
            request_id: request_id.clone(),
            unsigned_tx: unsigned_tx.clone(),
            signer_account_id_hex: hex::encode(account_id),
            signer_xrpl_address: signer.xrpl_address.clone(),
            response_tx: resp_tx,
            // Value path: a Payment needs no β1 bundle (β4 Thread A AC-β4-A2).
            quorum_bundle: None,
            // …but it DOES need the withdrawal claim: the peer's enclave will not sign
            // a Payment without being told whose balance it spends and how much, because
            // it checks both against its own state first.
            withdrawal_user_id: Some(user_id.to_string()),
            withdrawal_amount_fp8: Some(amount_fp8.to_string()),
            // …and the user's own authorisation, verbatim: each peer's enclave verifies it
            // against the Payment and will not sign without it.
            withdrawal_auth: Some(auth.clone()),
        })
        .await
        .map_err(|_| anyhow::anyhow!("P2P signing channel closed"))?;

    let response = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), resp_rx)
        .await
        .map_err(|_| anyhow::anyhow!("P2P signing timeout ({timeout_secs}s)"))?
        .map_err(|_| anyhow::anyhow!("P2P signing response dropped"))?;

    match response {
        SigningMessage::Response {
            der_signature: Some(der),
            compressed_pubkey: Some(pubkey),
            error: None,
            ..
        } => Ok((der, pubkey)),
        SigningMessage::Response { error: Some(e), .. } => {
            anyhow::bail!("remote signer error: {e}")
        }
        _ => anyhow::bail!("unexpected signing response"),
    }
}

// ── Main withdrawal flow ──────────────────────────────────────────

/// Submit a multisig withdrawal: margin check in enclave + 2-of-N signing.
/// If `signing_tx` is provided, uses P2P relay for remote signatures.
/// Otherwise falls back to direct HTTP (legacy, requires SSH tunnels).
/// GAP B: how many ledgers past the current validated ledger a withdrawal Payment stays valid
/// (LastLedgerSequence = validated + this). Bounds the reconcile window — the enclave's R-b
/// (expired-unconsumed) rollback can only fire once a validated ledger passes LastLedgerSequence;
/// ~40 ledgers ≈ 160s on XRPL, generous for multi-cosign collection + submit. Matches the
/// AccountSet path's LAST_LEDGER_WINDOW.
const WITHDRAWAL_LAST_LEDGER_WINDOW: u64 = 40;

pub async fn process_withdrawal(
    perp: &crate::perp_client::PerpClient,
    xrpl_url: &str,
    escrow_address: &str,
    signers_config: &SignersConfig,
    req: &WithdrawRequest,
    signing_tx: Option<&mpsc::Sender<SigningRelay>>,
    journal: Option<&crate::withdrawal_reconcile::JournalHandle>,
) -> Result<WithdrawResult> {
    info!(
        user = %req.user_id,
        amount = %req.amount,
        destination = %req.destination,
        "processing multisig withdrawal"
    );

    // Step 1: autofill + build the unsigned Payment, BEFORE the margin check.
    //
    // β4 Thread A (AC-β4-A2): the margin-check ecall signs the transaction
    // BLOB, so the tx must exist first. This also restores the atomicity the
    // ecall was designed for: it used to be handed a DUMMY all-zero hash (the
    // real signatures being collected separately below), which both subverted
    // "check and sign" and is now impossible — the escrow-role key refuses to
    // sign a bare hash at all. Nothing here depends on the margin result: the
    // sequence is an XRPL read and the rest comes from the request/config.
    let sequence = fetch_account_sequence(xrpl_url, escrow_address)
        .await
        .unwrap_or(1);

    // GAP B: bind the Payment to a ledger window (LastLedgerSequence) so a reserved-but-unconfirmed
    // withdrawal becomes DECIDABLY dead once a validated ledger passes it — the precondition for the
    // enclave's R-b (expired-unconsumed) rollback. Refuse the withdrawal if the current ledger is
    // unreadable: a wrong (or absent) LLS is worse than not withdrawing — it would make the Payment
    // either never-expiring (unreconcilable) or expire prematurely. Mirrors the AccountSet path.
    let validated_ledger = crate::unl_policy::own_validated_ledger(xrpl_url)
        .await
        .context("reading the validated ledger for the withdrawal LastLedgerSequence")?;
    let last_ledger = validated_ledger
        .checked_add(WITHDRAWAL_LAST_LEDGER_WINDOW)
        .context("withdrawal LastLedgerSequence overflow")?;
    let last_ledger_u32 = u32::try_from(last_ledger)
        .context("withdrawal LastLedgerSequence exceeds the XRPL 32-bit field")?;

    // The user's authorisation bounds the Payment: its LastLedgerSequence may not exceed the
    // user's max_ledger (each enclave refuses otherwise), and the amount is the exact drops the
    // user signed. Refuse here, before anything is reserved, if it cannot be honoured.
    let auth = req.auth.as_ref().context(
        "withdrawal authorisation missing (user_pubkey, user_sig, auth_nonce, max_ledger)",
    )?;
    auth.validate_shape()?;
    let last_ledger_u32 = last_ledger_u32.min(auth.max_ledger);
    if u64::from(last_ledger_u32) < validated_ledger.saturating_add(2) {
        anyhow::bail!(
            "the withdrawal authorisation expires at ledger {} — too close to the current {} to land; sign a new one",
            auth.max_ledger,
            validated_ledger
        );
    }

    // Fee for multisig = base_fee * (1 + N_signers). Use generous fee.
    let fee = format!("{}", 12 * (1 + signers_config.quorum as u64));
    // Derive the drops ONCE and use the same number for the blob and for the co-sign
    // claim. Each peer's enclave now re-derives FP8 from the blob's Amount and refuses
    // if it does not equal the claimed amount, so computing the two figures by separate
    // routes would turn a rounding difference into a refused withdrawal. One number, one
    // derivation — which is the same reason the claim travels as a raw integer.
    let amount_drops = crate::withdraw_auth::xrp_to_drops(&req.amount)?;
    let amount_fp8 = amount_drops
        .checked_mul(100)
        .context("withdrawal amount overflows fp8")?;
    let mut tx_json = serde_json::json!({
        "TransactionType": "Payment",
        "Account": escrow_address,
        "Destination": req.destination,
        "Amount": format!("{}", amount_drops),
        "Fee": fee,
        "Sequence": sequence,
        "LastLedgerSequence": last_ledger_u32,
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

    // Step 2: Margin check in local enclave — deducts balance if sufficient.
    // The local_signer is the credentials of THIS orchestrator's enclave, which
    // holds the user's deposit state. The signature it returns is this node's
    // own cosignature over the Payment; the quorum signatures are collected
    // per-signer below, so it is not consumed here.
    let local = signers_config
        .local_signer
        .as_ref()
        .or_else(|| signers_config.signers.first())
        .context("no signers configured (need local_signer or at least one signer)")?;
    let mut margin_blob = Vec::new();
    xrpl_mithril_codec::serializer::serialize_json_object(tx_map, &mut margin_blob, true)
        .map_err(|e| anyhow::anyhow!("serialise for signing failed: {e:?}"))?;
    let local_session_key = local.session_key.trim_start_matches("0x");
    let margin_result = perp
        .withdraw(
            &req.user_id,
            &req.amount,
            &local.address,
            local_session_key,
            &hex::encode(&margin_blob),
            auth,
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
                message: format!("Enclave error: {e}"),
            });
        }
    }

    // GAP B: the enclave has now RESERVED this withdrawal — the margin is debited into a
    // sealed PendingWithdrawal row and the per-escrow high-water has advanced over this
    // Sequence. Record it in the durable journal BEFORE we attempt to submit, so that an
    // insufficient-signatures return, a failed submission, or a crash all still leave the
    // reconcile loop able to settle (finalize) or unwind (R-b expired) the reserved row. The
    // submitted hash is only a lookup hint and is filled in after submission.
    let mk_journal_entry =
        |submitted_hash: Option<String>| crate::withdrawal_reconcile::PendingWithdrawal {
            escrow: escrow_address.to_string(),
            sequence,
            last_ledger_sequence: last_ledger_u32,
            amount_drops,
            destination: req.destination.clone(),
            destination_tag: req.destination_tag,
            submitted_hash,
            user_id: req.user_id.clone(),
            reserved_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            undelivered: Vec::new(),
            tx_json: Some(tx_json.clone()),
            amount_fp8,
            settled: false,
            auth: Some(auth.clone()),
        };
    if let Some(j) = journal {
        j.record(mk_journal_entry(None));
    }

    // Step 3: Collect signatures from quorum signers.
    //
    // O-L4 known gap: each `signer.enclave_url` points at a cross-VM
    // peer enclave that currently serves a self-signed cert, so we
    // must still accept-invalid-certs here. Replacing this with
    // default TLS verification is gated on enclave-side E-M2
    // (CA-signed certs or pinned-pubkey verification). Until then
    // this is the only non-loopback client in the orchestrator that
    // trusts the wire via the broken SGX cert chain rather than TLS.
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("failed to build HTTP client")?;

    let mut collected_signers: Vec<serde_json::Value> = Vec::new();
    // Follower reserve: every signer that does NOT sign now (not asked once the quorum is in,
    // refused, unreachable, or still owed an EARLIER withdrawal) is owed this request; the
    // delivery loop sends it, per signer strictly in Sequence order, so every enclave reserves
    // every withdrawal and their balances converge.
    let mut undelivered: Vec<String> = Vec::new();
    let earlier = journal.map(|j| j.snapshot()).unwrap_or_default();
    // THIS node's enclave already reserved the withdrawal when it took the request (the
    // perp.withdraw call above), so it is never owed a delivery. Recording it as owed made
    // the entry node re-deliver to its own enclave forever: the self-delivery can only be
    // refused (the row exists or is settled), and strict per-signer order then wedged every
    // later request owed to it.
    let is_local = |s: &SignerConfig| is_local_signer(signers_config.local_signer.as_ref(), s);

    for signer in &signers_config.signers {
        if collected_signers.len() >= signers_config.quorum {
            if !is_local(signer) {
                undelivered.push(signer.name.clone());
            }
            continue;
        }
        if crate::withdrawal_reconcile::signer_owed_earlier(&earlier, &signer.name, sequence) {
            info!(signer = %signer.name, sequence,
                "signer still owed an earlier withdrawal — queued behind it (strict order)");
            if !is_local(signer) {
                undelivered.push(signer.name.clone());
            }
            continue;
        }
        let account_id = match xrpl_signer::decode_xrpl_address(&signer.xrpl_address) {
            Ok(id) => id,
            Err(e) => {
                warn!(signer = %signer.name, "failed to decode address: {:#}", e);
                continue;
            }
        };

        // Use P2P relay if available, otherwise fall back to direct HTTP.
        // Both paths now send the full tx / its for-signing serialization and
        // let the ENCLAVE derive the hash (β4 Thread A AC-β4-A2) — neither ever
        // puts a digest on the wire.
        let sign_result = if let Some(stx) = signing_tx {
            sign_via_p2p(
                stx,
                signer,
                &tx_json,
                &account_id,
                30,
                &req.user_id,
                amount_fp8,
                auth,
            )
            .await
        } else {
            sign_with_enclave(&http, signer, tx_map, &req.user_id, amount_fp8, auth).await
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
                warn!(signer = %signer.name, "signing failed: {:#}", e);
                if !is_local(signer) {
                    undelivered.push(signer.name.clone());
                }
            }
        }
    }
    if let Some(j) = journal {
        let mut e = mk_journal_entry(None);
        e.undelivered = undelivered.clone();
        j.record(e);
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
            // Refresh the journal entry with the submitted hash (a lookup hint for the
            // reconcile loop; the reconcile key stays the Sequence slot + enclave content).
            if let Some(j) = journal {
                let mut e = mk_journal_entry(Some(xrpl_hash.clone()));
                e.undelivered = undelivered.clone();
                j.record(e);
            }
            Ok(WithdrawResult {
                status: "success".into(),
                amount: req.amount.clone(),
                destination: req.destination.clone(),
                xrpl_tx_hash: Some(xrpl_hash),
                message: "multisig withdrawal submitted to XRPL".into(),
            })
        }
        Err(e) => {
            error!(user = %req.user_id, "XRPL submission failed: {:#}", e);
            Ok(WithdrawResult {
                status: "signed_but_not_submitted".into(),
                amount: req.amount.clone(),
                destination: req.destination.clone(),
                xrpl_tx_hash: None,
                message: format!(
                    "Signatures collected but XRPL submission failed: {e}. Balance already deducted."
                ),
            })
        }
    }
}

// ── Follower reserve: delivery to every signer ─────────────────────

/// Is `s` THIS node's own signer? Matched on the XRPL address — the identity the enclave signs
/// as — not on the display name, so a renamed entry cannot slip through or be mistaken.
pub fn is_local_signer(local: Option<&SignerConfig>, s: &SignerConfig) -> bool {
    local.is_some_and(|l| l.xrpl_address == s.xrpl_address)
}

/// A co-signer's refusal that means "this Sequence is already handled here" (its enclave's -59,
/// surfaced by the co-sign route as `rc=-59`). Narrow on purpose: no other refusal marks a
/// delivery done.
pub fn is_already_handled_refusal(msg: &str) -> bool {
    // Parse the whole number after each "rc=" — a substring test would also accept rc=-590.
    msg.match_indices("rc=").any(|(i, _)| {
        let tail = &msg[i + 3..];
        let end = tail
            .char_indices()
            .find(|&(k, c)| !(c.is_ascii_digit() || (k == 0 && c == '-')))
            .map(|(k, _)| k)
            .unwrap_or(tail.len());
        tail[..end].parse::<i64>() == Ok(-59)
    })
}

/// One delivery pass: for each signer, send the LOWEST-Sequence request it is still owed, and
/// only that one — strict per-signer order, so its enclave never sees S2 before S1 (its
/// monotonic high-water would refuse S1 forever). A failure stops that signer for this pass;
/// the next pass retries the same request. A signer that answers holds the row: its enclave
/// reserved it (or recognised it as the same payment) and its own reconcile settles it by
/// proof. The signature itself is discarded — the Payment was submitted with the quorum.
pub async fn deliver_owed_requests(
    signers_config: &SignersConfig,
    signing_tx: &mpsc::Sender<SigningRelay>,
    journal: &crate::withdrawal_reconcile::JournalHandle,
) {
    let entries = journal.snapshot();
    for signer in &signers_config.signers {
        let Some(e) = crate::withdrawal_reconcile::next_owed(&entries, &signer.name) else {
            continue;
        };
        let Some(auth) = e.auth.as_ref() else {
            warn!(signer = %signer.name, sequence = e.sequence,
                "owed request has no stored user authorisation — cannot deliver");
            continue;
        };
        let Some(tx_json) = e.tx_json.as_ref() else {
            warn!(signer = %signer.name, sequence = e.sequence,
                "owed request has no stored tx_json — cannot deliver (operator: re-inject it)");
            continue;
        };
        let account_id = match xrpl_signer::decode_xrpl_address(&signer.xrpl_address) {
            Ok(id) => id,
            Err(err) => {
                warn!(signer = %signer.name, "failed to decode address: {:#}", err);
                continue;
            }
        };
        match sign_via_p2p(
            signing_tx,
            signer,
            tx_json,
            &account_id,
            30,
            &e.user_id,
            e.amount_fp8,
            auth,
        )
        .await
        {
            Ok(_) => {
                info!(signer = %signer.name, sequence = e.sequence,
                    "owed withdrawal delivered — the signer's enclave now holds the row");
                journal.delivered(&e.escrow, e.sequence, &signer.name);
            }
            Err(err) => {
                let msg = format!("{err:#}");
                if is_already_handled_refusal(&msg) {
                    // The signer's enclave holds this Sequence at or below its withdrawal
                    // high-water: it already co-signed it (reserve = co-sign) or rolled it back.
                    // Re-sending can only be refused again, so the delivery is done. This is
                    // delivery BOOKKEEPING only — no signature is recorded or counted from it.
                    info!(signer = %signer.name, sequence = e.sequence,
                        "owed withdrawal already handled by the signer (rc -59) — marking delivered");
                    journal.delivered(&e.escrow, e.sequence, &signer.name);
                } else {
                    warn!(signer = %signer.name, sequence = e.sequence,
                        "owed withdrawal not delivered yet (retry next pass): {msg}");
                }
            }
        }
    }
}

/// The delivery loop. Runs on the node that holds the journal (the one that took the
/// withdrawals); a no-op when nothing is owed.
pub async fn run_delivery_loop(
    signers_config: SignersConfig,
    signing_tx: mpsc::Sender<SigningRelay>,
    journal: crate::withdrawal_reconcile::JournalHandle,
    interval_secs: u64,
) {
    let period = std::time::Duration::from_secs(interval_secs.max(1));
    loop {
        deliver_owed_requests(&signers_config, &signing_tx, &journal).await;
        tokio::time::sleep(period).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_node_is_never_owed_its_own_delivery() {
        let mk = |name: &str, addr: &str| {
            let mut c: SignerConfig = serde_json::from_value(serde_json::json!({
                "name": name, "enclave_url": "https://127.0.0.1:9089/v1",
                "address": "0x0000000000000000000000000000000000000000",
                "session_key": "00", "compressed_pubkey": "02", "xrpl_address": addr
            }))
            .expect("signer config");
            c.name = name.into();
            c
        };
        let n1 = mk("node-1", "rJScWJ3vfoV2BZTHYxKPdFUkFUq3nyQVx5");
        let n3 = mk("node-3", "rGKHyMGiLocalAddressxxxxxxxxxxxxxx");
        assert!(
            is_local_signer(Some(&n3), &n3),
            "the local signer is recognised"
        );
        assert!(
            !is_local_signer(Some(&n3), &n1),
            "another signer is not local"
        );
        assert!(
            !is_local_signer(None, &n3),
            "no local signer configured: nobody is local"
        );
        // identity is the XRPL address, not the display name
        let renamed = mk("node-3-renamed", "rGKHyMGiLocalAddressxxxxxxxxxxxxxx");
        assert!(
            is_local_signer(Some(&n3), &renamed),
            "a renamed entry with the same address is still local"
        );
        let same_name_other_addr = mk("node-3", "rJScWJ3vfoV2BZTHYxKPdFUkFUq3nyQVx5");
        assert!(
            !is_local_signer(Some(&n3), &same_name_other_addr),
            "a name collision does not make it local"
        );
    }

    #[test]
    fn only_the_exact_already_handled_code_marks_a_delivery_done() {
        assert!(is_already_handled_refusal(
            "enclave HTTP 409: Co-sign refused: this Sequence is already handled here (rc=-59)"
        ));
        assert!(is_already_handled_refusal("rc=-59"));
        for other in [
            "rc=-590",
            "rc=-5",
            "rc=59",
            "rc=-58)",
            "Co-sign refused: this node will not vouch",
            "rc=",
            "rc=-",
        ] {
            assert!(
                !is_already_handled_refusal(other),
                "{other} must not count as delivered"
            );
        }
    }

    #[test]
    fn withdraw_request_reads_the_flattened_authorisation() {
        let full: WithdrawRequest = serde_json::from_str(
            r#"{"user_id":"rU","amount":"5","destination":"rD",
            "user_pubkey":"02ab","user_sig":"30","auth_nonce":"3","max_ledger":99}"#,
        )
        .unwrap();
        let a = full.auth.expect("all four fields present");
        assert_eq!((a.auth_nonce.as_str(), a.max_ledger), ("3", 99));
        let none: WithdrawRequest =
            serde_json::from_str(r#"{"user_id":"rU","amount":"5","destination":"rD"}"#).unwrap();
        assert!(
            none.auth.is_none(),
            "no authorisation = None, refused by the API with 400"
        );
    }
}
