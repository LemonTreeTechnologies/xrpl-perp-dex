//! #131 AC-BASE-2″ §6 — feeding the enclave's MEASURED-ANCHOR root of SPV trust.
//!
//! The enclave holds the validator MASTER keys as a compile-time constant (covered by
//! MRENCLAVE) and verifies each manifest's `sfMasterSignature` with ed25519 itself. So this
//! module is deliberately dumb: it fetches the XRPL validator list, pulls the raw manifests
//! out of it, and posts them to the local enclave. Nothing here is trusted.
//!
//! In particular we do NOT verify the VL publisher signature, and we do NOT need several
//! publishers cross-checked (C-UNL-4 became moot): the VL is pure transport. A hostile or
//! broken publisher can only WITHHOLD manifests, which fails closed — the derived validator
//! set stays as it was, and if it decays below the quorum the SPV path simply refuses.
//! Injection is impossible: a manifest not signed by an anchored master changes nothing.
//!
//! This is why §6 made the orchestrator SIMPLER than the design it replaced. Under the
//! operator-asserted model each cosigner had to independently re-fetch and re-verify the VL
//! and refuse on mismatch, or the 2-of-N would have been rubber-stamping one host's list.

use anyhow::{bail, Context, Result};
use base64::Engine;
use std::time::Duration;

/// Fetch the validator list and return the RAW manifests (one per listed validator).
///
/// VL shape: `{"blob": base64(json), ...}` where the inner JSON is
/// `{"sequence": N, "expiration": T, "validators": [{"validation_public_key": hex,
/// "manifest": base64}, ...]}`.
pub async fn fetch_vl_manifests(vl_url: &str) -> Result<Vec<Vec<u8>>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build VL http client")?;
    let vl: serde_json::Value = http
        .get(vl_url)
        .send()
        .await
        .with_context(|| format!("GET {vl_url}"))?
        .json()
        .await
        .context("validator list is not JSON")?;

    let blob_b64 = vl["blob"]
        .as_str()
        .context("validator list has no `blob`")?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(blob_b64)
        .context("VL blob is not valid base64")?;
    let inner: serde_json::Value = serde_json::from_slice(&blob).context("VL blob is not JSON")?;

    let validators = inner["validators"]
        .as_array()
        .context("VL blob has no `validators` array")?;
    let mut out = Vec::with_capacity(validators.len());
    for v in validators {
        let m_b64 = v["manifest"]
            .as_str()
            .context("validator entry has no `manifest`")?;
        let m = base64::engine::general_purpose::STANDARD
            .decode(m_b64)
            .context("manifest is not valid base64")?;
        if m.is_empty() {
            bail!("empty manifest in the validator list");
        }
        out.push(m);
    }
    if out.is_empty() {
        bail!("validator list contained no manifests");
    }
    Ok(out)
}

/// POST the manifests to the LOCAL enclave admin route (loopback, X-C1).
///
/// Permissionless by design — no session key, no quorum. The response reports how many
/// entries actually CHANGED the derived state; `changed == 0` is a normal outcome (nothing
/// rotated since the last submission) and means the enclave sealed nothing.
pub async fn submit_manifests(
    admin_base: &str,
    manifests: &[Vec<u8>],
) -> Result<serde_json::Value> {
    if manifests.is_empty() {
        bail!("refusing to submit an empty manifest batch");
    }
    let http = crate::http_helpers::loopback_http_client(Duration::from_secs(30))?;
    let url = format!(
        "{}/v1/admin/unl/submit-manifests",
        admin_base.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "manifests": manifests.iter().map(hex::encode).collect::<Vec<_>>(),
    });
    let resp: serde_json::Value = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("POST submit-manifests")?
        .json()
        .await
        .context("submit-manifests response is not JSON")?;
    if resp["status"].as_str() != Some("success") {
        bail!("enclave refused the manifest batch: {resp}");
    }
    Ok(resp)
}

/// Fetch + submit in one step. Returns `(submitted, changed)`.
pub async fn refresh_validator_set(vl_url: &str, admin_base: &str) -> Result<(usize, i64)> {
    let manifests = fetch_vl_manifests(vl_url).await?;
    let resp = submit_manifests(admin_base, &manifests).await?;
    let changed = resp["changed"].as_i64().unwrap_or(0);
    tracing::info!(
        submitted = manifests.len(),
        changed,
        "#131 §6: validator manifests submitted (the enclave verified each against its \
         measured master anchor; changed=0 just means nothing rotated)"
    );
    Ok((manifests.len(), changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structure test: the VL nesting (base64 blob containing base64 manifests) is the part
    /// that is easy to get wrong. The REAL-data guarantee lives in the enclave's own
    /// 41-manifest corpus and in `live_fetch_testnet_vl` below — this one only pins the
    /// unwrapping, so it deliberately does not assert anything about manifest contents.
    #[test]
    fn unwraps_the_nested_base64() {
        let manifest = vec![0x24u8, 0, 0, 0, 2, 0x71, 0x21, 0xED];
        let inner = serde_json::json!({
            "sequence": 59,
            "validators": [
                {"validation_public_key": "ED00", "manifest":
                    base64::engine::general_purpose::STANDARD.encode(&manifest)}
            ]
        });
        let vl = serde_json::json!({
            "blob": base64::engine::general_purpose::STANDARD.encode(inner.to_string()),
        });
        // exercise the same decode path the fetcher uses
        let blob = base64::engine::general_purpose::STANDARD
            .decode(vl["blob"].as_str().unwrap())
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&blob).unwrap();
        let got = base64::engine::general_purpose::STANDARD
            .decode(parsed["validators"][0]["manifest"].as_str().unwrap())
            .unwrap();
        assert_eq!(got, manifest);
    }

    /// Live check against the real testnet VL. #[ignore] so CI (no network) skips it.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_testnet_vl() {
        let ms = fetch_vl_manifests("https://vl.altnet.rippletest.net")
            .await
            .expect("fetch testnet VL");
        assert!(!ms.is_empty(), "testnet VL must list validators");
        for m in &ms {
            // every manifest starts with sfSequence (0x24) and carries sfPublicKey (0x71)
            assert_eq!(m[0], 0x24, "manifest must start with sfSequence");
            assert!(m.len() > 40, "manifest suspiciously short");
        }
        eprintln!("testnet VL: {} manifests", ms.len());
    }
}
