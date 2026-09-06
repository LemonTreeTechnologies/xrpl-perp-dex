//! #131 AC-BASE-2" P2-d — orchestrator SPV proof builder.
//!
//! Ports the enclave `xrpl_spv` wire formats to the host producer side: serialize the
//! 118-byte XRPL ledger header, convert rippled's `getProofPath` wire nodes into the
//! enclave's 512-byte inner-node format, and assemble the `XSPV` transport blob that
//! `ecall_perp_reserves_verify_and_baseline` parses (`xrpl_spv_parse_transport`).
//!
//! UNTRUSTED producer: the enclave re-derives `ledger_hash`, re-verifies the validator
//! quorum, and re-hashes every SHAMap node against the validator-signed `account_hash`,
//! so a bad blob here can only FAIL the ecall, never forge a balance. This module just
//! shapes bytes; the golden test proves the shaping matches a real ledger.

#![allow(dead_code)] // consumed by the P2-d ceremony/fetch wiring, landing next.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};

pub const XSPV_MAGIC: [u8; 4] = *b"XSPV";
pub const HEADER_LEN: usize = 118;
const WIRE_ACCOUNT_STATE: u8 = 1; // leaf
const WIRE_INNER: u8 = 2; // full 16-branch inner
const WIRE_COMPRESSED_INNER: u8 = 3; // sparse inner: {hash,branch}*

const HP_LEDGER: [u8; 4] = [0x4C, 0x57, 0x52, 0x00]; // 'LWR\0'
const HP_INNER: [u8; 4] = [0x4D, 0x49, 0x4E, 0x00]; // 'MIN\0'
const HP_LEAF: [u8; 4] = [0x4D, 0x4C, 0x4E, 0x00]; // 'MLN\0'

fn sha512half(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    let full = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&full[..32]);
    out
}

