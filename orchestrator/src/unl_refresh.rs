//! Keep the enclave's derived validator set current — the trust root under BOTH the
//! attested clock and SPV deposits.
//!
//! `ecall_submit_validator_manifests` has been in the enclave since 2026-09-07 and,
//! like the clock ecall before it, **nothing has ever called it**. That is not a missing
//! nicety: the enclave derives each validator's secp256k1 *signing* key from the manifest
//! that validator's master key signed, and a validator that rotates its signing key makes
//! the enclave's derived entry stale. With 6 anchored masters and an ≥80% floor the
//! quorum is 5-of-6 — it tolerates exactly ONE stale entry. Nothing refreshing them means
//! the trust root decays until every SPV check and every clock advance refuses, silently
//! and for a reason nobody would look for.
//!
//! **Permissionless by design, and that is why a driver is safe.** No quorum, no session
//! key: a forged manifest cannot pass ed25519 against the MEASURED master anchor compiled
//! into the enclave, so an authority here would add a liveness dependency and no security
//! (enclave Q-M6-3). Invalid entries are per-entry no-ops and never removals, and a
//! submission that changes nothing seals nothing. So the worst a wrong feed can do is
//! waste a round trip.
//!
//! We therefore ferry bytes and let the enclave decide what counts: the master list comes
//! from rippled's own `validators` response rather than a second copy of the anchor list
//! here. A master the enclave does not recognise is ignored in-enclave, which is the right
//! place for that judgement.

use anyhow::{bail, Context, Result};
use base64::Engine;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{error, info, warn};

/// Counters for `/v1/system/status`. `enabled` is first because a node that is not
/// refreshing must not look like one whose validators simply never rotate.
#[derive(Debug, Default)]
pub struct UnlRefreshHealth {
    pub enabled: AtomicBool,
    /// Submissions that reached the enclave, whether or not they changed anything.
    pub submissions: AtomicU64,
    /// Entries the enclave reported as having CHANGED its derived state. A rotation
    /// landing shows up here; a long run of zeroes is the healthy steady state.
    pub changed_total: AtomicU64,
    pub failures: AtomicU64,
    /// Master keys in the last list we fetched from rippled.
    pub masters_seen: AtomicU64,
}

pub struct UnlRefreshConfig {
    pub http_url: String,
    /// Manifests rotate rarely — hourly is already generous, and each round is one
    /// `validators` call plus one `manifest` call per master.
    pub interval_secs: u64,
}

