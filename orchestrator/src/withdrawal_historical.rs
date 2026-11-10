//! Historical withdrawal finalize — the proof for a withdrawal Payment that landed in a ledger
//! older than any validations this node holds.
//!
//! The ordinary finalize proves the Payment's own ledger with that ledger's validations, which
//! exist only for the last few hundred ledgers. Past that, the proof starts from a RECENT
//! ledger whose validations we do hold (the anchor) and reaches back through two commitments
//! every ledger carries: the LedgerHashes skip lists in its state, and its parent's hash in its
//! header. Only the anchor's state needs a proof path, so the producer needs historical
//! HEADERS (any full-history node) but no historical STATE.
//!
//! Every byte here is re-verified by the enclave; a producer mistake can only make it refuse.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::deposit_spv::tx_proof_parts;
use crate::spv_proof::{
    build_xhwf_blob, check_parent_chain, leaf_holds_hash, ledger_hash, proof_from_getproofpath,
    skip_hop_for, verify_inclusion, HEADER_LEN,
};

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let body = serde_json::json!({ "method": method, "params": [params] });
    let r: Value = http
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{method} request"))?
        .json()
        .await
        .with_context(|| format!("{method} response"))?;
    let res = r["result"].clone();
    if let Some(e) = res["error"].as_str() {
        bail!("{method}: {e}");
    }
    Ok(res)
}

fn header_from_ledger_result(res: &Value) -> Result<[u8; HEADER_LEN]> {
    let hex_s = res["ledger"]["ledger_data"]
        .as_str()
        .context("ledger result has no ledger_data (binary:true not honoured?)")?;
    let raw = hex::decode(hex_s).context("ledger_data hex")?;
    if raw.len() != HEADER_LEN {
        bail!("ledger_data is {} bytes, expected {HEADER_LEN}", raw.len());
    }
    let mut h = [0u8; HEADER_LEN];
    h.copy_from_slice(&raw);
    Ok(h)
}

fn seq_of(h: &[u8; HEADER_LEN]) -> u32 {
    u32::from_be_bytes([h[0], h[1], h[2], h[3]])
}

/// The serialized header of ledger `selector` (`{"ledger_index": n}` or `{"ledger_hash": h}`).
async fn fetch_header(
    http: &reqwest::Client,
    url: &str,
    mut selector: Value,
) -> Result<[u8; HEADER_LEN]> {
    selector["binary"] = Value::Bool(true);
    let res = rpc(http, url, "ledger", selector).await?;
    header_from_ledger_result(&res)
}

/// Where the pieces come from.
pub struct HistoricalSources<'a> {
    /// A node holding the ledgers from the target up to the skip-hop ledger, and the target's
    /// transactions: any full-history node.
    pub history_url: &'a str,
    /// A patched node (getProofPath) holding the ANCHOR's state. The anchor is recent, so a
    /// node that keeps only recent ledgers is enough.
    pub proof_url: &'a str,
}

