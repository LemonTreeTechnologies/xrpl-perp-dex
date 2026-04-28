//! CLI subcommands for operator onboarding and escrow setup.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::xrpl_signer;

/// O-L3: resolve an escrow seed from either an inline CLI arg
/// (deprecated — visible to `ps`) or a file. The file mode is
/// checked on unix: if anything outside the owner has access we
/// warn loudly so operators notice during the ceremony.
///
/// Exactly one of `seed` / `seed_file` must be set; clap already
/// enforces the conflicts-with rule at parse time, but we re-check
/// here for defence in depth.
pub fn resolve_escrow_seed(seed: Option<&str>, seed_file: Option<&Path>) -> Result<String> {
    match (seed, seed_file) {
        (Some(s), None) => {
            warn!(
                "escrow seed passed via --escrow-seed (argv); it is visible to every local \
                user via `ps`. Use --escrow-seed-file for future ceremonies."
            );
            Ok(s.trim().to_string())
        }
        (None, Some(path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::metadata(path)
                    .with_context(|| format!("cannot stat {}", path.display()))?;
                let mode = meta.mode() & 0o777;
                if mode & 0o077 != 0 {
                    warn!(
                        path = %path.display(),
                        mode = format!("{mode:o}"),
                        "escrow seed file is not 0600. `chmod 0600` before the next ceremony."
                    );
                }
            }
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let first = content
                .lines()
                .next()
                .ok_or_else(|| anyhow::anyhow!("escrow seed file is empty: {}", path.display()))?
                .trim();
            if first.is_empty() {
                anyhow::bail!("escrow seed file first line is empty: {}", path.display());
            }
            Ok(first.to_string())
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("--escrow-seed and --escrow-seed-file are mutually exclusive");
        }
        (None, None) => {
            anyhow::bail!("one of --escrow-seed or --escrow-seed-file is required");
        }
    }
}

// ── XRPL binary serialization ──────────────────────────────────
//
// Minimal implementation covering SignerListSet and AccountSet.
// Reference: https://xrpl.org/serialization.html

const HASH_PREFIX_TX_SIGN: [u8; 4] = [0x53, 0x54, 0x58, 0x00]; // "STX\0"

const XRPL_ALPHABET: &[u8; 58] = b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";

fn xrpl_alphabet() -> &'static bs58::Alphabet {
    static ALPHA: std::sync::OnceLock<bs58::Alphabet> = std::sync::OnceLock::new();
    ALPHA.get_or_init(|| bs58::Alphabet::new(XRPL_ALPHABET).expect("valid alphabet"))
}

fn decode_xrpl_address(address: &str) -> Result<[u8; 20]> {
    let decoded = bs58::decode(address)
        .with_alphabet(xrpl_alphabet())
        .into_vec()
        .context("invalid XRPL address encoding")?;
    if decoded.len() != 25 || decoded[0] != 0x00 {
        anyhow::bail!("invalid XRPL address: wrong length or prefix");
    }
    let mut account_id = [0u8; 20];
    account_id.copy_from_slice(&decoded[1..21]);
    Ok(account_id)
}

fn encode_field_id(type_code: u8, field_code: u8) -> Vec<u8> {
    match (type_code < 16, field_code < 16) {
        (true, true) => vec![(type_code << 4) | field_code],
        (true, false) => vec![type_code << 4, field_code],
        (false, true) => vec![field_code, type_code],
        (false, false) => vec![0, type_code, field_code],
    }
}

fn encode_vl_length(len: usize) -> Vec<u8> {
    if len <= 192 {
        vec![len as u8]
    } else {
        let adj = len - 193;
        vec![193 + (adj >> 8) as u8, (adj & 0xff) as u8]
    }
}

struct XrplField {
    type_code: u8,
    field_code: u8,
    data: Vec<u8>,
}

impl XrplField {
    fn uint16(field_code: u8, val: u16) -> Self {
        Self {
            type_code: 1,
            field_code,
            data: val.to_be_bytes().to_vec(),
        }
    }
    fn uint32(field_code: u8, val: u32) -> Self {
        Self {
            type_code: 2,
            field_code,
            data: val.to_be_bytes().to_vec(),
        }
    }
    fn amount_drops(field_code: u8, drops: u64) -> Self {
        Self {
            type_code: 6,
            field_code,
            data: (0x4000000000000000u64 | drops).to_be_bytes().to_vec(),
        }
    }
    fn blob(field_code: u8, bytes: &[u8]) -> Self {
        let mut data = encode_vl_length(bytes.len());
        data.extend_from_slice(bytes);
        Self {
            type_code: 7,
            field_code,
            data,
        }
    }
    fn account_id(field_code: u8, id: &[u8; 20]) -> Self {
        let mut data = vec![20u8];
        data.extend_from_slice(id);
        Self {
            type_code: 8,
            field_code,
            data,
        }
    }
    fn sort_key(&self) -> (u8, u8) {
        (self.type_code, self.field_code)
    }
    fn serialize(&self) -> Vec<u8> {
        let mut out = encode_field_id(self.type_code, self.field_code);
        out.extend_from_slice(&self.data);
        out
    }
}

fn serialize_signer_entries(entries: &[([u8; 20], u16)]) -> Vec<u8> {
    let mut out = encode_field_id(15, 4); // STArray SignerEntries
    for (account_id, weight) in entries {
        out.extend_from_slice(&encode_field_id(14, 11)); // STObject SignerEntry (field 11)
                                                         // Inner fields sorted: UInt16(1,3) then AccountID(8,1)
        out.extend_from_slice(&encode_field_id(1, 3)); // SignerWeight
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&encode_field_id(8, 1)); // Account
        out.push(20);
        out.extend_from_slice(account_id);
        out.push(0xe1); // ObjectEndMarker
    }
    out.push(0xf1); // ArrayEndMarker
    out
}

fn serialize_fields(fields: &mut [XrplField], array_suffix: Option<&[u8]>) -> Vec<u8> {
    fields.sort_by_key(|f| f.sort_key());
    let mut out = Vec::new();
    for f in fields.iter() {
        out.extend_from_slice(&f.serialize());
    }
    if let Some(suffix) = array_suffix {
        out.extend_from_slice(suffix);
    }
    out
}

fn sign_xrpl_tx(
    keypair: &XrplKeypair,
    fields: &mut Vec<XrplField>,
    array_suffix: Option<&[u8]>,
) -> Result<Vec<u8>> {
    use sha2::Digest;

    let pubkey_bytes = keypair.pubkey_bytes();
    fields.push(XrplField::blob(3, &pubkey_bytes)); // SigningPubKey

    let mut signing_data = HASH_PREFIX_TX_SIGN.to_vec();
    signing_data.extend_from_slice(&serialize_fields(fields, array_suffix));

    let sig_bytes = match &keypair.key {
        XrplSigningKey::Secp256k1(_) => {
            let hash = sha2::Sha512::digest(&signing_data);
            let hash_half: [u8; 32] = hash[..32].try_into().unwrap();
            keypair.sign_hash(&hash_half)?
        }
        XrplSigningKey::Ed25519(sk) => {
            use ed25519_dalek::Signer;
            sk.sign(&signing_data).to_bytes().to_vec()
        }
    };

    fields.push(XrplField::blob(4, &sig_bytes)); // TxnSignature
    Ok(serialize_fields(fields, array_suffix))
}

// ── node-bootstrap ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignerEntry {
    name: String,
    enclave_url: String,
    address: String,
    session_key: String,
    compressed_pubkey: String,
    xrpl_address: String,
    /// 33-byte ECDH identity public key, hex-encoded uppercase, no `0x`
    /// prefix. Populated by `node-bootstrap`. Mirrors what the operator
    /// publishes on chain via `AccountSet.Domain` per
    /// `docs/multi-operator-architecture.md` §6.2.
    /// Optional for backward-compatibility with pre-Phase-2.1c entry
    /// files that lacked the field; consumers that need the ECDH pubkey
    /// must fall back to a live `AccountInfo` query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ecdh_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GenerateResponse {
    status: String,
    address: Option<String>,
    public_key: Option<String>,
    session_key: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // JSON deserialization target; fields mirror enclave wire format
