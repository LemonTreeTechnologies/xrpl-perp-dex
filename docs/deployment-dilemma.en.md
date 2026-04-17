# Deployment Dilemma: Hetzner = Mainnet, Azure = Testnet

**Date:** 2026-04-17
**Status:** open, needs decision

## Problem

We moved the Hetzner server to mainnet early — before the code was
feature-complete. This creates a deployment bottleneck:

| Host | Role | Can deploy new code? | SGX? |
|------|------|---------------------|------|
| Hetzner EX44 (94.130.18.162) | **Mainnet** (~108 XRP in escrow) | No — risk to live funds | Yes (SGX1 HW mode, no DCAP attestation) |
| Azure node-1 (20.71.184.176) | Testnet | Yes | Yes (DCsv3, SGX2+DCAP) |
| Azure node-2 (20.224.243.60) | Testnet | Yes | Yes (DCsv3, SGX2+DCAP) |
| Azure node-3 (52.236.130.102) | Testnet | Yes | Yes (DCsv3, SGX2+DCAP) |

**The trap:** if we deallocate Azure VMs to save money, we have
**nowhere** to deploy and test new orchestrator builds. If we keep
Azure, we pay ~€300/month for three DCsv3 nodes that serve only
testnet traffic.

Hetzner cannot be updated because:
1. Live escrow with real XRP — a bad deploy loses funds
2. No rollback path — no blue/green, no canary
3. No staging environment — testnet and mainnet are physically separated
4. Build on Hetzner → deploy on Hetzner is atomic — there's no
   "deploy to staging first" step

## Options

### Option A: Second Hetzner server as staging

Buy a second Hetzner dedicated server (EX44 = €39/month, same as
current). Use it as a full staging environment: build, deploy, test
against XRPL testnet, then manually promote to mainnet Hetzner.

| Pros | Cons |
|------|------|
| Cheapest (€39/mo vs €300/mo Azure) | Likely no SGX — can't test DCAP (depends on CPU model) |
| Same glibc/arch as mainnet | Manual promotion step |
| SSH from anywhere | Still no blue/green |

### Option B: Keep one Azure VM as staging, deallocate two

Keep one Azure DCsv3 (e.g. node-1) as the staging/test node. Deallocate
node-2 and node-3. Save ~€200/month.

| Pros | Cons |
|------|------|
| Real SGX for staging | Can't test 3-node multisig |
| €100/mo instead of €300/mo | Single point of failure for testing |
| Can test DCAP end-to-end | |

### Option C: Blue/green on Hetzner itself

Run two orchestrator instances on Hetzner: one on port 3000 (mainnet),
one on port 3001 (staging). Both share the same enclave. nginx routes
traffic. Deploy to 3001, smoke test, then swap.

| Pros | Cons |
|------|------|
| Zero extra cost | Same machine = shared failure domain |
| Instant swap via nginx | Enclave state is shared — test data leaks into mainnet |
| Can test full mainnet config | Risk: staging bug crashes enclave, takes down mainnet |

### Option D: Hetzner EX130 + proper separation (recommended for Phase 2)

When revenue justifies it, upgrade to Hetzner EX130 (€130/month, Xeon
with SGX support) or rent a second EX130. This gives:
- SGX on Hetzner (EX130 has Ice Lake Xeon with SGX2)
- Enough RAM/CPU for mainnet + staging side by side
- Or: one EX130 = mainnet, original EX44 = staging

This is the long-term answer but costs €130-170/month extra.

### Option E: Docker-based staging on Hetzner (pragmatic short-term)

Run a second orchestrator instance in a Docker container on the same
Hetzner EX44, connected to XRPL testnet. It shares the machine but
uses a separate enclave instance, separate database, separate ports.

| Pros | Cons |
|------|------|
| Zero extra cost | Resource contention with mainnet |
| Can test orchestrator code | No SGX in Docker (simulation mode only) |
| Separate DB = no data leak | If Hetzner OOMs, mainnet goes down |

## Recommendation

**Short-term (now):** Option B — keep one Azure VM (node-1), deallocate
node-2 and node-3. Cost: ~€100/mo. We keep SGX staging + DCAP testing
capability. When we need 3-node multisig testing, spin up node-2 and
node-3 temporarily (takes 5 minutes).

**Medium-term (when PM work on Azure finishes):** Option A — second
Hetzner EX44 (€39/mo) as pure staging. Deallocate all Azure VMs.
No SGX on staging, but SGX-specific code changes are rare at this point.

**Long-term (revenue):** Option D — Hetzner EX130 with real SGX for
both mainnet and staging.

## Migration plan: updating mainnet on Hetzner

Regardless of the staging solution, we need a safe mainnet update
procedure. Proposed checklist:

1. **Build** on Hetzner from git (staging branch tested first)
2. **Snapshot** current state:
   - `cp /tmp/perp-9088/ /tmp/perp-9088.backup-$(date +%s)/`
   - `pg_dump perp_dex > /tmp/perp_dex_backup.sql`