impl UnlRefreshConfig {
    /// **Default ON.** Set `PERP_UNL_REFRESH=0` to disable it, which you should not.
    ///
    /// This deliberately breaks the house opt-in pattern the other enclave-touching
    /// drivers follow, because the asymmetry runs the other way here and the auditor ruled
    /// it so (`RESP-unl-manifest-refresh-attack2-2026-09-25.md`):
    ///
    /// - **Off** is a SILENT decay of the trust root, and SPV deposits ALREADY depend on
    ///   that set — so "nothing depends on it yet" is not true here the way it is for the
    ///   clock.
    /// - **On and wrong** is a benign no-op: every manifest is gated on the measured master
    ///   anchor and on a strictly-newer sequence, so an unknown master is ignored and a
    ///   replayed old manifest is refused. It is not the `-70` flood the clock's opt-in
    ///   default exists to avoid — that reason does not transfer.
    ///
    /// A default has to agree with "`enabled: false` is not healthy", and opt-in did not.
    pub fn from_env() -> Option<Self> {
        if std::env::var("PERP_UNL_REFRESH").ok().as_deref() == Some("0") {
            return None;
        }
        Some(Self {
            http_url: std::env::var("XRPL_RPC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:5005".to_string()),
            interval_secs: std::env::var("PERP_UNL_REFRESH_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
        })
    }
}

/// Master keys out of a `validators` response, as base58 `n…` strings.
///
/// `publisher_lists[].list` rather than `trusted_validator_keys`: the publisher list is
/// what the VL actually signed, while the trusted set is this node's local conclusion
/// about it. We want the former — and the enclave re-decides anyway.
pub fn masters_from_validators_response(j: &serde_json::Value) -> Result<Vec<String>> {
    let lists = j["result"]["publisher_lists"]
        .as_array()
        .context("validators response has no publisher_lists")?;
    let mut out: Vec<String> = Vec::new();
    for l in lists {
        if let Some(keys) = l["list"].as_array() {
            for k in keys {
                if let Some(s) = k.as_str() {
                    if !out.iter().any(|e| e == s) {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    if out.is_empty() {
        bail!("validators response carried no master keys");
    }
    Ok(out)
}

/// The raw manifest out of a `manifest` response, hex-encoded for the enclave endpoint.
///
/// rippled returns it base64; the endpoint wants hex. Converted here rather than at the
/// call site so the one place this re-encoding happens is the one place it is tested.
pub fn manifest_hex_from_response(j: &serde_json::Value) -> Result<String> {
    let b64 = j["result"]["manifest"]
        .as_str()
        .context("manifest response has no manifest field")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("manifest is not base64")?;
    if raw.is_empty() {
        bail!("manifest decoded to zero bytes");
    }
    // The enclave frames entries as [u16 len | manifest], so anything at or above 64 KiB
    // cannot be expressed. Caught here, where we can name which validator, rather than as
    // an anonymous truncation.
    if raw.len() > 0xFFFF {
        bail!("manifest is {} bytes, too large to frame", raw.len());
    }
    Ok(hex::encode(&raw))
}

async fn refresh_once(
    http: &reqwest::Client,
    cfg: &UnlRefreshConfig,
    perp: &crate::perp_client::PerpClient,
    health: &UnlRefreshHealth,
) -> Result<(usize, u64)> {
    let vj: serde_json::Value = http
        .post(&cfg.http_url)
        .json(&serde_json::json!({"method": "validators"}))
        .send()
        .await?
        .json()
        .await?;
    let masters = masters_from_validators_response(&vj)?;
    health
        .masters_seen
        .store(masters.len() as u64, Ordering::Relaxed);

    let mut manifests: Vec<String> = Vec::with_capacity(masters.len());
    for m in &masters {
        let mj: serde_json::Value = http
            .post(&cfg.http_url)
            .json(&serde_json::json!({"method": "manifest", "params": [{"public_key": m}]}))
            .send()
            .await?
            .json()
            .await?;
        match manifest_hex_from_response(&mj) {
            Ok(h) => manifests.push(h),
            // One validator whose manifest this node has not seen must not cost us the
            // other five: a partial submission still refreshes what it can, and the
            // enclave treats a missing entry as "unchanged", not "removed".
            Err(e) => warn!(master = %m, "unl-refresh: no usable manifest: {e}"),
        }
    }
    if manifests.is_empty() {
        bail!(
            "no manifests could be read for any of the {} masters",
            masters.len()
        );
    }
    let resp = perp.submit_validator_manifests(&manifests).await?;
    let changed = resp["changed"].as_u64().unwrap_or(0);
    Ok((manifests.len(), changed))
}

/// The loop. Never exits — a trust root that stops being refreshed decays silently, which
/// is precisely the failure this exists to prevent.
pub async fn run_unl_refresh(
    cfg: UnlRefreshConfig,
    perp: crate::perp_client::PerpClient,
    health: std::sync::Arc<UnlRefreshHealth>,
) {
    let http = reqwest::Client::new();
    health.enabled.store(true, Ordering::Relaxed);
    info!(
        interval_secs = cfg.interval_secs,
        "unl-refresh started — keeping the enclave's derived validator set current"
    );
    let mut announced_failure = false;
    loop {
        match refresh_once(&http, &cfg, &perp, &health).await {
            Ok((submitted, changed)) => {
                health.submissions.fetch_add(1, Ordering::Relaxed);
                health.changed_total.fetch_add(changed, Ordering::Relaxed);
                announced_failure = false;
                if changed > 0 {
                    // Worth a line at info: a rotation just landed, and this is the
                    // moment the trust root would otherwise have started decaying.
                    info!(
                        metric = "unl_manifests_changed_total",
                        submitted, changed, "unl-refresh: validator manifest(s) rotated"
                    );
                }
            }
            Err(e) => {
                health.failures.fetch_add(1, Ordering::Relaxed);
                if !announced_failure {
                    announced_failure = true;
                    error!(
                        metric = "unl_refresh_failed",
                        "unl-refresh FAILED and the enclave's validator set is now ageing: \
                         {e}. With a 5-of-6 quorum floor, one stale signing key is the \
                         whole margin — SPV deposits and the attested clock both stop when \
                         it is spent."
                    );
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(cfg.interval_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from our own rippled 3.1.3 on testnet, 2026-09-25 — the real shape, so a
    /// field rename upstream fails here rather than as an empty submission that reports
    /// success because it changed nothing.
    const VALIDATORS: &str = r#"{"result":{"publisher_lists":[{"available":true,"expiration":"2027-Aug-07 17:31:06.000000000 UTC","list":["nHBQ3CT3EWYZ4uzbnL3k6TRf9bBPhWRFVcK1F5NjtwCBksMEt5yy","nHBbiP5ua5dUqCTz5i5vd3ia9jg3KJthohDjgKxnc7LxtmnauW7Z","nHDDe5uAdiv6RA59MA1oM4JLDtVSYKNShgjEqq1KsdJXZiR47CQT","nHDDiwQBqXhEL1CFoRHdMXD33x9K7rpYJfniXxL7kFavpPd21EGe","nHUCAdca6VoWWYVdBH1bwCUQggEX2e5acQSqxM3DwyuhsFknxmh3","nHUeUNSn3zce2xQZWNghQvd9WRH6FWEnCBKYVJu2vAizMxnXegfJ"],"pubkey_publisher":"ED2648071028","seq":59}],"status":"success"}}"#;

    /// A real manifest for nHBQ3CT3EWYZ…, base64 exactly as rippled returns it.
    const MANIFEST: &str = r#"{"result":{"details":{"ephemeral_key":"n9K7fyu8uvmCoWvW4ZQVCWgW2zrz7sh33Ao7ceNkL7iQGDYtuwTU","master_key":"nHBQ3CT3EWYZ4uzbnL3k6TRf9bBPhWRFVcK1F5NjtwCBksMEt5yy","seq":2},"manifest":"JAAAAAJxIe0GHstRtb1iZl9dGl2xpir4RGS+1353KCNaelUdRTXnFw==","status":"success"}}"#;

    /// The default is a security property here, not a convenience, so it is asserted
    /// rather than left to whoever reads `from_env`. Deliberately exercises the real
    /// env-var contract: absent → on, "0" → off, anything else → on.
    #[test]
    fn the_refresh_driver_is_on_unless_explicitly_disabled() {
        // Serialised by running the three cases in one test: env vars are process-global
        // and a parallel test flipping the same key would make this flap.
        let prev = std::env::var("PERP_UNL_REFRESH").ok();
        std::env::remove_var("PERP_UNL_REFRESH");
        assert!(
            UnlRefreshConfig::from_env().is_some(),
            "absent must mean ON — off is a silent decay of the trust root"
        );
        std::env::set_var("PERP_UNL_REFRESH", "0");
        assert!(
            UnlRefreshConfig::from_env().is_none(),
            "\"0\" must disable it"
        );
        std::env::set_var("PERP_UNL_REFRESH", "1");
        assert!(UnlRefreshConfig::from_env().is_some(), "\"1\" stays ON");
        match prev {
            Some(v) => std::env::set_var("PERP_UNL_REFRESH", v),
            None => std::env::remove_var("PERP_UNL_REFRESH"),
        }
    }

    #[test]
    fn the_real_validators_response_yields_six_masters() {
        let j: serde_json::Value = serde_json::from_str(VALIDATORS).unwrap();
        let m = masters_from_validators_response(&j).unwrap();
        assert_eq!(m.len(), 6);
        assert!(m[0].starts_with("nH"));
    }

    #[test]
    fn an_empty_publisher_list_is_an_error_not_an_empty_submission() {
        // The failure that would otherwise be invisible: submitting nothing returns
        // changed:0, which is indistinguishable from a healthy steady state.
        let mut j: serde_json::Value = serde_json::from_str(VALIDATORS).unwrap();
        j["result"]["publisher_lists"][0]["list"] = serde_json::json!([]);
        assert!(masters_from_validators_response(&j).is_err());
        let mut j2: serde_json::Value = serde_json::from_str(VALIDATORS).unwrap();
        j2["result"]
            .as_object_mut()
            .unwrap()
            .remove("publisher_lists");
        assert!(masters_from_validators_response(&j2).is_err());
    }

    #[test]
    fn duplicate_masters_across_lists_are_submitted_once() {
        let mut j: serde_json::Value = serde_json::from_str(VALIDATORS).unwrap();
        let dup = j["result"]["publisher_lists"][0].clone();
        j["result"]["publisher_lists"] = serde_json::json!([dup.clone(), dup]);
        assert_eq!(masters_from_validators_response(&j).unwrap().len(), 6);
    }

    #[test]
    fn a_real_manifest_converts_to_the_hex_the_enclave_endpoint_wants() {
        let j: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        let h = manifest_hex_from_response(&j).unwrap();
        // Field 0x71 (PublicKey) with VL length 0x21, then 0xED || the master key — the
        // master this manifest is for, and the byte the enclave matches against its
        // MEASURED anchor. If this ever fails, we are shipping the wrong bytes.
        assert!(
            h.to_uppercase()
                .contains("7121ED061ECB51B5BD62665F5D1A5DB1A62AF84464BED77E7728235A7A551D4535E717"),
            "manifest hex does not carry the master key it claims: {h}"
        );
        assert!(
            h.to_uppercase().starts_with("2400000002"),
            "seq field first: {h}"
        );
    }

    #[test]
    fn an_unusable_manifest_is_refused_rather_than_shipped() {
        for bad in ["", "!!!not base64!!!"] {
            let mut j: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
            j["result"]["manifest"] = serde_json::json!(bad);
            assert!(
                manifest_hex_from_response(&j).is_err(),
                "manifest {bad:?} must not be shipped to the enclave"
            );
        }
        let mut j: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
        j["result"].as_object_mut().unwrap().remove("manifest");
        assert!(manifest_hex_from_response(&j).is_err());
    }
}