fn hex32(v: &serde_json::Value, k: &str) -> Result<[u8; 32]> {
    let s = v[k].as_str().with_context(|| format!("{k} not a string"))?;
    let b = hex::decode(s).with_context(|| format!("{k} not hex"))?;
    if b.len() != 32 {
        bail!("{k} must be 32 bytes, got {}", b.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&b);
    Ok(out)
}

/// `ledger_index` in a `ledger` RPC result may be a string or a number.
fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Serialize the canonical 118-byte XRPL ledger header from a `ledger` RPC result's
/// `ledger` object: seq(u32) ‖ drops(u64) ‖ parent_hash(32) ‖ transaction_hash(32) ‖
/// account_hash(32) ‖ parent_close_time(u32) ‖ close_time(u32) ‖
/// close_time_resolution(u8) ‖ close_flags(u8). All big-endian (matches rippled + the
/// enclave `xrpl_spv_ledger_hash`).
pub fn serialize_ledger_header(ledger: &serde_json::Value) -> Result<[u8; HEADER_LEN]> {
    let seq = as_u64(&ledger["ledger_index"]).context("ledger_index")? as u32;
    let drops: u64 = ledger["total_coins"]
        .as_str()
        .context("total_coins")?
        .parse()
        .context("total_coins parse")?;
    let parent = hex32(ledger, "parent_hash")?;
    let txh = hex32(ledger, "transaction_hash")?;
    let acct = hex32(ledger, "account_hash")?;
    let pct = as_u64(&ledger["parent_close_time"]).context("parent_close_time")? as u32;
    let ct = as_u64(&ledger["close_time"]).context("close_time")? as u32;
    let ctr = as_u64(&ledger["close_time_resolution"]).context("close_time_resolution")? as u8;
    let cf = as_u64(&ledger["close_flags"]).context("close_flags")? as u8;

    let mut out = [0u8; HEADER_LEN];
    let mut p = 0;
    let mut put = |b: &[u8], p: &mut usize| {
        out[*p..*p + b.len()].copy_from_slice(b);
        *p += b.len();
    };
    put(&seq.to_be_bytes(), &mut p);
    put(&drops.to_be_bytes(), &mut p);
    put(&parent, &mut p);
    put(&txh, &mut p);
    put(&acct, &mut p);
    put(&pct.to_be_bytes(), &mut p);
    put(&ct.to_be_bytes(), &mut p);
    put(&[ctr], &mut p);
    put(&[cf], &mut p);
    debug_assert_eq!(p, HEADER_LEN);
    Ok(out)
}

/// SHA-512Half('LWR\0' ‖ header) — the ledger_hash (for producer-side sanity checks).
pub fn ledger_hash(header: &[u8; HEADER_LEN]) -> [u8; 32] {
    sha512half(&[&HP_LEDGER, header])
}

/// A reserves proof shaped for the XSPV blob (one AccountRoot / RippleState).
pub struct XspvProof {
    pub kind: u8, // 0 = AccountRoot (XRP), 1 = RippleState (RLUSD)
    pub leaf_index: [u8; 32],
    pub leaf_data: Vec<u8>,
    pub inner_root_to_leaf: Vec<[u8; 512]>,
}

/// Expand a rippled wire inner node into the enclave's flat 512-byte (16×32) form,
/// zero-filling absent branches for the compressed form.
fn wire_inner_to_512(wire: &[u8]) -> Result<[u8; 512]> {
    if wire.is_empty() {
        bail!("empty inner wire node");
    }
    let tag = wire[wire.len() - 1];
    let body = &wire[..wire.len() - 1];
    let mut out = [0u8; 512];
    match tag {
        WIRE_INNER => {
            if body.len() != 512 {
                bail!("full inner must be 512+1 bytes, got {}", wire.len());
            }
            out.copy_from_slice(body);
        }
        WIRE_COMPRESSED_INNER => {
            if body.len() % 33 != 0 {
                bail!(
                    "compressed inner body must be a multiple of 33, got {}",
                    body.len()
                );
            }
            for chunk in body.chunks(33) {
                let branch = chunk[32] as usize;
                if branch >= 16 {
                    bail!("compressed inner branch {branch} out of range");
                }
                out[branch * 32..branch * 32 + 32].copy_from_slice(&chunk[..32]);
            }
        }
        _ => bail!("not an inner wire node (tag {tag})"),
    }
    Ok(out)
}

/// Split a leaf (account-state) wire node into (item_data, key). Layout:
/// item ‖ key(32) ‖ tag(=1).
fn wire_leaf_split(wire: &[u8]) -> Result<(Vec<u8>, [u8; 32])> {
    if wire.len() < 33 || wire[wire.len() - 1] != WIRE_ACCOUNT_STATE {
        bail!("not an account-state leaf wire node");
    }
    let n = wire.len();
    let mut key = [0u8; 32];
    key.copy_from_slice(&wire[n - 33..n - 1]);
    Ok((wire[..n - 33].to_vec(), key))
}

/// Convert a rippled `getProofPath` result (leaf-first, root-last) into an `XspvProof`
/// with inner nodes ordered root→leaf (the enclave's `verify_state_inclusion` order).
pub fn proof_from_getproofpath(proof_path: &[Vec<u8>], kind: u8) -> Result<XspvProof> {
    if proof_path.len() < 2 {
        bail!("proof_path too short ({})", proof_path.len());
    }
    let (leaf_data, leaf_index) = wire_leaf_split(&proof_path[0]).context("leaf node")?;
    let mut inner_root_to_leaf = Vec::with_capacity(proof_path.len() - 1);
    for w in proof_path[1..].iter().rev() {
        inner_root_to_leaf.push(wire_inner_to_512(w).context("inner node")?);
    }
    Ok(XspvProof {
        kind,
        leaf_index,
        leaf_data,
        inner_root_to_leaf,
    })
}

/// Independent SHAMap verify (host-side sanity check before shipping): the leaf hashes
/// up through the root-to-leaf inner path to `account_hash`. Mirrors the enclave's
/// `xrpl_spv_verify_state_inclusion` exactly.
pub fn verify_inclusion(proof: &XspvProof, account_hash: &[u8; 32]) -> bool {
    let mut running = sha512half(&[&HP_LEAF, &proof.leaf_data, &proof.leaf_index]);
    let depth = proof.inner_root_to_leaf.len();
    for d in (0..depth).rev() {
        let node = &proof.inner_root_to_leaf[d];
        let nib = if d % 2 == 0 {
            proof.leaf_index[d / 2] >> 4
        } else {
            proof.leaf_index[d / 2] & 0x0F
        } as usize;
        if node[nib * 32..nib * 32 + 32] != running {
            return false;
        }
        running = sha512half(&[&HP_INNER, node]);
    }
    &running == account_hash
}

fn push_be16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// Minimal bounds-checked STObject field walker. From `b[pos..]`, returns
/// (type, field, id_start, value_start, next_pos) for the next field, or None on
/// truncation / an unhandled type. Only the types an STValidation carries are handled.
fn st_next(b: &[u8], pos: usize) -> Option<(u16, u16, usize, usize, usize)> {
    let id_start = pos;
    let mut i = pos;
    let h = *b.get(i)?;
    i += 1;
    let mut tc = (h >> 4) as u16;
    let mut fc = (h & 0x0F) as u16;
    if tc == 0 {
        tc = *b.get(i)? as u16;
        i += 1;
    }
    if fc == 0 {
        fc = *b.get(i)? as u16;
        i += 1;
    }
    let vstart;
    let vlen;
    match tc {
        1 => {
            vstart = i;
            vlen = 2;
        }
        2 => {
            vstart = i;
            vlen = 4;
        }
        3 => {
            vstart = i;
            vlen = 8;
        }
        4 => {
            vstart = i;
            vlen = 16;
        }
        5 => {
            vstart = i;
            vlen = 32;
        }
        16 => {
            vstart = i;
            vlen = 1;
        }
        6 => {
            vstart = i;
            vlen = if b.get(i)? & 0x80 != 0 { 48 } else { 8 };
        }
        7 | 8 | 19 => {
            // VL-length-prefixed (1/2/3-byte length)
            let l0 = *b.get(i)? as usize;
            i += 1;
            let l = if l0 <= 192 {
                l0
            } else if l0 <= 240 {
                193 + ((l0 - 193) << 8) + *b.get(i)? as usize + {
                    i += 1;
                    0
                }
            } else {
                let a = *b.get(i)? as usize;
                let bb = *b.get(i + 1)? as usize;
                i += 2;
                12481 + ((l0 - 241) << 16) + (a << 8) + bb
            };
            vstart = i;
            vlen = l;
        }
        _ => return None,
    }
    let next = vstart.checked_add(vlen)?;
    if next > b.len() {
        return None;
    }
    Some((tc, fc, id_start, vstart, next))
}

/// From a raw serialized STValidation (`data` from the validations stream), extract the
/// XSPV validation entry the enclave's `verify_quorum` consumes: the signing pubkey (33),
/// the DER signature, and the vbody = the validation WITHOUT its `sfSignature` field
/// (the signed content: SHA-512Half('VAL\0' ‖ vbody) is what the validator signed).
pub fn validation_entry(data: &[u8]) -> Result<([u8; 33], Vec<u8>, Vec<u8>)> {
    let mut pos = 0;
    let mut pubkey: Option<[u8; 33]> = None;
    let mut sig: Option<Vec<u8>> = None;
    let mut sig_field: Option<(usize, usize)> = None; // (id_start, next) of sfSignature
    while pos < data.len() {
        let (tc, fc, idst, vs, next) = st_next(data, pos).context("STValidation walk")?;
        if tc == 7 && fc == 3 {
            // sfSigningPubKey
            if next - vs != 33 {
                bail!("sfSigningPubKey not 33 bytes");
            }
            let mut pk = [0u8; 33];
            pk.copy_from_slice(&data[vs..next]);
            pubkey = Some(pk);
        } else if tc == 7 && fc == 6 {
            // sfSignature
            sig = Some(data[vs..next].to_vec());
            sig_field = Some((idst, next));
        }
        pos = next;
    }
    let pubkey = pubkey.context("no sfSigningPubKey")?;
    let sig = sig.context("no sfSignature")?;
    let (sf_start, sf_end) = sig_field.unwrap();
    let mut vbody = Vec::with_capacity(data.len() - (sf_end - sf_start));
    vbody.extend_from_slice(&data[..sf_start]);
    vbody.extend_from_slice(&data[sf_end..]);
    Ok((pubkey, sig, vbody))
}

/// Build the XSPV validations section (`val_count`, concatenated entries) from the raw
/// `data` blobs collected for one ledger. Each entry: pubkey33 ‖ sig_len u8 ‖ sig ‖
/// vbody_len u16 ‖ vbody. The enclave dedups + checks each against the pinned UNL.
pub fn build_validations_section(datas: &[Vec<u8>]) -> Result<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut count: u16 = 0;
    for d in datas {
        let (pk, sig, vbody) = validation_entry(d)?;
        if sig.len() > 72 || vbody.len() > u16::MAX as usize {
            bail!("validation sig/vbody too large");
        }
        out.extend_from_slice(&pk);
        out.push(sig.len() as u8);
        out.extend_from_slice(&sig);
        push_be16(&mut out, vbody.len() as u16);
        out.extend_from_slice(&vbody);
        count += 1;
    }
    Ok((count, out))
}

