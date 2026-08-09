// The join-brain task (source role on Request, newcomer role on Deliver) is the
// β3.2c wiring increment; until main.rs spawns it, these adapters are exercised
// only by the unit tests below, which pin the enclave wire contract.
#![allow(dead_code)]

//! β3.2c / #127 / X-β3.2-3 — HTTP adapters for the DCAP-authenticated new-node
//! bootstrap transport (the loopback enclave admin routes):
//!
//!   - `bootstrap_bundle_export` — a `source` node wraps the current quorum
//!     bundle for a DCAP-verified `newcomer` over the verified-peer ECDH channel
//!     (`POST /v1/admin/signerlist/bootstrap-bundle-export`) → returns the sealed
//!     `BootstrapEnvelope`.
//!   - `bootstrap_bundle_import` — the `newcomer` unwraps off the verified-peer
//!     session, then the enclave runs the existing M-1 quorum verify + seal on
//!     the transport-authenticated bundle
//!     (`POST /v1/admin/signerlist/bootstrap-bundle-import`) → returns the epoch.
//!
//! JSON field names mirror `signerlist_handler.cpp::handleBootstrapBundle{Export,
//! Import}` VERBATIM. Signer sets go on the wire as `[{account_id, weight}]`
//! arrays (the enclave's `pack_signers` packs them server-side); the pure
//! builders/parsers are split out so the contract is unit-tested without live IO.
//!
//! `now_ts` feeds ONLY the enclave's peer-attest-cache TTL check — it is NOT part
//! of the ECIES AAD (which binds the statement's membership message hash), so the
//! source and newcomer each pass their own current time; they need not match.

use anyhow::{bail, Context, Result};

use crate::p2p::MembershipSignerWire;

const BOOTSTRAP_BUNDLE_EXPORT_PATH: &str = "/v1/admin/signerlist/bootstrap-bundle-export";
const BOOTSTRAP_BUNDLE_IMPORT_PATH: &str = "/v1/admin/signerlist/bootstrap-bundle-import";

/// The ECIES envelope the source's enclave produces and the newcomer's enclave
/// consumes. All fields lowercase hex, no `0x`. `sender_pk` is the source's
/// 33-byte compressed ECDH identity pubkey (the newcomer passes it back so its
/// enclave can look the source up in `peer_attest_cache` — A-PA-1 parity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapEnvelope {
    pub ceremony_nonce: String,
    pub iv: String,
    pub ct: String,
    pub tag: String,
    pub sender_pk: String,
}

/// The (public) membership statement + config a source wraps a bundle against.
/// The AUTHORITY set + escrow + epoch + prev_hash are what the ECIES AAD binds,
/// so the newcomer MUST import with the exact same values or the tag fails
/// (anti-splice). `attesting_*` is needed only on import (the M-1 quorum verify).
pub struct BootstrapExportInput<'a> {
    pub newcomer_pk_hex: &'a str,
    pub shard_id: u32,
    pub group_id_hex: &'a str,
    pub escrow_hex: &'a str,
    pub prev_epoch_hash_hex: &'a str,
    pub quorum_bundle_hex: &'a str,
    pub now_ts: u64,
    pub authority_signers: &'a [MembershipSignerWire],
    pub authority_quorum: u32,
    pub authority_epoch: u64,
}

/// Everything the newcomer's enclave needs to unwrap + verify + seal: the source
/// `sender_pk` + envelope, the authority statement (must match what was wrapped),
/// and the attesting (M-1) set + quorum for the bundle verify.
pub struct BootstrapImportInput<'a> {
    pub sender_pk_hex: &'a str,
    pub shard_id: u32,
    pub group_id_hex: &'a str,
    pub escrow_hex: &'a str,
    pub prev_epoch_hash_hex: &'a str,
    pub now_ts: u64,
    pub authority_signers: &'a [MembershipSignerWire],
    pub authority_quorum: u32,
    pub authority_epoch: u64,
    pub confirmed_epoch: u64,
    pub attesting_signers: &'a [MembershipSignerWire],
    pub attesting_quorum: u32,
    pub envelope: &'a BootstrapEnvelope,
}

// ── pure contract (testable without IO) ──────────────────────────

/// `[{account_id, weight}]` — the array shape the enclave's `pack_signers` reads.
fn signers_json(signers: &[MembershipSignerWire]) -> Vec<serde_json::Value> {
    signers
        .iter()
        .map(|s| {
            serde_json::json!({
                "account_id": s.account_id_hex,
                "weight": s.weight,
            })
        })
        .collect()
}

