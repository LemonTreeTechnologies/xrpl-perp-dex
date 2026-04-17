//! CLI subcommands for operator onboarding and escrow setup.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::xrpl_signer;

// ── operator-setup ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
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

    // Step 1: Derive escrow address from seed (or use override)
    println!("[1/4] Resolving escrow address...");
    let escrow_address = if let Some(addr) = escrow_address_override {
        println!("  Using provided address: {addr}");
        addr.to_string()
    } else {
        let addr = derive_xrpl_address_from_seed(escrow_seed)?;
        println!("  Derived from seed: {addr}");
        addr
    };

    // Step 2: Check account exists
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

    // Step 3: Submit SignerListSet
    println!("\n[3/4] Submitting SignerListSet...");
    let signer_entries: Vec<serde_json::Value> = config
        .signers
        .iter()
        .map(|s| {
            serde_json::json!({
                "SignerEntry": {
                    "Account": s.xrpl_address,
                    "SignerWeight": 1
                }
            })
        })
        .collect();

    let sls_tx = serde_json::json!({
        "TransactionType": "SignerListSet",
        "Account": escrow_address,
        "SignerQuorum": config.quorum,
        "SignerEntries": signer_entries,
    });

    let sls_result = sign_and_submit(xrpl_url, &escrow_seed, &sls_tx).await?;
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
        let acset_tx = serde_json::json!({
            "TransactionType": "AccountSet",
            "Account": escrow_address,
            "SetFlag": 4, // asfDisableMaster
        });

        let acset_result = sign_and_submit(xrpl_url, &escrow_seed, &acset_tx).await?;
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

/// Derive XRPL classic address from an XRPL seed (sEd... format).
/// Uses the Ed25519 or secp256k1 derivation depending on seed prefix.
fn derive_xrpl_address_from_seed(seed: &str) -> Result<String> {
    // XRPL seeds starting with sEd are Ed25519
    // For secp256k1 seeds (s...), we'd need different derivation
    // Use the sign_and_submit with account_info as a workaround:
    // we can't easily derive without xrpl-py, so we'll use the sign RPC
    // which returns the Account field.
    //
    // Actually, for the CLI tool we just need the address. The simplest
    // approach: sign a dummy tx and extract Account from the response.
    // But that requires network access.
    //
    // Alternative: decode the seed and derive. XRPL seeds are base58-encoded
    // with the XRPL alphabet. Let's decode and derive.

    use k256::ecdsa::SigningKey;
    use sha2::Digest;

    const XRPL_ALPHABET: &[u8; 58] =
        b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";
    let alphabet = bs58::Alphabet::new(XRPL_ALPHABET).expect("valid alphabet");

    let decoded = bs58::decode(seed)
        .with_alphabet(&alphabet)
        .into_vec()
        .context("invalid seed encoding")?;

    // decoded = [version_byte] [16_byte_entropy] [4_byte_checksum]
    if decoded.len() < 21 {
        anyhow::bail!("seed too short: {} bytes", decoded.len());
    }

    let version = decoded[0];
    let entropy = &decoded[1..17];

    // version 0x21 = secp256k1 family key, 0x01 = Ed25519
    // sEd... seeds use 0x01 (Ed25519), but XRPL testnet faucet
    // generates secp256k1 seeds by default.

    if version == 0x01 {
        // Ed25519 derivation
        // Private key = SHA-512(entropy)[0..32]
        use sha2::Sha512;
        let hash = Sha512::digest(entropy);
        let privkey_bytes = &hash[..32];

        // Ed25519 public key derivation
        // For Ed25519, we'd need the ed25519-dalek crate. Since XRPL
        // sEd seeds are Ed25519, let's handle this:
        let _ = &hash[..32]; // suppress unused warning
        anyhow::bail!(
            "Ed25519 seeds (sEd...) not supported yet. Use secp256k1 seed or provide --escrow-address directly."
        );
    }

    // secp256k1 derivation (version 0x21)
    // Root keypair: SHA-512(entropy + sequence)[0..32] until valid
    use sha2::Sha512;
    let mut seq: u32 = 0;
    let signing_key = loop {
        let mut hasher = Sha512::new();
        hasher.update(entropy);
        hasher.update(seq.to_be_bytes());
        let hash = hasher.finalize();
        let candidate = &hash[..32];
        if let Ok(sk) = SigningKey::from_slice(candidate) {
            break sk;
        }
        seq += 1;
        if seq > 100 {
            anyhow::bail!("failed to derive secp256k1 key from seed");
        }
    };

    // Get public key (compressed)
    let verifying_key = signing_key.verifying_key();
    let compressed = verifying_key.to_encoded_point(true);
    let compressed_bytes = compressed.as_bytes();

    // XRPL address from compressed pubkey
    let sha256 = sha2::Sha256::digest(compressed_bytes);
    let account_id = ripemd::Ripemd160::digest(sha256);

    let mut payload = Vec::with_capacity(25);
    payload.push(0x00);
    payload.extend_from_slice(&account_id);
    let h1 = sha2::Sha256::digest(&payload);
    let h2 = sha2::Sha256::digest(h1);
    payload.extend_from_slice(&h2[..4]);

    let address = bs58::encode(&payload)
        .with_alphabet(&alphabet)
        .into_string();

    Ok(address)
}

/// Sign a transaction using the XRPL `sign` RPC and submit it.
async fn sign_and_submit(
    xrpl_url: &str,
    seed: &str,
    tx_json: &serde_json::Value,
) -> Result<serde_json::Value> {
    let http = reqwest::Client::new();

    // Use XRPL sign RPC (sends seed to the server — acceptable for testnet)
    let sign_resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "sign",
            "params": [{
                "secret": seed,
                "tx_json": tx_json,
            }]
        }))
        .send()
        .await
        .context("XRPL sign request failed")?
        .json()
        .await
        .context("XRPL sign response parse failed")?;

    let signed_blob = sign_resp["result"]["tx_blob"]
        .as_str()
        .context("no tx_blob in sign response — check seed format")?;

    info!(
        tx_type = tx_json["TransactionType"].as_str().unwrap_or("?"),
        "signed, submitting..."
    );

    // Submit
    let submit_resp: serde_json::Value = http
        .post(xrpl_url)
        .json(&serde_json::json!({
            "method": "submit",
            "params": [{"tx_blob": signed_blob}]
        }))
        .send()
        .await
        .context("XRPL submit request failed")?
        .json()
        .await
        .context("XRPL submit response parse failed")?;

    Ok(submit_resp)
}