struct PoolStatusResponse {
    status: String,
    accounts: Option<Vec<PoolAccount>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // JSON deserialization target; fields mirror enclave wire format
struct PoolAccount {
    address: String,
    is_active: bool,
}

/// Phase 2.1c-A — `node-bootstrap` subcommand. Runs locally on a single
/// node by its operator. Generates a fresh XRPL keypair inside the
/// enclave, fetches the enclave's ECDH identity public key, and
/// optionally publishes the ECDH pubkey on chain via `AccountSet.Domain`
/// per `docs/multi-operator-architecture.md` §6.2.
///
/// Single-mode: the same code path runs on testnet (typically with
/// `--faucet-url` to auto-fund the new account) and mainnet (operator
/// pre-funds their account from their own XRP holdings). Domain
/// publication is opt-in via `--publish-domain` so the keypair-only
/// flow remains usable for the orchestrator's own startup needs.
///
/// Replaces the older `operator-setup` subcommand. Output entry JSON
/// gains an `ecdh_pubkey` field; existing consumers (`config_init`,
/// `operator_add`) treat it as optional for backward compatibility.
pub async fn node_bootstrap(
    enclave_url: &str,
    name: &str,
    output: Option<&std::path::Path>,
    publish_domain: bool,
    xrpl_url: Option<&str>,
    faucet_url: Option<&str>,
) -> Result<()> {
    // O-L4: operator tooling talks to the local enclave — enforce
    // loopback and reuse the shared factory so that remains
    // true if someone repoints `--enclave-url` at an attacker host.
    crate::http_helpers::ensure_loopback_url(enclave_url)
        .context("node_bootstrap requires a loopback enclave URL (O-L4)")?;
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))?;

    if publish_domain && xrpl_url.is_none() {
        anyhow::bail!("--publish-domain requires --xrpl-url");
    }

    println!("Node Bootstrap");
    println!("==============");
    println!("Enclave: {enclave_url}");
    println!("Name:    {name}");
    if publish_domain {
        println!("XRPL:    {} (publishing Domain)", xrpl_url.unwrap());
        if let Some(f) = faucet_url {
            println!("Faucet:  {f}");
        }
    }
    println!();

    // Step 1: Generate a new keypair in the enclave
    println!("[1/4] Generating keypair in enclave...");
    let resp: GenerateResponse = http
        .post(format!("{enclave_url}/pool/generate"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .context("failed to reach enclave /pool/generate")?
        .json()
        .await
        .context("invalid JSON from /pool/generate")?;

    if resp.status != "success" {
        anyhow::bail!(
            "enclave /pool/generate failed: {}",
            resp.message.unwrap_or_default()
        );
    }

    let eth_address = resp.address.context("missing address in response")?;
    let uncompressed_pubkey = resp.public_key.context("missing public_key in response")?;
    let session_key = resp
        .session_key
        .context("missing session_key in response")?;

    println!("  Ethereum address: {eth_address}");
    println!("  Session key:      {session_key}");

    // Step 2: Derive XRPL address from uncompressed pubkey
    println!("\n[2/4] Deriving XRPL address...");
    let xrpl_address = xrpl_signer::pubkey_to_xrpl_address(&uncompressed_pubkey)?;

    let compressed_hex = {
        let raw = hex::decode(
            uncompressed_pubkey
                .strip_prefix("0x")
                .unwrap_or(&uncompressed_pubkey),
        )?;
        let compressed = xrpl_signer::compress_pubkey(&raw)?;
        hex::encode_upper(&compressed)
    };

    println!("  XRPL address:     {xrpl_address}");
    println!("  Compressed pubkey: {compressed_hex}");

    // Step 3: Fetch ECDH identity pubkey from the same enclave.
    println!("\n[3/4] Fetching ECDH identity pubkey...");
    let ecdh_pubkey = fetch_ecdh_pubkey(&http, enclave_url).await?;
    println!("  ECDH pubkey:      {ecdh_pubkey}");

    // Step 4 (optional): publish Domain on chain.
    if publish_domain {
        println!("\n[4/4] Publishing AccountSet.Domain on XRPL...");
        let xrpl_url = xrpl_url.unwrap();
        if let Some(f) = faucet_url {
            faucet_fund(f, &xrpl_address).await?;
        }
        let signing_pubkey =
            hex::decode(&compressed_hex).context("invalid compressed_pubkey hex")?;
        let tx_hash = submit_domain_account_set(
            xrpl_url,
            enclave_url,
            &eth_address,
            &session_key,
            &xrpl_address,
            &signing_pubkey,
            &ecdh_pubkey,
        )
        .await?;
        println!("  TX hash: {tx_hash}");
    } else {
        println!("\n[4/4] Skipped Domain publish (no --publish-domain)");
    }

    // Step 5: Output signer entry
    let entry = SignerEntry {
        name: name.to_string(),
        enclave_url: enclave_url.to_string(),
        address: eth_address,
        session_key,
        compressed_pubkey: compressed_hex,
        xrpl_address: xrpl_address.clone(),
        ecdh_pubkey: Some(ecdh_pubkey),
    };

    let json = serde_json::to_string_pretty(&entry)?;

    println!("\nSigner entry:");
    println!("{json}");

    if let Some(path) = output {
        std::fs::write(path, &json)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("\nWritten to {}", path.display());
    }

    println!("\nNext steps:");
    println!("  1. Add this entry to signers_config.json");
    if !publish_domain {
        println!("  2. Re-run with --publish-domain --xrpl-url <url> once the account is funded,");
        println!("     OR submit the AccountSet.Domain transaction by other means.");
    }
    println!("  3. Verify on XRPL explorer: https://testnet.xrpl.org/accounts/{xrpl_address}");

    Ok(())
}

/// Fetches the enclave's ECDH identity public key. The enclave returns
/// `{pubkey: "0x<33-byte hex>"}` per `/v1/pool/ecdh/pubkey`.
async fn fetch_ecdh_pubkey(http: &reqwest::Client, enclave_url: &str) -> Result<String> {
    let resp: serde_json::Value = http
        .get(format!("{enclave_url}/pool/ecdh/pubkey"))
        .send()
        .await
        .context("failed to reach /pool/ecdh/pubkey")?
        .json()
        .await
        .context("invalid JSON from /pool/ecdh/pubkey")?;
    let pk = resp["pubkey"]
        .as_str()
        .context("missing pubkey field on /pool/ecdh/pubkey response")?;
    // Use removeprefix-equivalent: strip exactly "0x" if present, do NOT
    // use lstrip("0x") which strips any combination of {0,x} chars and
    // silently corrupts pubkeys that happen to start with `0` after the
    // prefix. See `feedback_dkg_cross_machine_bug.md` foot-gun #1.
    let trimmed = pk.strip_prefix("0x").unwrap_or(pk).to_uppercase();
    if trimmed.len() != 66 {
        anyhow::bail!(
            "ECDH pubkey has unexpected length: got {} hex chars, expected 66 (33 bytes)",
            trimmed.len()
        );
    }
    Ok(trimmed)
}

/// Faucet-fund an XRPL account on networks that have a faucet (testnet,
/// devnet). On mainnet there is no faucet — operators fund their own
/// accounts via other means and skip this call by omitting --faucet-url.
async fn faucet_fund(faucet_url: &str, address: &str) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = http
        .post(faucet_url)
        .json(&serde_json::json!({ "destination": address }))
        .send()
        .await
        .with_context(|| format!("faucet request to {faucet_url} failed"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("faucet returned HTTP {status}: {body}");
    }
    println!("  faucet OK ({status})");
    // Faucet-funded accounts take a few ledgers to validate. Wait a bit
    // before submitting AccountSet, otherwise we can race ahead and get
    // `actNotFound`.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    Ok(())
}

/// Builds an AccountSet transaction setting `Domain` to the structured
/// value `xperp-ecdh-v1:<33-byte-pubkey-hex>` (per
/// `docs/multi-operator-architecture.md` §3.3), signs it via the
/// operator's enclave-bound key, and submits via XRPL JSON-RPC.
/// Returns the on-chain tx hash.
async fn submit_domain_account_set(
    xrpl_url: &str,
    enclave_url: &str,
    eth_address: &str,
    session_key: &str,
    xrpl_address: &str,
    signing_pubkey: &[u8],
    ecdh_pubkey_hex: &str,
) -> Result<String> {
    use sha2::Digest;

    if signing_pubkey.len() != 33 {
        anyhow::bail!(
            "signing_pubkey must be 33-byte compressed secp256k1, got {} bytes",
            signing_pubkey.len()
        );
    }

    let domain_value = encode_domain_v1(ecdh_pubkey_hex);
    let domain_bytes = domain_value.as_bytes();
    if domain_bytes.len() > 256 {
        anyhow::bail!("Domain payload exceeds XRPL's 256-byte limit");
    }

    let account_id = decode_xrpl_address(xrpl_address)?;

    // Account must exist on chain — read its current Sequence.
    let account_info = fetch_account_info(xrpl_url, xrpl_address).await?;
    let sequence = account_info.sequence;

    // Build canonical AccountSet fields. Field codes per XRPL spec:
    //   TransactionType = 2 (UInt16),  AccountSet = 3
    //   Sequence        = 4 (UInt32)
    //   Fee             = 8 (Amount drops)
    //   Domain          = 7 (Blob)
    //   Account         = 1 (AccountID)
    //   SigningPubKey   = 3 (Blob)
    //   TxnSignature    = 4 (Blob)
    let mut fields = vec![
        XrplField::uint16(2, 3),
        XrplField::uint32(4, sequence),
        XrplField::amount_drops(8, 12),
        XrplField::blob(7, domain_bytes),
        XrplField::account_id(1, &account_id),
        XrplField::blob(3, signing_pubkey),
    ];

    // Compute the signing hash: SHA-512Half(STX\0 || canonical(fields))
    fields.sort_by_key(|f| f.sort_key());
    let mut signing_data = HASH_PREFIX_TX_SIGN.to_vec();
    for f in &fields {
        signing_data.extend_from_slice(&f.serialize());
    }
    let h = sha2::Sha512::digest(&signing_data);
    let mut signing_hash = [0u8; 32];
    signing_hash.copy_from_slice(&h[..32]);

    // Ask the enclave to sign.
    let sig_der = sign_via_enclave(enclave_url, eth_address, session_key, &signing_hash).await?;

    // Append TxnSignature, re-serialize, submit.
    fields.push(XrplField::blob(4, &sig_der));
    fields.sort_by_key(|f| f.sort_key());
    let mut blob = Vec::new();
    for f in &fields {
        blob.extend_from_slice(&f.serialize());
    }

    let result = submit_tx_blob(xrpl_url, &blob).await?;
    let engine = result["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");
    if !engine.starts_with("tes") {
        anyhow::bail!("AccountSet failed: engine_result={engine}: {result}");
    }
    let hash = result["result"]["tx_json"]["hash"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    Ok(hash)
}

/// Encodes the Domain field per `docs/multi-operator-architecture.md`
/// §3.3: ASCII `xperp-ecdh-v1:` + lowercase hex of the 33-byte ECDH
/// public key. The result is intended to be stored as raw bytes in the
/// XRPL `Domain` field (which the ledger treats as opaque hex of up to
/// 256 bytes). The format is the runtime discovery contract — peers
/// query each operator's `AccountInfo`, parse `Domain`, strip the
/// prefix, and hex-decode 33 bytes.
fn encode_domain_v1(ecdh_pubkey_hex: &str) -> String {
    format!("xperp-ecdh-v1:{}", ecdh_pubkey_hex.to_lowercase())
}

/// Reverse of `encode_domain_v1`. Used by `node-config-apply` (Phase
/// 2.1c-C) and any other consumer that learns peer ECDH pubkeys via
/// XRPL `AccountInfo`. Bails on prefix mismatch or wrong-length hex.
#[allow(dead_code)]
pub fn decode_domain_v1(domain_bytes: &[u8]) -> Result<[u8; 33]> {
    let s = std::str::from_utf8(domain_bytes).context("Domain is not UTF-8")?;
    let hex_part = s
        .strip_prefix("xperp-ecdh-v1:")
        .context("Domain missing 'xperp-ecdh-v1:' prefix")?;
    let bytes = hex::decode(hex_part).context("Domain hex part failed to decode")?;
    if bytes.len() != 33 {
        anyhow::bail!(
            "Domain ECDH pubkey has wrong length: got {} bytes, expected 33",
            bytes.len()
        );
    }
    let mut out = [0u8; 33];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Asks the operator's enclave to ECDSA-sign a 32-byte hash with the
/// account-bound key, returns the DER-encoded signature.
async fn sign_via_enclave(
    enclave_url: &str,
    eth_address: &str,
    session_key: &str,
    hash: &[u8; 32],
) -> Result<Vec<u8>> {
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))?;
    let resp: serde_json::Value = http
        .post(format!("{enclave_url}/pool/sign"))
        .json(&serde_json::json!({
            "from": eth_address,
            "hash": format!("0x{}", hex::encode(hash)),
            "session_key": session_key,
        }))
        .send()
        .await
        .context("/pool/sign failed")?
        .json()
        .await
        .context("invalid JSON from /pool/sign")?;
    if resp["status"].as_str() != Some("success") {
        anyhow::bail!("/pool/sign rejected: {resp}");
    }
    let r_hex = resp["signature"]["r"]
        .as_str()
        .context("missing r in /pool/sign response")?;
    let s_hex = resp["signature"]["s"]
        .as_str()
        .context("missing s in /pool/sign response")?;
    let r = hex::decode(r_hex).context("bad r hex")?;
    let s = hex::decode(s_hex).context("bad s hex")?;
    Ok(xrpl_signer::der_encode_signature(&r, &s))
}

/// Minimal `account_info` query — returns the next sequence number we
/// must use. Reused by submit_domain_account_set.
struct AccountInfo {
    sequence: u32,
}

async fn fetch_account_info(xrpl_url: &str, xrpl_address: &str) -> Result<AccountInfo> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({
        "method": "account_info",
        "params": [{
            "account": xrpl_address,
            "ledger_index": "validated",
        }],
    });
    let resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&body)
        .send()
        .await
        .context("account_info request failed")?
        .json()
        .await
        .context("invalid JSON from account_info")?;
    let result = &resp["result"];
    if let Some(err) = result["error"].as_str() {
        anyhow::bail!("account_info error: {err}");
    }
    let sequence = result["account_data"]["Sequence"]
        .as_u64()
        .context("account_info missing account_data.Sequence — is the account funded?")?
        as u32;
    Ok(AccountInfo { sequence })
}

