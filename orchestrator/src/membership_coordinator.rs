//! β1 (perp β-retrofit) single-bundle membership-epoch coordination —
//! orchestrator side. This module owns the ceremony **decision logic**: given
//! the current sealed epoch (read from the enclave) and a requested new signer
//! set, it produces the single `MembershipEpochStatement` the off-chain quorum
//! is asked to authorise.
//!
//! The (P) single-successor enforcer (REQ-β1.1 §1): exactly ONE statement is
//! prepared, ONE quorum bundle is collected over its `message_hash` via the
//! libp2p signing-relay (the same relay as Path-A delegation + withdrawals),
//! and the SAME `(statement, bundle)` is broadcast to every node, each of which
//! calls `ecall_seal_membership_epoch`. The per-node monotonic-epoch guard then
//! rejects any second/different epoch N+1 (the in-enclave half of (P)).
//!
//! Scope of THIS module: the pure, deterministic statement preparation +
//! validation (mirrors the enclave's pre-seal sanity so a doomed ceremony is
//! rejected before any signatures are collected). The libp2p relay collection,
//! the broadcast, and the per-node `ecall_seal_membership_epoch` HTTP call are
//! wired separately (they reuse the delegation-bundle relay + a new enclave
//! admin route). Deploy/cutover is β3.

#![allow(dead_code)] // ceremony wiring (relay + enclave call) lands next increment

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::membership_canonical::{compute_membership_message_hash, compute_set_hash, SignerEntry};
use crate::p2p::{MembershipEpochRelay, SigningMessage};

const MAX_SIGNERS: usize = 32; // XRPL SignerList limit (mirrors the enclave)

/// The fully-resolved membership transition presented to the quorum. Every
/// field except `message_hash` is an input to `ecall_seal_membership_epoch`;
/// `message_hash` is what the quorum signs and what the bundle is collected
/// over (the enclave recomputes it from the other fields and must agree).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipEpochStatement {
    pub escrow: [u8; 20],
    pub proposed_epoch: u64,
    pub prev_epoch_hash: [u8; 32],
    pub new_signers: Vec<SignerEntry>,
    pub new_quorum: u32,
    pub new_set_hash: [u8; 32],
    pub message_hash: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrepareError {
    EmptySet,
    TooManySigners,
    ZeroWeight,
    QuorumOutOfBounds,
}

/// Prepare the single membership-transition statement. `proposed_epoch =
/// current_epoch + 1`; `prev_epoch_hash = current_epoch_digest` (the hash-chain
/// link the enclave will re-derive and check against its CURRENT sealed epoch).
/// Pure + deterministic — the basis of the (P) single statement. Validation
/// mirrors the enclave's pre-seal quorum sanity so a doomed ceremony never
/// collects signatures.
pub fn prepare_statement(
    escrow: [u8; 20],
    current_epoch: u64,
    current_epoch_digest: [u8; 32],
    new_signers: Vec<SignerEntry>,
    new_quorum: u32,
) -> Result<MembershipEpochStatement, PrepareError> {
    if new_signers.is_empty() {
        return Err(PrepareError::EmptySet);
    }
    if new_signers.len() > MAX_SIGNERS {
        return Err(PrepareError::TooManySigners);
    }
    let mut weight_sum: u64 = 0;
    for s in &new_signers {
        if s.weight == 0 {
            return Err(PrepareError::ZeroWeight);
        }
        weight_sum += s.weight as u64;
    }
    if new_quorum == 0 || (new_quorum as u64) > weight_sum {
        return Err(PrepareError::QuorumOutOfBounds);
    }

    let proposed_epoch = current_epoch + 1;
    let prev_epoch_hash = current_epoch_digest;
    let new_set_hash = compute_set_hash(&new_signers, new_quorum);
    let message_hash =
        compute_membership_message_hash(&escrow, proposed_epoch, &prev_epoch_hash, &new_set_hash);

    Ok(MembershipEpochStatement {
        escrow,
        proposed_epoch,
        prev_epoch_hash,
        new_signers,
        new_quorum,
        new_set_hash,
        message_hash,
    })
}

