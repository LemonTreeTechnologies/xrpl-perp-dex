//! #131 AC-BASE-2″ §6 — the UNL POLICY ceremony.
//!
//! After §6 the operator quorum no longer chooses the validator set (derived from manifests
//! against the measured anchor) nor the reserves unit (anchored, C-M6-4). What it still sets
//! is exactly two dials, and both can only RESTRICT:
//!   * `quorum_num/den` — the SPV quorum fraction, already floored at ≥80% in-enclave;
//!   * `pinned_ledger_seq` — the freshness anchor a proof must sit at or above, already
//!     bounded from above by C-UNL-2's window.
//!
//! ⭐ Q-UNL-4: a cosigner must NOT simply sign the numbers it is handed. Independent nodes
//! never agree on an exact live ledger index, so the leader PROPOSES `L` and each cosigner
//! accepts only if `L <= its own validated ledger` AND `own - L <= ACCEPT_WINDOW` — never a
//! future ledger. Without that check a leader could anchor freshness to a ledger that does
//! not exist yet, which is precisely what the anchor exists to prevent.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Domain — must equal `kUnlPolicyDomain` in sealed_pinned_unl_canonical.cpp. Disjoint from
/// the retired v2 domain, so no v2 bundle (which authorised a whole validator set) can
/// authorise a v3 policy update.
const UNL_POLICY_DOMAIN: &[u8] = b"PERP_UNL_POLICY_v3"; // 18 bytes

/// How far behind its own validated ledger a cosigner will still accept the proposed
/// anchor. Small: the nodes are seconds apart, not hours.
pub const ACCEPT_WINDOW_LEDGERS: u64 = 200;

/// SHA-256 over the exact preimage the enclave hashes:
///   domain || le_u64(epoch) || prev_unl_hash[32] || le_u64(pinned_ledger_seq)
///   || le_u32(quorum_num) || le_u32(quorum_den)
pub fn unl_policy_message_hash(
    unl_epoch: u64,
    prev_unl_hash: &[u8; 32],
    pinned_ledger_seq: u64,
    quorum_num: u32,
    quorum_den: u32,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(UNL_POLICY_DOMAIN);
    h.update(unl_epoch.to_le_bytes());
    h.update(prev_unl_hash);
    h.update(pinned_ledger_seq.to_le_bytes());
    h.update(quorum_num.to_le_bytes());
    h.update(quorum_den.to_le_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}

/// The ≥80% floor, mirrored host-side so a leader refuses to even propose a weak threshold
/// (the enclave enforces it independently — this is early, friendly failure, not the gate).
pub fn quorum_floor_ok(quorum_num: u32, quorum_den: u32) -> bool {
    quorum_den != 0 && (quorum_num as u64) * 100 >= (quorum_den as u64) * 80
}

/// Q-UNL-4 acceptance rule for a cosigner: the proposed anchor must be at or below what
/// THIS node has actually validated, and not too far behind it.
pub fn anchor_acceptable(proposed: u64, own_validated: u64) -> bool {
    proposed <= own_validated && own_validated.saturating_sub(proposed) <= ACCEPT_WINDOW_LEDGERS
}

/// Ask a node's own XRPL endpoint for its current validated ledger index.
pub async fn own_validated_ledger(xrpl_url: &str) -> Result<u64> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({"method": "server_info"}))
        .send()
        .await
        .context("server_info request")?
        .json()
        .await
        .context("server_info response")?;
    resp["result"]["info"]["validated_ledger"]["seq"]
        .as_u64()
        .context("server_info has no validated_ledger.seq")
}

/// What a cosigner checks before endorsing. Returns Ok(()) or the reason to refuse.
pub async fn cosigner_accepts(
    xrpl_url: &str,
    pinned_ledger_seq: u64,
    quorum_num: u32,
    quorum_den: u32,
) -> Result<()> {
    if !quorum_floor_ok(quorum_num, quorum_den) {
        bail!("proposed quorum {quorum_num}/{quorum_den} is below the 80% floor — refuse");
    }
    let own = own_validated_ledger(xrpl_url).await.context(
        "cosigner could not read its OWN validated ledger — refuse rather than trust the proposal",
    )?;
    if !anchor_acceptable(pinned_ledger_seq, own) {
        bail!(
            "proposed anchor {pinned_ledger_seq} not acceptable against own validated {own} \
             (must be <= own and within {ACCEPT_WINDOW_LEDGERS}) — refuse"
        );
    }
    Ok(())
}

