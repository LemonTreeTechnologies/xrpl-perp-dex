# Deployment Dilemma: Hetzner = Mainnet, Azure = Testnet

**Date:** 2026-04-17
**Status:** open, needs decision

## Problem

We moved the Hetzner server to mainnet early — before the code was
feature-complete. This creates a deployment bottleneck:

| Host | Role | Can deploy new code? | SGX? |
|------|------|---------------------|------|
| Hetzner EX44 (94.130.18.162) | **Mainnet** (~108 XRP in escrow) | No — risk to live funds | No (Xeon E3, no SGX2) |
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
| Cheapest (€39/mo vs €300/mo Azure) | No SGX — can't test DCAP attestation |
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

## What NOT to do

- **Never** deploy untested code directly to Hetzner mainnet
- **Never** deallocate all Azure VMs while PM still uses them
- **Never** run `cargo build` on Hetzner during peak hours (CPU spike
  affects running mainnet orchestrator)
- **Never** restart mainnet without the backup checklist above