// ── escrow-setup ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EscrowSignersConfig {
    signers: Vec<EscrowSigner>,
    quorum: u32,
}

#[derive(Debug, Deserialize)]
struct EscrowSigner {
    name: String,
    xrpl_address: String,
    #[allow(dead_code)]
    compressed_pubkey: Option<String>,
}

pub async fn escrow_setup(
    xrpl_url: &str,
    signers_config_path: &std::path::Path,
    escrow_seed: &str,
    escrow_address_override: Option<&str>,
    disable_master: bool,
) -> Result<()> {
    let config_data = std::fs::read_to_string(signers_config_path)
        .with_context(|| format!("cannot read {}", signers_config_path.display()))?;
    let config: EscrowSignersConfig =
        serde_json::from_str(&config_data).context("invalid signers config JSON")?;

    println!("Escrow Setup");
    println!("============");
    println!("XRPL:    {xrpl_url}");
    println!("Config:  {}", signers_config_path.display());
    println!("Quorum:  {}", config.quorum);
    println!("Signers: {}", config.signers.len());
    for s in &config.signers {
        println!("  {} → {}", s.name, s.xrpl_address);
    }
    println!();

    let http = reqwest::Client::new();

    // Step 1: Derive keypair from seed
    println!("[1/4] Resolving escrow keypair...");
    let keypair = derive_keypair_from_seed(escrow_seed)?;
    let escrow_address = if let Some(addr) = escrow_address_override {
        println!("  Using provided address: {addr}");
        addr.to_string()
    } else {
        println!("  Derived from seed: {}", keypair.address);
        keypair.address.clone()
    };
    let account_id = decode_xrpl_address(&escrow_address)?;

    // Step 2: Check account exists + get sequence
    println!("\n[2/4] Checking account on XRPL...");
    let account_info: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "account_info",
            "params": [{"account": escrow_address}]
        }))
        .send()
        .await?
        .json()
        .await?;

    let balance = account_info["result"]["account_data"]["Balance"]
        .as_str()
        .unwrap_or("0");
    let sequence = account_info["result"]["account_data"]["Sequence"]
        .as_u64()
        .context("account not found on XRPL — fund it first")?;

    println!(
        "  Balance: {} XRP, Sequence: {}",
        balance.parse::<u64>().unwrap_or(0) as f64 / 1_000_000.0,
        sequence
    );

    // Step 3: Submit SignerListSet (locally signed)
    println!("\n[3/4] Submitting SignerListSet...");
    let signer_entries_bin: Vec<([u8; 20], u16)> = config
        .signers
        .iter()
        .map(|s| Ok((decode_xrpl_address(&s.xrpl_address)?, 1u16)))
        .collect::<Result<Vec<_>>>()?;
    let signer_entries_suffix = serialize_signer_entries(&signer_entries_bin);

    let mut sls_fields = vec![
        XrplField::uint16(2, 12),              // TransactionType = SignerListSet
        XrplField::uint32(4, sequence as u32), // Sequence
        XrplField::uint32(35, config.quorum),  // SignerQuorum
        XrplField::amount_drops(8, 12),        // Fee = 12 drops
        XrplField::account_id(1, &account_id), // Account
    ];

    let sls_blob = sign_xrpl_tx(&keypair, &mut sls_fields, Some(&signer_entries_suffix))?;

    let sls_result = submit_tx_blob(xrpl_url, &sls_blob).await?;
    let sls_status = sls_result["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");
    let sls_hash = sls_result["result"]["tx_json"]["hash"]
        .as_str()
        .unwrap_or("unknown");
    println!("  Status: {sls_status}");
    println!("  TX: {sls_hash}");

    if !sls_status.starts_with("tes") {
        anyhow::bail!("SignerListSet failed: {sls_status}");
    }

    // Step 4: Disable master key
    if disable_master {
        println!("\n[4/4] Disabling master key (AccountSet asfDisableMaster)...");
        let mut acset_fields = vec![
            XrplField::uint16(2, 3),                     // TransactionType = AccountSet
            XrplField::uint32(4, (sequence + 1) as u32), // Sequence (next)
            XrplField::uint32(33, 4),                    // SetFlag = asfDisableMaster
            XrplField::amount_drops(8, 12),              // Fee = 12 drops
            XrplField::account_id(1, &account_id),       // Account
        ];

        let acset_blob = sign_xrpl_tx(&keypair, &mut acset_fields, None)?;

        let acset_result = submit_tx_blob(xrpl_url, &acset_blob).await?;
        let acset_status = acset_result["result"]["engine_result"]
            .as_str()
            .unwrap_or("unknown");
        println!("  Status: {acset_status}");

        if !acset_status.starts_with("tes") {
            anyhow::bail!("AccountSet failed: {acset_status}");
        }
    } else {
        println!("\n[4/4] Skipping master key disable (use --disable-master to enable)");
    }

    // Verify
    println!("\n✓ Escrow setup complete!");
    println!("  Address: {escrow_address}");
    println!("  Explorer: https://testnet.xrpl.org/accounts/{escrow_address}");
    println!("\nNext: start orchestrators with --escrow-address {escrow_address} --signers-config <path>");

    Ok(())
}