/// Read the enclave's current UNL record state — needed to chain the next update.
pub async fn read_status(admin_base: &str) -> Result<(u64, [u8; 32], u32)> {
    let r = read_record(admin_base).await?;
    Ok((r.unl_epoch, r.digest, r.live_validators))
}

/// The §6 record a node currently holds. `read_status` returns the three fields
/// the update chain needs; this carries the anchor as well, because the anchor is
/// where a cluster split is legible (the live fork shows identical epochs and
/// DIFFERENT anchors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlRecord {
    pub unl_epoch: u64,
    pub digest: [u8; 32],
    pub live_validators: u32,
    pub pinned_ledger_seq: u64,
}

pub async fn read_record(admin_base: &str) -> Result<UnlRecord> {
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(20))?;
    let url = format!("{}/v1/admin/unl/status", admin_base.trim_end_matches('/'));
    let r: serde_json::Value = http.get(&url).send().await?.json().await?;
    parse_unl_status(&r)
}

/// Split out so the wire shape is testable against a REAL response body rather
/// than one written to match the parser.
pub fn parse_unl_status(r: &serde_json::Value) -> Result<UnlRecord> {
    if r["status"].as_str() != Some("success") {
        bail!("unl status failed: {r}");
    }
    let unl_epoch = r["unl_epoch"].as_u64().context("unl_epoch")?;
    let live_validators = r["live_validators"].as_u64().context("live_validators")? as u32;
    let pinned_ledger_seq = r["pinned_ledger_seq"]
        .as_u64()
        .context("pinned_ledger_seq")?;
    let d = hex::decode(r["digest"].as_str().context("digest")?).context("digest hex")?;
    if d.len() != 32 {
        bail!("digest must be 32 bytes");
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&d);
    Ok(UnlRecord {
        unl_epoch,
        digest,
        live_validators,
        pinned_ledger_seq,
    })
}

/// The p2p layer holds each node's enclave base as a `.../v1` URL while the admin
/// reader wants the host root. Converting in one tested place rather than at each
/// call site, because a silently wrong base would make every peer answer "cannot
/// read my record" and look like an outage.
pub fn enclave_root_from_v1(enclave_url: &str) -> String {
    let t = enclave_url.trim_end_matches('/');
    t.strip_suffix("/v1")
        .unwrap_or(t)
        .trim_end_matches('/')
        .to_string()
}

/// #131 sweep Finding 2 — one node's answer to the read-only status query.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct NodeUnlStatus {
    pub node: String,
    pub unl_epoch: u64,
    pub digest_hex: String,
    pub pinned_ledger_seq: u64,
    pub live_validators: u32,
    /// Set when that node could not read its own record. A node that cannot
    /// answer is NOT evidence of agreement, and is counted separately.
    pub error: Option<String>,
}

/// What the cluster looks like as a whole. `in_sync` is deliberately strict:
/// anything other than every responding node reporting the same record is a
/// disagreement worth showing.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ClusterUnlStatus {
    pub in_sync: bool,
    pub responded: usize,
    pub unreadable: usize,
    /// One entry per DISTINCT record, each naming the nodes holding it. With a
    /// split cluster this is what makes the shape legible at a glance: the live
    /// fork reports two groups at the same epoch with different anchors.
    pub groups: Vec<UnlStatusGroup>,
    pub nodes: Vec<NodeUnlStatus>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct UnlStatusGroup {
    pub unl_epoch: u64,
    pub digest_hex: String,
    pub pinned_ledger_seq: u64,
    pub nodes: Vec<String>,
}