// ── quorum-bundle codec ──────────────────────────────────────────
//
// The collected consent bundle is the SAME wire format the enclave's
// `seal_verify_quorum_bundle` (path_a.cpp) consumes — shared with Path-A
// delegation. Kept self-contained here (not coupled to path_a_delegation.rs)
// and pinned by a frozen golden-bytes test; the enclave verifier is the single
// source of truth that both Rust encoders must match.

#[derive(Debug)]
struct QuorumEntry {
    pk: Vec<u8>,  // 33-byte compressed secp256k1
    sig: Vec<u8>, // ECDSA-DER, 8..=72 bytes
}

/// Extract a `(pk, sig)` entry from a peer's signing `Response`, or `None` if
/// it errored or is structurally malformed (so a bad gossipsub message can't
/// corrupt the bundle). Signature/membership validity is the enclave's job.
fn parse_signing_response(msg: &SigningMessage) -> Option<QuorumEntry> {
    let SigningMessage::Response {
        der_signature,
        compressed_pubkey,
        error,
        ..
    } = msg
    else {
        return None;
    };
    if error.is_some() {
        return None;
    }
    let sig = hex::decode(der_signature.as_ref()?).ok()?;
    let pk = hex::decode(compressed_pubkey.as_ref()?).ok()?;
    if pk.len() != 33 || sig.len() < 8 || sig.len() > 72 {
        return None;
    }
    Some(QuorumEntry { pk, sig })
}

/// Quorum-bundle wire format (LE integers):
///   u32 version = 1
///   u32 entry_count
///   for each: pk_compressed[33] || u8 sig_len(8..=72) || sig[sig_len]
fn build_quorum_bundle(entries: &[QuorumEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&e.pk);
        out.push(e.sig.len() as u8);
        out.extend_from_slice(&e.sig);
    }
    out
}

// ── collector: gather off-chain consent over the libp2p relay ────

/// Collects the single off-chain quorum bundle authorising a prepared
/// `MembershipEpochStatement`. Trait so the driver can be unit-tested with a
/// mock (same posture as Path-A's `DelegationCollector`).
#[async_trait]
pub trait MembershipBundleCollector: Send + Sync {
    async fn collect(&self, statement: &MembershipEpochStatement) -> Result<Vec<u8>>;
}

/// Production collector: pushes one `MembershipEpochRelay` to the p2p run-loop
/// and accumulates `Response`s (each operator's pool signature over the
/// statement's `message_hash`) until quorum-many distinct signers reply or the
/// window closes. Mirrors `LibP2PDelegationCollector`.
pub struct LibP2PMembershipCollector {
    relay_tx: mpsc::Sender<MembershipEpochRelay>,
    timeout: Duration,
}

