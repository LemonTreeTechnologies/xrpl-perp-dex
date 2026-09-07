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
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(20))?;
    let url = format!("{}/v1/admin/unl/status", admin_base.trim_end_matches('/'));
    let r: serde_json::Value = http.get(&url).send().await?.json().await?;
    if r["status"].as_str() != Some("success") {
        bail!("unl status failed: {r}");
    }
    let epoch = r["unl_epoch"].as_u64().context("unl_epoch")?;
    let live = r["live_validators"].as_u64().context("live_validators")? as u32;
    let d = hex::decode(r["digest"].as_str().context("digest")?).context("digest hex")?;
    if d.len() != 32 {
        bail!("digest must be 32 bytes");
    }
    let mut prev = [0u8; 32];
    prev.copy_from_slice(&d);
    Ok((epoch, prev, live))
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

/// Drive a full policy update: read state -> propose -> collect >= quorum -> apply.
pub async fn run_unl_policy_ceremony(
    collector: &LibP2PUnlPolicyCollector,
    admin_base: &str,
    escrow_account_id_hex: &str,
    pinned_ledger_seq: u64,
    quorum_num: u32,
    quorum_den: u32,
    cosign_quorum: usize,
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
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))?;
    let url = format!(
        "{}/v1/admin/unl/govern-policy",
        admin_base.trim_end_matches('/')
    );
    let resp: serde_json::Value = http
        .post(&url)
        .json(&serde_json::json!({
            "escrow_account_id": escrow_account_id_hex,
            "proposed_epoch": epoch + 1,
            "prev_unl_hash": hex::encode(prev),
            "pinned_ledger_seq": pinned_ledger_seq,
            "quorum_num": quorum_num,
            "quorum_den": quorum_den,
            "quorum_bundle": hex::encode(&bundle),
        }))
        .send()
        .await?
        .json()
        .await?;
    if resp["status"].as_str() != Some("success") {
        bail!("enclave refused the policy update: {resp}");
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

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