/// Build the bootstrap-bundle-export POST body. Field names MUST match
/// `signerlist_handler.cpp::handleBootstrapBundleExport`.
fn build_export_request(input: &BootstrapExportInput) -> serde_json::Value {
    serde_json::json!({
        "newcomer_pk": input.newcomer_pk_hex,
        "shard_id": input.shard_id,
        "group_id": input.group_id_hex,
        "escrow_account_id": input.escrow_hex,
        "prev_epoch_hash": input.prev_epoch_hash_hex,
        "quorum_bundle": input.quorum_bundle_hex,
        "now_ts": input.now_ts,
        "authority_signers": signers_json(input.authority_signers),
        "authority_quorum": input.authority_quorum,
        "authority_epoch": input.authority_epoch,
    })
}

/// Parse the export response into the sealed envelope, or an error carrying the
/// enclave reject code (so a debug/wrong-MRENCLAVE newcomer or an unverified
/// peer session surfaces as a typed failure, never a silent success).
fn parse_export_response(body: &serde_json::Value) -> Result<BootstrapEnvelope> {
    match body.get("status").and_then(|v| v.as_str()) {
        Some("ok") => {
            let field = |k: &str| -> Result<String> {
                body.get(k)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .with_context(|| format!("export response missing `{k}`"))
            };
            Ok(BootstrapEnvelope {
                ceremony_nonce: field("ceremony_nonce")?,
                iv: field("iv")?,
                ct: field("ct")?,
                tag: field("tag")?,
                sender_pk: field("sender_pk")?,
            })
        }
        _ => {
            let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("bootstrap-bundle-export rejected (code {code}): {msg}");
        }
    }
}

/// Build the bootstrap-bundle-import POST body. Field names MUST match
/// `signerlist_handler.cpp::handleBootstrapBundleImport`.
fn build_import_request(input: &BootstrapImportInput) -> serde_json::Value {
    serde_json::json!({
        "sender_pk": input.sender_pk_hex,
        "shard_id": input.shard_id,
        "group_id": input.group_id_hex,
        "escrow_account_id": input.escrow_hex,
        "prev_epoch_hash": input.prev_epoch_hash_hex,
        "now_ts": input.now_ts,
        "authority_signers": signers_json(input.authority_signers),
        "authority_quorum": input.authority_quorum,
        "authority_epoch": input.authority_epoch,
        "confirmed_epoch": input.confirmed_epoch,
        "attesting_signers": signers_json(input.attesting_signers),
        "attesting_quorum": input.attesting_quorum,
        "ceremony_nonce": input.envelope.ceremony_nonce,
        "iv": input.envelope.iv,
        "ct": input.envelope.ct,
        "tag": input.envelope.tag,
    })
}

/// Parse the import response into the sealed epoch, or an error carrying the
/// enclave reject code. A spliced/stale bundle fails the AEAD tag (code
/// `BOOTSTRAP_XPORT_ERR_AEAD`); a non-quorum or wrong-set bundle fails the M-1
/// verify inside `ecall_bootstrap_from_quorum_attestation` — both land here.
fn parse_import_response(body: &serde_json::Value) -> Result<u64> {
    match body.get("status").and_then(|v| v.as_str()) {
        Some("ok") => body
            .get("epoch")
            .and_then(|v| v.as_u64())
            .context("import response missing `epoch`"),
        _ => {
            let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let msg = body
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            bail!("bootstrap-bundle-import rejected (code {code}): {msg}");
        }
    }
}

// ── live IO ──────────────────────────────────────────────────────

/// Source role: POST the export to THIS node's loopback enclave and return the
/// sealed envelope. `admin_base` is the local enclave admin base
/// (`scheme://host:port`); the caller must have ensured it is loopback (X-C1).
pub async fn bootstrap_bundle_export(
    client: &reqwest::Client,
    admin_base: &str,
    input: &BootstrapExportInput<'_>,
) -> Result<BootstrapEnvelope> {
    let url = format!(
        "{}{}",
        admin_base.trim_end_matches('/'),
        BOOTSTRAP_BUNDLE_EXPORT_PATH
    );
    let req = build_export_request(input);
    let resp = client
        .post(&url)
        .json(&req)
        .send()
        .await
        .with_context(|| format!("POST bootstrap-bundle-export to {admin_base}"))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .context("bootstrap-bundle-export response body")?;
    parse_export_response(&body)
}