/// Assemble the XHWF blob for the transaction `target_tx_index` of ledger `target_ledger`
/// (`target_ledger_json` = `ledger` with transactions/expand/binary), anchored at the ledger
/// whose hash is `anchor_hash` and whose buffered validations are `validations`.
pub async fn build_historical_proof(
    http: &reqwest::Client,
    src: &HistoricalSources<'_>,
    anchor_hash: &str,
    validations: &[Vec<u8>],
    target_ledger_json: &Value,
    target_tx_index: usize,
) -> Result<Vec<u8>> {
    let tx =
        tx_proof_parts(target_ledger_json, target_tx_index).context("target transaction proof")?;
    let target_seq = seq_of(&tx.header);

    let anchor = fetch_header(
        http,
        src.proof_url,
        serde_json::json!({ "ledger_hash": anchor_hash }),
    )
    .await
    .context("anchor header")?;
    if hex::encode_upper(ledger_hash(&anchor)) != anchor_hash.to_uppercase() {
        bail!("the proof node returned a header that does not hash to anchor {anchor_hash}");
    }
    let anchor_seq = seq_of(&anchor);
    let (hop_seq, key) = skip_hop_for(anchor_seq, target_seq)?;

    // The skip list, proven in the anchor's state. The node can lag a moment behind the
    // ledger the validations attest, so retry briefly before giving up.
    let mut path_hex: Option<Vec<Value>> = None;
    for _ in 0..10u32 {
        let r = rpc(
            http,
            src.proof_url,
            "ledger_entry",
            serde_json::json!({
                "index": hex::encode_upper(key), "ledger_hash": anchor_hash,
                "binary": true, "proof": true
            }),
        )
        .await;
        if let Ok(r) = r {
            if let Some(p) = r["proof_path"].as_array() {
                path_hex = Some(p.clone());
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let path: Vec<Vec<u8>> = path_hex
        .context("the proof node served no proof_path for the skip list (not a patched node, or the anchor is outside its history)")?
        .iter()
        .map(|v| hex::decode(v.as_str().unwrap_or_default()).context("proof node hex"))
        .collect::<Result<_>>()?;
    let skip = proof_from_getproofpath(&path, 0).context("skip-list proof path")?;
    if skip.leaf_index != key {
        bail!("the proof node proved a different object than the skip list we asked for");
    }
    let mut account_hash = [0u8; 32];
    account_hash.copy_from_slice(&anchor[76..108]);
    if !verify_inclusion(&skip, &account_hash) {
        bail!("the skip-list proof does not hash to anchor {anchor_seq}'s account_hash");
    }

    // Headers from the skip-hop ledger down to the target, newest first.
    let mut headers = Vec::with_capacity((hop_seq - target_seq + 1) as usize);
    for seq in (target_seq..=hop_seq).rev() {
        if seq == target_seq {
            headers.push(tx.header);
        } else {
            headers.push(
                fetch_header(
                    http,
                    src.history_url,
                    serde_json::json!({ "ledger_index": seq }),
                )
                .await
                .with_context(|| format!("header {seq}"))?,
            );
        }
    }
    if !leaf_holds_hash(&skip.leaf_data, &ledger_hash(&headers[0])) {
        bail!("anchor {anchor_seq}'s skip list does not name ledger {hop_seq}");
    }
    check_parent_chain(&headers)?;

    build_xhwf_blob(
        &anchor,
        validations,
        &skip,
        &headers,
        &tx.tx_blob,
        &tx.meta,
        &tx.inner_root_to_leaf,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live: assemble a real XHWF for a landed escrow Payment, anchored at the proof node's
    /// current validated ledger, with NO validations (the enclave-side vector test checks the
    /// chain and the tx part; the quorum is the ordinary, separately tested path). Writes the
    /// blob hex to $HIST_OUT. Run by hand:
    ///   HIST_PROOF_URL=http://127.0.0.1:15005 HIST_HISTORY_URL=https://s.altnet.rippletest.net:51234 \
    ///   HIST_TX=<hash> HIST_OUT=/path cargo test xhwf_live_fixture -- --ignored
    #[tokio::test]
    #[ignore]
    async fn xhwf_live_fixture() {
        let proof_url = std::env::var("HIST_PROOF_URL").expect("HIST_PROOF_URL");
        let history_url = std::env::var("HIST_HISTORY_URL").expect("HIST_HISTORY_URL");
        let tx = std::env::var("HIST_TX").expect("HIST_TX");
        let out = std::env::var("HIST_OUT").expect("HIST_OUT");
        let http = reqwest::Client::new();

        let t = rpc(
            &http,
            &history_url,
            "tx",
            serde_json::json!({ "transaction": tx }),
        )
        .await
        .unwrap();
        let ledger_index = t["ledger_index"].as_u64().unwrap() as u32;
        let ledger = rpc(
            &http,
            &history_url,
            "ledger",
            serde_json::json!({ "ledger_index": ledger_index, "transactions": true, "expand": true, "binary": true }),
        )
        .await
        .unwrap();
        let mut id = [0u8; 32];
        id.copy_from_slice(&hex::decode(&tx).unwrap());
        let idx =
            crate::withdrawal_reconcile::find_target_index(&ledger, &id).expect("tx in its ledger");

        let v = rpc(
            &http,
            &proof_url,
            "ledger",
            serde_json::json!({ "ledger_index": "validated" }),
        )
        .await
        .unwrap();
        let anchor_hash = v["ledger_hash"].as_str().unwrap().to_string();

        let src = HistoricalSources {
            history_url: &history_url,
            proof_url: &proof_url,
        };
        let blob = build_historical_proof(&http, &src, &anchor_hash, &[], &ledger, idx)
            .await
            .expect("XHWF builds and passes every producer-side check");
        std::fs::write(&out, hex::encode_upper(&blob)).unwrap();
        println!(
            "XHWF {} bytes, anchor {anchor_hash}, target ledger {ledger_index}",
            blob.len()
        );
    }

    #[test]
    fn skip_hop_rule_matches_the_enclave() {
        use crate::spv_proof::{skip_keylet_group, skip_keylet_recent};
        // the recent key is the well-known LedgerHashes index
        assert_eq!(
            hex::encode_upper(skip_keylet_recent()),
            "B4979A36CDC7F3D3D5C31A4EAE2AC7D7209DDA877588B9AFC66799692AB0D66B"
        );
        // live testnet 2026-11-10: the group list holding 22218496
        assert_eq!(
            hex::encode_upper(skip_keylet_group(22218496)),
            "18237E9E5060F9A98C4713F286FAD611738548549BD4305ED7B500A3C085CC5C"
        );
        // within 256 of the anchor: the target itself, recent list
        assert_eq!(
            skip_hop_for(1000, 744).unwrap(),
            (744, skip_keylet_recent())
        );
        // 257 back: the first flag ledger after it (768), still within 256 -> recent list
        assert_eq!(
            skip_hop_for(1000, 743).unwrap(),
            (768, skip_keylet_recent())
        );
        // far back: the first flag ledger, through its group list
        assert_eq!(
            skip_hop_for(22222452, 22218280).unwrap(),
            (22218496, skip_keylet_group(22218496))
        );
        // a flag-ledger target is its own hop
        assert_eq!(
            skip_hop_for(30000, 25600).unwrap(),
            (25600, skip_keylet_group(25600))
        );
        assert!(skip_hop_for(100, 100).is_err());
        assert!(skip_hop_for(100, 200).is_err());
    }
}
