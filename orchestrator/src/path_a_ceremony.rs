// Allowed because the `pub` API of this module is consumed by an
// admin endpoint that lands in a follow-on commit alongside the
// C++ REST endpoints in EthSignerEnclave/server/server.cpp. Until
// those endpoints exist, the production driver has no caller and
// every public item appears dead. Tests in this file exercise the
// full state machine, so the logic IS covered — the lint just
// can't see consumers from cargo check on the binary target.
#![allow(dead_code)]

//! REQ-8 Path A migration ceremony driver — orchestrator state machine.
//!
//! Runs on the operator's machine after `node-deploy --side-by-side`
//! has brought NEW enclave alive on port 9089 alongside OLD on 9088.
//! Drives the six-step ceremony defined in `docs/audit/REQ-7.md` §3.3:
//!
//!   1. Quorum agrees on MRENCLAVE_new (out-of-band — pre-condition).
//!   2. NEW enclave running side-by-side (pre-condition).
//!   3. NEW generates ephemeral migration keypair + LA report.
//!   4. OLD verifies + delegation quorum + encrypts state + emits LA report.
//!   5. NEW decrypts state + re-seals + writes M4 manifest LAST + emits
//!      completion LA report.
//!   6. OLD verifies completion → seals retired-marker + flips flag.
//!
//! The driver is **abstract over the enclave HTTP client** via the
//! `EnclaveApi` trait. Production uses a real HTTPS client targeting
//! the C++ server's `/v1/path-a/*` REST endpoints (those endpoints
//! land in a follow-on integration commit alongside on-hardware E2E
//! tests). Tests use a deterministic mock — see the `tests` module
//! at the bottom of this file.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

/// Inputs the operator (or higher-level admin endpoint) gives the
/// ceremony driver.
#[derive(Debug, Clone)]
pub struct CeremonyParams {
    /// Hex-encoded 32-byte MRENCLAVE the operator quorum agreed on.
    pub expected_mrenclave_new: String,
    /// Base URL of OLD's enclave REST API (typically https://localhost:9088).
    pub old_api_base: String,
    /// Base URL of NEW's enclave REST API (typically https://localhost:9089).
    pub new_api_base: String,
}

/// Outputs the driver returns on successful ceremony completion.
#[derive(Debug, Clone)]
pub struct CeremonySuccess {
    pub ceremony_nonce_hex: String,
    pub mrenclave_new_hex: String,
    /// Hex-encoded 32-byte SHA-256 of the MigrationPayload plaintext.
    /// Useful in operator logs for cross-correlation against OLD's
    /// retired-marker forensic record.
    pub manifest_hash_hex: String,
}

/// Phase the ceremony is currently in. Used for operator visibility
/// (status endpoint, logs) and for resuming after an orchestrator
/// process restart (out of scope for this commit — restart aborts
/// the ceremony; operator retries with fresh ceremony_nonce).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeremonyState {
    /// Initial — no ecall has been issued yet.
    Idle,
    /// Asked NEW for its target_info + (independently) generated a
    /// ceremony_nonce.
    PreparingHandshake,
    /// Asked NEW to generate the migration keypair (step 3); awaiting
    /// peer_pk_compressed + la_report_of_new.
    KeypairRequested,
    /// Collecting M-of-N delegation signatures from operator quorum
    /// via libp2p signing-relay (step 1 in spec; orchestrator calls
    /// it after step 3 because we need ceremony_nonce in the message).
    CollectingDelegation,
    /// Asked OLD to export state (step 4); awaiting ciphertext + tag
    /// + ephemeral_pk + la_report_of_old.
    ExportRequested,
    /// Asked NEW to import state (step 5); awaiting completion report.
    ImportRequested,
    /// Asked OLD to verify import confirmation (step 6); awaiting
    /// retired-flag flip.
    ConfirmationRequested,
    /// Ceremony finished successfully. OLD is retired (in-memory +
    /// sealed marker); NEW is the active enclave. Operator should
    /// run promotion sequence per docs/path-a-runbook.
    Succeeded,
    /// Ceremony aborted. Distinct from Idle so an admin-status endpoint
    /// can distinguish «never started» from «started and rolled back».
    Failed,
}