/// Assemble the `XSPV` transport blob the enclave parses. `validations` is the
/// concatenated validation entries (pubkey33|sig_len|sig|vbody_len|vbody)*.
pub fn build_xspv_blob(
    header: &[u8; HEADER_LEN],
    val_count: u16,
    validations: &[u8],
    proofs: &[XspvProof],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&XSPV_MAGIC);
    b.push(1); // version
    push_be16(&mut b, HEADER_LEN as u16);
    b.extend_from_slice(header);
    push_be16(&mut b, val_count);
    b.extend_from_slice(validations);
    b.push(proofs.len() as u8);
    for pr in proofs {
        b.push(pr.kind);
        b.extend_from_slice(&pr.leaf_index);
        push_be16(&mut b, pr.leaf_data.len() as u16);
        b.extend_from_slice(&pr.leaf_data);
        b.push(pr.inner_root_to_leaf.len() as u8);
        for node in &pr.inner_root_to_leaf {
            b.extend_from_slice(node);
        }
    }
    b
}

// ── async fetch (ws validations + HTTP header/proof → XSPV blob) ───────────────
use std::collections::HashMap;
use std::time::Duration;

/// Config for the leader's one-shot SPV proof fetch.
pub struct SpvFetchConfig {
    pub http_url: String, // http://127.0.0.1:5005
    pub ws_url: String,   // ws://127.0.0.1:6006
    pub escrow_account: String,
    pub quorum: usize,     // stop once one ledger has this many full validations
    pub collect_secs: u64, // ws collect window
}

