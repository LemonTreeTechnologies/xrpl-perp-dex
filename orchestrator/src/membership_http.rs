// Production wiring (the admin trigger route + main.rs channel hookup) is the
// β3 deploy increment — analogous to Path-A's PRG-2 part 4/4, which likewise
// left set_path_a_delegation_channel unwired. Until then these adapters are
// exercised by the unit tests below, which pin the enclave wire contract.
#![allow(dead_code)]

//! β1 (perp β-retrofit) HTTP adapters for the membership-change driver:
//!
//!   - `HttpEpochDigestSource` — GET the LOCAL enclave's current epoch + digest
//!     so the driver can build the next transition.
//!   - `HttpEpochSealSink` — POST a `(statement, bundle)` to ONE node's
//!     seal-epoch admin route → that node's `ecall_seal_membership_epoch`.
//!
//! JSON field names mirror the enclave's `signerlist_handler.cpp` VERBATIM
//! (`escrow_account_id`, `prev_epoch_hash`, `quorum_bundle`, `signers[]` of
//! `{account_id, weight}`, `quorum_threshold`, `proposed_epoch`; response
//! `{status, epoch, epoch_digest}`). A drift surfaces as a parse error here,
//! never a silent governance break. The pure builders/parsers are split out so
//! the contract is unit-tested without live IO.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use crate::membership_coordinator::{
    EpochDigestSource, EpochSealSink, GenesisBootstrapSink, MembershipEpochStatement,
};
use crate::membership_projection::{MembershipSyncState, ProjectionConfirmer, SyncStateSource};

const EPOCH_DIGEST_PATH: &str = "/v1/admin/signerlist/epoch-digest";
const SEAL_EPOCH_PATH: &str = "/v1/admin/signerlist/seal-epoch";
const SYNC_STATE_PATH: &str = "/v1/admin/signerlist/sync-state";
const RECORD_CONFIRMATION_PATH: &str = "/v1/admin/signerlist/record-projection-confirmation";
const BOOTSTRAP_ATTESTED_PATH: &str = "/v1/admin/signerlist/bootstrap-attested";
/* β4 Thread B — governed MRENCLAVE allowlist */
const MRENCLAVES_STATUS_PATH: &str = "/v1/admin/mrenclaves/status";
const MRENCLAVES_GOVERN_PATH: &str = "/v1/admin/mrenclaves/govern";

// ── pure contract (testable without IO) ──────────────────────────

/// Build the seal-epoch POST body. Field names MUST match
/// `signerlist_handler.cpp::handle_seal_epoch` / `pack_signers`.
fn build_seal_request(statement: &MembershipEpochStatement, bundle: &[u8]) -> serde_json::Value {
    let signers: Vec<serde_json::Value> = statement
        .new_signers
        .iter()
        .map(|s| {
            serde_json::json!({
                "account_id": hex::encode(s.account_id),
                "weight": s.weight,
            })
        })
        .collect();
    serde_json::json!({
        "escrow_account_id": hex::encode(statement.escrow),
        "prev_epoch_hash": hex::encode(statement.prev_epoch_hash),
        "quorum_bundle": hex::encode(bundle),
        "signers": signers,
        "quorum_threshold": statement.new_quorum,
        "proposed_epoch": statement.proposed_epoch,
    })
}

/// β4 Thread A genesis (RESP-β4-threadA-impl.1 option 2): build the
/// bootstrap-attested POST body. Field names MUST match
/// `signerlist_handler.cpp::handleBootstrapFromQuorumAttestation`.
///
/// At genesis the ATTESTING set IS the AUTHORITY set, and the epoch is sealed
/// already in-sync (`confirmed_epoch == authority_epoch`) because there is no
/// prior projection to catch up to. Both are derived here rather than taken as
/// parameters, so a caller cannot express a mismatch.
fn build_bootstrap_attested_request(
    statement: &MembershipEpochStatement,
    bundle: &[u8],
) -> serde_json::Value {
    let signers: Vec<serde_json::Value> = statement
        .new_signers
        .iter()
        .map(|s| {
            serde_json::json!({
                "account_id": hex::encode(s.account_id),
                "weight": s.weight,
            })
        })
        .collect();
    serde_json::json!({
        "escrow_account_id": hex::encode(statement.escrow),
        "authority_signers": signers,
        "authority_quorum": statement.new_quorum,
        "authority_epoch": statement.proposed_epoch,
        "confirmed_epoch": statement.proposed_epoch,
        "prev_epoch_hash": hex::encode(statement.prev_epoch_hash),
        "attesting_signers": signers,
        "attesting_quorum": statement.new_quorum,
        "quorum_bundle": hex::encode(bundle),
    })
}