/// Group the answers by the record they report and decide whether the cluster
/// agrees. Pure so the verdict can be tested against the REAL responses the live
/// cluster gives rather than ones invented to match this function.
///
/// An indicator, never a gate: Q-FORK-3 ruled the existing split
/// accept-and-document, so this reports and refuses nothing.
pub fn summarise_unl_agreement(mut nodes: Vec<NodeUnlStatus>) -> ClusterUnlStatus {
    nodes.sort_by(|a, b| a.node.cmp(&b.node));
    let unreadable = nodes.iter().filter(|n| n.error.is_some()).count();
    let mut groups: Vec<UnlStatusGroup> = Vec::new();
    for n in nodes.iter().filter(|n| n.error.is_none()) {
        // Group on the DIGEST alone, because the digest already IS the record's
        // identity: `compute_pinned_unl_digest` hashes the escrow, epoch, prev
        // hash, anchor, quorum and the derived validator set
        // (`sealed_pinned_unl_canonical.cpp:56`). Adding epoch or anchor to the
        // key would be a discriminator that can never discriminate — two records
        // cannot share a digest and differ in either. They are reported on each
        // group because they are what a human reads, not because they decide
        // membership.
        match groups.iter_mut().find(|g| g.digest_hex == n.digest_hex) {
            Some(g) => g.nodes.push(n.node.clone()),
            None => groups.push(UnlStatusGroup {
                unl_epoch: n.unl_epoch,
                digest_hex: n.digest_hex.clone(),
                pinned_ledger_seq: n.pinned_ledger_seq,
                nodes: vec![n.node.clone()],
            }),
        }
    }
    // Nobody answering is not agreement, and neither is a single node answering
    // for a cluster of three — but this function cannot know the expected size,
    // so it reports what it saw and leaves "enough of them" to the caller.
    let in_sync = groups.len() == 1 && unreadable == 0;
    ClusterUnlStatus {
        in_sync,
        responded: nodes.len(),
        unreadable,
        groups,
        nodes,
    }
}

/// Collector for the read-only status query.
pub struct LibP2PUnlStatusCollector {
    relay_tx: tokio::sync::mpsc::Sender<crate::p2p::UnlStatusRelay>,
    timeout: std::time::Duration,
}