/// Phase 2.1c-B — `escrow-init` subcommand. The genesis ceremony for a
/// fresh cluster: founder generates a brand-new XRPL escrow account,
/// optionally faucet-funds it, registers the agreed operator addresses
/// as a SignerList, and immediately disables the master key. Replaces
/// `orchestrator/scripts/setup_testnet_escrow.py`.
///
/// Single-mode: `--faucet-url` is the only network-specific input.
/// Testnet operators provide it for auto-funding; mainnet operators
/// omit it and pre-fund the account from their own XRP holdings before
/// running this subcommand. The same code path runs in both cases.
///
/// Per `docs/multi-operator-architecture.md` §6.4, after master is
/// disabled the founder has no on-going authority over the escrow —
/// every future change goes through current-quorum multisig. The
/// founder's seed file is preserved at the canonical path as proof of
/// provenance, but post-disable it has no operational power.
pub async fn escrow_init(
    xrpl_url: &str,
    signers: &[(String, String)],
    quorum: u32,
    seed_file: &Path,
    faucet_url: Option<&str>,
) -> Result<()> {
    if seed_file.exists() {
        anyhow::bail!(
            "seed file already exists at {} — refusing to overwrite. \
             Move it aside (e.g. add a .prev-<TS> suffix) before re-running.",
            seed_file.display()
        );
    }
    validate_escrow_init_args(signers, quorum)?;

    println!("Escrow Init");
    println!("===========");
    println!("XRPL:    {xrpl_url}");
    if let Some(f) = faucet_url {
        println!("Faucet:  {f}");
    } else {
        println!("Faucet:  (none — escrow account must be pre-funded)");
    }
    println!("Quorum:  {quorum}-of-{}", signers.len());
    for (name, addr) in signers {
        println!("  {name} → {addr}");
    }
    println!();

    // Step 1: generate fresh secp256k1 family seed.
    println!("[1/5] Generating fresh secp256k1 escrow keypair...");
    let seed = generate_secp256k1_family_seed();
    let keypair = derive_keypair_from_seed(&seed)?;
    let escrow_address = keypair.address.clone();
    let account_id = decode_xrpl_address(&escrow_address)?;
    println!("  Address: {escrow_address}");
    println!("  Seed:    (not echoed — will be persisted to seed-file in step 5)");

    // Step 2: fund (faucet if URL given, otherwise verify pre-funded).
    if let Some(f) = faucet_url {
        println!("\n[2/5] Faucet-funding {escrow_address}...");
        faucet_fund(f, &escrow_address).await?;
    } else {
        println!("\n[2/5] Verifying account is pre-funded...");
    }

    // Step 3: read sequence (also confirms the account exists on chain).
    println!("\n[3/5] Reading account state...");
    let info = fetch_account_info(xrpl_url, &escrow_address).await?;
    let sequence = info.sequence;
    println!("  Sequence: {sequence}");

    // Step 4a: submit SignerListSet.
    println!(
        "\n[4/5] Submitting SignerListSet ({}-of-{})...",
        quorum,
        signers.len()
    );
    let signer_entries_bin: Vec<([u8; 20], u16)> = signers
        .iter()
        .map(|(_, addr)| Ok((decode_xrpl_address(addr)?, 1u16)))
        .collect::<Result<Vec<_>>>()?;
    let signer_entries_suffix = serialize_signer_entries(&signer_entries_bin);
    let mut sls_fields = vec![
        XrplField::uint16(2, 12),
        XrplField::uint32(4, sequence),
        XrplField::uint32(35, quorum),
        XrplField::amount_drops(8, 12),
        XrplField::account_id(1, &account_id),
    ];
    let sls_blob = sign_xrpl_tx(&keypair, &mut sls_fields, Some(&signer_entries_suffix))?;
    let sls_result = submit_tx_blob(xrpl_url, &sls_blob).await?;
    let sls_status = sls_result["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");
    let sls_hash = sls_result["result"]["tx_json"]["hash"]
        .as_str()
        .unwrap_or("unknown");
    println!("  Status: {sls_status}");
    println!("  TX:     {sls_hash}");
    if !sls_status.starts_with("tes") {
        anyhow::bail!("SignerListSet failed: {sls_status}: {sls_result}");
    }

    // Wait one ledger before submitting AccountSet so the new sequence
    // is observable (avoids transient `terPRE_SEQ` retries).
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    // Step 4b: submit AccountSet asfDisableMaster.
    println!("\n[5/5] Submitting AccountSet asfDisableMaster...");
    let mut acset_fields = vec![
        XrplField::uint16(2, 3),
        XrplField::uint32(4, sequence + 1),
        XrplField::uint32(33, 4), // asfDisableMaster
        XrplField::amount_drops(8, 12),
        XrplField::account_id(1, &account_id),
    ];
    let acset_blob = sign_xrpl_tx(&keypair, &mut acset_fields, None)?;
    let acset_result = submit_tx_blob(xrpl_url, &acset_blob).await?;
    let acset_status = acset_result["result"]["engine_result"]
        .as_str()
        .unwrap_or("unknown");
    let acset_hash = acset_result["result"]["tx_json"]["hash"]
        .as_str()
        .unwrap_or("unknown");
    println!("  Status: {acset_status}");
    println!("  TX:     {acset_hash}");
    if !acset_status.starts_with("tes") {
        anyhow::bail!("AccountSet asfDisableMaster failed: {acset_status}: {acset_result}");
    }

    // Step 5: persist seed to canonical path.
    if let Some(parent) = seed_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot mkdir -p {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let signers_json: Vec<serde_json::Value> = signers
        .iter()
        .map(|(name, addr)| serde_json::json!({"name": name, "xrpl_address": addr}))
        .collect();
    let seed_json = serde_json::json!({
        "escrow_address": escrow_address,
        "escrow_seed": seed,
        "quorum": quorum,
        "signers": signers_json,
        "signer_list_set_tx_hash": sls_hash,
        "disable_master_tx_hash": acset_hash,
    });
    std::fs::write(seed_file, serde_json::to_string_pretty(&seed_json)?)
        .with_context(|| format!("cannot write {}", seed_file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(seed_file, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot chmod 0600 {}", seed_file.display()))?;
    }

    println!();
    println!("============================================================");
    println!("ESCROW_ADDRESS={escrow_address}");
    println!("SEED_FILE={}", seed_file.display());
    println!("Explorer: https://testnet.xrpl.org/accounts/{escrow_address}");
    println!();
    println!("Master key disabled. All future escrow changes require");
    println!(
        "{quorum}-of-{} multisig signed by current operators.",
        signers.len()
    );

    Ok(())
}

/// Phase 2.1c-C — `node-config-apply` subcommand. Runs locally on a
/// single node by its operator. Discovers the cluster roster from the
/// on-chain SignerList of the escrow account: for each member, queries
/// `AccountInfo`, parses `Domain` via `decode_domain_v1` to extract the
/// ECDH public key, builds a `signers_config.json` with `local_signer`
/// set to this node's entry. The discovered roster only carries
/// `xrpl_address` + `ecdh_pubkey` per peer — fields that depend on
/// per-operator credentials (`enclave_url`, `address`, `session_key`,
/// `compressed_pubkey`) are empty for non-local entries because they
/// are not knowable from on-chain. The local entry is fully populated
/// from `--node-entry` (the file emitted by `node-bootstrap`).
///
/// Per `docs/multi-operator-architecture.md` §6.5: each operator runs
/// this on their own node after the founder publishes the escrow
/// address. The orchestrator daemon then boots, joins libp2p, and does
/// any cross-operator coordination (e.g., FROST signing) over the mesh
/// — direct HTTP-to-peer-enclave is not used. The empty credential
/// fields on non-local entries are intentional: anything that tries to
/// use them today (the legacy testnet `withdrawal.rs` HTTP-to-each-
/// signer flow) will fail loudly until the libp2p signing coordinator
/// lands in a follow-up wedge.
pub async fn node_config_apply(
    xrpl_url: &str,
    escrow_address: &str,
    node_entry_path: &Path,
    output: &Path,
) -> Result<()> {
    println!("Node Config Apply");
    println!("=================");
    println!("XRPL:    {xrpl_url}");
    println!("Escrow:  {escrow_address}");
    println!("Entry:   {}", node_entry_path.display());
    println!("Output:  {}", output.display());
    println!();

    // 1. Read local node-entry to populate `local_signer`.
    let local_data = std::fs::read_to_string(node_entry_path)
        .with_context(|| format!("cannot read {}", node_entry_path.display()))?;
    let local: SignerEntry = serde_json::from_str(&local_data)
        .with_context(|| format!("invalid SignerEntry JSON in {}", node_entry_path.display()))?;
    println!("[1/4] Loaded local entry");
    println!("  xrpl_address: {}", local.xrpl_address);
    if local.ecdh_pubkey.is_none() {
        anyhow::bail!(
            "local entry {} is missing `ecdh_pubkey` — re-run `node-bootstrap` to regenerate it",
            node_entry_path.display()
        );
    }

    // 2. Query SignerList from on-chain.
    println!("\n[2/4] Querying on-chain SignerList...");
    let (signer_addresses, quorum) = fetch_signer_list(xrpl_url, escrow_address).await?;
    println!("  Quorum: {} of {}", quorum, signer_addresses.len());
    for a in &signer_addresses {
        println!("    {a}");
    }

    // 3. For each signer, fetch AccountInfo + decode Domain → ECDH pubkey.
    println!("\n[3/4] Discovering ECDH pubkeys from each operator's Domain field...");
    let mut roster: Vec<SignerEntry> = Vec::with_capacity(signer_addresses.len());
    let mut local_seen = false;
    for addr in &signer_addresses {
        let pubkey = fetch_domain_ecdh_pubkey(xrpl_url, addr).await?;
        let pubkey_hex = hex::encode_upper(pubkey);
        println!("  {addr}");
        println!("    ecdh_pubkey: {pubkey_hex}");

        if addr == &local.xrpl_address {
            roster.push(local.clone());
            local_seen = true;
        } else {
            roster.push(SignerEntry {
                name: format!("operator-{}", &addr[..addr.len().min(8)]),
                enclave_url: String::new(),
                address: String::new(),
                session_key: String::new(),
                compressed_pubkey: String::new(),
                xrpl_address: addr.clone(),
                ecdh_pubkey: Some(pubkey_hex),
            });
        }
    }
    if !local_seen {
        anyhow::bail!(
            "local node entry's xrpl_address {} is not on the on-chain SignerList — \
             this node is not a registered cluster member yet, or the founder used a \
             different address. Verify with `escrow-init` output.",
            local.xrpl_address
        );
    }

    // 4. Write the merged signers_config.json.
    println!("\n[4/4] Writing {}", output.display());
    let config = FullSignersConfig {
        escrow_address: escrow_address.to_string(),
        escrow_seed: String::new(),
        quorum,
        signer_list_set_tx_hash: String::new(),
        signers: roster,
        local_signer: Some(local),
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot mkdir -p {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(output, &json)
        .with_context(|| format!("failed to write {}", output.display()))?;

    println!();
    println!(
        "✓ Wrote signers_config.json with {} roster entries",
        config.signers.len()
    );
    println!();
    println!("Next: restart the local orchestrator service so it picks up");
    println!("the new config. (Operator action — `sudo systemctl restart");
    println!("perp-dex-orchestrator` on this node only.)");
    Ok(())
}

/// Fetches the SignerList for an XRPL account via `account_objects`.
/// Returns `(signer_addresses_in_on-chain-order, quorum)`. Bails if the
/// account has no SignerList (unconfigured escrow).
async fn fetch_signer_list(xrpl_url: &str, account: &str) -> Result<(Vec<String>, u32)> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({
        "method": "account_objects",
        "params": [{
            "account": account,
            "type": "signer_list",
            "ledger_index": "validated",
        }],
    });
    let resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&body)
        .send()
        .await
        .context("account_objects request failed")?
        .json()
        .await
        .context("invalid JSON from account_objects")?;
    parse_signer_list_response(&resp)
        .with_context(|| format!("failed to extract SignerList for {account}"))
}

fn parse_signer_list_response(resp: &serde_json::Value) -> Result<(Vec<String>, u32)> {
    let result = &resp["result"];
    if let Some(err) = result["error"].as_str() {
        anyhow::bail!("account_objects error: {err}");
    }
    let objects = result["account_objects"]
        .as_array()
        .context("account_objects.account_objects missing")?;
    let entry = objects
        .iter()
        .find(|o| o["LedgerEntryType"].as_str() == Some("SignerList"))
        .context("account has no SignerList — escrow not configured yet")?;
    let quorum = entry["SignerQuorum"]
        .as_u64()
        .context("SignerList missing SignerQuorum")? as u32;
    let signer_entries = entry["SignerEntries"]
        .as_array()
        .context("SignerList missing SignerEntries")?;
    let mut addresses = Vec::with_capacity(signer_entries.len());
    for se in signer_entries {
        let acct = se["SignerEntry"]["Account"]
            .as_str()
            .context("SignerEntry missing Account")?;
        addresses.push(acct.to_string());
    }
    Ok((addresses, quorum))
}

/// Fetches an account's `Domain` field via `account_info` and decodes
/// it to a 33-byte ECDH pubkey using `decode_domain_v1`. Bails if the
/// account has no Domain or it is malformed.
async fn fetch_domain_ecdh_pubkey(xrpl_url: &str, account: &str) -> Result<[u8; 33]> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let body = serde_json::json!({
        "method": "account_info",
        "params": [{
            "account": account,
            "ledger_index": "validated",
        }],
    });
    let resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&body)
        .send()
        .await
        .context("account_info request failed")?
        .json()
        .await
        .context("invalid JSON from account_info")?;
    parse_domain_from_account_info(&resp)
        .with_context(|| format!("failed to extract Domain for {account}"))
}

fn parse_domain_from_account_info(resp: &serde_json::Value) -> Result<[u8; 33]> {
    let result = &resp["result"];
    if let Some(err) = result["error"].as_str() {
        anyhow::bail!("account_info error: {err}");
    }
    let domain_hex = result["account_data"]["Domain"].as_str().context(
        "account_info missing account_data.Domain — operator has not run `node-bootstrap \
             --publish-domain` yet",
    )?;
    let domain_bytes = hex::decode(domain_hex).context("Domain field is not valid hex")?;
    decode_domain_v1(&domain_bytes)
}

/// Pure-logic argument validation for `escrow_init`. Extracted so the
/// failure modes (too-few/too-many signers, bad quorum, malformed
/// XRPL address) are unit-testable without touching the network. Real
/// network errors during the ceremony are reported separately.
fn validate_escrow_init_args(signers: &[(String, String)], quorum: u32) -> Result<()> {
    if signers.len() < 2 || signers.len() > 32 {
        anyhow::bail!(
            "need 2..=32 signers, got {} (XRPL SignerList limit)",
            signers.len()
        );
    }
    if quorum < 1 || quorum as usize > signers.len() {
        anyhow::bail!("quorum must be in 1..={}, got {}", signers.len(), quorum);
    }
    for (name, addr) in signers {
        decode_xrpl_address(addr)
            .with_context(|| format!("signer {name} has invalid XRPL address {addr:?}"))?;
    }
    Ok(())
}

/// Generate a fresh XRPL secp256k1 family seed: 16 random bytes wrapped
/// in `[0x21] || entropy || sha256_double(prefix||entropy)[..4]`,
/// base58-encoded with the XRPL alphabet. Operator never types this
/// seed; it's persisted to the canonical seed file by `escrow_init` and
/// re-loaded by future runs of `escrow-setup` if the founder ever needs
/// to re-submit (which they cannot post-master-disable).
fn generate_secp256k1_family_seed() -> String {
    use rand::RngCore;
    use sha2::Digest;

    let mut entropy = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut entropy);

    let mut payload = Vec::with_capacity(21);
    payload.push(0x21);
    payload.extend_from_slice(&entropy);

    let h1 = sha2::Sha256::digest(&payload);
    let h2 = sha2::Sha256::digest(h1);
    payload.extend_from_slice(&h2[..4]);

    bs58::encode(&payload)
        .with_alphabet(xrpl_alphabet())
        .into_string()
}

