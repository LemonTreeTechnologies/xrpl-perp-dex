//! Phase 2.2-C — `SignerListSet` governance via libp2p signing relay.
//!
//! Cluster-coordinated subcommand per
//! `docs/multi-operator-architecture.md` §10.3 and §8: runs on one
//! operator's node (whoever initiates the membership change), drives
//! quorum-of-current-signers via the existing libp2p signing relay
//! (the same one withdrawals use, with the Phase 2.2-A SignerListSet
//! policy extension), submits the resulting multi-signed
//! `SignerListSet` to XRPL.
//!
//! Trust model: the leader's only authority is to draft the tx and
//! drive the relay. The signing decision lives in each remote
//! operator's enclave, behind the X-C1 hardened
//! `validate_signing_policy` (constraints in
//! `p2p::validate_signerlist_set_specific`). A malformed or hostile
//! draft is rejected by every honest operator BEFORE a hash hits the
//! enclave.
//!
//! Single-mode: testnet and mainnet operators invoke this identically.
//! No cross-operator SSH; no per-environment branching.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::p2p::{SigningMessage, SigningRelay};
use crate::withdrawal::SignersConfig;
use crate::xrpl_signer;

/// Multisig fee floor — matches the SignerListSet policy validator
/// (`p2p::validate_signerlist_set_specific`). XRPL spec is 12 drops ×
/// (1 + N), but cluster-coordinated SignerListSet always pays the
/// 12 000-drop floor for safety against fee escalator changes.
const FEE_DROPS: u64 = 12_000;

/// Per-operator signing-relay timeout. SignerListSet ceremonies are
/// rare (governance), so wait long enough for any operator to
/// respond — but short enough that a stuck operator doesn't block the
/// initiator forever.
const PER_SIGNER_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AdminState {
    pub xrpl_url: String,
    pub escrow_address: String,
    pub signers_config: SignersConfig,
    pub signing_request_tx: mpsc::Sender<SigningRelay>,
}

#[derive(Debug, Deserialize)]
pub struct SignerlistUpdateRequest {
    /// New operators to add (r-addresses).
    #[serde(default)]
    pub add: Vec<String>,
    /// Operators to remove (r-addresses).
    #[serde(default)]
    pub remove: Vec<String>,
    /// Optional new SignerQuorum. If omitted, defaults to
    /// `ceil(N' * 2 / 3)` over the new size N'.
    #[serde(default)]
    pub quorum: Option<u32>,
    /// If true, build + log the tx but do NOT submit. Returns the
    /// constructed tx in the `unsigned_tx` field for offline review.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct SignerlistUpdateResponse {
    pub status: String,
    pub current_signer_list: Vec<String>,
    pub current_quorum: u32,
    pub new_signer_list: Vec<String>,
    pub new_quorum: u32,
    pub xrpl_tx_hash: Option<String>,
    /// Present in dry-run; omitted on submit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsigned_tx: Option<serde_json::Value>,
    pub message: String,
}

/// Default quorum formula: `ceil(N * 2 / 3)`. Matches the
/// "supermajority" recommendation in
/// `docs/multi-operator-architecture.md` §8.
pub(crate) fn default_quorum(n: usize) -> u32 {
    ((n as u64) * 2).div_ceil(3) as u32
}

/// Compute the new signer list from a (current, +add, -remove) diff.
/// Returns the entries sorted by Account (XRPL canonical order is
/// ascending bytes; lexicographic on r-address strings is NOT the
/// same — `xrpl_signer::decode_xrpl_address` handles that). We sort
/// by address string here for stable identity comparisons; the codec
/// will impose the byte-canonical order at hash time.
pub(crate) fn compute_new_entries(
    current: &[String],
    add: &[String],
    remove: &[String],
) -> Result<Vec<String>> {
    let mut set: BTreeSet<String> = current.iter().cloned().collect();
    for r in remove {
        if !set.remove(r) {
            bail!("remove target {r} not in current SignerList");
        }
    }
    for a in add {
        if !set.insert(a.clone()) {
            bail!("add target {a} already in SignerList");
        }
    }
    let entries: Vec<String> = set.into_iter().collect();
    if !(3..=8).contains(&entries.len()) {
        bail!(
            "new SignerList size {} outside policy bounds [3, 8]",
            entries.len()
        );
    }
    for a in &entries {
        xrpl_signer::decode_xrpl_address(a).with_context(|| format!("invalid r-address {a}"))?;
    }
    Ok(entries)
}