/// HTTP-API surface the driver uses to talk to OLD + NEW enclaves
/// + the libp2p signing-relay. Trait-abstracted so tests can mock
/// without spinning up real SGX enclaves. Production implementation
/// (lands with the C++ REST endpoints commit) wraps reqwest +
/// existing libp2p signer.
///
/// `export_state` and `import_state` each take ~8 arguments because
/// the underlying ecalls do — they're a verbatim mirror of the EDL
/// signatures, packed into a Rust trait. Bundling args into a struct
/// would obscure that mapping; we accept the lint exception in the
/// name of trait/EDL parity.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait EnclaveApi: Send + Sync {
    /// GET /v1/path-a/target-info on the given enclave base. Returns
    /// 512-byte sgx_target_info_t.
    async fn get_target_info(&self, base: &str) -> Result<Vec<u8>>;

    /// POST /v1/path-a/generate-keypair on NEW. Returns
    /// (peer_pk_compressed [33], la_report_of_new [432]).
    async fn generate_keypair(
        &self,
        new_base: &str,
        expected_mrenclave_old: &[u8; 32],
        ceremony_nonce: &[u8; 32],
        target_info_of_old: &[u8; 512],
    ) -> Result<(Vec<u8>, Vec<u8>)>;

    /// Collect M-of-N delegation signatures via libp2p signing-relay.
    /// Returns the wire-format delegation_bundle bytes (v1 layout per
    /// REQ-7 amendment 2026-05-07 (b)).
    async fn collect_delegation_bundle(
        &self,
        mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
    ) -> Result<Vec<u8>>;

    /// POST /v1/path-a/export-state on OLD. Returns
    /// (ciphertext_blob, ephemeral_pk [33], tag [16], la_report_old [432]).
    /// The ciphertext_blob is the wire format defined in REQ-7
    /// amendment (b): manifest_hash(32) || iv(12) || gcm_ct(N).
    async fn export_state(
        &self,
        old_base: &str,
        target_info_of_new: &[u8; 512],
        la_report_of_new: &[u8; 432],
        expected_mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
        peer_pk_compressed: &[u8; 33],
        delegation_bundle: &[u8],
    ) -> Result<ExportResult>;

    /// POST /v1/path-a/import-state on NEW. Returns the completion
    /// LA report (432 bytes).
    async fn import_state(
        &self,
        new_base: &str,
        target_info_of_old: &[u8; 512],
        la_report_of_old: &[u8; 432],
        ceremony_nonce: &[u8; 32],
        ciphertext: &[u8],
        ephemeral_pk: &[u8; 33],
        tag: &[u8; 16],
    ) -> Result<Vec<u8>>;

    /// POST /v1/path-a/verify-confirmation on OLD. Returns Ok(()) on
    /// success; Err on any verification failure.
    async fn verify_import_confirmation(
        &self,
        old_base: &str,
        completion_la_report: &[u8; 432],
        expected_blob_hash: &[u8; 32],
        expected_ceremony_nonce: &[u8; 32],
        expected_manifest_hash: &[u8; 32],
    ) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub ciphertext: Vec<u8>,
    pub ephemeral_pk: [u8; 33],
    pub tag: [u8; 16],
    pub la_report_old: [u8; 432],
}

pub struct CeremonyDriver<A: EnclaveApi> {
    api: A,
    state: CeremonyState,
}

impl<A: EnclaveApi> CeremonyDriver<A> {
    pub fn new(api: A) -> Self {
        Self {
            api,
            state: CeremonyState::Idle,
        }
    }

    pub fn state(&self) -> CeremonyState {
        self.state
    }

    /// Drive the ceremony to completion. Each phase transitions
    /// `self.state` so an outside observer (admin status endpoint,
    /// logger) can see progress. Failure transitions to Failed and
    /// returns the underlying error — the operator must restart with
    /// a fresh nonce per §3.6 since recent_ceremony_nonces marks
    /// this nonce as consumed.
    pub async fn run(&mut self, params: &CeremonyParams) -> Result<CeremonySuccess> {
        let result = self.run_inner(params).await;
        if result.is_err() {
            self.state = CeremonyState::Failed;
        }
        result
    }