async fn submit_tx_blob(xrpl_url: &str, blob: &[u8]) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();
    let hex = hex::encode_upper(blob);
    info!(blob_len = blob.len(), "submitting tx_blob");
    let resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "submit",
            "params": [{"tx_blob": hex}]
        }))
        .send()
        .await
        .context("XRPL submit request failed")?
        .json()
        .await
        .context("XRPL submit response parse failed")?;
    Ok(resp)
}

enum XrplSigningKey {
    Secp256k1(k256::ecdsa::SigningKey),
    Ed25519(ed25519_dalek::SigningKey),
}

struct XrplKeypair {
    key: XrplSigningKey,
    compressed_pubkey_hex: String,
    address: String,
}

impl XrplKeypair {
    fn pubkey_bytes(&self) -> Vec<u8> {
        hex::decode(&self.compressed_pubkey_hex).expect("valid hex")
    }

    fn sign_hash(&self, hash: &[u8; 32]) -> Result<Vec<u8>> {
        match &self.key {
            XrplSigningKey::Secp256k1(sk) => {
                use k256::ecdsa::{signature::hazmat::PrehashSigner, Signature};
                let (sig, _): (Signature, _) = sk
                    .sign_prehash(hash)
                    .map_err(|e| anyhow::anyhow!("secp256k1 signing failed: {e}"))?;
                Ok(sig.to_der().as_bytes().to_vec())
            }
            XrplSigningKey::Ed25519(sk) => {
                use ed25519_dalek::Signer;
                let sig = sk.sign(hash);
                Ok(sig.to_bytes().to_vec())
            }
        }
    }
}

fn derive_address_from_pubkey(pubkey_bytes: &[u8]) -> String {
    use sha2::Digest;
    let sha256 = sha2::Sha256::digest(pubkey_bytes);
    let account_id = ripemd::Ripemd160::digest(sha256);

    let mut payload = Vec::with_capacity(25);
    payload.push(0x00);
    payload.extend_from_slice(&account_id);
    let h1 = sha2::Sha256::digest(&payload);
    let h2 = sha2::Sha256::digest(h1);
    payload.extend_from_slice(&h2[..4]);

    bs58::encode(&payload)
        .with_alphabet(xrpl_alphabet())
        .into_string()
}