3. **Stop** mainnet orchestrator: `kill <PID>` (nginx returns 502)
4. **Swap** binary: `cp target/release/perp-dex-orchestrator /tmp/perp-9088/`
5. **Start** new binary with same arguments
6. **Health check**: `curl http://localhost:3000/v1/health`
7. **Verify** XRPL mainnet connectivity: check escrow balance unchanged
8. **Rollback** if health check fails:
   - `kill <PID>`
   - `cp /tmp/perp-9088.backup-*/perp-dex-orchestrator /tmp/perp-9088/`
   - Restart with old binary

**Downtime:** ~30 seconds (stop → swap → start → health check).
Acceptable for current traffic level (near zero).

---

## The Hard Problem: Enclave Upgrade

Everything above discusses **orchestrator** deployment (Rust binary).
The enclave (C++ `enclave.signed.so`) is a fundamentally harder problem
because of **MRENCLAVE binding**.

### Why enclave upgrades are different from orchestrator upgrades

| Property | Orchestrator (Rust) | Enclave (C++ SGX) |
|----------|--------------------|--------------------|
| State | PostgreSQL (external) | Sealed to CPU + MRENCLAVE |
| Binary swap | Stop → replace → start | New MRENCLAVE → **all sealed keys lost** |
| Rollback | Copy old binary back | Old MRENCLAVE sealed data still works |
| Key continuity | Keys live in enclave, orchestrator has none | Keys **are** the enclave state |
| Impact of bad deploy | 502 for 30 seconds | **Permanent key loss** if old binary not preserved |

### The MRENCLAVE problem in detail

SGX `sgx_seal_data()` binds sealed blobs to:
1. **MRENCLAVE** — hash of the enclave binary (changes on ANY code change)
2. **CPU identity** — per-platform provisioning key

This means:
- A new `enclave.signed.so` (even fixing a typo in a comment that
  affects compilation) produces a **different MRENCLAVE**
- The new enclave **cannot unseal** any data sealed by the old enclave
- Private keys generated inside the old enclave are **irrecoverable**
  by the new enclave
- This is by SGX design, not a bug — it prevents code substitution attacks

### What we lose if we naively rebuild the enclave

1. **All enclave-generated private keys** — the secp256k1 keys that
   derive to XRPL addresses used in SignerListSet
2. **XRPL multisig becomes unusable** — the escrow's SignerListSet
   points to XRPL addresses derived from now-inaccessible keys
3. **User funds locked** — the escrow can only be spent by 2-of-3
   signers whose keys are sealed inside old-MRENCLAVE enclaves
4. **FROST shares** (if used) — sealed shares cannot be transported
   cross-machine (see SHARED-ENCLAVE-BUGS.md Bug 1)

### When we WILL need to rebuild the enclave

Known triggers (not hypothetical):

1. **FROST cross-machine share transport** — currently broken
   (`sgx_seal_data` makes shares non-portable). Fix requires C++ change
   → new MRENCLAVE. See `SHARED-ENCLAVE-BUGS.md` Bug 1.
2. **BTC signing path** — if Phoenix PM needs it, several bugs need fixing
3. **`MAX_FROST_GROUPS = 4`** — too low for production, needs bump
4. **DKG "share already received" no reset** — blocks retry
5. **Security patches** — any CVE in enclave code forces a rebuild
6. **New features** — spending limits, perp margin engine inside enclave

### Enclave upgrade strategies

#### Strategy 1: Dual-Server Migration (documented in enclave_versioning.md)

Run old enclave (port 9088) and new enclave (port 9089) side by side.

```
Old enclave v1.0 (port 9088)     New enclave v1.1 (port 9089)
├── enclave.signed.so-OLD        ├── enclave.signed.so-NEW
├── accounts/ (sealed to OLD)    ├── accounts/ (empty)
└── MRENCLAVE: 0xabc...          └── MRENCLAVE: 0xdef...
```

**Migration procedure:**
1. Deploy new enclave on separate port, keep old running
2. Generate new keypair in new enclave → new XRPL address
3. Use old enclave to sign a SignerListSet transaction that
   **adds the new address** to the signer list (e.g. 2-of-4 temporarily)
4. Use old enclave to sign another SignerListSet that
   **removes the old address** (back to 2-of-3 with new key)
5. Old enclave can now be decommissioned

**Critical requirement:** the old enclave must remain running and
functional throughout the migration. If it crashes before step 4,
the key rotation is incomplete and manual intervention is needed.

**Orchestrator changes needed:**
- Support talking to two enclave URLs simultaneously
- Route signing requests to the correct enclave based on which
  key (old or new) is being used
- CLI command: `orchestrator enclave-migrate --old-url ... --new-url ...`

**Time window:** migration per node takes ~5 minutes (generate key +
2 XRPL transactions + verification). For 3-node cluster, sequential
migration takes ~15 minutes.