/// Parse the epoch-digest GET response into `(epoch, digest)`. Errors if the
/// enclave reports no sealed epoch yet (`bootstrapped:false` — genesis is a
/// separate path, NOT a transition) or the body is malformed.
fn parse_epoch_digest(body: &serde_json::Value) -> Result<(u64, [u8; 32])> {
    if body["status"].as_str() != Some("ok") {
        bail!(
            "epoch-digest error: {}",
            body.get("code")
                .or_else(|| body.get("message"))
                .unwrap_or(body)
        );
    }
    if body["bootstrapped"] == serde_json::json!(false) {
        bail!(
            "enclave has no sealed membership epoch yet \
             (genesis bootstrap required before a transition)"
        );
    }
    let epoch = body["epoch"]
        .as_u64()
        .context("epoch-digest: missing/invalid epoch")?;
    let digest_hex = body["epoch_digest"]
        .as_str()
        .context("epoch-digest: missing epoch_digest")?;
    let bytes = hex::decode(digest_hex).context("epoch-digest: epoch_digest not hex")?;
    if bytes.len() != 32 {
        bail!(
            "epoch-digest: epoch_digest is {} bytes, want 32",
            bytes.len()
        );
    }
    let mut d = [0u8; 32];
    d.copy_from_slice(&bytes);
    Ok((epoch, d))
}

/// Parse the seal-epoch POST response — `Ok(())` on `status:ok`, else the
/// enclave's error code/message (e.g. a monotonic-epoch or bundle-quorum
/// rejection from `ecall_seal_membership_epoch`).
fn parse_seal_response(body: &serde_json::Value) -> Result<()> {
    if body["status"].as_str() == Some("ok") {
        return Ok(());
    }
    bail!(
        "seal-epoch rejected: code={} message={}",
        body.get("code")
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        body.get("message").and_then(|m| m.as_str()).unwrap_or("?")
    );
}

// ── HTTP adapters ────────────────────────────────────────────────

/// Reads the current epoch + authority digest from the LOCAL enclave.
pub struct HttpEpochDigestSource {
    client: reqwest::Client,
    enclave_base: String,
}

impl HttpEpochDigestSource {
    pub fn new(client: reqwest::Client, enclave_base: String) -> Self {
        Self {
            client,
            enclave_base,
        }
    }
}

