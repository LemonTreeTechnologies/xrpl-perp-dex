# Infrastructure State — 2026-04-17

## MAINNET (Hetzner — DO NOT TOUCH without explicit approval)

| Component | Value |
|-----------|-------|
| **Host** | 94.130.18.162 (Hetzner EX44) |
| **Escrow** | `r4rwwSM9PUu7VcvPRWdu9pmZpmhCZS9mmc` |
| **Balance** | ~108.36 XRP (108,359,878 drops) as of 2026-04-17 |
| **XRPL URL** | `https://s1.ripple.com:51234` (mainnet) |
| **Enclave** | PID 1601200, port 9088 (127.0.0.1), running since Apr 7 |
| **Orchestrator** | PID 2044357, port 3000 (127.0.0.1) + P2P 4001 (0.0.0.0), running since Apr 12 |
| **Signers config** | `/tmp/perp-9088/multisig_escrow_mainnet.json` |
| **Vault flags** | `--vault-mm --vault-dn` |
| **Cross-signing** | SSH tunnels to Azure enclaves (ports 9188/9189/9190 → remote 9088) |
| **Frontend** | xperp.fi/trade → nginx → localhost:3000 |
| **Seed in config** | `escrow_seed` present in multisig_escrow_mainnet.json — migration risk |

### Mainnet multisig signers (2-of-3)

| Signer | XRPL Address | Azure VM IP | Tunnel port |
|--------|-------------|-------------|-------------|
| sgx-node-1 | rKm1wQe8rXyeYytfBL87z1thwLeZBUkk8S | 20.71.184.176 | 9188 |
| sgx-node-2 | rN8yc8EDTTR9RpeqL2KtP2tGaAPm9JV1Ph | 20.224.243.60 | 9189 |
| sgx-node-3 | r96ZkV62h4VQViCWeqQ35xAaUTwiFAcxrK | 52.236.130.102 | 9190 |

### Other services on Hetzner

| Service | PID | Port | Since |
|---------|-----|------|-------|
| ethsigner-server | 1432357 | 8085 | Apr 3 |
| auto_publish_daemon.py | 1632949 | — | Apr 8 |
| bitcoind | 2417092 | 18443 | — |
| nginx | — | 80, 443 | — |

---

## TESTNET (Azure VMs — safe for testing)

| Component | Value |
|-----------|-------|
| **Escrow** | `r98XyfYpkpzfbGoaTw5jn5AF4Tug924U5z` |
| **XRPL URL** | `https://s.altnet.rippletest.net:51234` (testnet) |
| **Quorum** | 2-of-3 |

### Testnet nodes

| Node | IP | Priority | XRPL Address |
|------|-----|----------|-------------|
| node-1 | 20.71.184.176 | 0 | r4pmNX1b4jHQUtbVKnxGuS6Mozy4abg59J |
| node-2 | 20.224.243.60 | 1 | rExSvwKDdVUnMB3wGDsqtjvRLNqU2PZBBd |
| node-3 | 52.236.130.102 | 2 | rBb8KCxQCC1qjaAfJF5PrQs5dJ8kPuqYxT |

**Note:** Azure VMs run BOTH mainnet enclaves (port 9088, used by Hetzner via SSH tunnel) AND testnet orchestrators (port 3000). The testnet orchestrators use the same enclave but with testnet escrow addresses — enclave doesn't know or care about networks.

---

## Pre-deployment checklist

Before ANY deployment or restart on ANY host:

1. **Identify environment**: Is this mainnet (Hetzner) or testnet (Azure)?
2. **Check running processes**: `ps aux | grep -E "(perp-dex|orchestrator|enclave)"` — identify PIDs
3. **Check ports**: `ss -tlnp | grep -E "(3000|4001|9088|9188|9189|9190)"` — what's bound?
4. **Check escrow balance**: `curl -s https://s1.ripple.com:51234 -X POST -d '{"method":"account_info","params":[{"account":"r4rwwSM9PUu7VcvPRWdu9pmZpmhCZS9mmc"}]}'` — verify funds safe
5. **NEVER kill PID 2044357 or PID 1601200 on Hetzner** without explicit approval
6. **NEVER restart enclaves on Azure** without checking if Hetzner mainnet uses their signing keys (it does — ports 9188-9190)
7. **Testnet orchestrators on Azure** (port 3000) are safe to restart — they don't affect mainnet signing

## Critical risk: Azure enclave restart

The Azure enclaves serve DUAL purpose:
- **Mainnet**: Hetzner orchestrator reaches them via SSH tunnels (9188→9088, etc.) for multisig signing
- **Testnet**: Local testnet orchestrators use them for testnet operations

**Restarting an Azure enclave will break mainnet multisig signing** until the tunnel is re-established and the enclave re-seals its keys. If all 3 go down simultaneously, the 108 XRP in escrow becomes inaccessible until at least 2 enclaves are restored.