fn derive_keypair_from_seed(seed: &str) -> Result<XrplKeypair> {
    use sha2::Digest;

    let decoded = bs58::decode(seed)
        .with_alphabet(xrpl_alphabet())
        .into_vec()
        .context("invalid seed encoding")?;

    if decoded.len() < 21 {
        anyhow::bail!("seed too short: {} bytes", decoded.len());
    }

    // Ed25519 seeds have 3-byte prefix [0x01, 0xE1, 0x4B], entropy at [3..19]
    // secp256k1 seeds have 1-byte prefix [0x21], entropy at [1..17]
    let is_ed25519 =
        decoded.len() >= 23 && decoded[0] == 0x01 && decoded[1] == 0xE1 && decoded[2] == 0x4B;

    if is_ed25519 {
        let entropy = &decoded[3..19];
        // Ed25519: private key = SHA-512(entropy)[0..32]
        let hash = sha2::Sha512::digest(entropy);
        let secret_bytes: [u8; 32] = hash[..32].try_into().unwrap();
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&secret_bytes);
        let verifying_key = signing_key.verifying_key();

        // XRPL Ed25519 pubkey: 0xED prefix + 32 bytes
        let mut pubkey = Vec::with_capacity(33);
        pubkey.push(0xED);
        pubkey.extend_from_slice(verifying_key.as_bytes());
        let pubkey_hex = hex::encode(&pubkey);
        let address = derive_address_from_pubkey(&pubkey);

        return Ok(XrplKeypair {
            key: XrplSigningKey::Ed25519(signing_key),
            compressed_pubkey_hex: pubkey_hex,
            address,
        });
    }

    // secp256k1 derivation per XRPL Family Generator spec
    // (https://xrpl.org/cryptographic-keys.html#key-derivation).
    //
    // The user-visible classic_address is derived from the MASTER
    // keypair, not the ROOT keypair. The previous implementation
    // returned the ROOT keypair, which produces a different address
    // — this silently breaks auth for any user whose wallet was
    // generated by a standard XRPL tool (xrpl-py, XUMM, rippled, ...).
    // See APP-AUTH-2 in SECURITY-REAUDIT-4-FIXPLAN.md Appendix A.
    //
    //   1. ROOT priv  = first valid SHA-512(entropy ‖ counter₁_be32)[:32]
    //   2. INTER priv = first valid SHA-512(root_pub_compressed ‖
    //                                       0u32_be ‖ counter₂_be32)[:32]
    //   3. MASTER priv = (root_priv + inter_priv) mod n
    //   4. classic_address = derive_address(master_pub)
    let entropy = &decoded[1..17];
    use k256::ecdsa::SigningKey;
    use k256::elliptic_curve::PrimeField;
    use k256::Scalar;

    fn derive_priv_from_seed_bytes(seed_bytes: &[u8]) -> anyhow::Result<SigningKey> {
        let mut counter: u32 = 0;
        loop {
            let mut hasher = sha2::Sha512::new();
            hasher.update(seed_bytes);
            hasher.update(counter.to_be_bytes());
            let hash = hasher.finalize();
            if let Ok(sk) = SigningKey::from_slice(&hash[..32]) {
                return Ok(sk);
            }
            counter = counter
                .checked_add(1)
                .context("secp256k1 derivation counter overflow")?;
        }
    }

    // 1. Root priv from seed entropy.
    let root_priv = derive_priv_from_seed_bytes(entropy)
        .context("failed to derive secp256k1 root private key")?;
    let root_pub = root_priv.verifying_key().to_encoded_point(true);
    let root_pub_bytes = root_pub.as_bytes();

    // 2. Intermediate priv from (root_pub ‖ 0u32_be).
    //    The "0u32" is the master sequence id — XRPL only uses 0 here.
    let mut inter_seed = Vec::with_capacity(root_pub_bytes.len() + 4);
    inter_seed.extend_from_slice(root_pub_bytes);
    inter_seed.extend_from_slice(&0u32.to_be_bytes());
    let inter_priv = derive_priv_from_seed_bytes(&inter_seed)
        .context("failed to derive secp256k1 intermediate private key")?;

    // 3. Master priv = (root + intermediate) mod n.
    let root_scalar = Scalar::from_repr(root_priv.to_bytes())
        .into_option()
        .context("root_priv → scalar")?;
    let inter_scalar = Scalar::from_repr(inter_priv.to_bytes())
        .into_option()
        .context("inter_priv → scalar")?;
    let master_scalar = root_scalar + inter_scalar;
    let signing_key = SigningKey::from_slice(&master_scalar.to_repr())
        .context("master scalar → SigningKey (would zero out only with negligible probability)")?;

    // 4. Master pubkey + classic address.
    let verifying_key = signing_key.verifying_key();
    let compressed = verifying_key.to_encoded_point(true);
    let compressed_bytes = compressed.as_bytes();
    let pubkey_hex = hex::encode(compressed_bytes);
    let address = derive_address_from_pubkey(compressed_bytes);

    Ok(XrplKeypair {
        key: XrplSigningKey::Secp256k1(signing_key),
        compressed_pubkey_hex: pubkey_hex,
        address,
    })
}

#[allow(dead_code)] // utility helper kept for CLI debugging scripts
fn derive_xrpl_address_from_seed(seed: &str) -> Result<String> {
    Ok(derive_keypair_from_seed(seed)?.address)
}

// ── config-init ───────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct FullSignersConfig {
    escrow_address: String,
    #[serde(default)]
    escrow_seed: String,
    quorum: u32,
    #[serde(default)]
    signer_list_set_tx_hash: String,
    signers: Vec<SignerEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_signer: Option<SignerEntry>,
}

pub async fn config_init(
    entry_files: &[std::path::PathBuf],
    escrow_address: &str,
    quorum: u32,
    output: &std::path::Path,
) -> Result<()> {
    println!("Config Init");
    println!("===========");

    let mut signers = Vec::new();
    for path in entry_files {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let entry: SignerEntry = serde_json::from_str(&data)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        println!("  + {} → {}", entry.name, entry.xrpl_address);
        signers.push(entry);
    }

    if signers.is_empty() {
        anyhow::bail!("no signer entries provided");
    }

    if quorum as usize > signers.len() {
        anyhow::bail!(
            "quorum ({}) cannot exceed signer count ({})",
            quorum,
            signers.len()
        );
    }

    let config = FullSignersConfig {
        escrow_address: escrow_address.to_string(),
        escrow_seed: String::new(),
        quorum,
        signer_list_set_tx_hash: String::new(),
        signers,
        local_signer: None,
    };

    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(output, &json)
        .with_context(|| format!("failed to write {}", output.display()))?;

    println!(
        "\nCreated {} with {} signers, quorum={}",
        output.display(),
        config.signers.len(),
        quorum
    );
    println!("\nNext steps:");
    println!("  1. Add escrow_seed to the config (keep it secret!)");
    println!(
        "  2. Run `escrow-setup --signers-config {} --escrow-seed <seed>`",
        output.display()
    );

    Ok(())
}

// ── operator-add ──────────────────────────────────────────────