#[async_trait]
impl EpochDigestSource for HttpEpochDigestSource {
    async fn current_epoch(&self) -> Result<(u64, [u8; 32])> {
        let url = format!(
            "{}{}",
            self.enclave_base.trim_end_matches('/'),
            EPOCH_DIGEST_PATH
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET epoch-digest from local enclave")?;
        let body: serde_json::Value = resp.json().await.context("epoch-digest response body")?;
        parse_epoch_digest(&body)
    }
}

/// Applies a `(statement, bundle)` on ONE node by POSTing to its seal-epoch
/// admin route. `node_admin_url` is the node's admin base (scheme://host:port).
pub struct HttpEpochSealSink {
    client: reqwest::Client,
}

impl HttpEpochSealSink {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl EpochSealSink for HttpEpochSealSink {
    async fn seal_on_node(
        &self,
        node_admin_url: &str,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<()> {
        let url = format!(
            "{}{}",
            node_admin_url.trim_end_matches('/'),
            SEAL_EPOCH_PATH
        );
        let req = build_seal_request(statement, bundle);
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST seal-epoch to {node_admin_url}"))?;
        let body: serde_json::Value = resp.json().await.context("seal-epoch response body")?;
        parse_seal_response(&body)
    }
}

/// β4 Thread B: reads the local enclave's allowlist head (the epoch + digest the
/// next operation must chain onto). The enclave reports the IMPLICIT genesis
/// state when nothing has been governed yet, so the first operation chains
/// naturally off epoch 0 without a special case here.
pub struct HttpAllowlistStatusSource {
    client: reqwest::Client,
    enclave_base: String,
}

impl HttpAllowlistStatusSource {
    pub fn new(client: reqwest::Client, enclave_base: String) -> Self {
        Self {
            client,
            enclave_base,
        }
    }
}

#[async_trait]
impl crate::mrenclave_governance::AllowlistStatusSource for HttpAllowlistStatusSource {
    async fn current(&self) -> Result<(u64, [u8; 32])> {
        let url = format!(
            "{}{}",
            self.enclave_base.trim_end_matches('/'),
            MRENCLAVES_STATUS_PATH
        );
        let body: serde_json::Value = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET mrenclaves/status")?
            .json()
            .await
            .context("mrenclaves/status response body")?;
        if body["status"].as_str() != Some("ok") {
            bail!(
                "mrenclaves/status error: {}",
                body.get("message").unwrap_or(&body)
            );
        }
        let epoch = body["allowlist_epoch"]
            .as_u64()
            .context("mrenclaves/status: missing allowlist_epoch")?;
        let digest_hex = body["allowlist_digest"]
            .as_str()
            .context("mrenclaves/status: missing allowlist_digest")?;
        let bytes = hex::decode(digest_hex).context("mrenclaves/status: bad digest hex")?;
        if bytes.len() != 32 {
            bail!(
                "mrenclaves/status: digest must be 32 bytes, got {}",
                bytes.len()
            );
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&bytes);
        Ok((epoch, digest))
    }
}

/// β4 Thread B: applies one allowlist operation on a node's enclave.
pub struct HttpGovernSink {
    client: reqwest::Client,
}

impl HttpGovernSink {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Field names MUST match `signerlist_handler.cpp::handleGovernTrustedMrenclaves`.
    /// `repro_bundle` is omitted entirely for a veto — the enclave requires it
    /// only for an admit.
    pub fn build_request(
        op: &crate::mrenclave_governance::GovernanceOp,
        quorum_bundle: &[u8],
        repro_bundle: &[u8],
    ) -> serde_json::Value {
        let mut req = serde_json::json!({
            "op": op.op,
            "mrenclave": hex::encode(op.mrenclave),
            "escrow_account_id": hex::encode(op.escrow),
            "proposed_epoch": op.proposed_epoch,
            "prev_allowlist_hash": hex::encode(op.prev_allowlist_hash),
            "quorum_bundle": hex::encode(quorum_bundle),
        });
        if !repro_bundle.is_empty() {
            req["repro_bundle"] = serde_json::json!(hex::encode(repro_bundle));
        }
        req
    }