/// Newcomer role: POST the import to THIS node's loopback enclave; on success the
/// enclave has verified the transport session + the M-1 quorum + sealed in-sync.
/// Returns the sealed epoch. `admin_base` must be loopback (X-C1).
pub async fn bootstrap_bundle_import(
    client: &reqwest::Client,
    admin_base: &str,
    input: &BootstrapImportInput<'_>,
) -> Result<u64> {
    let url = format!(
        "{}{}",
        admin_base.trim_end_matches('/'),
        BOOTSTRAP_BUNDLE_IMPORT_PATH
    );
    let req = build_import_request(input);
    let resp = client
        .post(&url)
        .json(&req)
        .send()
        .await
        .with_context(|| format!("POST bootstrap-bundle-import to {admin_base}"))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .context("bootstrap-bundle-import response body")?;
    parse_import_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(hexchar: &str) -> Vec<MembershipSignerWire> {
        vec![MembershipSignerWire {
            account_id_hex: hexchar.repeat(20),
            weight: 1,
        }]
    }

    #[test]
    fn export_request_matches_enclave_contract() {
        let signers = wire("01");
        let newcomer_pk = "02".repeat(33);
        let input = BootstrapExportInput {
            newcomer_pk_hex: &newcomer_pk,
            shard_id: 0,
            group_id_hex: &"aa".repeat(32),
            escrow_hex: &"bb".repeat(20),
            prev_epoch_hash_hex: &"cc".repeat(32),
            quorum_bundle_hex: "deadbeef",
            now_ts: 1_700_000_000,
            authority_signers: &signers,
            authority_quorum: 2,
            authority_epoch: 5,
        };
        let v = build_export_request(&input);
        // exact field names the enclave reads
        assert_eq!(v["shard_id"], 0);
        assert_eq!(v["escrow_account_id"], "bb".repeat(20));
        assert_eq!(v["authority_quorum"], 2);
        assert_eq!(v["authority_epoch"], 5);
        assert_eq!(v["now_ts"], 1_700_000_000u64);
        assert_eq!(v["authority_signers"][0]["account_id"], "01".repeat(20));
        assert_eq!(v["authority_signers"][0]["weight"], 1);
        // export must NOT leak the attesting set (that is import-only).
        assert!(v.get("attesting_signers").is_none());
    }

    #[test]
    fn import_request_carries_both_sets_and_envelope() {
        let authority = wire("01");
        let attesting = wire("02");
        let env = BootstrapEnvelope {
            ceremony_nonce: "11".repeat(32),
            iv: "22".repeat(12),
            ct: "33".repeat(48),
            tag: "44".repeat(16),
            sender_pk: "55".repeat(33),
        };
        let input = BootstrapImportInput {
            sender_pk_hex: &env.sender_pk,
            shard_id: 3,
            group_id_hex: &"aa".repeat(32),
            escrow_hex: &"bb".repeat(20),
            prev_epoch_hash_hex: &"cc".repeat(32),
            now_ts: 1_700_000_001,
            authority_signers: &authority,
            authority_quorum: 2,
            authority_epoch: 5,
            confirmed_epoch: 5,
            attesting_signers: &attesting,
            attesting_quorum: 2,
            envelope: &env,
        };
        let v = build_import_request(&input);
        assert_eq!(v["sender_pk"], "55".repeat(33));
        assert_eq!(v["authority_signers"][0]["account_id"], "01".repeat(20));
        assert_eq!(v["attesting_signers"][0]["account_id"], "02".repeat(20));
        assert_eq!(v["attesting_quorum"], 2);
        assert_eq!(v["confirmed_epoch"], 5);
        assert_eq!(v["ceremony_nonce"], "11".repeat(32));
        assert_eq!(v["tag"], "44".repeat(16));
    }

    #[test]
    fn parse_export_ok_and_error() {
        let ok = serde_json::json!({
            "status": "ok",
            "ceremony_nonce": "11", "iv": "22", "ct": "33", "tag": "44", "sender_pk": "55"
        });
        let env = parse_export_response(&ok).expect("ok envelope");
        assert_eq!(env.tag, "44");
        assert_eq!(env.sender_pk, "55");

        let err = serde_json::json!({
            "status": "error", "code": -9, "message": "debug peer"
        });
        let e = parse_export_response(&err).unwrap_err();
        assert!(e.to_string().contains("-9"), "carries enclave code: {e}");
    }

    #[test]
    fn parse_import_ok_and_error() {
        let ok = serde_json::json!({"status": "ok", "epoch": 5});
        assert_eq!(parse_import_response(&ok).expect("epoch"), 5);

        // spliced/stale bundle → AEAD tag failure carries its code.
        let err = serde_json::json!({"status": "error", "code": -5, "message": "aead"});
        let e = parse_import_response(&err).unwrap_err();
        assert!(e.to_string().contains("-5"), "carries AEAD code: {e}");
    }
}