#### Strategy 2: MRSIGNER-based Sealing (weaker security, simpler migration)

Change `sgx_seal_data()` policy from MRENCLAVE to MRSIGNER:
```cpp
// Instead of: sgx_seal_data(0, NULL, ...) → MRENCLAVE-bound
// Use:        sgx_seal_data_ex(SGX_KEYPOLICY_MRSIGNER, ...) → MRSIGNER-bound
```

This means any enclave signed by the same vendor key can unseal the
data. The new enclave v1.1 can directly read v1.0's sealed keys.

| Pros | Cons |
|------|------|
| Zero-downtime upgrade | Weaker security: any enclave signed by same key can read secrets |
| No key migration needed | A compromised build pipeline → full key extraction |
| Simple: just replace binary and restart | Loses the code-binding guarantee of MRENCLAVE |
| Works for cross-machine FROST too | Security auditors will flag this |

**Verdict:** acceptable for testnet/dev, NOT for mainnet with real funds.
Could be used as a **transition step** — switch to MRSIGNER temporarily
during migration, then switch back to MRENCLAVE after new keys are
generated.

#### Strategy 3: Key Export via Recovery Mechanism

The enclave already has `account.recovery` files (encrypted private key
backups). Use them:

1. Before upgrade: export recovery for all active keys
2. Deploy new enclave
3. Import keys via recovery mechanism into new enclave
4. Keys now sealed under new MRENCLAVE

**Problem:** recovery files are encrypted with a user-held key. For the
perp-DEX escrow, the "user" is the operator. This means the operator
momentarily has the raw private key outside the enclave — violating the
core trust model.

**Verdict:** only acceptable if the recovery key is itself held inside
another enclave or HSM. Otherwise defeats the purpose of SGX.

#### Strategy 4: Pre-planned Key Rotation (recommended)

Design the system so key rotation is a **normal operation**, not an
emergency procedure:

1. **Each enclave version gets its own keypair.** On deployment,
   the new enclave generates a fresh key, and the orchestrator
   initiates a SignerListSet rotation automatically.
2. **SignerListSet supports 3+ signers temporarily.** During rotation,
   the list has 4 signers (3 old + 1 new). Quorum stays at 2. The
   old signer's key is removed after the new one is confirmed.
3. **Rolling upgrade:** rotate one node at a time. At no point are
   more than 1 out of 3 signers in migration state.
4. **Automated by orchestrator CLI:**
   ```
   orchestrator enclave-upgrade \
     --node node-1 \
     --new-enclave /path/to/enclave.signed.so \
     --escrow-address rLTFG...
   ```
   This command:
   a. Starts new enclave on a staging port
   b. Generates new keypair → new XRPL address
   c. Submits SignerListSet (add new signer, quorum unchanged)
   d. Waits for XRPL validation
   e. Submits SignerListSet (remove old signer)
   f. Stops old enclave
   g. Promotes new enclave to production port

**This is the only strategy that preserves the security model
(MRENCLAVE binding), is automated, and does not require operator
access to private keys.**

### Recommendation

| Phase | Strategy | Why |
|-------|----------|-----|
| **Now** (testnet) | Strategy 2 (MRSIGNER) | Fast iteration, no real funds at risk |
| **Pre-mainnet** | Build Strategy 4 tooling | Must be ready before mainnet goes live with real funds |
| **Mainnet** | Strategy 4 (rolling key rotation) | Only option that preserves SGX trust model |
| **Emergency** | Strategy 1 (dual-server) as fallback | Manual but safe, for when automated rotation fails |

### What this means for the deployment dilemma

The enclave upgrade problem **compounds** the Hetzner-vs-Azure problem:

1. **Hetzner runs SGX in HW mode** — MRENCLAVE binding is real, sealing
   works, keys are genuinely hardware-bound. An enclave upgrade on
   Hetzner will lose access to sealed keys just like on Azure.
2. **Hetzner does NOT support DCAP attestation** — no PCK provisioning,
   no PCCS. So while enclave upgrade (key rotation) can be tested on
   Hetzner, attestation-dependent flows (quote generation, remote
   verification) can only be tested on Azure DCsv3.
3. **Enclave upgrade CAN be tested on Hetzner** — the key rotation
   procedure (dual-server, SignerListSet swap) works identically
   because SGX HW mode is real. However, testing on mainnet with live
   funds is risky — a staging Hetzner (Option A) or one Azure VM
   (Option B) is still needed for safe rehearsal.

The argument for **keeping at least one Azure VM** is specifically
about DCAP attestation testing — not about enclave upgrades, which
can be rehearsed on any SGX-capable hardware including Hetzner.

---

## What NOT to do

- **Never** deploy untested code directly to Hetzner mainnet
- **Never** deallocate all Azure VMs while PM still uses them
- **Never** run `cargo build` on Hetzner during peak hours (CPU spike
  affects running mainnet orchestrator)
- **Never** restart mainnet without the backup checklist above
