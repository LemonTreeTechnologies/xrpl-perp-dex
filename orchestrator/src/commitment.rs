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

// The Gnosis-Safe execTransaction ABI has 11 params (fixed by the contract) and the
// submit helper mirrors it — clippy's arg-count lint is spurious for an external ABI.
#![allow(clippy::too_many_arguments)]

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
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

sol! {
    #[sol(rpc)]
    contract GnosisSafe {
        function nonce() external view returns (uint256);
        // Gnosis Safe v1.3.0/1.4.1 — the enclave signs the SafeTxHash; this orchestrator
        // only relays the owner signature + pays gas (AC-R2-1). operation=0 (CALL),
        // gas fields 0, gasToken/refundReceiver = zero address.
        function execTransaction(
            address to, uint256 value, bytes data, uint8 operation,
            uint256 safeTxGas, uint256 baseGas, uint256 gasPrice,
            address gasToken, address refundReceiver, bytes signatures
        ) external payable returns (bool success);
    }
}

/// The latest reserves commitment as read from the on-chain registry.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by the 3d publisher + attestation endpoint
pub struct LatestReserves {
    pub epoch: u64,
    pub root: [u8; 32],
    pub snapshot_hash: [u8; 32],
    pub committed_at: u64,
    pub committer: String,
}

/// Read the latest published reserves root from `ReservesRegistry` on Base-Sepolia.
/// `rpc_url` + `registry` come from operator config. Read-only (no signer).
#[allow(dead_code)] // wired by the 3d publisher
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
#[allow(dead_code)] // wired by the 3d publisher (Safe execTransaction data)
pub fn encode_publish_reserves(epoch: u64, root: [u8; 32], snapshot_hash: [u8; 32]) -> Vec<u8> {
    use alloy::sol_types::SolCall;
    ReservesRegistry::publishReservesCall {
        epoch,
        root: FixedBytes::<32>::from(root),
        snapshotHash: FixedBytes::<32>::from(snapshot_hash),
    }
    .abi_encode()
}

/// Read the Safe's current on-chain nonce — the value the enclave must bind into
/// the SafeTxHash it signs. Read-only (no signer).
#[allow(dead_code)] // wired by the 3d publisher
pub async fn query_safe_nonce(rpc_url: &str, safe: &str) -> Result<u64> {
    let safe_addr: Address = safe.parse().context("invalid safe address")?;
    let provider = ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .context("connect Base-Sepolia RPC")?;
    let s = GnosisSafe::new(safe_addr, &provider);
    let n = s.nonce().call().await.context("safe.nonce()")?;
    Ok(n.to::<u64>())
}

/// Submit the enclave-signed `publishReserves` via the Safe (1-of-1 at Tier-1).
///
/// `owner_sig` is the enclave's 65-byte `[r‖s‖v]` over the SafeTxHash (v ∈ {27,28}),
/// which is exactly the Safe's expected ECDSA owner-signature encoding. `gas_key` is
/// a gas-paying EOA private key (hex) — NOT the enclave key: it is only `msg.sender`
/// for `execTransaction` and pays Base-Sepolia gas; the Safe verifies the owner sig,
/// so this orchestrator can never forge the authorised call (AC-R2-1).
#[allow(dead_code)] // wired by the 3d publisher
pub async fn submit_reserves_via_safe(
    rpc_url: &str,
    gas_key: &str,
    safe: &str,
    registry: &str,
    epoch: u64,
    root: [u8; 32],
    snapshot_hash: [u8; 32],
    owner_sig: [u8; 65],
) -> Result<String> {
    let safe_addr: Address = safe.parse().context("invalid safe address")?;
    let registry_addr: Address = registry.parse().context("invalid registry address")?;
    let signer: PrivateKeySigner = gas_key
        .trim_start_matches("0x")
        .parse()
        .context("parse gas EOA key")?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(rpc_url)
        .await
        .context("connect Base-Sepolia RPC (wallet)")?;

    let data = encode_publish_reserves(epoch, root, snapshot_hash);
    let s = GnosisSafe::new(safe_addr, &provider);
    let pending = s
        .execTransaction(
            registry_addr,                   // to = ReservesRegistry
            U256::ZERO,                      // value
            Bytes::from(data),               // data = publishReserves(epoch, root, snapshot)
            0u8,                             // operation = CALL
            U256::ZERO,                      // safeTxGas
            U256::ZERO,                      // baseGas
            U256::ZERO,                      // gasPrice
            Address::ZERO,                   // gasToken
            Address::ZERO,                   // refundReceiver
            Bytes::from(owner_sig.to_vec()), // signatures = enclave r‖s‖v
        )
        .send()
        .await
        .context("send Safe execTransaction")?;
    let receipt = pending
        .get_receipt()
        .await
        .context("await execTransaction receipt")?;
    Ok(format!("{:#x}", receipt.transaction_hash))
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