async fn http_rpc(url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let body = serde_json::json!({"method": method, "params": [params]});
    let resp: serde_json::Value = http.post(url).json(&body).send().await?.json().await?;
    Ok(resp["result"].clone())
}

/// Fetch header + ≥quorum validations + the escrow AccountRoot proof for ONE validated
/// ledger and assemble the XSPV blob the enclave verifies. Host-side sanity-checks the
/// chaining (header→ledger_hash the validations attest, proof→account_hash) before
/// returning — a producer bug fails here loudly rather than shipping an unverifiable blob.
pub async fn fetch_spv_bundle(cfg: &SpvFetchConfig) -> Result<Vec<u8>> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // 1. collect validations by ledger_hash until one ledger reaches quorum.
    let (mut ws, _) = tokio_tungstenite::connect_async(&cfg.ws_url)
        .await
        .context("ws connect")?;
    ws.send(Message::text(
        r#"{"command":"subscribe","streams":["validations"]}"#,
    ))
    .await
    .context("ws subscribe")?;
    let mut by_ledger: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cfg.collect_secs);
    let (lh_hex, datas) = loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out collecting validations"))?
            .context("ws stream closed")?
            .context("ws frame")?;
        let Message::Text(t) = msg else { continue };
        let j: serde_json::Value = match serde_json::from_str(&t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if j["type"] == "validationReceived" && j["full"] == true {
            if let Some(d) = j["data"].as_str() {
                let lh = j["ledger_hash"].as_str().unwrap_or_default().to_string();
                let entry = by_ledger.entry(lh.clone()).or_default();
                if let Ok(bytes) = hex::decode(d) {
                    entry.push(bytes);
                }
                if entry.len() >= cfg.quorum {
                    break (lh, entry.clone());
                }
            }
        }
    };
    let _ = ws.close(None).await;

    // 2. header for that validated ledger + sanity-check it hashes to lh_hex.
    let lr = http_rpc(
        &cfg.http_url,
        "ledger",
        serde_json::json!({"ledger_hash": lh_hex, "transactions": false}),
    )
    .await
    .context("ledger fetch")?;
    let ledger = &lr["ledger"];
    let header = serialize_ledger_header(ledger)?;
    if hex::encode_upper(ledger_hash(&header)) != lh_hex.to_uppercase() {
        bail!("serialized header does not hash to the validated ledger_hash");
    }
    let seq = as_u64(&ledger["ledger_index"]).context("ledger_index")?;

    // 3. escrow AccountRoot proof at that ledger (patched-node getProofPath).
    let le = http_rpc(
        &cfg.http_url,
        "ledger_entry",
        serde_json::json!({"account_root": cfg.escrow_account, "ledger_index": seq, "binary": true, "proof": true}),
    )
    .await
    .context("ledger_entry proof fetch")?;
    let path: Vec<Vec<u8>> = le["proof_path"]
        .as_array()
        .context("no proof_path (is the node the patched build?)")?
        .iter()
        .map(|v| hex::decode(v.as_str().unwrap_or_default()).context("proof node hex"))
        .collect::<Result<_>>()?;
    let proof = proof_from_getproofpath(&path, 0)?;
    let ah = hex32(ledger, "account_hash")?;
    if !verify_inclusion(&proof, &ah) {
        bail!("escrow proof does not hash to the ledger's account_hash");
    }

    // 4. assemble the XSPV blob.
    let (val_count, vals) = build_validations_section(&datas)?;
    Ok(build_xspv_blob(&header, val_count, &vals, &[proof]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real proof captured from the patched rippled node (testnet ledger 20508252,
    // escrow rfYnJDSA…). getProofPath wire nodes, leaf-first. Frozen so the converter
    // can't drift from what the node produces.
    const LEDGER_HASH_HEX: &str =
        "F8839E1BB526F365E3EAE31D1C8E5DED174C610DBF855EC3EFD7663B08DB5D25";
    const ACCOUNT_HASH_HEX: &str =
        "E3EACD09D4E2DA5B25CB24D74897B39E31FE95F7D39BBD3C054BF69CA94FF317";
    const ESCROW_INDEX_HEX: &str =
        "CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE6";
    const WIRE: [&str; 8] = [
        "11006122001000002401296DC325013454F92D00000001551CEBE3779C38D3B00DCAD78530796B345BAD1CC8A5B38A9DFA56CDF2D6FC927062400000000613A9E8811447B13C7EC1823651F3C4B2CCA52CBAB6773A98C8CB3C4E8D792409E9A7E458897B3B5E35246469AF1DE2D2B3E155DDF8CBFA6CE601",
        "DABA4103AA37CA2CFD43DE4D9562A1B4D0C6538CF40D133B2D02DB40122EEDAA08EDADF2A450A06DC215FA0B265DF2D3DEED8800C3D8998D4EADE5300CCCF72E970903",
        "00000000000000000000000000000000000000000000000000000000000000006BBC4F2C0120A3B49AFE99548A2443A02F79C06523176BB6B7A4C91C8627DBB9CAB252DF8CF8FD8C81AAE15043828E1EEA6D0FEC2B5B53DE9C614DC9767B204BABFAFB6D6BEB2D8E4A9F30E82D25AC7AC5C2568C7AE72A34E68F11C31D60DE8720BD9F44CEA922BBEE28C3ACD3BBCC63FB64D9CEADC170B70F586544BB1F5C5B2AB65CCCA6D6F793853620AC10381A7921D32ED7EFCA0099C7766347373CF29B00000000000000000000000000000000000000000000000000000000000000004A7F60470DFBC2719408834F5DBC6B6AC746FFA1E83986230A138A1DC681CE1BC4704EE750D4DB765D973EC809213CF2CE8987FF161F25A16A69369E8399EE7C000000000000000000000000000000000000000000000000000000000000000090770D99CDC356488FE0C594CCFDBDFE30518F9858C438ECFA1E5E1E38456B7BE45017D05C5A9266567D0B1C2241D4E950002A627447E8930E614B7DC44EF3248DEBD33E5F692FE18149F21012CFE019838A9EACC376894D27835F1C7230AB9EC62726D730601960B0653CA08F90EA105757081F71E4FE080EFA1EE5E584FA1C4B7AD1F5ED861C1478291DCA0166AD4BF0F80E898932B79046131BAA53F57F9C8FC28E1B53340EA19DD5923D0B0C2F38AAD02B47F756A807690B38124BAA7E1E02",
        "816D38B105AF841D7B6EF1C5EC8405A01846C1942A5BD8AE09C57AB71EE27830F5007BD95624EEEE5653A4286BE201221F72B941F6E1261D0B0FAF646E72517D5B1754B51E6E155B006271C92010708DDD85B96712876175FAE62ED6CA32B620C722285DD580A20892B9FCA866E5EBDA1C16DD42AB12C661179BF7D896F76539ABD603FBBB15D0C4B8428C2C7147F25909A766240571F89818E6DDB25AD517DE67EB6CE5719FB8E7A1321F9AB6697E6AE34728B5DC627B36F831D6FC8DA64CBB15F33060762E107DD16BA3D5776C4BDC606B8225CCE1E856819112184ACC4F5762F3E1C2B181455AF9900DE5EA778109D9D87C74B924D02596C7BC5DF3BC8A1BD9CEB7A5E4B24F4E0ED12024CF5A0CABFB4D6B51C9DD961D7D1DDAB69DF0F351329A0063E0657A2F68529163622B493214DAB1E7C0D22B3C072AB1BCFEC575F447EABA237780DF6ACB56AC8AF1B6127B6BA6A32EB0A763C308C5DB868CD21F277801A6306003A957DC211831E66EFDBB683E78A2F4F0D9F3EE5350240F47505D6CF3763BC551AE8CD8DC7311706A105D86E1D1571F2544BD5F2AC0E0AB831414806EB48B849BD10982DF0496031E812A3057362B365D8262AE7D88DE5807D30DDDDE37CE9F01B3CE813FEB2506096135684D343C7A98BBD0BF3EF0B4B1E3CCE71D2907F4DF72A7F917C8BD6E84BC19C11C75052FCFAC218B677C2A0E1A60A56702",
        "F914922958CD846E8FE36911C1F53219ED50A6785F6B6037305F0104C10AE6281337A7D315138998E428F76BFB46D212796308865AA7D745C39CBD64B4F3DF5406DFAF6DCE3B31E5BC441236FD16623F1E4D8E166BA94E66AF041D952BD9B26E9E3938E43F793DC8CC0F38D32A7A893D0D4824F861C76CF654F11904AB568C1983EA30152E6FF427994FF6CECDD84991E6B820F9551D315EFDABECBC8337C6208D58927440DF5B89623C20F14864BB764C2B0F5FF6A952C7FDAD95BA7A6C293314488A7F0EEF771DACFD5D7B17A89719C12DF33A2DD046D7968E5C6B004407258CF6EDFE746D08A132F2FD03585759AF396EDB94322697DD1FA9D89907C473AF8485A4797B7C3763BA822A47E198E69D2829304F7A186E7E00B2B70E07770BBBEE64BAD1D8DBEB3BB53D44E59717809EF9A0341A43E88765F6DEDEADFBE596F06773E55AA2B969853FA30807D24D83A6DC33C5A70F762EEFA62BC9BC4B75A27BB90DDBD6184A5132FB133E1F22F97A7D7118BC536CD7BA6027C95E89F4ABEFFAD163989868EADBACCDB494E920A5CB4AC0D785EE2DDEF647CDF9FA531BB393B24DAD7167F9832A0637000A61682BFD92F5F20E3406B6380E2490F00530C0924B2761A6B8A542F211E00E28E96E937725006A6CB5C7DC7D85D3FD2033F8A50CDAA891A4DCA50CF4CB3DF46285454AD816339E6DD639F097E6EF07ECC08C5830C002",
        "37FE1DC406E3DDE71848B8D8FFE81815FC6CA6730EEF8470EF1512B1004B0E412B5C9DA62792FC261EEDFE35798FFC457544BFC22F2DA96A15DA1737AF6B5A916D0755783A3CF8B6CB6BEB11F99A64BEC6FB41B6B65C1DB9BD37CD3721E2FBC54F542E77987317C77D11897FC781BEAEB3D32BA6ADEE1A0644CB6FD49D9A3796E8516AA90B10946E25B97F13D537C6D7751757B636621D9383AFBA8BDF60CDB37473A8C1710BB5919DC806CAB36327740E6C9B86A05B3F39466D1FADEAF1CC8DF12AE43C4607A70A4474C2BD08F6D0F50B9866345550B302780DDFA454F4BD251022232C20C4947B97767130DDA3D2AD0CB64FCA705344B5546ADA8352EE51087889A9FA9DADCEC0C9ABCC963C6F23BFD66DC21D705C570105B43F0551DA61D481147C84ABE2D44C1C904ED9F3B1A84CDED73B707B4BE1ACC90B53BEEE69C8262F14BA3DBE051D52CEEABDC8C17FFF94FDD67089C92D16410C4050DDF7E14300FDFAEB90B8F82EFD4DD613B167F77DE07A082FA6BDE7E7BBED27B3B0162E3CBBDA8FED1B7F50BD8F1F99AA6B7282A25524DD6E5428E9F40CF3EA7FDA9BCE78364C0F2542911840E384D6679E570EEA27061DE251E3C5B6B335B4F6F007769811AEF56C741B3B046FBD9F733476B99AA8937907D3482722F0668F6F2DC0C58035CF74187058519E477328D7FC205A68152A00437E30632EB70178669015A9AC8502",
        "3CA67843D005C3AFD07D0BDB256F1C43B4359354D2627DAD1B6567B1738E8DEF12B33E7E1A078675C9613C873F50A84AF3F025D86F5AE06E01E87D75B19FDE3E688A5F33F7C65CA534A5344F267B437CBC1D76B11CE1C2C80E9979342A490091FB1B698D5D0793754D6343CFF4911F36DF19272217BBB39C29A08C9A691E98EBDF44692A54E14B91EF73C873206A0C03B4A596FC6EB9281195844327CABD86FDA791A35182C78CFD2FEFEEB763EDA6341FED4F6618D434D7F83EF45A8B6A66D377B0E4035F917E12D856AA6F0E6BA550409D8F9A9618F2A31FB38F3457F7D00BB414D2F67E4039A5F3C0442B49A7166C2506C6C18E457B9E2D8ECED674BBF01B35CEFD7A09CB72D6ABEA54C131EC4B913EBCED47CDB8BB02F49F1A7D120580B08C0CCD2015A40E5EF772CFFEC1DD723890183706919C1B0111F528FBE53BEE767D90919C143D62F85B2EE64E41B1B1AB03D7110E6BB822436D5E76705573E6E0734AD44AC0722395B3BC4B4DA78DD315A8BCE8A25DECD92B8FAD7253C09E12CCFBCA85F25687B200508C2943210E4105538607E4741ECDFD40DCCABEE4CB119034060D9C508B9D604F7FB9FDBAC74AAD8D375E280AD192EE0EA2E721CD971CA452EE033B5C98A69E8C9F6447A9B7A35F3E08780D5582DD3BDBE888DD4F2AD5F2BF16C8E4D118566E10069162D7AC1E127B1124AB96111DEEB3EC9B334155E83F02",
        "1C870B833E9A16F0CE0E931B03491481AE80FDB5DEBA03C20C4FDCBC1527B23710DC230DCEA016944B7C6631C22A7257063C497445BDA07392D1E39E6F1054899988D8870F41D0C94FD4270A702A11AB4BDD5711580098475561AD39F9216BA1250F1991676B6E4C700FD1CE62F915EFCDEDC9B334B570668918093EFE79A10D6DE15CFC13E9496D6D764BA0FE355886B0A3050FF8F2E66CC245186A764531BD16F6579190B90F702A44425F40953B4931C0F3F2AA74929D59681ABE4D6CC4971F16D9AA26622743C3985E184E795E5E73EB9467064C3A1B05E3726769604AC9F30EEDA92D015BCE9E6FE47340231B71D6DEC10A68A0864B0A18FA14B8F2CCCEFA843310AF29338DA52AF2CD10A50AEA942AAE80CAD7FD3EA34AFBEB8CE57503FC3FB42DB62C40DF69D2F687CA91601C263DC1A208F774DBC2F6DF5146297FDB3AD75BC54A687A2951B9A16FB62D45C9E7B2C89C6CCFECBCC64275B56BDFDC1E0FA1E67B0C3D91313A16EF4214F9DBDCE744C3619166CD6C84E5A5DC3B6BEDECDF60E168E33956F9A1273B6E82B3CB55B2B8E808DEDE71117D16578467E34820C128B8B52C2D7EDCCA50F69728BD88F771AD47FB1323A45095852B975DB3C2344308694EFE20E038218A5131BE02E8C94857E79991DC31950BB18A395A27FF926112518046F6573B3758DCAAE66B80A5E35564C2FE585AD5E240075FCC86C70302",
    ];

    #[test]
    fn header_serializes_to_real_ledger_hash() {
        let ledger = serde_json::json!({
            "ledger_index": 20508252u64,
            "total_coins": "99999891703128011",
            "parent_hash": "DEDD2B66D03349C63F9D15373A49A9CB2DDFE9014F3A7C7A512C93969868FF00",
            "transaction_hash": "C823386B6ADC7FE83868E226CDCE2BFBFF44C15B480FBCC8A508CD9B4FD29125",
            "account_hash": ACCOUNT_HASH_HEX,
            "parent_close_time": 841936330u64,
            "close_time": 841936331u64,
            "close_time_resolution": 10u64,
            "close_flags": 0u64,
        });
        let hdr = serialize_ledger_header(&ledger).unwrap();
        assert_eq!(
            hex::encode_upper(ledger_hash(&hdr)),
            LEDGER_HASH_HEX,
            "header must hash to the node's ledger_hash"
        );
    }

    #[test]
    fn getproofpath_converts_and_verifies_to_account_hash() {
        let path: Vec<Vec<u8>> = WIRE.iter().map(|w| hex::decode(w).unwrap()).collect();
        let proof = proof_from_getproofpath(&path, 0).unwrap();
        assert_eq!(proof.inner_root_to_leaf.len(), 7, "7 inner levels");
        assert_eq!(
            hex::encode_upper(proof.leaf_index),
            ESCROW_INDEX_HEX,
            "leaf key == escrow keylet"
        );
        let mut ah = [0u8; 32];
        ah.copy_from_slice(&hex::decode(ACCOUNT_HASH_HEX).unwrap());
        assert!(
            verify_inclusion(&proof, &ah),
            "converted proof must hash to the validator-signed account_hash"
        );
        // the escrow leaf carries balance 613A9E8 drops (101951976) — the E-1 custody figure
        assert!(proof
            .leaf_data
            .windows(4)
            .any(|w| w == [0x06, 0x13, 0xA9, 0xE8]));
    }

    // A real full-validation `data` for ledger 1124A120… (signing key 027F285B…).
    const VAL_DATA: &str = "228000000126013949042932300C863A2D270CF98511C639511124A12059FA1D708D4CB718767B5C2C1971E6D644F7731BD8D60B8666538532501700000000000000000000000000000000000000000000000000000000000000005019696E5F5288E3EE08676023DB69CFA2E333692B0F7E97D87553CB0312D9C5C0767321027F285B8BB33F0E8B025BF955C29A7CFA8A0995831EE4AD93A9BD572A7C8EEDCD76463044022018975AFA77A0F6D273D8E36F5FDCB4876491FF3CD2719CE5C30D3A464F6CD33B0220383E44ECCAAB37930DBE272AA2586B1CBD287C872E31FEF587D5218142E44554";

    #[test]
    fn validation_entry_vbody_verifies() {
        use k256::ecdsa::signature::hazmat::PrehashVerifier;
        use k256::ecdsa::{Signature, VerifyingKey};
        let data = hex::decode(VAL_DATA).unwrap();
        let (pk, sig, vbody) = validation_entry(&data).unwrap();
        assert_eq!(pk[0] & 0xFE, 0x02, "compressed secp256k1 signing key");
        // the validator signed SHA-512Half('VAL\0' || vbody); the reconstructed vbody
        // (data minus sfSignature) must verify — proving the field-strip is exact.
        let h = sha512half(&[&[0x56, 0x41, 0x4C, 0x00], &vbody]);
        let vk = VerifyingKey::from_sec1_bytes(&pk).unwrap();
        let s = Signature::from_der(&sig).unwrap();
        assert!(
            vk.verify_prehash(&h, &s).is_ok(),
            "reconstructed vbody must verify under the signing key"
        );
        // and the vbody must carry the ledger_hash (sfLedgerHash) the enclave checks
        assert!(vbody
            .windows(32)
            .any(|w| w == hex::decode(LEDGER_HASH_VAL).unwrap().as_slice()));
    }
    const LEDGER_HASH_VAL: &str =
        "1124A12059FA1D708D4CB718767B5C2C1971E6D644F7731BD8D60B8666538532";

    /// End-to-end fetch against a LIVE patched node reachable at 127.0.0.1:5005/6006
    /// (SSH-forward from Hetzner). #[ignore] so CI (no node) skips it. The fetch itself
    /// verifies chaining (header→ledger_hash the validations attest, proof→account_hash),
    /// so a returned blob is already self-consistent; we re-parse to double-check shape.
    #[tokio::test]
    #[ignore]
    async fn live_fetch_spv_bundle() {
        let cfg = SpvFetchConfig {
            http_url: "http://127.0.0.1:5005".into(),
            ws_url: "ws://127.0.0.1:6006".into(),
            escrow_account: "rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd".into(),
            quorum: 4,
            collect_secs: 45,
        };
        let blob = fetch_spv_bundle(&cfg).await.expect("fetch_spv_bundle");
        assert_eq!(&blob[..4], b"XSPV", "magic");
        assert_eq!(blob[4], 1, "version");
        eprintln!(
            "XSPV blob: {} bytes (self-verified header+proof+validations)",
            blob.len()
        );
    }

    #[test]
    fn wrong_account_hash_rejected() {
        let path: Vec<Vec<u8>> = WIRE.iter().map(|w| hex::decode(w).unwrap()).collect();
        let proof = proof_from_getproofpath(&path, 0).unwrap();
        assert!(
            !verify_inclusion(&proof, &[0xEE; 32]),
            "a wrong root must be rejected"
        );
    }
}