impl LibP2PUnlStatusCollector {
    pub fn new(relay_tx: tokio::sync::mpsc::Sender<crate::p2p::UnlStatusRelay>) -> Self {
        Self {
            relay_tx,
            timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Broadcast the query and gather answers until the timeout. Never errors on
    /// a partial cluster: a node that does not answer is the very thing an
    /// operator needs to see, so it is reported as absent rather than raised.
    pub async fn collect(&self) -> Result<ClusterUnlStatus> {
        use uuid::Uuid;
        let request_id = format!("unl-status-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = tokio::sync::mpsc::channel(32);
        self.relay_tx
            .send(crate::p2p::UnlStatusRelay {
                request_id,
                responses_tx,
            })
            .await
            .context("send UnlStatusRelay to the p2p run-loop")?;

        let mut rows: Vec<NodeUnlStatus> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match tokio::time::timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                _ => break,
            };
            if let crate::p2p::SigningMessage::UnlStatusResponse {
                signer_xrpl_address,
                unl_epoch,
                digest_hex,
                pinned_ledger_seq,
                live_validators,
                error,
                ..
            } = resp
            {
                // One row per node: a node answering twice must not turn one
                // opinion into two, which would hide a split behind a majority.
                if rows.iter().any(|r| r.node == signer_xrpl_address) {
                    continue;
                }
                rows.push(NodeUnlStatus {
                    node: signer_xrpl_address,
                    unl_epoch,
                    digest_hex,
                    pinned_ledger_seq,
                    live_validators,
                    error,
                });
            }
        }
        Ok(summarise_unl_agreement(rows))
    }
}

/// Collector for the policy ceremony — mirrors the reserves collectors.
pub struct LibP2PUnlPolicyCollector {
    relay_tx: tokio::sync::mpsc::Sender<crate::p2p::UnlPolicyRelay>,
    timeout: std::time::Duration,
}

impl LibP2PUnlPolicyCollector {
    pub fn new(relay_tx: tokio::sync::mpsc::Sender<crate::p2p::UnlPolicyRelay>) -> Self {
        Self {
            relay_tx,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    pub async fn collect(
        &self,
        proposed_epoch: u64,
        prev_unl_hash: [u8; 32],
        pinned_ledger_seq: u64,
        quorum_num: u32,
        quorum_den: u32,
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
        use uuid::Uuid;
        let request_id = format!("unl-policy-{}", Uuid::new_v4());
        let (responses_tx, mut responses_rx) = tokio::sync::mpsc::channel(32);
        self.relay_tx
            .send(crate::p2p::UnlPolicyRelay {
                request_id,
                proposed_epoch,
                prev_unl_hash,
                pinned_ledger_seq,
                quorum_num,
                quorum_den,
                responses_tx,
            })
            .await
            .context("send UnlPolicyRelay to the p2p run-loop")?;

        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let resp = match tokio::time::timeout(remaining, responses_rx.recv()).await {
                Ok(Some(m)) => m,
                _ => break,
            };
            if let crate::p2p::SigningMessage::Response {
                der_signature: Some(der_hex),
                compressed_pubkey: Some(pk_hex),
                error: None,
                ..
            } = resp
            {
                let pk = hex::decode(&pk_hex).unwrap_or_default();
                let der = hex::decode(&der_hex).unwrap_or_default();
                if pk.len() == 33 && !der.is_empty() && !entries.iter().any(|(p, _)| *p == pk) {
                    entries.push((pk, der));
                }
            }
        }
        if entries.is_empty() {
            bail!(
                "no operator endorsed the policy proposal within {:?} — most likely the \
                 freshness anchor did not match their own validated ledgers (Q-UNL-4)",
                self.timeout
            );
        }
        let pubkeys: Vec<Vec<u8>> = entries.iter().map(|(p, _)| p.clone()).collect();
        Ok((
            crate::reserves_baseline::build_quorum_bundle(&entries),
            pubkeys,
        ))
    }
}

/// C-FORK-ORCH-2: decide whether an apply-broadcast actually reached the whole cluster.
///
/// Returns `Some(reason)` when it did NOT — a partial apply is a hard failure, never a
/// warning. "Leader applied, others refused" returning 200/ok is the exact signature that
/// hid the original policy fork for a whole round: the operator saw success and moved on
/// while the chains split irreversibly.
///
/// `failures` alone cannot express the shortfall. A node that never answers contributes no
/// ack at all, so `applied = 1, failures = []` means one node sealed and two silently did
/// nothing — indistinguishable from success if you only check `failures.is_empty()`.
fn apply_shortfall(applied: usize, expected: usize, failures: &[String]) -> Option<String> {
    if applied >= expected {
        return None;
    }
    let silent = expected.saturating_sub(applied + failures.len());
    Some(format!(
        "UNL policy applied on {applied}/{expected} nodes - the cluster is now SPLIT and the \
         policy chains will not rejoin. Refused: [{}]. Silent (no ack): {silent}. Retry: the \
         enclave's epoch chaining makes re-running safe and idempotent.",
        failures.join("; ")
    ))
}

/// Drive a full policy update: read state -> propose -> collect >= quorum -> apply on
/// EVERY node. Matches the sibling ceremony drivers' shape, hence the same lint waiver.
#[allow(clippy::too_many_arguments)]
pub async fn run_unl_policy_ceremony(
    collector: &LibP2PUnlPolicyCollector,
    admin_base: &str,
    escrow_account_id_hex: &str,
    pinned_ledger_seq: u64,
    quorum_num: u32,
    quorum_den: u32,
    cosign_quorum: usize,
    applier: &crate::membership_apply::LibP2PMembershipApplier,
) -> Result<serde_json::Value> {
    if !quorum_floor_ok(quorum_num, quorum_den) {
        bail!("refusing to propose {quorum_num}/{quorum_den}: below the 80% floor");
    }
    let (epoch, prev, live) = read_status(admin_base).await?;
    tracing::info!(
        current_epoch = epoch,
        live_validators = live,
        pinned_ledger_seq,
        "#131 §6: proposing a UNL policy update"
    );
    let (bundle, pubkeys) = collector
        .collect(epoch + 1, prev, pinned_ledger_seq, quorum_num, quorum_den)
        .await?;
    if pubkeys.len() < cosign_quorum {
        bail!(
            "collected {} endorsement(s), need >= {cosign_quorum} — refuse",
            pubkeys.len()
        );
    }
    // Apply on EVERY node, not just this one. The bundle is already cosigned, so each
    // node's enclave re-derives the hash and re-verifies it against its own sealed
    // SignerList — the broadcast only spares the operator from re-running the whole
    // ceremony per node, which is what previously left the cluster pinning three
    // different freshness anchors.
    let (applied, expected, failures) = applier
        .apply_unl_policy(
            escrow_account_id_hex,
            epoch + 1,
            &hex::encode(prev),
            pinned_ledger_seq,
            quorum_num,
            quorum_den,
            &hex::encode(&bundle),
        )
        .await?;
    if let Some(msg) = apply_shortfall(applied, expected, &failures) {
        bail!("{msg}");
    }
    tracing::info!(
        applied_nodes = applied,
        new_epoch = epoch + 1,
        pinned_ledger_seq,
        "#131 §6: UNL policy update applied on every node"
    );
    Ok(serde_json::json!({
        "status": "ok",
        "unl_epoch": epoch + 1,
        "pinned_ledger_seq": pinned_ledger_seq,
        "applied_nodes": applied,
        "expected_nodes": expected,
    }))
}

#[cfg(test)]
mod tests {

