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
use tracing::{error, info};

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
    /// The PUBLISHED validator list, not a rippled RPC endpoint.
    ///
    /// The first cut asked a local rippled for `validators` + `manifest` per master. That
    /// could never work here and the live deploy said so on its first tick: the Azure
    /// nodes have no rippled (`error sending request for url (http://127.0.0.1:5005/)`),
    /// and both of those commands are ADMIN RPC, so pointing them at a public endpoint
    /// would not have worked either.
    ///
    /// The VL is the right source anyway: it is the signed artefact the network publishes,
    /// it carries every validator's manifest inline, and it needs no privileged access.
    /// Trusting the transport is not required — the enclave verifies each manifest with
    /// ed25519 against its MEASURED master anchor, so a hostile mirror can withhold but
    /// never forge.
    pub vl_url: String,
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
            vl_url: std::env::var("XRPL_VL_URL")
                .unwrap_or_else(|_| "https://vl.altnet.rippletest.net".to_string()),
            interval_secs: std::env::var("PERP_UNL_REFRESH_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3600),
        })
    }
}

/// Every (master key, manifest-hex) pair in a published validator list.
///
/// The VL is `{public_key, manifest, blob, signature}` where `blob` is base64 of
/// `{sequence, expiration, validators:[{validation_public_key, manifest}]}` and each
/// inner `manifest` is base64 of the binary STObject the enclave wants. So one fetch
/// yields everything, and the hex conversion happens here — the one place it is tested.
///
/// The VL's own signature is NOT checked here, deliberately. Checking it would only move
/// the trust to the publisher key we would then have to pin, while the enclave already
/// refuses any manifest that does not verify against its measured anchor. Withholding is
/// the only power a bad mirror has, and `enabled`/`failures` on the status endpoint is
/// what makes withholding visible.
pub fn manifests_from_vl(vl: &serde_json::Value) -> Result<Vec<(String, String)>> {
    use base64::engine::general_purpose::STANDARD;
    let blob_b64 = vl["blob"]
        .as_str()
        .context("validator list has no blob field")?;
    let raw = STANDARD.decode(blob_b64).context("VL blob is not base64")?;
    let blob: serde_json::Value = serde_json::from_slice(&raw).context("VL blob is not JSON")?;
    let entries = blob["validators"]
        .as_array()
        .context("VL blob has no validators array")?;

    let mut out: Vec<(String, String)> = Vec::with_capacity(entries.len());
    for e in entries {
        let master = e["validation_public_key"].as_str().unwrap_or_default();
        let m_b64 = match e["manifest"].as_str() {
            Some(m) => m,
            // One entry without a manifest must not cost us the other five: the enclave
            // treats a missing master as unchanged, never as removed.
            None => continue,
        };
        let m = match STANDARD.decode(m_b64) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // The enclave frames entries as [u16 len | manifest], so anything at or above
        // 64 KiB cannot be expressed. Caught here, where the validator can be named.
        if m.is_empty() || m.len() > 0xFFFF {
            continue;
        }
        out.push((master.to_string(), hex::encode(&m)));
    }
    if out.is_empty() {
        bail!("validator list carried no usable manifests");
    }
    Ok(out)
}