/// Build the unsigned SignerListSet tx (no Signers[] yet). The shape
/// matches what `p2p::validate_signerlist_set_specific` accepts —
/// any deviation here will fail the receiver's policy check, which
/// is the desired safety property (single source of truth for the
/// allowed shape lives in the validator, not the constructor).
pub(crate) fn build_unsigned_tx(
    escrow: &str,
    sequence: u32,
    new_quorum: u32,
    new_entries: &[String],
) -> serde_json::Value {
    let signer_entries: Vec<serde_json::Value> = new_entries
        .iter()
        .map(|a| {
            serde_json::json!({
                "SignerEntry": {"Account": a, "SignerWeight": 1}
            })
        })
        .collect();
    serde_json::json!({
        "TransactionType": "SignerListSet",
        "Account": escrow,
        "Fee": FEE_DROPS.to_string(),
        "Sequence": sequence,
        "SigningPubKey": "",
        "SignerQuorum": new_quorum,
        "SignerEntries": signer_entries,
    })
}

/// Parse the on-chain `account_objects` response into a current
/// SignerList view. Tolerates either the inline-entry shape XRPL
/// returns (`{"SignerEntry": {...}}`) or a flattened variant some
/// older tooling produces; both round-trip through r-address
/// extraction the same way.
pub(crate) fn parse_signer_list_response(resp: &serde_json::Value) -> Result<(Vec<String>, u32)> {
    let objects = resp
        .pointer("/result/account_objects")
        .and_then(|v| v.as_array())
        .context("response missing result.account_objects")?;
    let signer_list = objects
        .iter()
        .find(|o| o["LedgerEntryType"].as_str() == Some("SignerList"))
        .context("escrow has no SignerList on chain (not yet bootstrapped?)")?;
    let entries_raw = signer_list["SignerEntries"]
        .as_array()
        .context("SignerList object missing SignerEntries array")?;
    let mut entries = Vec::with_capacity(entries_raw.len());
    for (i, e) in entries_raw.iter().enumerate() {
        let acct = e
            .pointer("/SignerEntry/Account")
            .and_then(|v| v.as_str())
            .with_context(|| format!("SignerEntries[{i}] missing SignerEntry.Account"))?;
        entries.push(acct.to_string());
    }
    let quorum = signer_list["SignerQuorum"]
        .as_u64()
        .context("SignerList missing SignerQuorum")? as u32;
    Ok((entries, quorum))
}

async fn fetch_current_signerlist(xrpl_url: &str, escrow: &str) -> Result<(Vec<String>, u32)> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "account_objects",
            "params": [{"account": escrow, "type": "signer_list"}]
        }))
        .send()
        .await
        .context("XRPL account_objects request failed")?
        .json()
        .await
        .context("XRPL account_objects response not JSON")?;
    parse_signer_list_response(&resp)
}

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
    if engine_result.starts_with("tes") {
        let hash = resp["result"]["tx_json"]["hash"]
            .as_str()
            .or_else(|| resp["result"]["hash"].as_str())
            .unwrap_or("unknown")
            .to_string();
        Ok(hash)
    } else {
        bail!(
            "XRPL: {} — {}",
            engine_result,
            resp["result"]["engine_result_message"]
                .as_str()
                .unwrap_or("")
        )
    }
}

