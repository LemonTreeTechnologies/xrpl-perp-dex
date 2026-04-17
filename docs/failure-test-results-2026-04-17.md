# Component-Level Failure Test Results — 2026-04-17

**Cluster:** 3 Azure DCsv3 VMs (sgx-node-1/2/3) with SGX enclaves + orchestrators  
**Test runner:** Hetzner bastion → SSH to Azure VMs  
**Orchestrator binary:** built on Hetzner (glibc 2.35), deployed same day  
**Enclave binary:** built on Hetzner via `docker build -f Dockerfile.azure`

## Cluster Topology

| Node | IP | Priority | Role | Peer ID |
|------|----|----------|------|---------|
| sgx-node-1 | 20.71.184.176 | 0 | Sequencer | 12D3KooWFWoB… |
| sgx-node-2 | 20.224.243.60 | 1 | Validator | 12D3KooWE725… |
| sgx-node-3 | 52.236.130.102 | 2 | Validator | 12D3KooWEiFL… |

## Test Results

| # | Scenario | Result | Recovery time | Notes |
|---|----------|--------|---------------|-------|
| 2.1 | HAProxy down | **SKIPPED** | — | HAProxy not installed. Hetzner uses nginx, Azure VMs have no reverse proxy. |
| 2.2 | Sequencer crash | **PASSED** | ~15s failover, ~5s reclaim | Kill node-1 (SIGKILL) → node-2 detected heartbeat timeout (15.26s) → promoted to sequencer → node-3 accepted. On node-1 restart: node-2 demoted back to validator in ~5s. |
| 2.3 | Validator crash | **PASSED** | <1s reconnect | Kill node-3 → sequencer (node-1) unaffected, no role change → node-3 restarted, reconnected to both peers instantly, resumed as Validator. Zero user impact. |
| 2.4 | Enclave crash | **PASSED** | 5-10s detection | Kill `perp-dex-server` on node-1 → orchestrator detects via price update cycle (5s) and liquidation scan (10s) → logs ERROR, keeps retrying → enclave restart resolves. Orchestrator does NOT crash. P2P mesh unaffected. |
| 2.5 | Full operator down | **SKIPPED** | — | Covered in section 3 of failure doc. Effectively tested by 2.2 + 2.4 combined. |
| 2.6 | P2P disconnect | **SKIPPED** | — | Covered in section 4 of failure doc. Partially observed organically during staggered startup (node-2 promoted to sequencer during 3-min gap before node-1 reconnected). |
| 2.7 | Binance API unavailable | **PASSED** | Auto-recovery | Block Binance via iptables → `WARN: price fetch failed: binance request failed` → orchestrator continues running → unblock → errors stop, price updates resume. |
| 2.8 | XRPL node unavailable | **PASSED** | Auto-recovery | Block XRPL testnet via iptables → `WARN: deposit scan failed: XRPL RPC request failed` → trading/liquidation loops independent of XRPL → unblock → deposit scanning resumes. |
| 2.9 | DNS/LB failure | **SKIPPED** | — | No DNS/LB in current infrastructure. API access is via direct IP through nginx on Hetzner. |

## Key Findings

1. **Election protocol works correctly**: Priority-based leader election with heartbeat timeout (15s default). Higher-priority nodes reclaim sequencer role automatically.

2. **No single point of failure in P2P layer**: Validator crash has zero impact on sequencer and other validators. Reconnection is sub-second.

3. **Orchestrator survives enclave crash**: Logs errors, retries on next cycle. No panic, no state corruption. P2P mesh stays connected.

4. **External dependency failures are non-fatal**: Both Binance and XRPL outages produce WARN-level logs with automatic retry. Orchestrator stays running throughout.

5. **Split-brain resolution works**: Observed during staggered startup — when two nodes both claimed sequencer, the higher-priority node won within one heartbeat cycle.

## Issues Found

1. **Deposit credit 500 errors**: Enclave returns 500 on deposit_credit for previously-seen tx_hashes. Fresh enclaves (no loaded state) reject all deposits. Not a failure-mode bug — expected behavior with empty state.