    // ── #131 sweep Finding 2 — the cross-node UNL indicator ──
    //
    // The fixtures below are the VERBATIM bodies the three live testnet nodes
    // returned on 2026-09-21, not bodies written to match the parser. The cluster
    // is genuinely split 2-vs-1 at the same epoch with different anchors, which is
    // the case the indicator exists for.
    const LIVE_NODE1: &str = r#"{"anchored_masters":6,"digest":"dba62a986b987bfda0c07d560f84702aa160e6fba79dcd41165c20d0a5dfd18f","live_validators":6,"pinned_ledger_seq":20591270,"status":"success","unl_epoch":1}"#;
    const LIVE_NODE2: &str = r#"{"anchored_masters":6,"digest":"1089323632beffb3289f73775b26661d152912680bef9e252a9f1a738d7ee8af","live_validators":6,"pinned_ledger_seq":20591293,"status":"success","unl_epoch":1}"#;

    fn row(node: &str, body: &str) -> super::NodeUnlStatus {
        let r = super::parse_unl_status(&serde_json::from_str(body).unwrap()).unwrap();
        super::NodeUnlStatus {
            node: node.into(),
            unl_epoch: r.unl_epoch,
            digest_hex: hex::encode(r.digest),
            pinned_ledger_seq: r.pinned_ledger_seq,
            live_validators: r.live_validators,
            error: None,
        }
    }

    #[test]
    fn parses_the_real_status_body() {
        let r = super::parse_unl_status(&serde_json::from_str(LIVE_NODE1).unwrap()).unwrap();
        assert_eq!(r.unl_epoch, 1);
        assert_eq!(r.live_validators, 6);
        assert_eq!(r.pinned_ledger_seq, 20591270);
        assert_eq!(
            hex::encode(r.digest),
            "dba62a986b987bfda0c07d560f84702aa160e6fba79dcd41165c20d0a5dfd18f"
        );
    }

    #[test]
    fn the_live_fork_is_reported_as_a_split() {
        let out = super::summarise_unl_agreement(vec![
            row("rLhf-node-1", LIVE_NODE1),
            row("r4XF-node-2", LIVE_NODE2),
            row("rNBY-node-3", LIVE_NODE2),
        ]);
        assert!(!out.in_sync, "the live cluster does NOT agree");
        assert_eq!(out.groups.len(), 2, "one group per distinct record");
        assert_eq!(out.responded, 3);
        assert_eq!(out.unreadable, 0);
        let sizes: Vec<usize> = out.groups.iter().map(|g| g.nodes.len()).collect();
        assert!(
            sizes.contains(&1) && sizes.contains(&2),
            "2-vs-1: {sizes:?}"
        );
        // The epochs AGREE while the anchors do not — grouping on the epoch alone
        // would call this cluster healthy.
        assert!(out.groups.iter().all(|g| g.unl_epoch == 1));
        assert_ne!(
            out.groups[0].pinned_ledger_seq,
            out.groups[1].pinned_ledger_seq
        );
    }

    #[test]
    fn an_agreeing_cluster_is_one_group() {
        let out = super::summarise_unl_agreement(vec![
            row("r4XF-node-2", LIVE_NODE2),
            row("rNBY-node-3", LIVE_NODE2),
        ]);
        assert!(out.in_sync);
        assert_eq!(out.groups.len(), 1);
        assert_eq!(out.groups[0].nodes.len(), 2);
    }

    #[test]
    fn a_node_that_cannot_read_its_record_is_not_agreement() {
        // Two nodes agree and the third cannot answer. Counting only the readable
        // ones would report a healthy cluster while a third of it is unknown.
        let mut broken = row("rNBY-node-3", LIVE_NODE2);
        broken.error = Some("enclave unreachable".into());
        let out = super::summarise_unl_agreement(vec![
            row("rLhf-node-1", LIVE_NODE2),
            row("r4XF-node-2", LIVE_NODE2),
            broken,
        ]);
        assert!(!out.in_sync, "an unknown node is not a agreeing node");
        assert_eq!(out.unreadable, 1);
        assert_eq!(
            out.groups.len(),
            1,
            "the readable ones still group together"
        );
    }

    #[test]
    fn enclave_root_drops_the_v1_suffix_once() {
        assert_eq!(
            super::enclave_root_from_v1("https://localhost:9088/v1"),
            "https://localhost:9088"
        );
        assert_eq!(
            super::enclave_root_from_v1("https://localhost:9088/v1/"),
            "https://localhost:9088"
        );
        // Already a root: must be left alone, not have a path segment eaten.
        assert_eq!(
            super::enclave_root_from_v1("https://localhost:9088"),
            "https://localhost:9088"
        );
    }