    pub async fn govern_on_node(
        &self,
        node_admin_url: &str,
        op: &crate::mrenclave_governance::GovernanceOp,
        quorum_bundle: &[u8],
        repro_bundle: &[u8],
    ) -> Result<()> {
        let url = format!(
            "{}{}",
            node_admin_url.trim_end_matches('/'),
            MRENCLAVES_GOVERN_PATH
        );
        let resp = self
            .client
            .post(&url)
            .json(&Self::build_request(op, quorum_bundle, repro_bundle))
            .send()
            .await
            .with_context(|| format!("POST mrenclaves/govern to {node_admin_url}"))?;
        let body: serde_json::Value = resp.json().await.context("govern response body")?;
        parse_seal_response(&body)
    }
}

/// β4 Thread A genesis: POSTs `bootstrap-attested` to a node's enclave admin route.
pub struct HttpGenesisBootstrapSink {
    client: reqwest::Client,
}

impl HttpGenesisBootstrapSink {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl GenesisBootstrapSink for HttpGenesisBootstrapSink {
    async fn bootstrap_on_node(
        &self,
        node_admin_url: &str,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<()> {
        let url = format!(
            "{}{}",
            node_admin_url.trim_end_matches('/'),
            BOOTSTRAP_ATTESTED_PATH
        );
        let req = build_bootstrap_attested_request(statement, bundle);
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST bootstrap-attested to {node_admin_url}"))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .context("bootstrap-attested response body")?;
        parse_seal_response(&body)
    }
}

// ── β2 pure contract (sync-state + projection confirmation) ───────

/// Parse the sync-state GET response. `Ok(None)` if `bootstrapped:false`.
/// Field names mirror `signerlist_handler.cpp::handleMembershipSyncState`.
fn parse_sync_state(body: &serde_json::Value) -> Result<Option<MembershipSyncState>> {
    if body["status"].as_str() != Some("ok") {
        bail!(
            "sync-state error: {}",
            body.get("code")
                .or_else(|| body.get("message"))
                .unwrap_or(body)
        );
    }
    if body["bootstrapped"] == serde_json::json!(false) {
        return Ok(None);
    }
    let authority_epoch = body["authority_epoch"]
        .as_u64()
        .context("sync-state: missing authority_epoch")?;
    let projection_confirmed_epoch = body["projection_confirmed_epoch"]
        .as_u64()
        .context("sync-state: missing projection_confirmed_epoch")?;
    let in_sync = body["in_sync"]
        .as_bool()
        .context("sync-state: missing in_sync")?;
    Ok(Some(MembershipSyncState {
        authority_epoch,
        projection_confirmed_epoch,
        in_sync,
    }))
}

/// Build the record-projection-confirmation POST body. Field names MUST match
/// `signerlist_handler.cpp::handleRecordProjectionConfirmation`.
fn build_confirmation_request(
    escrow: &[u8; 20],
    signed_xrpl_tx_blob: &[u8],
    tx_hash: &[u8; 32],
    ledger_index: u64,
) -> serde_json::Value {
    serde_json::json!({
        "escrow_account_id": hex::encode(escrow),
        "signed_xrpl_tx_blob": hex::encode(signed_xrpl_tx_blob),
        "tx_hash": hex::encode(tx_hash),
        "ledger_index": ledger_index,
    })
}

/// Parse the record-projection-confirmation POST response.
fn parse_confirmation_response(body: &serde_json::Value) -> Result<()> {
    if body["status"].as_str() == Some("ok") {
        return Ok(());
    }
    bail!(
        "record-projection-confirmation rejected: code={} message={}",
        body.get("code")
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        body.get("message").and_then(|m| m.as_str()).unwrap_or("?")
    );
}

// ── β2 HTTP adapters ─────────────────────────────────────────────

/// Reads the membership sync state from one enclave (local for the spend gate;
/// per-node for cross-node drift reconciliation).
pub struct HttpSyncStateSource {
    client: reqwest::Client,
    enclave_base: String,
}

impl HttpSyncStateSource {
    pub fn new(client: reqwest::Client, enclave_base: String) -> Self {
        Self {
            client,
            enclave_base,
        }
    }
}

#[async_trait]
impl SyncStateSource for HttpSyncStateSource {
    async fn sync_state(&self) -> Result<Option<MembershipSyncState>> {
        let url = format!(
            "{}{}",
            self.enclave_base.trim_end_matches('/'),
            SYNC_STATE_PATH
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET sync-state from enclave")?;
        let body: serde_json::Value = resp.json().await.context("sync-state response body")?;
        parse_sync_state(&body)
    }
}

/// Records the on-chain projection confirmation on one node's enclave.
pub struct HttpProjectionConfirmer {
    client: reqwest::Client,
}

impl HttpProjectionConfirmer {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ProjectionConfirmer for HttpProjectionConfirmer {
    async fn record_confirmation(
        &self,
        node_admin_url: &str,
        escrow: &[u8; 20],
        signed_xrpl_tx_blob: &[u8],
        tx_hash: &[u8; 32],
        ledger_index: u64,
    ) -> Result<()> {
        let url = format!(
            "{}{}",
            node_admin_url.trim_end_matches('/'),
            RECORD_CONFIRMATION_PATH
        );
        let req = build_confirmation_request(escrow, signed_xrpl_tx_blob, tx_hash, ledger_index);
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST record-projection-confirmation to {node_admin_url}"))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .context("record-projection-confirmation response body")?;
        parse_confirmation_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership_canonical::SignerEntry;
    use crate::membership_coordinator::prepare_statement;

    fn entry(fill: u8, weight: u32) -> SignerEntry {
        SignerEntry {
            account_id: [fill; 20],
            weight,
        }
    }

    fn sample_statement() -> MembershipEpochStatement {
        prepare_statement(
            [0xAA; 20],
            4,
            [0xBB; 32],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
        )
        .expect("valid")
    }

    /// β4 Thread A genesis: the bootstrap-attested body must (a) use the enclave
    /// handler's field names, (b) carry the ATTESTING set identical to the
    /// AUTHORITY set, and (c) seal already in-sync. (b)+(c) are derived, never
    /// caller-supplied, so a mismatch is unrepresentable on the wire.
    #[test]
    fn bootstrap_attested_request_is_self_attesting_and_in_sync() {
        let st = prepare_statement([0xAA; 20], 0, [0u8; 32], vec![entry(0x01, 1)], 1).expect("ok");
        let body = build_bootstrap_attested_request(&st, &[0xBE, 0xEF]);

        assert_eq!(body["escrow_account_id"], "aa".repeat(20));
        assert_eq!(body["prev_epoch_hash"], "00".repeat(32));
        assert_eq!(body["quorum_bundle"], "beef");
        assert_eq!(body["authority_epoch"], 1);
        // in-sync at genesis: there is no prior projection to catch up to
        assert_eq!(body["confirmed_epoch"], body["authority_epoch"]);
        // self-attesting: the founding members attest their own founding epoch
        assert_eq!(body["attesting_signers"], body["authority_signers"]);
        assert_eq!(body["attesting_quorum"], body["authority_quorum"]);
    }

    #[test]
    fn seal_request_matches_enclave_field_names() {
        let st = sample_statement();
        let body = build_seal_request(&st, &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(body["escrow_account_id"], "aa".repeat(20));
        assert_eq!(body["prev_epoch_hash"], "bb".repeat(32));
        assert_eq!(body["quorum_bundle"], "deadbeef");
        assert_eq!(body["quorum_threshold"], 2);
        assert_eq!(body["proposed_epoch"], 5);
        let signers = body["signers"].as_array().unwrap();
        assert_eq!(signers.len(), 2);
        assert_eq!(signers[0]["account_id"], "01".repeat(20));
        assert_eq!(signers[0]["weight"], 1);
        assert_eq!(signers[1]["weight"], 2);
    }

    #[test]
    fn parse_epoch_digest_ok() {
        let body = serde_json::json!({
            "status": "ok",
            "bootstrapped": true,
            "epoch": 7,
            "epoch_digest": "cc".repeat(32),
        });
        let (epoch, digest) = parse_epoch_digest(&body).unwrap();
        assert_eq!(epoch, 7);
        assert_eq!(digest, [0xCC; 32]);
    }

    #[test]
    fn parse_epoch_digest_rejects_unbootstrapped() {
        let body = serde_json::json!({"status": "ok", "bootstrapped": false});
        let err = parse_epoch_digest(&body).unwrap_err().to_string();
        assert!(err.contains("genesis bootstrap required"), "got: {err}");
    }

    #[test]
    fn parse_epoch_digest_rejects_bad_length() {
        let body = serde_json::json!({
            "status": "ok",
            "epoch": 1,
            "epoch_digest": "cc".repeat(16), // 16 bytes, not 32
        });
        assert!(parse_epoch_digest(&body).is_err());
    }

    #[test]
    fn parse_epoch_digest_rejects_error_status() {
        let body = serde_json::json!({"status": "error", "code": 500});
        assert!(parse_epoch_digest(&body).is_err());
    }

    #[test]
    fn parse_seal_response_ok_and_error() {
        assert!(parse_seal_response(&serde_json::json!({"status": "ok", "epoch": 5})).is_ok());
        let err = parse_seal_response(&serde_json::json!({
            "status": "error",
            "code": -18,
            "message": "seal_membership_epoch failed",
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("-18"), "got: {err}");
        assert!(err.contains("seal_membership_epoch failed"), "got: {err}");
    }

    // ── β2 sync-state + projection confirmation contract ─────────

    #[test]
    fn parse_sync_state_ok() {
        let body = serde_json::json!({
            "status": "ok",
            "bootstrapped": true,
            "authority_epoch": 6,
            "projection_confirmed_epoch": 5,
            "in_sync": false,
        });
        let st = parse_sync_state(&body).unwrap().unwrap();
        assert_eq!(st.authority_epoch, 6);
        assert_eq!(st.projection_confirmed_epoch, 5);
        assert!(!st.in_sync);
    }

    #[test]
    fn parse_sync_state_unbootstrapped_is_none() {
        let body = serde_json::json!({"status": "ok", "bootstrapped": false});
        assert!(parse_sync_state(&body).unwrap().is_none());
    }

    #[test]
    fn parse_sync_state_error() {
        let body = serde_json::json!({"status": "error", "code": 500});
        assert!(parse_sync_state(&body).is_err());
    }

    #[test]
    fn confirmation_request_matches_enclave_field_names() {
        let body = build_confirmation_request(&[0xAA; 20], &[0xDE, 0xAD], &[0xBB; 32], 9_001);
        assert_eq!(body["escrow_account_id"], "aa".repeat(20));
        assert_eq!(body["signed_xrpl_tx_blob"], "dead");
        assert_eq!(body["tx_hash"], "bb".repeat(32));
        assert_eq!(body["ledger_index"], 9_001);
    }

    #[test]
    fn parse_confirmation_ok_and_error() {
        assert!(parse_confirmation_response(
            &serde_json::json!({"status": "ok", "ledger_index": 9001})
        )
        .is_ok());
        let err = parse_confirmation_response(&serde_json::json!({
            "status": "error",
            "code": -21,
            "message": "record_projection_confirmation failed",
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("-21"), "got: {err}");
    }
}