    async fn run_inner(&mut self, params: &CeremonyParams) -> Result<CeremonySuccess> {
        self.state = CeremonyState::PreparingHandshake;

        // Decode operator-supplied MRENCLAVE_new.
        let mrenclave_new = decode_hex_32(&params.expected_mrenclave_new)
            .ok_or_else(|| anyhow!("expected_mrenclave_new not 64-char hex"))?;

        // Generate fresh ceremony_nonce (32 random bytes). The OLD
        // enclave's recent_ceremony_nonces set guarantees freshness
        // per §3.6 amendment.
        let ceremony_nonce = generate_ceremony_nonce();

        // Get OLD's target_info — NEW packs report addressed to OLD.
        let target_info_of_old_vec = self.api.get_target_info(&params.old_api_base).await?;
        let target_info_of_old = vec_to_array_512(target_info_of_old_vec)?;

        // Get NEW's target_info — OLD will pack its outgoing LA report
        // (§3.4 c amendment) addressed to NEW.
        let target_info_of_new_vec = self.api.get_target_info(&params.new_api_base).await?;
        let target_info_of_new = vec_to_array_512(target_info_of_new_vec)?;

        // OLD's MRENCLAVE — for NEW's expected_mrenclave_old check at
        // step 3. Derive from OLD's target_info bytes.
        let mrenclave_old = mrenclave_from_target_info(&target_info_of_old);

        // Step 3: NEW generates migration keypair.
        self.state = CeremonyState::KeypairRequested;
        let (peer_pk_vec, la_report_of_new_vec) = self
            .api
            .generate_keypair(
                &params.new_api_base,
                &mrenclave_old,
                &ceremony_nonce,
                &target_info_of_old,
            )
            .await?;
        let peer_pk = vec_to_array_33(peer_pk_vec)?;
        let la_report_of_new = vec_to_array_432(la_report_of_new_vec)?;

        // Step 1 (deferred to here so we have ceremony_nonce):
        // collect delegation signatures from operator quorum.
        self.state = CeremonyState::CollectingDelegation;
        let delegation_bundle = self
            .api
            .collect_delegation_bundle(&mrenclave_new, &ceremony_nonce)
            .await?;

        // Step 4: OLD exports state.
        self.state = CeremonyState::ExportRequested;
        let export = self
            .api
            .export_state(
                &params.old_api_base,
                &target_info_of_new,
                &la_report_of_new,
                &mrenclave_new,
                &ceremony_nonce,
                &peer_pk,
                &delegation_bundle,
            )
            .await?;

        // The ciphertext blob carries manifest_hash as its first 32
        // bytes (per amendment (b) wire layout). Driver doesn't need
        // to inspect it cryptographically — the enclaves do — but we
        // capture it for the confirmation handshake at step 6 +
        // operator log forensics.
        if export.ciphertext.len() < 44 {
            return Err(anyhow!(
                "OLD export returned ciphertext shorter than wire-prefix (44 bytes); \
                 got {} bytes",
                export.ciphertext.len()
            ));
        }
        let mut manifest_hash = [0u8; 32];
        manifest_hash.copy_from_slice(&export.ciphertext[..32]);

        // Compute SHA-256 of the wire ciphertext blob. OLD captured the
        // same hash at end-of-export (g_pending_migration_old.ciphertext_hash);
        // step 6 verifies our value matches.
        let blob_hash = sha256(&export.ciphertext);

        // Step 5: NEW imports state.
        self.state = CeremonyState::ImportRequested;
        let completion_la_report_vec = self
            .api
            .import_state(
                &params.new_api_base,
                &target_info_of_old,
                &export.la_report_old,
                &ceremony_nonce,
                &export.ciphertext,
                &export.ephemeral_pk,
                &export.tag,
            )
            .await?;
        let completion_la_report = vec_to_array_432(completion_la_report_vec)?;

        // Step 6: OLD verifies completion + transitions to retired.
        self.state = CeremonyState::ConfirmationRequested;
        self.api
            .verify_import_confirmation(
                &params.old_api_base,
                &completion_la_report,
                &blob_hash,
                &ceremony_nonce,
                &manifest_hash,
            )
            .await?;

        self.state = CeremonyState::Succeeded;
        Ok(CeremonySuccess {
            ceremony_nonce_hex: hex_encode(&ceremony_nonce),
            mrenclave_new_hex: hex_encode(&mrenclave_new),
            manifest_hash_hex: hex_encode(&manifest_hash),
        })
    }
}

