//! On-chain state commitment to Sepolia CommitmentRegistryV4.
//!
//! Periodically publishes TEE-signed Merkle root of perp state to Ethereum,
//! providing proof-of-reserves and audit trail.
//!
//! Flow:
//! 1. Query enclave for state hash (balances, positions, insurance fund)
//! 2. Enclave signs keccak256(root || snapshot_hash) with ECDSA key
//! 3. Orchestrator submits commit() to CommitmentRegistryV4 on Sepolia

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// CommitmentRegistryV4 on Sepolia
pub const REGISTRY_ADDRESS: &str = "0x77291022F57D2E94E70D619623f917C6D7edA626";
pub const SEPOLIA_RPC: &str = "https://rpc.sepolia.org";

/// State commitment data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCommitment {
    /// Merkle root of state (users + positions + balances)
    pub root: [u8; 32],
    /// Hash of raw state snapshot
    pub snapshot_hash: [u8; 32],
    /// ECDSA signature (v, r, s) from enclave
    pub v: u8,
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// Market/state identifier
    pub market_id: [u8; 32],
    /// Enclave address that signed
    pub enclave_address: String,
    /// Timestamp
    pub timestamp: u64,
}

/// Compute state hash from enclave balance data.
/// Returns (root, snapshot_hash) as hex strings.
pub fn compute_state_hashes(balance_json: &str) -> Result<(String, String)> {
    use sha2::{Digest, Sha256};

    // snapshot_hash = SHA-256 of raw balance JSON
    let snapshot_hash = Sha256::digest(balance_json.as_bytes());

    // root = SHA-256 of snapshot_hash (simplified Merkle — single leaf for PoC)
    // In production: proper Merkle tree of individual user balances
    let root = Sha256::digest(&snapshot_hash);

    Ok((hex::encode(root), hex::encode(snapshot_hash)))
}

/// Sign state commitment via enclave.
/// Enclave signs keccak256(root || snapshot_hash) with its ECDSA key.
pub async fn sign_commitment(
    enclave_url: &str,
    account_address: &str,
    session_key: &str,
    root_hex: &str,
    snapshot_hash_hex: &str,
) -> Result<(String, String, u8)> {
    // Compute keccak256(root || snapshot_hash)
    // For Ethereum ecrecover compatibility
    use sha3::{Digest, Keccak256};
    let root_bytes = hex::decode(root_hex).context("invalid root hex")?;
    let snap_bytes = hex::decode(snapshot_hash_hex).context("invalid snapshot hex")?;

    let mut hasher = Keccak256::new();
    hasher.update(&root_bytes);
    hasher.update(&snap_bytes);
    let digest = hasher.finalize();
    let hash_hex = format!("0x{}", hex::encode(&digest));

    // Sign via enclave
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client
        .post(format!("{}/pool/sign", enclave_url))
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

/// Log commitment info (for PoC — actual Sepolia submission requires ethers.rs or web3)
pub fn log_commitment(
    market_id: &str,
    root: &str,
    snapshot_hash: &str,
    r: &str,
    s: &str,
    v: u8,
    enclave_address: &str,
) {
    info!(
        market_id = %market_id,
        root = %root,
        snapshot_hash = %snapshot_hash,
        enclave = %enclave_address,
        "State commitment ready for Sepolia submission"
    );
    info!(
        "CommitmentRegistryV4.commit(marketId, root, snapshotHash, v={}, r=0x{}, s=0x{})",
        v, r, s
    );
    info!(
        "Contract: {} on Sepolia",
        REGISTRY_ADDRESS
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_state_hashes() {
        let json = r#"{"margin_balance":"100.00000000","positions":[]}"#;
        let (root, snap) = compute_state_hashes(json).unwrap();
        assert_eq!(root.len(), 64); // 32 bytes hex
        assert_eq!(snap.len(), 64);
        assert_ne!(root, snap); // root != snapshot (root = hash of hash)
    }
}