pub async fn operator_add(
    enclave_url: &str,
    name: &str,
    config_path: &std::path::Path,
    xrpl_url: Option<&str>,
    escrow_seed: Option<&str>,
) -> Result<()> {
    println!("Operator Add");
    println!("============");
    println!("Enclave: {enclave_url}");
    println!("Name:    {name}");
    println!("Config:  {}", config_path.display());
    println!();

    let config_data = std::fs::read_to_string(config_path)
        .with_context(|| format!("cannot read {}", config_path.display()))?;
    let mut config: FullSignersConfig =
        serde_json::from_str(&config_data).context("invalid signers config JSON")?;

    if config.signers.iter().any(|s| s.name == name) {
        anyhow::bail!("signer '{name}' already exists in config");
    }

    // Step 1: Generate keypair
    // O-L4: operator_add targets the local enclave — enforce loopback
    // and reuse the shared factory.
    crate::http_helpers::ensure_loopback_url(enclave_url)
        .context("operator_add requires a loopback enclave URL (O-L4)")?;
    let http = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))?;

    println!("[1/3] Generating keypair in enclave...");
    let resp: GenerateResponse = http
        .post(format!("{enclave_url}/pool/generate"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .context("failed to reach enclave")?
        .json()
        .await?;

    if resp.status != "success" {
        anyhow::bail!("enclave failed: {}", resp.message.unwrap_or_default());
    }

    let eth_address = resp.address.context("missing address")?;
    let uncompressed_pubkey = resp.public_key.context("missing public_key")?;
    let session_key = resp.session_key.context("missing session_key")?;

    // Step 2: Derive XRPL address
    println!("[2/3] Deriving XRPL address...");
    let xrpl_address = xrpl_signer::pubkey_to_xrpl_address(&uncompressed_pubkey)?;
    let compressed_hex = {
        let raw = hex::decode(
            uncompressed_pubkey
                .strip_prefix("0x")
                .unwrap_or(&uncompressed_pubkey),
        )?;
        hex::encode_upper(&xrpl_signer::compress_pubkey(&raw)?)
    };

    println!("  XRPL address:     {xrpl_address}");
    println!("  Compressed pubkey: {compressed_hex}");

    let entry = SignerEntry {
        name: name.to_string(),
        enclave_url: enclave_url.to_string(),
        address: eth_address,
        session_key,
        compressed_pubkey: compressed_hex,
        xrpl_address: xrpl_address.clone(),
        ecdh_pubkey: None,
    };

    // Step 3: Update config
    println!("[3/3] Updating config...");
    config.signers.push(entry);

    let json = serde_json::to_string_pretty(&config)?;
    std::fs::write(config_path, &json)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("  Added signer #{} ({})", config.signers.len(), name);
    println!("  Config saved to {}", config_path.display());

    // Optional: re-submit SignerListSet if credentials provided
    if let (Some(xrpl_url), Some(seed)) = (xrpl_url, escrow_seed) {
        println!(
            "\n  Re-submitting SignerListSet with {} signers...",
            config.signers.len()
        );
        let escrow_addr = if config.escrow_address.is_empty() {
            None
        } else {
            Some(config.escrow_address.as_str())
        };
        escrow_setup(xrpl_url, config_path, seed, escrow_addr, false).await?;
    } else {
        println!("\nNext: run `escrow-setup` to update the on-chain SignerListSet");
    }

    Ok(())
}

// ── sign-request (Bug 4) ───────────────────────────────────────

fn sign_body(keypair: &XrplKeypair, body: &[u8], timestamp: u64) -> Result<String> {
    use sha2::Digest;

    let mut hasher = sha2::Sha256::new();
    hasher.update(body);
    hasher.update(timestamp.to_string().as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();

    let sig_bytes = keypair.sign_hash(&hash)?;
    Ok(hex::encode(&sig_bytes))
}

pub async fn sign_request(seed: &str, method: &str, url: &str, body: Option<&str>) -> Result<()> {
    let keypair = derive_keypair_from_seed(seed)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let body_bytes = body.unwrap_or("").as_bytes();
    let sig_hex = sign_body(&keypair, body_bytes, timestamp)?;

    println!("# XRPL address: {}", keypair.address);
    println!("# Public key:   {}", keypair.compressed_pubkey_hex);
    println!();

    let mut cmd = format!(
        "curl -X {method} '{url}' \\\n  -H 'Content-Type: application/json' \\\n  -H 'X-XRPL-Address: {}' \\\n  -H 'X-XRPL-PublicKey: {}' \\\n  -H 'X-XRPL-Signature: {sig_hex}' \\\n  -H 'X-XRPL-Timestamp: {timestamp}'",
        keypair.address, keypair.compressed_pubkey_hex,
    );
    if let Some(b) = body {
        cmd.push_str(&format!(" \\\n  -d '{b}'"));
    }

    println!("{cmd}");
    Ok(())
}

// ── withdraw (Bug 4) ───────────────────────────────────────────

pub async fn cli_withdraw(
    api_url: &str,
    seed: &str,
    amount: &str,
    destination: &str,
    destination_tag: Option<u32>,
) -> Result<()> {
    let keypair = derive_keypair_from_seed(seed)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut body = serde_json::json!({
        "user_id": keypair.address,
        "amount": amount,
        "destination": destination,
    });
    if let Some(tag) = destination_tag {
        body["destination_tag"] = serde_json::json!(tag);
    }
    let body_str = serde_json::to_string(&body)?;
    let sig_hex = sign_body(&keypair, body_str.as_bytes(), timestamp)?;

    println!("Withdraw");
    println!("========");
    println!("From:        {}", keypair.address);
    println!("Amount:      {amount}");
    println!("Destination: {destination}");
    if let Some(tag) = destination_tag {
        println!("Dest Tag:    {tag}");
    }
    println!("API:         {api_url}");
    println!();

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = http
        .post(format!("{api_url}/v1/withdraw"))
        .header("Content-Type", "application/json")
        .header("X-XRPL-Address", &keypair.address)
        .header("X-XRPL-PublicKey", &keypair.compressed_pubkey_hex)
        .header("X-XRPL-Signature", &sig_hex)
        .header("X-XRPL-Timestamp", timestamp.to_string())
        .body(body_str)
        .send()
        .await
        .context("failed to reach API")?;

    let status = resp.status();
    let resp_body: serde_json::Value = resp.json().await.context("invalid JSON response")?;
    let pretty = serde_json::to_string_pretty(&resp_body)?;

    if status.is_success() {
        println!("✓ {pretty}");
    } else {
        println!("✗ HTTP {status}\n{pretty}");
    }

    Ok(())
}

// ── balance (Bug 4) ────────────────────────────────────────────

pub async fn cli_balance(api_url: &str, seed: &str) -> Result<()> {
    let keypair = derive_keypair_from_seed(seed)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let uri_path = format!("/v1/account/balance?user_id={}", keypair.address);
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(uri_path.as_bytes());
    hasher.update(timestamp.to_string().as_bytes());
    let hash: [u8; 32] = hasher.finalize().into();

    let sig_bytes = keypair.sign_hash(&hash)?;
    let sig_hex = hex::encode(&sig_bytes);

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = http
        .get(format!("{api_url}{uri_path}"))
        .header("X-XRPL-Address", &keypair.address)
        .header("X-XRPL-PublicKey", &keypair.compressed_pubkey_hex)
        .header("X-XRPL-Signature", &sig_hex)
        .header("X-XRPL-Timestamp", timestamp.to_string())
        .send()
        .await
        .context("failed to reach API")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.context("invalid JSON response")?;
    let pretty = serde_json::to_string_pretty(&body)?;

    if status.is_success() {
        println!("{pretty}");
    } else {
        println!("✗ HTTP {status}\n{pretty}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_escrow_seed_prefers_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed");
        std::fs::write(&path, "shSeedValueOnFirstLine\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let got = resolve_escrow_seed(None, Some(&path)).unwrap();
        assert_eq!(got, "shSeedValueOnFirstLine");
    }

    #[test]
    fn resolve_escrow_seed_accepts_argv_with_warning() {
        let got = resolve_escrow_seed(Some("shArgvSeed"), None).unwrap();
        assert_eq!(got, "shArgvSeed");
    }

    #[test]
    fn resolve_escrow_seed_rejects_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed");
        std::fs::write(&path, "sh\n").unwrap();
        assert!(resolve_escrow_seed(Some("s"), Some(&path)).is_err());
    }

    #[test]
    fn resolve_escrow_seed_rejects_neither() {
        assert!(resolve_escrow_seed(None, None).is_err());
    }

    #[test]
    fn resolve_escrow_seed_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed");
        std::fs::write(&path, "\n").unwrap();
        assert!(resolve_escrow_seed(None, Some(&path)).is_err());
    }

    // ── Phase 2.1c-A Domain-field encoding (multi-operator §3.3) ─

    #[test]
    fn encode_domain_v1_format_lowercases_and_prefixes() {
        let pk_upper = "03D3869DF7C134DA8066006A6304C3F3AFB9357BABE6326F5D8655A3DD2DE0CF57";
        let got = encode_domain_v1(pk_upper);
        assert_eq!(
            got,
            "xperp-ecdh-v1:03d3869df7c134da8066006a6304c3f3afb9357babe6326f5d8655a3dd2de0cf57"
        );
        // 14 prefix chars + 66 hex chars = 80 bytes — well within
        // XRPL's 256-byte Domain limit.
        assert_eq!(got.len(), 80);
    }

    #[test]
    fn encode_domain_v1_idempotent_on_lowercase() {
        let pk_lower = "03d3869df7c134da8066006a6304c3f3afb9357babe6326f5d8655a3dd2de0cf57";
        assert_eq!(encode_domain_v1(pk_lower).len(), 80);
    }

    #[test]
    fn decode_domain_v1_round_trips_with_encode() {
        let pk_hex = "03D3869DF7C134DA8066006A6304C3F3AFB9357BABE6326F5D8655A3DD2DE0CF57";
        let encoded = encode_domain_v1(pk_hex);
        let decoded = decode_domain_v1(encoded.as_bytes()).unwrap();
        assert_eq!(hex::encode(decoded).to_uppercase(), pk_hex);
    }

    #[test]
    fn decode_domain_v1_rejects_missing_prefix() {
        let raw = "03d3869df7c134da8066006a6304c3f3afb9357babe6326f5d8655a3dd2de0cf57";
        let err = decode_domain_v1(raw.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("xperp-ecdh-v1"), "got: {err}");
    }

    #[test]
    fn decode_domain_v1_rejects_wrong_length_pubkey() {
        // 32 bytes of hex (64 chars), not 33 — well-formed hex but
        // wrong length → length-check failure, not codec failure.
        let bad = "xperp-ecdh-v1:03d3869df7c134da8066006a6304c3f3afb9357babe6326f5d8655a3dd2de0c1";
        // Make sure we built an even-length hex (33 - 1 = 32 bytes = 64 chars).
        assert_eq!(
            bad.strip_prefix("xperp-ecdh-v1:").unwrap().len(),
            64,
            "fixture must be even-length hex"
        );
        let err = decode_domain_v1(bad.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("wrong length"), "got: {err}");
    }

    #[test]
    fn decode_domain_v1_rejects_non_utf8() {
        let bad: [u8; 4] = [0xff, 0xfe, 0xfd, 0xfc];
        let err = decode_domain_v1(&bad).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "got: {err}");
    }

    // ── Phase 2.1c-B escrow-init helpers ───────────────────────────

    fn three_valid_signers() -> Vec<(String, String)> {
        vec![
            ("node-1".into(), "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ".into()),
            ("node-2".into(), "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j".into()),
            ("node-3".into(), "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn".into()),
        ]
    }

    #[test]
    fn validate_escrow_init_accepts_2_of_3() {
        validate_escrow_init_args(&three_valid_signers(), 2).unwrap();
    }

    #[test]
    fn validate_escrow_init_rejects_one_signer() {
        let one = three_valid_signers()
            .into_iter()
            .take(1)
            .collect::<Vec<_>>();
        let err = validate_escrow_init_args(&one, 1).unwrap_err();
        assert!(err.to_string().contains("need 2..=32"), "got: {err}");
    }

    #[test]
    fn validate_escrow_init_rejects_quorum_zero() {
        let err = validate_escrow_init_args(&three_valid_signers(), 0).unwrap_err();
        assert!(err.to_string().contains("quorum must be"), "got: {err}");
    }

    #[test]
    fn validate_escrow_init_rejects_quorum_above_n() {
        let err = validate_escrow_init_args(&three_valid_signers(), 4).unwrap_err();
        assert!(err.to_string().contains("quorum must be"), "got: {err}");
    }

    #[test]
    fn validate_escrow_init_rejects_invalid_signer_address() {
        let mut bad = three_valid_signers();
        bad[1].1 = "rThisIsNotAValidAddress".into();
        let err = validate_escrow_init_args(&bad, 2).unwrap_err();
        // The bs58 decoder calls a malformed XRPL address an "invalid
        // encoding"; surface that distinct from quorum errors.
        assert!(
            err.to_string().contains("invalid XRPL address") || err.to_string().contains("invalid"),
            "got: {err}"
        );
    }

    #[test]
    fn family_seed_starts_with_s_and_decodes_back() {
        let seed = generate_secp256k1_family_seed();
        // XRPL secp256k1 family seeds always start with 's' and are
        // typically 29 chars long (1-byte prefix + 16 entropy +
        // 4 checksum, base58-encoded).
        assert!(seed.starts_with('s'), "got: {seed}");
        assert!(
            seed.len() == 29 || seed.len() == 28,
            "unexpected length {} for seed {seed}",
            seed.len()
        );
        // Decoding via the same path as `derive_keypair_from_seed`
        // round-trips: produces the right entropy length.
        let decoded = bs58::decode(&seed)
            .with_alphabet(xrpl_alphabet())
            .into_vec()
            .expect("seed must decode");
        assert_eq!(
            decoded.len(),
            21,
            "expected 1+16+4 bytes, got {}",
            decoded.len()
        );
        assert_eq!(
            decoded[0], 0x21,
            "expected secp256k1 family-seed prefix 0x21"
        );
    }

    #[test]
    fn family_seed_derives_to_a_valid_xrpl_keypair() {
        // The derived keypair must have a valid r-address (round-trip
        // through `decode_xrpl_address`) — proves the seed is usable
        // immediately by the rest of the XRPL flow.
        let seed = generate_secp256k1_family_seed();
        let kp = derive_keypair_from_seed(&seed).expect("freshly-generated seed must derive");
        decode_xrpl_address(&kp.address).expect("derived address must be valid XRPL r-address");
    }

    // ── Phase 2.1c-C node-config-apply parsers ─────────────────────

    #[test]
    fn parse_signer_list_response_extracts_quorum_and_addresses() {
        let resp = serde_json::json!({
            "result": {
                "account_objects": [
                    {
                        "LedgerEntryType": "SignerList",
                        "SignerQuorum": 2,
                        "SignerEntries": [
                            {"SignerEntry": {"Account": "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ", "SignerWeight": 1}},
                            {"SignerEntry": {"Account": "rL3LYCP6gkduRoiD9pB6KDEUyNVPXeDo2j", "SignerWeight": 1}},
                            {"SignerEntry": {"Account": "rwoAC7KZD3UYtzpWSB4jQUt1qvQjhqXTUn", "SignerWeight": 1}},
                        ],
                    }
                ]
            }
        });
        let (addrs, quorum) = parse_signer_list_response(&resp).unwrap();
        assert_eq!(quorum, 2);
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "rKe1hu3iRvyRnJB4xHBMXvzEwsnXTHMxnJ");
    }

    #[test]
    fn parse_signer_list_response_rejects_account_without_signerlist() {
        let resp = serde_json::json!({"result": {"account_objects": []}});
        let err = parse_signer_list_response(&resp).unwrap_err();
        assert!(
            err.to_string().contains("no SignerList") || err.to_string().contains("escrow"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_signer_list_response_propagates_xrpl_error() {
        let resp = serde_json::json!({"result": {"error": "actNotFound"}});
        let err = parse_signer_list_response(&resp).unwrap_err();
        assert!(err.to_string().contains("actNotFound"), "got: {err}");
    }

    #[test]
    fn parse_domain_from_account_info_round_trips_with_encode_v1() {
        // Round-trip a real-looking ECDH pubkey through the on-chain
        // representation: encode_domain_v1 → ASCII bytes → hex (which is
        // how XRPL stores Domain) → parse → 33-byte pubkey.
        let pk_hex_upper = "03D3869DF7C134DA8066006A6304C3F3AFB9357BABE6326F5D8655A3DD2DE0CF57";
        let domain_str = encode_domain_v1(pk_hex_upper);
        let domain_hex = hex::encode_upper(domain_str.as_bytes());
        let resp = serde_json::json!({
            "result": {"account_data": {"Domain": domain_hex}}
        });
        let pk = parse_domain_from_account_info(&resp).unwrap();
        assert_eq!(hex::encode(pk).to_uppercase(), pk_hex_upper);
    }

    #[test]
    fn parse_domain_from_account_info_rejects_missing_domain() {
        let resp = serde_json::json!({"result": {"account_data": {}}});
        let err = parse_domain_from_account_info(&resp).unwrap_err();
        assert!(err.to_string().contains("Domain"), "got: {err}");
    }

    #[test]
    fn parse_domain_from_account_info_rejects_wrong_prefix() {
        // Domain is hex-encoded ASCII for "example.com" — common but
        // not our protocol prefix. Should fail.
        let domain_hex = hex::encode_upper(b"example.com");
        let resp = serde_json::json!({
            "result": {"account_data": {"Domain": domain_hex}}
        });
        let err = parse_domain_from_account_info(&resp).unwrap_err();
        assert!(
            err.to_string().contains("xperp-ecdh-v1") || err.to_string().contains("prefix"),
            "got: {err}"
        );
    }

    #[test]
    fn family_seed_two_calls_produce_different_seeds() {
        // Cheap entropy sanity check: two consecutive calls must NOT
        // produce the same seed. Probability of collision is 2^-128
        // when entropy is sound.
        let a = generate_secp256k1_family_seed();
        let b = generate_secp256k1_family_seed();
        assert_ne!(a, b, "two consecutive calls produced the same seed");
    }

    // ── XRPL key derivation conformance (APP-AUTH-2) ───────────
    //
    // Test vectors come from the XRPL spec
    // https://xrpl.org/cryptographic-keys.html#key-derivation
    // The secp256k1 path requires the family-generator (root +
    // intermediate → master); a root-only derivation produces a
    // different address and silently breaks auth for any user
    // whose wallet was generated by a standard XRPL tool.

    #[test]
    fn derive_keypair_secp256k1_xrpl_spec_vector() {
        // XRPL spec test vector — secp256k1.
        let seed = "snoPBrXtMeMyMHUVTgbuqAfg1SUTb";
        let expected_address = "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh";
        let expected_pubkey = "0330E7FC9D56BB25D6893BA3F317AE5BCF33B3291BD63DB32654A313222F7FD020";

        let kp = derive_keypair_from_seed(seed)
            .expect("derive_keypair_from_seed must accept the canonical XRPL secp256k1 seed");
        assert_eq!(
            kp.address, expected_address,
            "secp256k1 master address mismatch — XRPL family generator (root + intermediate → master) is not implemented"
        );
        assert_eq!(
            kp.compressed_pubkey_hex.to_uppercase(),
            expected_pubkey,
            "secp256k1 master pubkey mismatch"
        );
    }

    #[test]
    fn derive_keypair_ed25519_xrpl_spec_vector() {
        // XRPL ed25519 — verified against xrpl-py 2026-04-26 for the same
        // seed. Our ed25519 path is correct (no family generator on
        // ed25519); this test pins the canonical mapping so it stays
        // correct. The seed/address pair may also be cross-checked on
        // https://xrpl.org/accounts/{address} via testnet faucet.
        let seed = "sEdTM1uX8pu2do5XvTnutH6HsouMaM2";
        let expected_address = "rG31cLyErnqeVj2eomEjBZtq7PYaupGYzL";
        let expected_pubkey = "EDA57EBBCB502C2009EFE17229E8DC865DCCB192C52D7888D624DC9EBADDB815F0";

        let kp = derive_keypair_from_seed(seed).expect("ed25519 seed must derive");
        assert_eq!(kp.address, expected_address, "ed25519 address mismatch");
        assert_eq!(
            kp.compressed_pubkey_hex.to_uppercase(),
            expected_pubkey,
            "ed25519 pubkey mismatch"
        );
    }
}