impl LibP2PMembershipCollector {
    /// 30 s default window — operators have already coordinated the change
    /// out-of-band, so wall-clock is gossipsub propagation + one ecall.
    pub fn new(relay_tx: mpsc::Sender<MembershipEpochRelay>) -> Self {
        Self {
            relay_tx,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

#[async_trait]
impl MembershipBundleCollector for LibP2PMembershipCollector {
    async fn collect(&self, statement: &MembershipEpochStatement) -> Result<Vec<u8>> {
        let request_id = format!("beta1-membership-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = mpsc::channel(32);

        let relay = MembershipEpochRelay {
            request_id,
            escrow: statement.escrow,
            proposed_epoch: statement.proposed_epoch,
            prev_epoch_hash: statement.prev_epoch_hash,
            new_signers: statement.new_signers.clone(),
            new_quorum: statement.new_quorum,
            responses_tx,
        };
        self.relay_tx
            .send(relay)
            .await
            .context("send MembershipEpochRelay to p2p run-loop")?;

        let mut entries: Vec<QuorumEntry> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => break, // sender dropped
                Err(_) => break,   // window closed
            };
            if let Some(entry) = parse_signing_response(&resp) {
                // Dedup by pk: the local self-sign + the same operator's
                // gossipsub round-trip can both land. The enclave dedups too,
                // but a clean bundle is friendlier to operator logs.
                if !entries.iter().any(|e| e.pk == entry.pk) {
                    entries.push(entry);
                }
            }
        }

        if entries.is_empty() {
            return Err(anyhow!(
                "collected zero membership-epoch consents within {:?}; \
                 check the operator quorum is online + gossipsub mesh is healthy",
                self.timeout
            ));
        }

        Ok(build_quorum_bundle(&entries))
    }
}

// ── driver: collect consent, then apply on every node ────────────

/// Reads the CURRENT sealed epoch + digest from the local enclave. The driver
/// needs both to build the next statement (`proposed_epoch = current + 1`,
/// `prev_epoch_hash = current digest`). Trait for testability.
#[async_trait]
pub trait EpochDigestSource: Send + Sync {
    async fn current_epoch(&self) -> Result<(u64, [u8; 32])>;
}

/// Applies a `(statement, bundle)` on ONE node (POST the seal-epoch admin
/// route → that node's enclave `ecall_seal_membership_epoch`, which re-derives
/// the message hash, verifies the bundle against its CURRENT epoch, and the
/// monotonic-epoch guard enforces the single-successor (P) half). Trait for
/// testability.
#[async_trait]
pub trait EpochSealSink: Send + Sync {
    async fn seal_on_node(
        &self,
        node_admin_url: &str,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<()>;
}

/// β4 Thread A genesis (RESP-β4-threadA-impl.1 option 2): seal the FOUNDING
/// epoch on one node from the β1 quorum attestation, instead of an XRPL
/// SignerListSet cosigned pre-seal by the escrow pool key — which the retired
/// bare oracle can no longer produce. Same loopback discipline as `EpochSealSink`.
#[async_trait]
pub trait GenesisBootstrapSink: Send + Sync {
    async fn bootstrap_on_node(
        &self,
        node_admin_url: &str,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<()>;
}

/// Applies a sealed `(statement, bundle)` across the WHOLE cluster in one shot,
/// returning one `NodeSealResult` per node. Under the loopback-enclave topology
/// (X-C1: the enclave admin API is never network-exposed) this is NOT a per-node
/// HTTP POST to a remote enclave — the production impl (`LibP2PMembershipApplier`)
/// broadcasts ONE `MembershipApply` over p2p and every node applies it to its
/// OWN localhost enclave + acks. Each node independently verifies the bundle and
/// enforces monotonic-epoch, so a partial apply is safe (no node adopts a
/// different set), just incomplete — the caller inspects `all_sealed()`.
#[async_trait]
pub trait ClusterSealApplier: Send + Sync {
    async fn apply_seal(
        &self,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>>;
}

/// β4 Thread A genesis: applies the FOUNDING epoch across the whole cluster in
/// one broadcast (same loopback topology as `ClusterSealApplier` — each node
/// bootstraps its OWN enclave and acks).
#[async_trait]
pub trait ClusterGenesisApplier: Send + Sync {
    async fn apply_genesis(
        &self,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>>;
}

/// Per-node application result of a membership change.
#[derive(Debug, Clone)]
pub struct NodeSealResult {
    pub node: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Outcome of one membership-change ceremony.
#[derive(Debug, Clone)]
pub struct MembershipChangeOutcome {
    pub proposed_epoch: u64,
    pub message_hash: [u8; 32],
    pub bundle_len: usize,
    pub node_results: Vec<NodeSealResult>,
    /// β4 Thread A (AC-β4-A1): the SAME β1 quorum bundle that authorised this
    /// epoch, hex-encoded. The β2 projection must forward it to each signer's
    /// enclave — the governance signing path refuses a SignerListSet without a
    /// bundle proving the cluster authorised the epoch being projected.
    pub quorum_bundle_hex: String,
}

impl MembershipChangeOutcome {
    /// True iff every node sealed the new epoch. A partial result means the
    /// cluster's read-projection of membership is now split — operator must
    /// retry the failed nodes (the in-enclave monotonic guard makes the retry
    /// idempotent: a node that already sealed N+1 rejects a second attempt).
    pub fn all_sealed(&self) -> bool {
        !self.node_results.is_empty() && self.node_results.iter().all(|r| r.ok)
    }
}

/// Run one off-chain membership change end-to-end: read the current epoch from
/// the local enclave, prepare the single successor statement (P), collect ONE
/// off-chain quorum bundle over its message hash, and apply that SAME
/// `(statement, bundle)` across the cluster via the `applier` (a single p2p
/// broadcast under the loopback topology). Each node independently verifies the
/// bundle and enforces monotonic-epoch — so a partial apply is safe (no node
/// adopts a different set), just incomplete; the caller inspects `all_sealed()`.
pub async fn run_membership_change(
    escrow: [u8; 20],
    new_signers: Vec<SignerEntry>,
    new_quorum: u32,
    digest_src: &dyn EpochDigestSource,
    collector: &dyn MembershipBundleCollector,
    applier: &dyn ClusterSealApplier,
) -> Result<MembershipChangeOutcome> {
    let (current_epoch, current_digest) = digest_src
        .current_epoch()
        .await
        .context("read current epoch digest from local enclave")?;

    let statement = prepare_statement(
        escrow,
        current_epoch,
        current_digest,
        new_signers,
        new_quorum,
    )
    .map_err(|e| anyhow!("prepare membership statement: {e:?}"))?;

    let bundle = collector
        .collect(&statement)
        .await
        .context("collect off-chain quorum consent")?;

    let node_results = applier
        .apply_seal(&statement, &bundle)
        .await
        .context("apply sealed epoch across the cluster")?;

    Ok(MembershipChangeOutcome {
        proposed_epoch: statement.proposed_epoch,
        message_hash: statement.message_hash,
        bundle_len: bundle.len(),
        node_results,
        quorum_bundle_hex: hex::encode(&bundle),
    })
}

/// β4 Thread A genesis (RESP-β4-threadA-impl.1 option 2) — found the cluster's
/// FIRST epoch without any pre-seal XRPL signature.
///
/// Since the retire, the escrow pool key cannot sign a bare hash, and the
/// governance path needs a sealed authority that does not exist yet at genesis.
/// So the founding members instead attest their own founding epoch with the β1
/// consent bundle, and each enclave seals it via
/// `ecall_bootstrap_from_quorum_attestation`.
///
/// The statement is the ordinary β1 one with `current_epoch = 0` and a zero
/// `prev_epoch_hash`, which yields `proposed_epoch = 1` and the exact message
/// hash the enclave recomputes — no genesis-specific canonical encoding exists,
/// and none should.
///
/// Honest bound (auditor §5): attesting == authority == the operator-supplied
/// set, so this is SELF-AUTHORISING — it equals 6(e)'s incoming-quorum strength
/// and no more. Epoch-1 authenticity still rests on the escrow master key and
/// the DCAP mesh, not on this bundle.
pub async fn run_genesis_bootstrap(
    escrow: [u8; 20],
    genesis_signers: Vec<SignerEntry>,
    genesis_quorum: u32,
    collector: &dyn MembershipBundleCollector,
    applier: &dyn ClusterGenesisApplier,
) -> Result<MembershipChangeOutcome> {
    let statement = prepare_statement(
        escrow,
        /* current_epoch */ 0,
        /* current_epoch_digest */ [0u8; 32],
        genesis_signers,
        genesis_quorum,
    )
    .map_err(|e| anyhow!("prepare genesis statement: {e:?}"))?;

    let bundle = collector
        .collect(&statement)
        .await
        .context("collect founding-quorum consent")?;

    let node_results = applier
        .apply_genesis(&statement, &bundle)
        .await
        .context("bootstrap the founding epoch across the cluster")?;

    Ok(MembershipChangeOutcome {
        proposed_epoch: statement.proposed_epoch,
        message_hash: statement.message_hash,
        bundle_len: bundle.len(),
        node_results,
        quorum_bundle_hex: hex::encode(&bundle),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn entry(fill: u8, weight: u32) -> SignerEntry {
        SignerEntry {
            account_id: [fill; 20],
            weight,
        }
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The statement's message_hash MUST equal the cross-language golden vector:
    /// transition current_epoch 4 → 5 adopting {A,B} quorum 2, escrow 0xAA*20,
    /// current digest = 0xBB*32. Ties the coordinator to the frozen contract.
    #[test]
    fn prepare_statement_matches_golden_message_hash() {
        let st = prepare_statement(
            [0xAA; 20],
            4,
            [0xBB; 32],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
        )
        .expect("valid");
        assert_eq!(st.proposed_epoch, 5);
        assert_eq!(st.prev_epoch_hash, [0xBB; 32]);
        assert_eq!(
            hex(&st.new_set_hash),
            "2321fc70f09b9e269683cb04b42704db324d67c37ba09e1bb5a7be277761cf5b"
        );
        assert_eq!(
            hex(&st.message_hash),
            "01ad9ce518f2e5dd4b970fd03746322621311acf1820d6d3f45d5b22f3c2f8f2"
        );
    }

    #[test]
    fn prepare_statement_rejects_bad_input() {
        let esc = [0u8; 20];
        let dig = [0u8; 32];
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![], 1),
            Err(PrepareError::EmptySet)
        );
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 0)], 1),
            Err(PrepareError::ZeroWeight)
        );
        // quorum 0 and quorum > weight sum (1+2=3)
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 1), entry(2, 2)], 0),
            Err(PrepareError::QuorumOutOfBounds)
        );
        assert_eq!(
            prepare_statement(esc, 1, dig, vec![entry(1, 1), entry(2, 2)], 4),
            Err(PrepareError::QuorumOutOfBounds)
        );
        let too_many: Vec<SignerEntry> = (0..33).map(|i| entry(i as u8, 1)).collect();
        assert_eq!(
            prepare_statement(esc, 1, dig, too_many, 1),
            Err(PrepareError::TooManySigners)
        );
    }