// ── helper functions ───────────────────────────────────────────────

fn decode_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = hex::decode(s).ok()?;
    let mut out = [0u8; 32];
    if bytes.len() != 32 {
        return None;
    }
    out.copy_from_slice(&bytes);
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

fn generate_ceremony_nonce() -> [u8; 32] {
    use rand::RngCore;
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let r = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&r);
    out
}

fn vec_to_array_33(v: Vec<u8>) -> Result<[u8; 33]> {
    if v.len() != 33 {
        return Err(anyhow!("expected 33 bytes, got {}", v.len()));
    }
    let mut out = [0u8; 33];
    out.copy_from_slice(&v);
    Ok(out)
}

fn vec_to_array_432(v: Vec<u8>) -> Result<[u8; 432]> {
    if v.len() != 432 {
        return Err(anyhow!(
            "expected 432 bytes (sgx_report_t), got {}",
            v.len()
        ));
    }
    let mut out = [0u8; 432];
    out.copy_from_slice(&v);
    Ok(out)
}

fn vec_to_array_512(v: Vec<u8>) -> Result<[u8; 512]> {
    if v.len() != 512 {
        return Err(anyhow!(
            "expected 512 bytes (sgx_target_info_t), got {}",
            v.len()
        ));
    }
    let mut out = [0u8; 512];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Extract MRENCLAVE from sgx_target_info_t. Per SGX SDK layout, the
/// 32-byte mr_enclave field is at offset 0 of sgx_target_info_t. This
/// is stable across SDK versions (the struct is part of the EREPORT
/// contract).
fn mrenclave_from_target_info(target_info: &[u8; 512]) -> [u8; 32] {
    let mut mr = [0u8; 32];
    mr.copy_from_slice(&target_info[..32]);
    mr
}

// ── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mock EnclaveApi that records every call + returns
    /// configurable responses. Lets us assert the driver issued
    /// the right call sequence with the right arguments.
    ///
    /// Default isn't derived because Rust's std blanket-impl for
    /// `[T; N]: Default` stops at N=32. We've got 33-byte and
    /// 432-byte and 512-byte arrays; happy_path_mock() below is the
    /// canonical constructor.
    #[derive(Clone)]
    struct MockApi {
        calls: Arc<Mutex<Vec<String>>>,
        old_target_info: [u8; 512],
        new_target_info: [u8; 512],
        peer_pk: [u8; 33],
        la_report_new: [u8; 432],
        la_report_old: [u8; 432],
        completion_report: [u8; 432],
        delegation_bundle: Vec<u8>,
        ciphertext: Vec<u8>,
        tag: [u8; 16],
        ephemeral_pk: [u8; 33],
        // When set, these injection points cause the next matching
        // call to return Err — exercises the driver's failure path.
        fail_on_step: Option<&'static str>,
    }

    fn record(calls: &Arc<Mutex<Vec<String>>>, step: &str) {
        calls.lock().unwrap().push(step.to_string());
    }

    fn maybe_fail(fail: Option<&'static str>, step: &str) -> Result<()> {
        if fail == Some(step) {
            Err(anyhow!("mock injection: fail at {step}"))
        } else {
            Ok(())
        }
    }

    #[async_trait]
    impl EnclaveApi for MockApi {
        async fn get_target_info(&self, base: &str) -> Result<Vec<u8>> {
            record(&self.calls, &format!("get_target_info:{base}"));
            maybe_fail(self.fail_on_step, "get_target_info")?;
            if base.contains("9088") {
                Ok(self.old_target_info.to_vec())
            } else {
                Ok(self.new_target_info.to_vec())
            }
        }

        async fn generate_keypair(
            &self,
            _new_base: &str,
            _expected_mrenclave_old: &[u8; 32],
            _ceremony_nonce: &[u8; 32],
            _target_info_of_old: &[u8; 512],
        ) -> Result<(Vec<u8>, Vec<u8>)> {
            record(&self.calls, "generate_keypair");
            maybe_fail(self.fail_on_step, "generate_keypair")?;
            Ok((self.peer_pk.to_vec(), self.la_report_new.to_vec()))
        }

        async fn collect_delegation_bundle(
            &self,
            _mrenclave_new: &[u8; 32],
            _ceremony_nonce: &[u8; 32],
        ) -> Result<Vec<u8>> {
            record(&self.calls, "collect_delegation_bundle");
            maybe_fail(self.fail_on_step, "collect_delegation_bundle")?;
            Ok(self.delegation_bundle.clone())
        }

        async fn export_state(
            &self,
            _old_base: &str,
            _target_info_of_new: &[u8; 512],
            _la_report_of_new: &[u8; 432],
            _expected_mrenclave_new: &[u8; 32],
            _ceremony_nonce: &[u8; 32],
            _peer_pk_compressed: &[u8; 33],
            _delegation_bundle: &[u8],
        ) -> Result<ExportResult> {
            record(&self.calls, "export_state");
            maybe_fail(self.fail_on_step, "export_state")?;
            Ok(ExportResult {
                ciphertext: self.ciphertext.clone(),
                ephemeral_pk: self.ephemeral_pk,
                tag: self.tag,
                la_report_old: self.la_report_old,
            })
        }

        async fn import_state(
            &self,
            _new_base: &str,
            _target_info_of_old: &[u8; 512],
            _la_report_of_old: &[u8; 432],
            _ceremony_nonce: &[u8; 32],
            _ciphertext: &[u8],
            _ephemeral_pk: &[u8; 33],
            _tag: &[u8; 16],
        ) -> Result<Vec<u8>> {
            record(&self.calls, "import_state");
            maybe_fail(self.fail_on_step, "import_state")?;
            Ok(self.completion_report.to_vec())
        }

        async fn verify_import_confirmation(
            &self,
            _old_base: &str,
            _completion_la_report: &[u8; 432],
            _expected_blob_hash: &[u8; 32],
            _expected_ceremony_nonce: &[u8; 32],
            _expected_manifest_hash: &[u8; 32],
        ) -> Result<()> {
            record(&self.calls, "verify_import_confirmation");
            maybe_fail(self.fail_on_step, "verify_import_confirmation")?;
            Ok(())
        }
    }

    fn happy_path_mock() -> MockApi {
        // Construct a synthetic-but-valid mock state. The driver
        // doesn't validate cryptographic content of these bytes —
        // it routes them between OLD and NEW. So fixed dummy values
        // are sufficient to exercise the state-machine logic.
        let mut ciphertext = vec![0u8; 44 + 256]; // wire prefix (44) + body
                                                  // Synthetic manifest_hash in the first 32 bytes of ciphertext.
        for (i, b) in ciphertext.iter_mut().enumerate().take(32) {
            *b = (i as u8).wrapping_mul(7);
        }
        // Synthetic IV in next 12 bytes.
        for (offset, b) in ciphertext.iter_mut().enumerate().skip(32).take(12) {
            *b = (offset as u8).wrapping_mul(11);
        }

        MockApi {
            calls: Default::default(),
            old_target_info: [0xAA; 512],
            new_target_info: [0xBB; 512],
            peer_pk: [0xCC; 33],
            la_report_new: [0xDD; 432],
            la_report_old: [0xEE; 432],
            completion_report: [0xFF; 432],
            delegation_bundle: vec![0x11; 128],
            ciphertext,
            tag: [0x22; 16],
            ephemeral_pk: [0x33; 33],
            fail_on_step: None,
        }
    }

    fn happy_params() -> CeremonyParams {
        CeremonyParams {
            expected_mrenclave_new: "00".repeat(32),
            old_api_base: "https://localhost:9088".into(),
            new_api_base: "https://localhost:9089".into(),
        }
    }

    #[tokio::test]
    async fn happy_path_runs_six_phases_in_order() {
        let mock = happy_path_mock();
        let calls = mock.calls.clone();
        let mut driver = CeremonyDriver::new(mock);
        let result = driver.run(&happy_params()).await.unwrap();

        assert_eq!(driver.state(), CeremonyState::Succeeded);
        assert_eq!(result.mrenclave_new_hex, "00".repeat(32));

        let calls = calls.lock().unwrap().clone();
        // Expected call sequence:
        //   get_target_info (OLD), get_target_info (NEW),
        //   generate_keypair (NEW), collect_delegation_bundle,
        //   export_state (OLD), import_state (NEW),
        //   verify_import_confirmation (OLD)
        assert_eq!(calls.len(), 7, "got call sequence: {calls:?}");
        assert!(calls[0].starts_with("get_target_info:https://localhost:9088"));
        assert!(calls[1].starts_with("get_target_info:https://localhost:9089"));
        assert_eq!(calls[2], "generate_keypair");
        assert_eq!(calls[3], "collect_delegation_bundle");
        assert_eq!(calls[4], "export_state");
        assert_eq!(calls[5], "import_state");
        assert_eq!(calls[6], "verify_import_confirmation");
    }

    #[tokio::test]
    async fn invalid_mrenclave_hex_rejects_fast() {
        let mock = happy_path_mock();
        let calls = mock.calls.clone();
        let mut driver = CeremonyDriver::new(mock);
        let bad = CeremonyParams {
            expected_mrenclave_new: "not-hex".into(),
            ..happy_params()
        };
        let err = driver.run(&bad).await.unwrap_err();
        assert!(err.to_string().contains("expected_mrenclave_new"));
        assert_eq!(driver.state(), CeremonyState::Failed);
        // Should reject before issuing any API call.
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn export_failure_aborts_at_export_phase() {
        let mut mock = happy_path_mock();
        mock.fail_on_step = Some("export_state");
        let mut driver = CeremonyDriver::new(mock);
        let err = driver.run(&happy_params()).await.unwrap_err();
        assert!(err.to_string().contains("fail at export_state"));
        assert_eq!(driver.state(), CeremonyState::Failed);
    }

    #[tokio::test]
    async fn import_failure_does_not_call_verify_confirmation() {
        let mock_inner = MockApi {
            fail_on_step: Some("import_state"),
            ..happy_path_mock()
        };
        let calls = mock_inner.calls.clone();
        let mut driver = CeremonyDriver::new(mock_inner);
        let _ = driver.run(&happy_params()).await;
        let calls = calls.lock().unwrap().clone();
        // OLD's verify_import_confirmation must NOT fire if NEW's
        // import failed — the spec invariant is OLD only zeroises
        // on receipt of a valid completion report. The driver must
        // surface the import error without forwarding to OLD.
        assert!(!calls.iter().any(|c| c == "verify_import_confirmation"));
        assert_eq!(driver.state(), CeremonyState::Failed);
    }

    #[tokio::test]
    async fn confirmation_failure_leaves_old_unscrubbed() {
        // From OLD's perspective: a failed verify_import_confirmation
        // (e.g., manifest_hash mismatch) means OLD does NOT seal the
        // retired-marker, does NOT flip the in-memory flag, and stays
        // fully operational. The 5-min timeout per §3.5 M2 lets the
        // operator retry. From the driver's perspective: surface the
        // error without retrying — the recent_nonces set has already
        // consumed this nonce on both sides.
        let mut mock = happy_path_mock();
        mock.fail_on_step = Some("verify_import_confirmation");
        let mut driver = CeremonyDriver::new(mock);
        let err = driver.run(&happy_params()).await.unwrap_err();
        assert!(err.to_string().contains("verify_import_confirmation"));
        assert_eq!(driver.state(), CeremonyState::Failed);
    }

    #[tokio::test]
    async fn ciphertext_too_short_caught_before_import() {
        let mut mock = happy_path_mock();
        mock.ciphertext = vec![0u8; 30]; // < 44-byte wire prefix
        let mut driver = CeremonyDriver::new(mock);
        let err = driver.run(&happy_params()).await.unwrap_err();
        assert!(err.to_string().contains("wire-prefix"));
        assert_eq!(driver.state(), CeremonyState::Failed);
    }

    #[test]
    fn ceremony_nonce_is_fresh_each_call() {
        let n1 = generate_ceremony_nonce();
        let n2 = generate_ceremony_nonce();
        assert_ne!(
            n1, n2,
            "rng collision in 32 bytes is astronomically unlikely"
        );
    }

    #[test]
    fn mrenclave_extraction_picks_first_32_bytes_of_target_info() {
        let mut ti = [0u8; 512];
        for (i, b) in ti.iter_mut().enumerate().take(32) {
            *b = i as u8;
        }
        let mr = mrenclave_from_target_info(&ti);
        assert_eq!(mr[0], 0);
        assert_eq!(mr[31], 31);
    }
}