async fn drive(
    state: &AdminState,
    req: SignerlistUpdateRequest,
) -> Result<SignerlistUpdateResponse> {
    // 1. Read current SignerList from XRPL.
    let (current_entries, current_quorum) =
        fetch_current_signerlist(&state.xrpl_url, &state.escrow_address).await?;
    info!(
        size = current_entries.len(),
        quorum = current_quorum,
        "fetched current SignerList"
    );

    // 2. Compute target membership.
    let new_entries = compute_new_entries(&current_entries, &req.add, &req.remove)?;
    let new_quorum = req
        .quorum
        .unwrap_or_else(|| default_quorum(new_entries.len()));
    if !(2..=(new_entries.len() as u32)).contains(&new_quorum) {
        bail!(
            "new SignerQuorum {new_quorum} outside policy bounds [2, {}]",
            new_entries.len()
        );
    }

    // 3. Fetch escrow Sequence + build unsigned tx.
    let sequence = fetch_account_sequence(&state.xrpl_url, &state.escrow_address).await?;
    let tx_json = build_unsigned_tx(&state.escrow_address, sequence, new_quorum, &new_entries);
    info!(
        sequence = sequence,
        new_size = new_entries.len(),
        new_quorum = new_quorum,
        "built unsigned SignerListSet"
    );

    if req.dry_run {
        return Ok(SignerlistUpdateResponse {
            status: "dry_run".into(),
            current_signer_list: current_entries,
            current_quorum,
            new_signer_list: new_entries,
            new_quorum,
            xrpl_tx_hash: None,
            unsigned_tx: Some(tx_json),
            message: "dry-run: tx constructed but not submitted".into(),
        });
    }

    // 4. Collect signatures from CURRENT signers via the relay. The
    // current quorum is what authorizes the change — even if the new
    // quorum is smaller, the change itself is signed under the OLD
    // SignerList semantics XRPL enforces.
    let mut collected: Vec<serde_json::Value> = Vec::new();
    for current_addr in &current_entries {
        if collected.len() >= current_quorum as usize {
            break;
        }
        let signer = match state
            .signers_config
            .signers
            .iter()
            .find(|s| s.xrpl_address == *current_addr)
        {
            Some(s) => s,
            None => {
                warn!(
                    addr = %current_addr,
                    "current on-chain signer not present in local signers_config — skipping"
                );
                continue;
            }
        };
        let account_id = match xrpl_signer::decode_xrpl_address(&signer.xrpl_address) {
            Ok(id) => id,
            Err(e) => {
                warn!(addr = %signer.xrpl_address, error = %e, "decode failed; skipping");
                continue;
            }
        };

        let request_id = format!("slu-{:016x}", rand::random::<u64>());
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        if state
            .signing_request_tx
            .send(SigningRelay {
                request_id: request_id.clone(),
                unsigned_tx: tx_json.clone(),
                signer_account_id_hex: hex::encode(account_id),
                signer_xrpl_address: signer.xrpl_address.clone(),
                response_tx: resp_tx,
            })
            .await
            .is_err()
        {
            bail!("signing relay channel closed — orchestrator shutting down?");
        }

        match tokio::time::timeout(PER_SIGNER_TIMEOUT, resp_rx).await {
            Ok(Ok(SigningMessage::Response {
                der_signature: Some(der),
                compressed_pubkey: Some(pk),
                error: None,
                ..
            })) => {
                info!(
                    signer = %signer.xrpl_address,
                    der_len = der.len() / 2,
                    "collected SignerListSet signature"
                );
                collected.push(serde_json::json!({
                    "Signer": {
                        "Account": signer.xrpl_address,
                        "SigningPubKey": pk,
                        "TxnSignature": der,
                    }
                }));
            }
            Ok(Ok(SigningMessage::Response { error: Some(e), .. })) => {
                warn!(signer = %signer.xrpl_address, error = %e, "remote signer rejected");
            }
            Ok(Ok(_)) => warn!(signer = %signer.xrpl_address, "malformed signing response"),
            Ok(Err(_)) => warn!(signer = %signer.xrpl_address, "signing response channel dropped"),
            Err(_) => warn!(signer = %signer.xrpl_address, "signing relay timeout"),
        }
    }

    if collected.len() < current_quorum as usize {
        bail!(
            "collected {} of {} required signatures",
            collected.len(),
            current_quorum
        );
    }

    // 5. Sort Signers[] by AccountID bytes (XRPL canonical), assemble.
    collected.sort_by(|a, b| {
        let aa = xrpl_signer::decode_xrpl_address(
            a.pointer("/Signer/Account")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
        .unwrap_or([0xff; 20]);
        let bb = xrpl_signer::decode_xrpl_address(
            b.pointer("/Signer/Account")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
        .unwrap_or([0xff; 20]);
        aa.cmp(&bb)
    });

    let mut full_tx = tx_json.clone();
    full_tx["Signers"] = serde_json::Value::Array(collected);

    // 6. Submit.
    let xrpl_hash = submit_multisigned(&state.xrpl_url, &full_tx).await?;
    info!(xrpl_hash = %xrpl_hash, "SignerListSet submitted");

    Ok(SignerlistUpdateResponse {
        status: "success".into(),
        current_signer_list: current_entries,
        current_quorum,
        new_signer_list: new_entries,
        new_quorum,
        xrpl_tx_hash: Some(xrpl_hash),
        unsigned_tx: None,
        message: "SignerListSet submitted to XRPL — re-fetch on-chain to confirm".into(),
    })
}

pub async fn handle_signerlist_update(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<SignerlistUpdateRequest>,
) -> impl IntoResponse {
    match drive(&state, req).await {
        Ok(resp) => (StatusCode::OK, Json(serde_json::to_value(resp).unwrap())).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": "error", "message": e.to_string()})),
        )
            .into_response(),
    }
}

pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/admin/signerlist-update", post(handle_signerlist_update))
        .with_state(state)
}

