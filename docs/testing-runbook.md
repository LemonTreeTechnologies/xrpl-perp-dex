# Testing Runbook — Failure Mode & Operator-Level Tests

**Status:** Manual procedures documented after ad-hoc testing on 2026-04-17.
These procedures were used to produce the results in `failure-test-results-2026-04-17.md` but were NOT part of a formalized, auditable test pipeline.

**TODO:** Convert to reproducible scripts under `tests/` with proper CI integration.

---

## Prerequisites

### Infrastructure
- 3 Azure DCsv3 VMs with SGX enclaves running (`perp-dex-server` on localhost:9088)
- 3 Orchestrators with P2P mesh (ports 4001 open between VMs)
- Hetzner bastion (94.130.18.162) as jump host and test runner
- PostgreSQL running on each Azure VM

### Build & Deploy (currently manual)

```bash
# On Hetzner — build enclave for Azure
cd ~/xrpl-perp-dex-enclave && docker build -f Dockerfile.azure -t perp-enclave-azure .
docker cp $(docker create perp-enclave-azure):/app/perp-dex-server /tmp/perp-dex-server

# On Hetzner — build orchestrator (must use Hetzner's glibc 2.35, not local 2.38)
cd ~/llm-perp-xrpl/orchestrator && cargo build --release

# Deploy to Azure VMs (via Hetzner)
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    scp target/release/perp-dex-orchestrator azureuser@$ip:~/perp/
    scp /tmp/perp-dex-server azureuser@$ip:~/perp/
done
```

**Gap:** No deployment script, no version tracking, no rollback procedure.

### XRPL Testnet Escrow Setup (currently manual Python)

```bash
pip install xrpl-py

python3 << 'EOF'
from xrpl.clients import JsonRpcClient
from xrpl.wallet import Wallet
from xrpl.models.transactions import SignerListSet, SignerEntry, AccountSet, AccountSetAsfFlag
from xrpl.transaction import submit_and_wait

client = JsonRpcClient("https://s.altnet.rippletest.net:51234")

# 1. Fund escrow via faucet (external step)
escrow = Wallet.from_seed("ESCROW_SEED_HERE")

# 2. Set 2-of-3 SignerList
signers = [
    SignerEntry(account="XRPL_ADDR_NODE1", signer_weight=1),
    SignerEntry(account="XRPL_ADDR_NODE2", signer_weight=1),
    SignerEntry(account="XRPL_ADDR_NODE3", signer_weight=1),
]
tx = SignerListSet(account=escrow.address, signer_quorum=2, signer_entries=signers)
submit_and_wait(tx, client, escrow)

# 3. Disable master key
tx = AccountSet(account=escrow.address, set_flag=AccountSetAsfFlag.ASF_DISABLE_MASTER)
submit_and_wait(tx, client, escrow)
EOF
```

**Gap:** XRPL addresses are derived from enclave pubkeys via ad-hoc Python (base58 encode + SHA-256 + RIPEMD-160). This derivation is not in any production code — the orchestrator's `xrpl_signer.rs` handles it, but the test used a separate Python implementation.

---

## Component-Level Tests (Section 2)

### 2.2 — Sequencer Crash & Failover

```bash
# From Hetzner:

# Kill sequencer (node-1, priority 0)
ssh azureuser@20.71.184.176 "pkill -9 -f perp-dex-orchestrator"

# Wait 20s, then check node-2 logs for promotion
ssh azureuser@20.224.243.60 "grep 'promoting self to sequencer' ~/perp/orchestrator.log"

# Restart node-1, verify it reclaims sequencer
ssh azureuser@20.71.184.176 "cd ~/perp && ./perp-dex-orchestrator [args] &"
# After ~5s, check node-2 demoted back to validator
ssh azureuser@20.224.243.60 "grep 'role change.*Validator' ~/perp/orchestrator.log | tail -1"
```

**Verification:** `grep "election"` on all nodes shows correct role transitions.
**Gap:** No automated assertions. Human reads logs and judges pass/fail.

### 2.3 — Validator Crash

```bash
# Kill validator (node-3, priority 2)
ssh azureuser@52.236.130.102 "pkill -9 -f perp-dex-orchestrator"

# Verify sequencer (node-1) unaffected
ssh azureuser@20.71.184.176 "grep -c 'role change' ~/perp/orchestrator.log"
# Should show 0 new role changes after kill

# Restart, verify reconnection
ssh azureuser@52.236.130.102 "cd ~/perp && ./perp-dex-orchestrator [args] &"
ssh azureuser@52.236.130.102 "grep 'connected' ~/perp/orchestrator.log | tail -2"
```

### 2.4 — Enclave Crash (Orchestrator Alive)

```bash
# Kill enclave server (NOT the orchestrator)
ssh azureuser@20.71.184.176 "sudo pkill -9 -f perp-dex-server"

# Wait 10s, check orchestrator logs for error detection
ssh azureuser@20.71.184.176 "grep 'price update failed\|liquidation scan failed' ~/perp/orchestrator.log | tail -3"

# Restart enclave
ssh azureuser@20.71.184.176 "cd ~/perp && ./perp-dex-server &"
```

**Gap:** Binary name is `perp-dex-server`, not `eth_signer` — the kill command must match the actual binary name.

### 2.7 — Binance API Unavailable