async fn refresh_once(
    http: &reqwest::Client,
    cfg: &UnlRefreshConfig,
    perp: &crate::perp_client::PerpClient,
    health: &UnlRefreshHealth,
) -> Result<(usize, u64)> {
    let vl: serde_json::Value = http.get(&cfg.vl_url).send().await?.json().await?;
    let pairs = manifests_from_vl(&vl)?;
    health
        .masters_seen
        .store(pairs.len() as u64, Ordering::Relaxed);
    let manifests: Vec<String> = pairs.into_iter().map(|(_, hexm)| hexm).collect();
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

    /// A REAL published validator list, trimmed to two of its six entries and re-encoded.
    /// The structure and both manifests are the network's own bytes, captured from
    /// https://vl.altnet.rippletest.net on 2026-09-25 — not a hand-built shape, because a
    /// fixture we invented could agree with a parser we invented and both be wrong about
    /// what the publisher actually serves.
    const VL: &str = r#"{"public_key":"ED264807102805220DA0F312E71FC2C69E1552C9C5790F6C25E3729DEB573D5860","version":1,"blob":"eyJzZXF1ZW5jZSI6NTksImV4cGlyYXRpb24iOjg3MDk3NTA2NiwidmFsaWRhdG9ycyI6W3sidmFsaWRhdGlvbl9wdWJsaWNfa2V5IjoiRUQwNjFFQ0I1MUI1QkQ2MjY2NUY1RDFBNURCMUE2MkFGODQ0NjRCRUQ3N0U3NzI4MjM1QTdBNTUxRDQ1MzVFNzE3IiwibWFuaWZlc3QiOiJKQUFBQUFKeEllMEdIc3RSdGIxaVpsOWRHbDJ4cGlyNFJHUysxMzUzS0NOYWVsVWRSVFhuRjNNaEFubkJza0psamRlRkZLV21DaUJ2b3d3WW8rNDNCWktnV0tnUHFqNWNSUENYZGtjd1JRSWhBS1JpTFhldko2MXVraFp0aWt2Q3VLZ0dSblY4SDA4ZU0vUEV2Sk5FZGwwNEFpQmkvQms2OFZWZWZSMUd0Z0k0WWV6UlFWc3huRWlOL0xtV1NObVFZS1FSSUhBU1FBdDhoS2Z4a3FQTWVCT1RoMngyaGpya0FhN2xlVGVuQnRmOUR4dWh3bGdzQjlOL3h4VGZwek1Ra2pVWW9ZaXlYa1haeWgxTlZzTkxES1VtT2RXWkxBTT0ifSx7InZhbGlkYXRpb25fcHVibGljX2tleSI6IkVEQURCNkU2RjcyMjlGOTI5MDlFNUE2REJBRjgxQUQxRUM3MjNEMzFCNjc2Q0Q4RjVGM0U5MjZBRDA0M0QxODdDMCIsIm1hbmlmZXN0IjoiSkFBQUFBSnhJZTJ0dHViM0lwK1NrSjVhYmJyNEd0SHNjajB4dG5iTmoxOCtrbXJRUTlHSHdITWhBbjhvVzR1elB3NkxBbHY1VmNLYWZQcUtDWldESHVTdGs2bTlWeXA4anUzTmRrY3dSUUloQU5RbEZiaUROZmEvTEpJcitlYVoyS0tjMDRHbGRaTXJBRzRiRFdGTUx5VVJBaUFsd0FmTkl1dmVJMEhtaE0wSStGdzR5Z0FzSEZXdXdWcmNXS2FiWkxIdGdIQVNRQkJFVFRSRHhDc1FvNHdJSyt6NUNkOU9ta3UweUR4Qk9NVEE3MFJTcUVvcFY5REhCZ1ZWOWc4MmoxbW4wb0pYRHowcE5YcnJDbjNEcU1id0EwdkMrUUE9In1dfQ=="}"#;

    #[test]
    fn the_real_validator_list_yields_master_and_manifest_pairs() {
        let j: serde_json::Value = serde_json::from_str(VL).unwrap();
        let pairs = manifests_from_vl(&j).unwrap();
        assert_eq!(pairs.len(), 2);
        // The VL spells the master key as HEX (0xED || 32 bytes), not the base58 `nH…`
        // that rippled's admin `validators` call returns. Asserted because assuming the
        // other spelling is exactly the mistake this real fixture caught.
        assert_eq!(
            pairs[0].0.len(),
            66,
            "master key hex length: {}",
            pairs[0].0
        );
        assert!(
            pairs[0].0.starts_with("ED"),
            "master key is ed25519 hex: {}",
            pairs[0].0
        );
        // Field 0x24 (Sequence), then 0x71 0x21 (PublicKey, VL len 33), then 0xED and the
        // master key the enclave matches against its measured anchor. If we ever ship the
        // wrong bytes, this is what fails.
        let h = pairs[0].1.to_uppercase();
        assert!(h.starts_with("2400000002"), "seq field first: {h}");
        assert!(
            h.contains("7121ED061ECB51B5BD62665F5D1A5DB1A62AF84464BED77E7728235A7A551D4535E717"),
            "manifest does not carry the master key it claims: {h}"
        );
    }

    #[test]
    fn the_blob_is_decoded_not_taken_on_trust() {
        // Feeding the OUTER json without decoding the blob would find no validators.
        // Each of these must be an error, not an empty success that reports changed:0 —
        // which is indistinguishable from a healthy steady state.
        let mut j: serde_json::Value = serde_json::from_str(VL).unwrap();
        j["blob"] = serde_json::json!("!!! not base64 !!!");
        assert!(manifests_from_vl(&j).is_err(), "non-base64 blob must fail");

        let mut j2: serde_json::Value = serde_json::from_str(VL).unwrap();
        j2.as_object_mut().unwrap().remove("blob");
        assert!(manifests_from_vl(&j2).is_err(), "missing blob must fail");

        use base64::engine::general_purpose::STANDARD;
        let mut j3: serde_json::Value = serde_json::from_str(VL).unwrap();
        j3["blob"] = serde_json::json!(STANDARD.encode(b"{\"sequence\":1}"));
        assert!(
            manifests_from_vl(&j3).is_err(),
            "blob with no validators must fail"
        );

        let mut j4: serde_json::Value = serde_json::from_str(VL).unwrap();
        j4["blob"] = serde_json::json!(STANDARD.encode(b"{\"validators\":[]}"));
        assert!(
            manifests_from_vl(&j4).is_err(),
            "empty validators must fail"
        );
    }

    #[test]
    fn one_unusable_entry_does_not_lose_the_others() {
        use base64::engine::general_purpose::STANDARD;
        let j: serde_json::Value = serde_json::from_str(VL).unwrap();
        let raw = STANDARD.decode(j["blob"].as_str().unwrap()).unwrap();
        let mut blob: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        blob["validators"][0]["manifest"] = serde_json::json!("%% not base64 %%");
        let mut j2 = j.clone();
        j2["blob"] = serde_json::json!(STANDARD.encode(serde_json::to_vec(&blob).unwrap()));
        let pairs = manifests_from_vl(&j2).unwrap();
        assert_eq!(pairs.len(), 1, "the good entry must still be submitted");
    }

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
}