    // ── quorum-bundle codec ──────────────────────────────────────

    #[test]
    fn build_quorum_bundle_layout() {
        let entries = vec![QuorumEntry {
            pk: vec![0x02; 33],
            sig: vec![0x30, 0x45, 0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03],
        }];
        let b = build_quorum_bundle(&entries);
        assert_eq!(b.len(), 4 + 4 + 33 + 1 + 8);
        assert_eq!(&b[..4], &1u32.to_le_bytes()); // version
        assert_eq!(&b[4..8], &1u32.to_le_bytes()); // entry_count
        assert_eq!(&b[8..41], &[0x02; 33]); // pk
        assert_eq!(b[41], 8); // sig_len
        assert_eq!(
            &b[42..50],
            &[0x30, 0x45, 0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03]
        );
    }

    #[test]
    fn build_quorum_bundle_zero_entries() {
        let b = build_quorum_bundle(&[]);
        assert_eq!(b.len(), 8);
        assert_eq!(&b[4..8], &0u32.to_le_bytes());
    }

    fn response(sig_hex: Option<&str>, pk_hex: Option<&str>, err: Option<&str>) -> SigningMessage {
        SigningMessage::Response {
            request_id: "beta1-membership-test".into(),
            signer_xrpl_address: "rTest".into(),
            der_signature: sig_hex.map(String::from),
            compressed_pubkey: pk_hex.map(String::from),
            error: err.map(String::from),
        }
    }