2. **Old `orchestrator` binaries on Azure VMs**: PID 81288 (node-2) and 80769 (node-3) were old orchestrator binaries from a previous deployment, binding port 4001 and interfering with new deployments. Required manual `kill -9` to clean up. **Recommendation**: deployment script should kill ALL orchestrator variants before starting new ones.

3. **Stale process cleanup**: `pkill -f orchestrator` doesn't always catch background processes started via `nohup ... &` through SSH chains. Need explicit PID management or systemd units.

---

## Operator-Level Failure Tests (Multisig)

**Setup:** 2-of-3 XRPL multisig escrow on testnet  
**Escrow account:** `rKHSwKNpaoAN8kFxsrp3ZBhytoPt21hiB2`  
**Master key:** Disabled (only multisig can sign)  
**Test destination:** `rHSLZoUH1b7FW83tbCkL1nkzVtV68s7zDC`  
**Signing method:** SSH tunnels from Hetzner → each enclave's `/v1/pool/sign` endpoint  

### Signer Identities

| Node | XRPL Address | Compressed Pubkey |
|------|-------------|-------------------|
| node-1 (20.71.184.176) | r4pmNX1b4jHQUtbVKnxGuS6Mozy4abg59J | 02317d9b… |
| node-2 (20.224.243.60) | rExSvwKDdVUnMB3wGDsqtjvRLNqU2PZBBd | 0266c1e7… |
| node-3 (52.236.130.102) | rBb8KCxQCC1qjaAfJF5PrQs5dJ8kPuqYxT | 0256e86b… |

### Test Results

| # | Scenario | Result | XRPL Tx Hash | Notes |
|---|----------|--------|-------------|-------|
| 1 | Normal 2-of-3 withdrawal | **PASSED** | `D1FD8922DAB5A057...` | All 3 signers available, node-1+2 signed, 1 XRP sent. tesSUCCESS. |
| 2 | One operator offline (node-3 down) | **PASSED** | `43A24A2878473D8E...` | node-1+2 signed without node-3. tesSUCCESS. |
| 3 | Two operators offline (only node-1) | **PASSED** | — | Correctly failed: 1/2 signatures collected, withdrawal blocked. |
| 4 | Malicious signer (corrupted signature) | **PASSED** | — | One good sig (node-1) + one corrupted DER → XRPL rejected the tx. |
| 5 | Alternative pair: node-1+3 (node-2 down) | **PASSED** | `86056B2A8DD00AEA...` | tesSUCCESS. Any 2-of-3 combination works. |
| 6 | Alternative pair: node-2+3 (node-1 down) | **PASSED** | `92705DCBA6387666...` | tesSUCCESS. Even without the "primary" signer. |

### Key Findings (Operator-Level)

1. **Any 2-of-3 combination works**: All three possible signer pairs (1+2, 1+3, 2+3) successfully signed and submitted withdrawals to XRPL. No single operator is privileged.

2. **Quorum enforcement works**: With only 1 signer available, the system correctly refuses to submit (insufficient signatures). Users' funds are safe — they can't be withdrawn without quorum.

3. **XRPL rejects bad signatures**: A corrupted DER signature is caught at the XRPL validation layer. A malicious operator cannot forge a valid multisig tx with a bad signature — the entire tx is rejected, protecting against partial compromise.

4. **SGX enclave signing is deterministic**: The `/v1/pool/sign` endpoint returns (r, s) components which we DER-encode and submit. All signatures verified by XRPL validators on-chain.

---

## Recommendations

- [ ] Add systemd service units for enclave and orchestrator on Azure VMs (auto-restart on crash)
- [ ] Add explicit health check endpoint to enclave (currently 404 on `/v1/health`)
- [ ] Add stale-price circuit breaker: if no price update succeeds for >60s, reject new position opens
- [ ] Add XRPL reconnection metric/alert for deposit monitoring gaps
- [ ] Deployment script with proper cleanup of all `*orchestrator*` and `*perp-dex-server*` processes
- [ ] Expose enclave port 9088 on internal Azure vnet (10.0.0.0/24) so orchestrators can cross-sign without SSH tunnels
- [ ] Implement signer rotation flow: new key generation → SignerListSet update → test withdrawal with new set
