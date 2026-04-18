//! CLI subcommands for operator onboarding and escrow setup.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::xrpl_signer;

// ── XRPL binary serialization ──────────────────────────────────
//
// Minimal implementation covering SignerListSet and AccountSet.
// Reference: https://xrpl.org/serialization.html

const HASH_PREFIX_TX_SIGN: [u8; 4] = [0x53, 0x54, 0x58, 0x00]; // "STX\0"

const XRPL_ALPHABET: &[u8; 58] =
    b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";

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
        Self { type_code: 1, field_code, data: val.to_be_bytes().to_vec() }
    }
    fn uint32(field_code: u8, val: u32) -> Self {
        Self { type_code: 2, field_code, data: val.to_be_bytes().to_vec() }
    }
    fn amount_drops(field_code: u8, drops: u64) -> Self {
        Self { type_code: 6, field_code, data: (0x4000000000000000u64 | drops).to_be_bytes().to_vec() }
    }
    fn blob(field_code: u8, bytes: &[u8]) -> Self {
        let mut data = encode_vl_length(bytes.len());
        data.extend_from_slice(bytes);
        Self { type_code: 7, field_code, data }
    }
    fn account_id(field_code: u8, id: &[u8; 20]) -> Self {
        let mut data = vec![20u8];
        data.extend_from_slice(id);
        Self { type_code: 8, field_code, data }
    }
    fn sort_key(&self) -> (u8, u8) { (self.type_code, self.field_code) }
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

fn serialize_fields(fields: &mut Vec<XrplField>, array_suffix: Option<&[u8]>) -> Vec<u8> {
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

// ── operator-setup ───────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct SignerEntry {
    name: String,
    enclave_url: String,
    address: String,
    session_key: String,
    compressed_pubkey: String,
    xrpl_address: String,
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
struct PoolStatusResponse {
    status: String,
    accounts: Option<Vec<PoolAccount>>,
}

#[derive(Debug, Deserialize)]
struct PoolAccount {
    address: String,
    is_active: bool,
}

pub async fn operator_setup(
    enclave_url: &str,
    name: &str,
    output: Option<&std::path::Path>,
) -> Result<()> {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    println!("Operator Setup");
    println!("==============");
    println!("Enclave: {enclave_url}");
    println!("Name:    {name}");
    println!();

    // Step 1: Generate a new keypair in the enclave
    println!("[1/3] Generating keypair in enclave...");
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
    let uncompressed_pubkey = resp
        .public_key
        .context("missing public_key in response")?;
    let session_key = resp
        .session_key
        .context("missing session_key in response")?;

    println!("  Ethereum address: {eth_address}");
    println!("  Session key:      {session_key}");

    // Step 2: Derive XRPL address from uncompressed pubkey
    println!("\n[2/3] Deriving XRPL address...");
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

    // Step 3: Output signer entry
    let entry = SignerEntry {
        name: name.to_string(),
        enclave_url: enclave_url.to_string(),
        address: eth_address,
        session_key,
        compressed_pubkey: compressed_hex,
        xrpl_address: xrpl_address.clone(),
    };

    let json = serde_json::to_string_pretty(&entry)?;

    println!("\n[3/3] Signer entry:");
    println!("{json}");

    if let Some(path) = output {
        std::fs::write(path, &json)
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("\nWritten to {}", path.display());
    }

    println!("\nNext steps:");
    println!("  1. Add this entry to signers_config.json");
    println!("  2. Run `escrow-setup` to configure XRPL SignerListSet");
    println!(
        "  3. Verify on XRPL explorer: https://testnet.xrpl.org/accounts/{xrpl_address}"
    );

    Ok(())
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
        XrplField::uint16(2, 12),                              // TransactionType = SignerListSet
        XrplField::uint32(4, sequence as u32),                 // Sequence
        XrplField::uint32(35, config.quorum),                  // SignerQuorum
        XrplField::amount_drops(8, 12),                        // Fee = 12 drops
        XrplField::account_id(1, &account_id),                 // Account
    ];

    let sls_blob = sign_xrpl_tx(
        &keypair,
        &mut sls_fields,
        Some(&signer_entries_suffix),
    )?;

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
            XrplField::uint16(2, 3),                           // TransactionType = AccountSet
            XrplField::uint32(4, (sequence + 1) as u32),       // Sequence (next)
            XrplField::uint32(33, 4),                          // SetFlag = asfDisableMaster
            XrplField::amount_drops(8, 12),                    // Fee = 12 drops
            XrplField::account_id(1, &account_id),             // Account
        ];

        let acset_blob = sign_xrpl_tx(
            &keypair,
            &mut acset_fields,
            None,
        )?;

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
    println!(
        "  Explorer: https://testnet.xrpl.org/accounts/{escrow_address}"
    );
    println!("\nNext: start orchestrators with --escrow-address {escrow_address} --signers-config <path>");

    Ok(())
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
    let is_ed25519 = decoded.len() >= 23
        && decoded[0] == 0x01
        && decoded[1] == 0xE1
        && decoded[2] == 0x4B;

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

    // secp256k1 derivation (version 0x21)
    let entropy = &decoded[1..17];
    use k256::ecdsa::SigningKey;
    let mut seq: u32 = 0;
    let signing_key = loop {
        let mut hasher = sha2::Sha512::new();
        hasher.update(entropy);
        hasher.update(seq.to_be_bytes());
        let hash = hasher.finalize();
        if let Ok(sk) = SigningKey::from_slice(&hash[..32]) {
            break sk;
        }
        seq += 1;
        if seq > 100 {
            anyhow::bail!("failed to derive secp256k1 key from seed");
        }
    };

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
        let entry: SignerEntry =
            serde_json::from_str(&data).with_context(|| format!("invalid JSON in {}", path.display()))?;
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

    println!("\nCreated {} with {} signers, quorum={}", output.display(), config.signers.len(), quorum);
    println!("\nNext steps:");
    println!("  1. Add escrow_seed to the config (keep it secret!)");
    println!("  2. Run `escrow-setup --signers-config {} --escrow-seed <seed>`", output.display());

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
        anyhow::bail!("signer '{}' already exists in config", name);
    }

    // Step 1: Generate keypair
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

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
            uncompressed_pubkey.strip_prefix("0x").unwrap_or(&uncompressed_pubkey),
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
        println!("\n  Re-submitting SignerListSet with {} signers...", config.signers.len());
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

pub async fn sign_request(
    seed: &str,
    method: &str,
    url: &str,
    body: Option<&str>,
) -> Result<()> {
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
) -> Result<()> {
    let keypair = derive_keypair_from_seed(seed)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let body = serde_json::json!({
        "user_id": keypair.address,
        "amount": amount,
        "destination": destination,
    });
    let body_str = serde_json::to_string(&body)?;
    let sig_hex = sign_body(&keypair, body_str.as_bytes(), timestamp)?;

    println!("Withdraw");
    println!("========");
    println!("From:        {}", keypair.address);
    println!("Amount:      {amount}");
    println!("Destination: {destination}");
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