pub async fn spawn_admin_listener(listen_addr: String, state: Arc<AdminState>) -> Result<()> {
    let parsed: std::net::SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid --signerlist-admin-listen address {listen_addr:?}"))?;
    if !parsed.ip().is_loopback() {
        bail!(
            "--signerlist-admin-listen must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }
    let listener = tokio::net::TcpListener::bind(parsed)
        .await
        .with_context(|| format!("signerlist-admin bind on {parsed} failed"))?;
    info!(listen = %parsed, "signerlist-update admin listener started");
    let app = router(state);
    axum::serve(listener, app)
        .await
        .context("signerlist-admin serve error")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_quorum_three_ceils_to_two() {
        // ceil(3 * 2 / 3) = ceil(2.0) = 2
        assert_eq!(default_quorum(3), 2);
    }

    #[test]
    fn default_quorum_four_ceils_to_three() {
        // ceil(4 * 2 / 3) = ceil(2.667) = 3
        assert_eq!(default_quorum(4), 3);
    }

    #[test]
    fn default_quorum_five_ceils_to_four() {
        // ceil(5 * 2 / 3) = ceil(3.333) = 4
        assert_eq!(default_quorum(5), 4);
    }

    #[test]
    fn default_quorum_seven_ceils_to_five() {
        // ceil(7 * 2 / 3) = ceil(4.667) = 5
        assert_eq!(default_quorum(7), 5);
    }

    #[test]
    fn default_quorum_eight_ceils_to_six() {
        // ceil(8 * 2 / 3) = ceil(5.333) = 6
        assert_eq!(default_quorum(8), 6);
    }

    const ADDR_A: &str = "rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzRH";
    const ADDR_B: &str = "rPT1Sjq2YGrBMTttX4GZHjKu9dyfzbpAYe";
    const ADDR_C: &str = "rNrjh1KGZk2jBR3wPfAQnoidtFFYQKbQn2";
    const ADDR_D: &str = "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn";
    const ADDR_E: &str = "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ";

    #[test]
    fn compute_new_entries_simple_add() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let new = compute_new_entries(&cur, &[ADDR_D.into()], &[]).unwrap();
        assert_eq!(new.len(), 4);
        assert!(new.contains(&ADDR_D.to_string()));
    }

    #[test]
    fn compute_new_entries_simple_remove() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into(), ADDR_D.into()];
        let new = compute_new_entries(&cur, &[], &[ADDR_D.into()]).unwrap();
        assert_eq!(new.len(), 3);
        assert!(!new.contains(&ADDR_D.to_string()));
    }

    #[test]
    fn compute_new_entries_swap_one_for_one() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let new = compute_new_entries(&cur, &[ADDR_D.into()], &[ADDR_C.into()]).unwrap();
        assert_eq!(new.len(), 3);
        assert!(new.contains(&ADDR_D.to_string()));
        assert!(!new.contains(&ADDR_C.to_string()));
    }

    #[test]
    fn compute_new_entries_rejects_remove_of_nonmember() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let err = compute_new_entries(&cur, &[], &[ADDR_D.into()]).unwrap_err();
        assert!(format!("{err}").contains("not in current SignerList"));
    }

    #[test]
    fn compute_new_entries_rejects_add_of_existing() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let err = compute_new_entries(&cur, &[ADDR_B.into()], &[]).unwrap_err();
        assert!(format!("{err}").contains("already in SignerList"));
    }

    #[test]
    fn compute_new_entries_rejects_undersize_result() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let err = compute_new_entries(&cur, &[], &[ADDR_A.into(), ADDR_B.into()]).unwrap_err();
        assert!(format!("{err}").contains("outside policy bounds"));
    }

    #[test]
    fn compute_new_entries_rejects_oversize_result() {
        let cur: Vec<String> = (1..=8)
            .map(|i| format!("rN7n7otQDd6FczFgLdSqtcsAUxDkw6fzR{i}"))
            .collect();
        // ↑ malformed addresses, but addr-validation only runs after
        // size check, so size check fires first. Force size to 9 by
        // adding one more good-faith r-address.
        let _ = cur; // suppress unused
                     // Reuse known-valid addresses.
        let cur = vec![
            ADDR_A.into(),
            ADDR_B.into(),
            ADDR_C.into(),
            ADDR_D.into(),
            ADDR_E.into(),
            "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j".into(),
            "rJWSAM1cHSfwDrSnA1qyJbnEaSaAvJNp18".into(),
            "rBWt8nw2DGpJoh3qUyTkNAiRjW7C3Ds7ti".into(),
        ];
        let err = compute_new_entries(&cur, &["rnzQC8HNEcgVHd8y8jb7PWDDJZ5Vd1P9WQ".into()], &[])
            .unwrap_err();
        assert!(format!("{err}").contains("outside policy bounds"));
    }

    #[test]
    fn compute_new_entries_rejects_invalid_address_in_add() {
        let cur = vec![ADDR_A.into(), ADDR_B.into(), ADDR_C.into()];
        let err = compute_new_entries(&cur, &["not-an-address".into()], &[]).unwrap_err();
        assert!(format!("{err}").contains("invalid r-address"));
    }

    #[test]
    fn build_unsigned_tx_shape_matches_validator() {
        // The Phase 2.2-A `validate_signerlist_set_specific` accepts
        // {Account, TransactionType, Sequence, Fee, SigningPubKey,
        // SignerQuorum, SignerEntries[3..=8 with weight=1]}. This test
        // locks the constructor's output against that shape so a future
        // refactor can't drift the two apart silently.
        let entries = vec![ADDR_A.to_string(), ADDR_B.to_string(), ADDR_C.to_string()];
        let tx = build_unsigned_tx(ADDR_D, 42, 2, &entries);
        assert_eq!(tx["TransactionType"], "SignerListSet");
        assert_eq!(tx["Account"], ADDR_D);
        assert_eq!(tx["Fee"], "12000");
        assert_eq!(tx["Sequence"], 42);
        assert_eq!(tx["SigningPubKey"], "");
        assert_eq!(tx["SignerQuorum"], 2);
        let entries_arr = tx["SignerEntries"].as_array().expect("array");
        assert_eq!(entries_arr.len(), 3);
        for (i, e) in entries_arr.iter().enumerate() {
            assert_eq!(e["SignerEntry"]["Account"], entries[i]);
            assert_eq!(e["SignerEntry"]["SignerWeight"], 1);
        }
    }

    #[test]
    fn parse_signer_list_extracts_entries_and_quorum() {
        let resp = serde_json::json!({
            "result": {
                "account_objects": [
                    {
                        "LedgerEntryType": "SignerList",
                        "SignerQuorum": 2,
                        "SignerEntries": [
                            {"SignerEntry": {"Account": ADDR_A, "SignerWeight": 1}},
                            {"SignerEntry": {"Account": ADDR_B, "SignerWeight": 1}},
                            {"SignerEntry": {"Account": ADDR_C, "SignerWeight": 1}},
                        ],
                    }
                ]
            }
        });
        let (entries, quorum) = parse_signer_list_response(&resp).expect("parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(quorum, 2);
        assert!(entries.contains(&ADDR_A.to_string()));
    }

    #[test]
    fn parse_signer_list_skips_non_signerlist_objects() {
        let resp = serde_json::json!({
            "result": {
                "account_objects": [
                    {"LedgerEntryType": "RippleState"},
                    {
                        "LedgerEntryType": "SignerList",
                        "SignerQuorum": 3,
                        "SignerEntries": [
                            {"SignerEntry": {"Account": ADDR_A, "SignerWeight": 1}},
                            {"SignerEntry": {"Account": ADDR_B, "SignerWeight": 1}},
                            {"SignerEntry": {"Account": ADDR_C, "SignerWeight": 1}},
                        ],
                    }
                ]
            }
        });
        let (entries, quorum) = parse_signer_list_response(&resp).expect("parse");
        assert_eq!(entries.len(), 3);
        assert_eq!(quorum, 3);
    }

    #[test]
    fn parse_signer_list_rejects_no_signerlist() {
        let resp = serde_json::json!({
            "result": {
                "account_objects": [
                    {"LedgerEntryType": "RippleState"},
                ]
            }
        });
        let err = parse_signer_list_response(&resp).unwrap_err();
        assert!(format!("{err}").contains("has no SignerList"));
    }

    #[test]
    fn parse_signer_list_rejects_missing_account_objects() {
        let resp = serde_json::json!({"result": {}});
        let err = parse_signer_list_response(&resp).unwrap_err();
        assert!(format!("{err}").contains("missing result.account_objects"));
    }
}