    #[test]
    fn parse_signing_response_filters_malformed() {
        let pk = "02".to_string() + &"AB".repeat(32);
        let sig = "30".to_string() + &"45".repeat(69);
        assert!(parse_signing_response(&response(Some(&sig), Some(&pk), None)).is_some());
        // error response
        assert!(parse_signing_response(&response(None, None, Some("e"))).is_none());
        // wrong pk size (32 bytes)
        let pk32 = "02".to_string() + &"AB".repeat(31);
        assert!(parse_signing_response(&response(Some(&sig), Some(&pk32), None)).is_none());
        // sig too short
        assert!(parse_signing_response(&response(Some("3045"), Some(&pk), None)).is_none());
    }

    // ── collector ────────────────────────────────────────────────

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

    #[tokio::test]
    async fn collector_times_out_on_zero_responses() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PMembershipCollector::new(relay_tx).with_timeout(Duration::from_millis(100));
        let drain = tokio::spawn(async move {
            let _relay = relay_rx.recv().await;
            tokio::time::sleep(Duration::from_secs(1)).await; // hold relay alive
        });
        let err = collector
            .collect(&sample_statement())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("zero membership-epoch consents"), "got: {err}");
        drain.abort();
    }

    #[tokio::test]
    async fn collector_relay_carries_beta1_prefix_and_statement() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PMembershipCollector::new(relay_tx).with_timeout(Duration::from_millis(150));
        let st = sample_statement();
        let st2 = st.clone();
        let h = tokio::spawn(async move {
            let relay = relay_rx.recv().await.unwrap();
            assert!(relay.request_id.starts_with("beta1-membership-"));
            assert_eq!(relay.proposed_epoch, st2.proposed_epoch);
            assert_eq!(relay.escrow, st2.escrow);
            assert_eq!(relay.prev_epoch_hash, st2.prev_epoch_hash);
            assert_eq!(relay.new_quorum, st2.new_quorum);
            assert_eq!(relay.new_signers, st2.new_signers);
            // drop relay → responses_tx closes → collector returns zero-consent
        });
        let _ = collector.collect(&st).await; // errs on zero responses — fine
        h.await.unwrap();
    }

    #[tokio::test]
    async fn collector_packages_and_dedups_by_pk() {
        let (relay_tx, mut relay_rx) = mpsc::channel(1);
        let collector =
            LibP2PMembershipCollector::new(relay_tx).with_timeout(Duration::from_millis(500));
        let inject = tokio::spawn(async move {
            let relay = relay_rx.recv().await.unwrap();
            let pk = "02".to_string() + &"AB".repeat(32);
            let sig = "30".to_string() + &"45".repeat(69);
            for _ in 0..2 {
                // same pk twice → dedups to a single bundle entry
                relay
                    .responses_tx
                    .send(response(Some(&sig), Some(&pk), None))
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let bundle = collector.collect(&sample_statement()).await.unwrap();
        assert_eq!(&bundle[4..8], &1u32.to_le_bytes()); // entry_count == 1
        assert_eq!(bundle.len(), 4 + 4 + 33 + 1 + 70);
        inject.abort();
    }

    // ── driver ───────────────────────────────────────────────────

    struct MockDigest(u64, [u8; 32]);
    #[async_trait]
    impl EpochDigestSource for MockDigest {
        async fn current_epoch(&self) -> Result<(u64, [u8; 32])> {
            Ok((self.0, self.1))
        }
    }

    struct MockCollector(Vec<u8>);
    #[async_trait]
    impl MembershipBundleCollector for MockCollector {
        async fn collect(&self, _: &MembershipEpochStatement) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    /// Mock `ClusterSealApplier`: records the SINGLE (statement, bundle) it was
    /// broadcast (the (P) single-successor — one apply, not per-node) and returns
    /// a configured per-node result set. `Err(fail_call)` simulates a broadcast
    /// that never reaches the run-loop.
    struct MockApplier {
        nodes: Vec<String>,
        fail_nodes: Vec<String>,
        fail_call: bool,
        seen: tokio::sync::Mutex<Option<(u64, Vec<u8>)>>,
    }
    impl MockApplier {
        fn new(nodes: Vec<String>, fail_nodes: Vec<String>) -> Self {
            Self {
                nodes,
                fail_nodes,
                fail_call: false,
                seen: tokio::sync::Mutex::new(None),
            }
        }
        fn failing() -> Self {
            Self {
                nodes: vec![],
                fail_nodes: vec![],
                fail_call: true,
                seen: tokio::sync::Mutex::new(None),
            }
        }
    }
    #[async_trait]
    impl ClusterGenesisApplier for MockApplier {
        async fn apply_genesis(
            &self,
            st: &MembershipEpochStatement,
            bundle: &[u8],
        ) -> Result<Vec<NodeSealResult>> {
            // Same bookkeeping as apply_seal so the genesis tests can assert on
            // exactly what was broadcast.
            self.apply_seal(st, bundle).await
        }
    }

    #[async_trait]
    impl ClusterSealApplier for MockApplier {
        async fn apply_seal(
            &self,
            st: &MembershipEpochStatement,
            bundle: &[u8],
        ) -> Result<Vec<NodeSealResult>> {
            if self.fail_call {
                bail!("apply broadcast failed");
            }
            *self.seen.lock().await = Some((st.proposed_epoch, bundle.to_vec()));
            Ok(self
                .nodes
                .iter()
                .map(|n| {
                    let ok = !self.fail_nodes.iter().any(|f| f == n);
                    NodeSealResult {
                        node: n.clone(),
                        ok,
                        error: (!ok).then(|| format!("node {n} unreachable")),
                    }
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn driver_happy_path_seals_all_nodes_with_same_bundle() {
        let digest = MockDigest(4, [0xBB; 32]);
        let collector = MockCollector(vec![1, 2, 3]);
        let applier = MockApplier::new(vec!["n1".into(), "n2".into(), "n3".into()], vec![]);
        let out = run_membership_change(
            [0xAA; 20],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
            &digest,
            &collector,
            &applier,
        )
        .await
        .unwrap();

        assert_eq!(out.proposed_epoch, 5);
        // ties the driver to the same frozen cross-language message hash
        assert_eq!(
            hex(&out.message_hash),
            "01ad9ce518f2e5dd4b970fd03746322621311acf1820d6d3f45d5b22f3c2f8f2"
        );
        assert_eq!(out.bundle_len, 3);
        assert!(out.all_sealed());
        assert_eq!(out.node_results.len(), 3);

        // the cluster was broadcast the SAME (epoch, bundle) ONCE — (P).
        let seen = self_seen(&applier).await;
        assert_eq!(seen, (5, vec![1, 2, 3]));
    }

    /// β4 Thread A genesis (RESP-β4-threadA-impl.1 option 2): the founding epoch
    /// is 1 over a ZERO prev-hash, and it is broadcast once like any other apply.
    /// Genesis must reuse the ordinary β1 canonical encoding — a genesis-specific
    /// message hash would silently diverge from what the enclave recomputes.
    #[tokio::test]
    async fn genesis_driver_founds_epoch_one_over_zero_prev_hash() {
        let collector = MockCollector(vec![7, 7, 7]);
        let applier = MockApplier::new(vec!["n1".into(), "n2".into(), "n3".into()], vec![]);

        let out = run_genesis_bootstrap(
            [0xAA; 20],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
            &collector,
            &applier,
        )
        .await
        .unwrap();

        assert_eq!(out.proposed_epoch, 1, "genesis founds epoch 1");
        assert!(out.all_sealed());
        // Same canonical encoding as a normal transition: epoch 1, prev = 0.
        let expected = compute_membership_message_hash(
            &[0xAA; 20],
            1,
            &[0u8; 32],
            &compute_set_hash(&[entry(0x01, 1), entry(0x02, 2)], 2),
        );
        assert_eq!(out.message_hash, expected);
        // one broadcast, carrying the founding epoch + the collected bundle
        let seen = self_seen(&applier).await;
        assert_eq!(seen, (1, vec![7, 7, 7]));
    }

    /// A genesis set that cannot reach its own quorum must be refused BEFORE any
    /// consent is collected — the enclave would reject it anyway, but a cluster
    /// must never be founded on an unusable set.
    #[tokio::test]
    async fn genesis_driver_rejects_unreachable_quorum() {
        let collector = MockCollector(vec![1]);
        let applier = MockApplier::new(vec!["n1".into()], vec![]);
        let err = run_genesis_bootstrap(
            [0xAA; 20],
            vec![entry(0x01, 1), entry(0x02, 1)],
            99, // > sum of weights
            &collector,
            &applier,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("prepare genesis statement"),
            "got: {err}"
        );
    }

    async fn self_seen(a: &MockApplier) -> (u64, Vec<u8>) {
        a.seen.lock().await.clone().expect("apply_seal was called")
    }

    #[tokio::test]
    async fn driver_reports_partial_failure() {
        let digest = MockDigest(4, [0xBB; 32]);
        let collector = MockCollector(vec![9]);
        let applier = MockApplier::new(
            vec!["n1".into(), "n2".into(), "n3".into()],
            vec!["n2".into()],
        );
        let out = run_membership_change(
            [0xAA; 20],
            vec![entry(0x01, 1), entry(0x02, 2)],
            2,
            &digest,
            &collector,
            &applier,
        )
        .await
        .unwrap();

        assert!(!out.all_sealed());
        assert_eq!(out.node_results.iter().filter(|r| r.ok).count(), 2);
        let failed = out.node_results.iter().find(|r| !r.ok).unwrap();
        assert_eq!(failed.node, "n2");
        assert!(failed.error.as_ref().unwrap().contains("unreachable"));
    }

    #[tokio::test]
    async fn driver_propagates_apply_broadcast_failure() {
        let digest = MockDigest(4, [0xBB; 32]);
        let collector = MockCollector(vec![1]);
        let applier = MockApplier::failing();
        let err = run_membership_change(
            [0xAA; 20],
            vec![entry(0x01, 1)],
            1,
            &digest,
            &collector,
            &applier,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("apply sealed epoch across the cluster"),
            "got: {err}"
        );
    }
}
