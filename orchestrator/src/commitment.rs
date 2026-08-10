//! On-chain proof-of-liabilities: publish the enclave-signed reserves root to the
//! `ReservesRegistry` on **Base-Sepolia** (chain 84532).
//!
//! #131 chunk 2 (alloy tx-path). This module is now ONLY the alloy transaction /
//! query layer — it does NOT produce or sign the root. Per RESP AC-4 the root is
//! built + signed INSIDE the enclave (a new ecall, chunk 3), and the actual
//! `publishReserves` call is authorised by the cluster's 2-of-3 Safe (chunk 5), so
//! the earlier orchestrator-side `compute_state_hashes` / `sign_commitment` path —
//! which computed the root outside the TEE and had the enclave blind-sign it — is
//! deleted. The old ethers-rs stack (43 crates, the cargo-audit ethereum tail) is
//! gone with it.
//!
//! RPC URL + registry address are operator config (never hardcoded) — the RPC
//! embeds an API key.

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::ProviderBuilder;
use alloy::sol;
use anyhow::{Context, Result};

/// Base-Sepolia chain id (informational; the provider derives it from the RPC).
pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

sol! {
    #[sol(rpc)]
    contract ReservesRegistry {
        function publishReserves(uint64 epoch, bytes32 root, bytes32 snapshotHash) external;
        function latestReserves() external view returns (
            uint64 epoch, bytes32 root, bytes32 snapshotHash, uint64 committedAt, address committer
        );
        function authority() external view returns (address);
    }
}

/// The latest reserves commitment as read from the on-chain registry.
#[derive(Debug, Clone)]
pub struct LatestReserves {
    pub epoch: u64,
    pub root: [u8; 32],
    pub snapshot_hash: [u8; 32],
    pub committed_at: u64,
    pub committer: String,
}

/// Read the latest published reserves root from `ReservesRegistry` on Base-Sepolia.
/// `rpc_url` + `registry` come from operator config. Read-only (no signer).
pub async fn query_latest_reserves(rpc_url: &str, registry: &str) -> Result<LatestReserves> {
    let addr: Address = registry.parse().context("invalid registry address")?;
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .context("connect Base-Sepolia RPC")?;
    let reg = ReservesRegistry::new(addr, &provider);
    let r = reg
        .latestReserves()
        .call()
        .await
        .context("eth_call latestReserves")?;
    Ok(LatestReserves {
        epoch: r.epoch,
        root: r.root.0,
        snapshot_hash: r.snapshotHash.0,
        committed_at: r.committedAt,
        committer: r.committer.to_string(),
    })
}

/// Encode the `publishReserves(epoch, root, snapshotHash)` calldata. This is what
/// the 2-of-3 Safe execTransaction wraps (chunk 5); returning the calldata keeps
/// this module signer-free — the enclave-produced root goes in, the Safe (not this
/// orchestrator) authorises the send.
pub fn encode_publish_reserves(epoch: u64, root: [u8; 32], snapshot_hash: [u8; 32]) -> Vec<u8> {
    use alloy::sol_types::SolCall;
    ReservesRegistry::publishReservesCall {
        epoch,
        root: FixedBytes::<32>::from(root),
        snapshotHash: FixedBytes::<32>::from(snapshot_hash),
    }
    .abi_encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_reserves_calldata_is_well_formed() {
        // selector (4 bytes) + 3 * 32-byte args = 100 bytes; deterministic encoding.
        let cd = encode_publish_reserves(7, [0x11u8; 32], [0x22u8; 32]);
        assert_eq!(cd.len(), 4 + 32 * 3);
        // uint64 epoch left-pads into the first word; last byte of word 1 == 7.
        assert_eq!(cd[4 + 31], 7);
        // same inputs → identical calldata (ABI determinism — AC-5 behavioural check).
        assert_eq!(cd, encode_publish_reserves(7, [0x11u8; 32], [0x22u8; 32]));
    }
}
