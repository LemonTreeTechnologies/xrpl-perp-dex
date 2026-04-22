//! On-chain state commitment to Sepolia CommitmentRegistryV4.
//!
//! Periodically publishes TEE-signed Merkle root of perp state to Ethereum,
//! providing proof-of-reserves and audit trail.

use anyhow::{Context, Result};
use ethers::prelude::*;
use ethers::types::{Address, U64};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, info};

/// CommitmentRegistryV4 on Sepolia
pub const REGISTRY_ADDRESS: &str = "0x77291022F57D2E94E70D619623f917C6D7edA626";
#[allow(dead_code)]
pub const SEPOLIA_RPC: &str = "https://rpc.sepolia.org";
#[allow(dead_code)]
pub const SEPOLIA_CHAIN_ID: u64 = 11155111;

// ABI for commit() function
abigen!(
    CommitmentRegistry,
    r#"[
        function commit(bytes32 marketId, bytes32 root, bytes32 snapshotHash, uint8 v, bytes32 r, bytes32 s) external
        function get(bytes32 marketId) external view returns (bytes32 root, bytes32 snapshotHash, uint64 committedAt, address committer)
        function isCommitted(bytes32 marketId) external view returns (bool)
    ]"#
);

/// State commitment data
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct StateCommitment {
    pub root: String,
    pub snapshot_hash: String,
    pub v: u8,
    pub r: String,
    pub s: String,
    pub market_id: String,
    pub enclave_address: String,
    pub timestamp: u64,
}

/// Compute state hash from enclave balance data.
#[allow(dead_code)]
pub fn compute_state_hashes(balance_json: &str) -> Result<(String, String)> {
    use sha2::{Digest, Sha256};
    let snapshot_hash = Sha256::digest(balance_json.as_bytes());
    let root = Sha256::digest(snapshot_hash);
    Ok((hex::encode(root), hex::encode(snapshot_hash)))
}

/// Sign state commitment via enclave (keccak256(root || snapshot_hash)).
#[allow(dead_code)]
pub async fn sign_commitment(
    enclave_url: &str,
    account_address: &str,
    session_key: &str,
    root_hex: &str,
    snapshot_hash_hex: &str,
) -> Result<(String, String, u8)> {
    use sha3::{Digest, Keccak256};

    let root_bytes = hex::decode(root_hex).context("invalid root hex")?;
    let snap_bytes = hex::decode(snapshot_hash_hex).context("invalid snapshot hex")?;

    let mut hasher = Keccak256::new();
    hasher.update(&root_bytes);
    hasher.update(&snap_bytes);
    let digest = hasher.finalize();
    let hash_hex = format!("0x{}", hex::encode(digest));

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .post(format!("{enclave_url}/pool/sign"))
        .json(&serde_json::json!({
            "from": account_address,
            "hash": hash_hex,
            "session_key": session_key,
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let sig = resp.get("signature").context("no signature in response")?;
    let r = sig.get("r").and_then(|v| v.as_str()).context("no r")?;
    let s = sig.get("s").and_then(|v| v.as_str()).context("no s")?;
    let v = sig.get("v").and_then(|v| v.as_u64()).context("no v")? as u8;

    Ok((r.to_string(), s.to_string(), v))
}

/// Submit commitment to Sepolia CommitmentRegistryV4.
#[allow(dead_code)]
pub async fn submit_to_sepolia(
    private_key_hex: &str,
    market_id: [u8; 32],
    root: [u8; 32],
    snapshot_hash: [u8; 32],
    v: u8,
    r: [u8; 32],
    s: [u8; 32],
) -> Result<String> {
    let provider =
        Provider::<Http>::try_from(SEPOLIA_RPC).context("failed to connect to Sepolia RPC")?;

    let wallet: LocalWallet = private_key_hex
        .parse::<LocalWallet>()
        .context("invalid private key")?
        .with_chain_id(SEPOLIA_CHAIN_ID);

    let client = SignerMiddleware::new(provider, wallet);
    let client = Arc::new(client);

    let registry_addr: Address = REGISTRY_ADDRESS
        .parse()
        .context("invalid registry address")?;
    let registry = CommitmentRegistry::new(registry_addr, client);

    let tx = registry.commit(market_id, root, snapshot_hash, v, r, s);

    info!("Submitting commitment to Sepolia...");
    let pending = tx.send().await.context("failed to send tx")?;
    let tx_hash = pending.tx_hash();
    info!(tx_hash = %tx_hash, "Commitment tx submitted");

    let receipt = pending
        .await
        .context("failed to get receipt")?
        .context("tx dropped")?;

    let status = receipt.status.unwrap_or(U64::zero());
    if status == U64::zero() {
        error!(tx_hash = %tx_hash, "Commitment tx REVERTED");
        anyhow::bail!("tx reverted");
    }

    info!(
        tx_hash = %tx_hash,
        block = %receipt.block_number.unwrap_or_default(),
        gas = %receipt.gas_used.unwrap_or_default(),
        "Commitment published on Sepolia"
    );

    Ok(format!("0x{tx_hash:x}"))
}

/// Query existing commitment from Sepolia.
#[allow(dead_code)]
pub async fn query_commitment(market_id: [u8; 32]) -> Result<Option<StateCommitment>> {
    let provider =
        Provider::<Http>::try_from(SEPOLIA_RPC).context("failed to connect to Sepolia RPC")?;
    let client = Arc::new(provider);

    let registry_addr: Address = REGISTRY_ADDRESS.parse()?;
    let registry = CommitmentRegistry::new(registry_addr, client);

    let is_committed = registry.is_committed(market_id).call().await?;
    if !is_committed {
        return Ok(None);
    }

    let (root, snapshot_hash, committed_at, committer) = registry.get(market_id).call().await?;

    Ok(Some(StateCommitment {
        root: hex::encode(root),
        snapshot_hash: hex::encode(snapshot_hash),
        v: 0,
        r: String::new(),
        s: String::new(),
        market_id: hex::encode(market_id),
        enclave_address: format!("{committer:?}"),
        timestamp: committed_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_state_hashes() {
        let json = r#"{"margin_balance":"100.00000000","positions":[]}"#;
        let (root, snap) = compute_state_hashes(json).unwrap();
        assert_eq!(root.len(), 64);
        assert_eq!(snap.len(), 64);
        assert_ne!(root, snap);
    }
}