```bash
# Block Binance outbound on node-1
ssh azureuser@20.71.184.176 "sudo iptables -A OUTPUT -d api.binance.com -j DROP"

# Wait 15s, check for price feed warnings
ssh azureuser@20.71.184.176 "grep 'price fetch failed' ~/perp/orchestrator.log | tail -3"

# Restore
ssh azureuser@20.71.184.176 "sudo iptables -F OUTPUT"
```

**Gap:** `iptables -d api.binance.com` relies on DNS resolution at rule creation time. If Binance uses multiple IPs, some may not be blocked. A more robust approach: block all outbound HTTPS except Azure vnet.

### 2.8 — XRPL Node Unavailable

```bash
# Block XRPL testnet
ssh azureuser@20.71.184.176 "sudo iptables -A OUTPUT -p tcp --dport 51234 -j DROP"

# Check for deposit scan failures
ssh azureuser@20.71.184.176 "grep 'deposit scan failed' ~/perp/orchestrator.log | tail -3"

# Restore
ssh azureuser@20.71.184.176 "sudo iptables -F OUTPUT"
```

**Gap:** `iptables -F OUTPUT` flushes ALL output rules, not just the one we added. Should use `-D` to delete the specific rule.

---

## Operator-Level Tests (Multisig)

### SSH Tunnel Setup (required because port 9088 is localhost-only)

```bash
# On Hetzner — create tunnels to all 3 enclaves
ssh -f -N -L 9091:localhost:9088 azureuser@20.71.184.176
ssh -f -N -L 9092:localhost:9088 azureuser@20.224.243.60
ssh -f -N -L 9093:localhost:9088 azureuser@52.236.130.102
```

**Gap:** These tunnels are ephemeral. If the SSH connection drops, the tunnel dies. No reconnection logic. For production, enclaves should listen on the Azure vnet interface (10.0.0.x) with firewall rules, not rely on SSH tunnels.

### Multisig Signing Flow (currently: ad-hoc Python script)

The test script (`/tmp/operator_tests.py`, NOT checked into the repo) performs:

1. **Serialize XRPL Payment tx** using `xrpl-py` binary codec
2. **For each signer:**
   - Derive XRPL AccountID from r-address (base58 decode)
   - Compute `multi_signing_hash` = SHA-512Half(0x534D5400 + tx_blob + account_id)
   - Call `POST /v1/pool/sign` on the signer's enclave with the hash
   - Receive (r, s) components, DER-encode them
3. **Sort Signers by AccountID** (XRPL canonical order)
4. **Submit via `submit_multisigned` RPC** to XRPL

**Gap:** This Python implementation duplicates logic already in the orchestrator's `withdrawal.rs`. The proper test should exercise the orchestrator's actual withdrawal endpoint, but that requires:
- Enclave port 9088 open on Azure vnet (currently localhost-only)
- `--signers-config` passed to orchestrator at startup
- A deposit + balance in the enclave to pass the margin check

### Test Scenarios

| # | Scenario | Available Signers | Expected Result |
|---|----------|-------------------|-----------------|
| 1 | Normal withdrawal | node-1, node-2, node-3 | tesSUCCESS |
| 2 | One operator offline | node-1, node-2 | tesSUCCESS |
| 3 | Two operators offline | node-1 only | Blocked (1/2 quorum) |
| 4 | Malicious signer | node-1 (good) + node-2 (corrupted DER) | XRPL rejects |
| 5 | Alt pair: 1+3 | node-1, node-3 | tesSUCCESS |
| 6 | Alt pair: 2+3 | node-2, node-3 | tesSUCCESS |

### Cleanup

```bash
# Kill SSH tunnels
pkill -f "ssh.*-L.*909"

# Optionally stop orchestrators on Azure VMs to save costs
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    ssh azureuser@$ip "pkill -f perp-dex-orchestrator; pkill -f perp-dex-server"
done
```

---

## What's NOT Tested Yet

| Scenario | Why not tested | What's needed |
|----------|---------------|---------------|
| Key rotation (new signer replaces old) | No `SignerListSet` update flow in orchestrator | Automation: generate new key → update SignerList → verify withdrawal with new set |
| Catastrophic recovery (all keys lost) | Destructive test, skipped | Separate recovery procedure: fund new escrow, re-derive from XRPL deposit history |
| End-to-end withdrawal via orchestrator API | Port 9088 localhost-only | Open 9088 on vnet or add signing proxy to orchestrator |
| Automated CI | No test harness | Move Python scripts to `tests/`, add assertions, run from GitHub Actions with Azure credentials |

---

## Action Items

1. **Move test scripts into `tests/operator_tests.py`** — checked into repo, not ad-hoc `/tmp/` files
2. **Add `tests/component_tests.sh`** — shell script for component-level tests with automated log assertions
3. **Open port 9088 on Azure vnet** — so orchestrators can cross-sign without SSH tunnels
4. **Add `--signers-config` to orchestrator startup** — include in deployment procedure
5. **Create `scripts/deploy.sh`** — build + deploy + cleanup stale processes
6. **Add pass/fail assertions** — currently tests are "human reads the log". Need `grep + exit code` checks.
7. **Document the XRPL address derivation** — the pubkey → compressed → ripemd160 → base58 chain is only in ad-hoc Python. Should reference `xrpl_signer::decode_xrpl_address()` in the Rust orchestrator.