    #[tokio::test]
    async fn the_collector_gives_one_row_per_node_however_often_it_answers() {
        // A node answering twice must not turn one opinion into two: that would let
        // a 2-vs-1 split read as 3-vs-1 and hide which side is actually bigger.
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::channel::<crate::p2p::UnlStatusRelay>(4);
        tokio::spawn(async move {
            let relay = relay_rx.recv().await.unwrap();
            let mk = |node: &str, anchor: u64| crate::p2p::SigningMessage::UnlStatusResponse {
                request_id: relay.request_id.clone(),
                signer_xrpl_address: node.into(),
                unl_epoch: 1,
                digest_hex: "aa".repeat(32),
                pinned_ledger_seq: anchor,
                live_validators: 6,
                error: None,
            };
            let _ = relay.responses_tx.send(mk("node-a", 1)).await;
            let _ = relay.responses_tx.send(mk("node-a", 999)).await;
            let _ = relay.responses_tx.send(mk("node-b", 1)).await;
        });
        let collector = super::LibP2PUnlStatusCollector::new(relay_tx);
        let out = collector.collect().await.unwrap();
        assert_eq!(out.responded, 2, "one row per node: {:?}", out.nodes);
        assert!(
            out.in_sync,
            "the duplicate carried a different anchor and must be ignored"
        );
    }

    use super::*;

    /// The three shapes an apply-broadcast can end in. The third is the one that matters:
    /// silent non-responders leave `failures` EMPTY, so a check that only looked at
    /// `failures` would call a two-node no-op a success — which is how the fork formed.
    #[test]
    fn partial_apply_is_a_hard_failure() {
        assert!(
            apply_shortfall(3, 3, &[]).is_none(),
            "full apply must succeed"
        );

        let refused = apply_shortfall(1, 3, &["node-2: PREVHASH_MISMATCH".into()])
            .expect("partial apply must fail");
        assert!(refused.contains("1/3"), "{refused}");
        assert!(refused.contains("PREVHASH_MISMATCH"), "{refused}");
        assert!(refused.contains("Silent (no ack): 1"), "{refused}");

        let silent = apply_shortfall(1, 3, &[]).expect("silent shortfall must ALSO fail");
        assert!(silent.contains("Silent (no ack): 2"), "{silent}");
    }

    /// Cross-language golden: the Rust preimage must equal the C++ enclave's
    /// compute_pinned_unl_policy_hash, byte for byte. Inputs match the frozen vector in
    /// tests/test_pinned_unl_canonical.cpp (epoch=1, prev=zeros, seq=20435037, 4/5).
    #[test]
    fn policy_hash_matches_enclave_golden() {
        let prev = [0u8; 32];
        let h = unl_policy_message_hash(1, &prev, 20_435_037, 4, 5);
        assert_eq!(
            hex::encode_upper(h),
            "13AE808837780C83D7A6914A9162F920EC7D87E04FEC9E2E722A5EAA920EB4E3",
            "Rust policy hash must match the C++ enclave golden"
        );
    }

    #[test]
    fn quorum_floor_matches_the_enclave_rule() {
        assert!(quorum_floor_ok(4, 5), "4/5 = 80% passes");
        assert!(!quorum_floor_ok(3, 5), "3/5 = 60% refused");
        assert!(
            !quorum_floor_ok(1, 64),
            "1/64 refused — the shrink-the-ratio attack"
        );
        assert!(!quorum_floor_ok(1, 0), "den=0 refused");
    }

    /// ⭐ Q-UNL-4: the cosigner's acceptance rule. A FUTURE ledger must always be refused —
    /// that is the case the freshness anchor exists to prevent.
    #[test]
    fn anchor_rule_refuses_future_and_stale() {
        let own = 20_500_000u64;
        assert!(anchor_acceptable(own, own), "own ledger is acceptable");
        assert!(
            anchor_acceptable(own - 10, own),
            "slightly behind is acceptable"
        );
        assert!(
            anchor_acceptable(own - ACCEPT_WINDOW_LEDGERS, own),
            "exactly at the window edge is acceptable"
        );
        assert!(
            !anchor_acceptable(own + 1, own),
            "a FUTURE ledger must be refused (the leader cannot anchor ahead of reality)"
        );
        assert!(
            !anchor_acceptable(own - ACCEPT_WINDOW_LEDGERS - 1, own),
            "too far behind is refused"
        );
    }
}
