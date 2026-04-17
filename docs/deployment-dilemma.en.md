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

#### ~~Strategy 2: MRSIGNER-based Sealing~~ — REJECTED

**Rejected.** Switching `sgx_seal_data()` from MRENCLAVE to MRSIGNER
means any enclave signed by the same vendor key can unseal all secrets.
This:
- Fails security audit — attestation verifiers check MRENCLAVE, not MRSIGNER
- Creates a one-way door: once sealed data is MRSIGNER-bound, migrating
  back to MRENCLAVE requires the same key-rotation procedure as Strategy 4,
  so there is no shortcut
- A compromised build pipeline → full key extraction

**Not even for testnet.** Building habits around MRSIGNER sealing means
the testnet code diverges from the mainnet security model, and the
divergence will bite when it matters most.

#### ~~Strategy 3: Key Export via Recovery Mechanism~~ — REJECTED

The enclave already has `account.recovery` files (encrypted private key
backups). Use them:

1. Before upgrade: export recovery for all active keys
2. Deploy new enclave
3. Import keys via recovery mechanism into new enclave
4. Keys now sealed under new MRENCLAVE

**Problem:** the recovery mechanism allows the operator to extract private
keys from the enclave at any time — this is a fundamental violation of
the trust model, not a temporary compromise. If the key can be extracted,
the SGX enclave ceases to be a trust boundary.

**Verdict:** rejected. Violates the trust model by definition.

#### Strategy 4: Pre-planned Key Rotation (recommended)

Design the system so key rotation is a **normal operation**, not an
emergency procedure:

1. **Each enclave version gets its own keypair.** On deployment,
   the new enclave generates a fresh key, and the orchestrator
   initiates a SignerListSet rotation automatically.
2. **SignerListSet has 4 signers temporarily, quorum raised to 3.**
   During rotation the list has 4 signers (3 old + 1 new). Quorum
   is raised from 2 to **3** for the duration of the rotation window.
   This prevents a single compromised old key + the new key from
   reaching quorum. The old signer is removed and quorum lowered
   back to 2 after the new one is confirmed.
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
   c. New enclave produces DCAP attestation quote binding its MRENCLAVE
      to the new public key
   d. Each existing enclave independently verifies the DCAP quote before
      agreeing to co-sign the SignerListSet change
   e. Submits SignerListSet (add new signer, **raise quorum to 3**)
   f. Waits for XRPL validation (bounded by `LastLedgerSequence`)
   g. Submits SignerListSet (remove old signer, **lower quorum to 2**)
   h. Stops old enclave, securely deletes old sealed data
   i. Promotes new enclave to production port

**This is the only strategy that preserves the security model
(MRENCLAVE binding), is automated, and does not require operator
access to private keys.**

#### Attack surface analysis

Key rotation via SignerListSet is functionally an ownership transfer —
historically the #1 target in multisig attacks. Each vector below has
a concrete defense; a vector without a defense is not acceptable.

**Vector 1: Quorum theft during 4-signer window**

*Attack:* during rotation the signer list has 4 entries. If quorum stays
at 2, an attacker controlling 1 compromised old key + 1 new key has
quorum and can drain the escrow.

*Defense:* quorum is **raised to 3** for the duration of the rotation
window (step e above). With 4 signers and quorum=3, an attacker needs
3 keys — strictly harder than the normal 2-of-3. Quorum returns to 2
only after the old signer is removed and the list is back to 3 entries.

**Vector 2: New key substitution**

*Attack:* the orchestrator (or a MITM) replaces the new enclave's real
public key with an attacker-controlled address in the SignerListSet
transaction.

*Defense:* the new enclave generates its keypair internally and produces
a **DCAP attestation quote** that binds the MRENCLAVE hash to the new
public key. Each of the 3 existing enclaves independently verifies this
quote before co-signing the SignerListSet transaction. The orchestrator
proposes the transaction but cannot forge enclave signatures — it needs
2-of-3 existing enclaves to agree, and each one checks the DCAP proof.
A substituted key would fail quote verification on all honest enclaves.

**Vector 3: Old signer removal failure (stale key)**

*Attack:* step g (remove old signer) fails — network timeout, fee spike,
orchestrator crash. The old key remains in the signer list indefinitely,
expanding the attack surface to 4 keys instead of 3.

*Defense:*
1. `LastLedgerSequence` on the add-signer tx: if it doesn't confirm
   within N ledgers (~20 seconds), the entire migration aborts and no
   key was added.
2. After successful add: the remove-signer tx is retried with
   exponential backoff until confirmed. The orchestrator refuses to
   report migration as "complete" until the on-chain signer list is
   back to exactly 3 entries.
3. **Hard timeout:** if remove-signer hasn't confirmed within 10 minutes,
   the system rolls back — submits a SignerListSet that removes the
   NEW key instead, returning to the original 3-of-3 state. Better
   to abort the upgrade than leave a 4-key window open.
4. Health check monitors signer count: any count ≠ 3 triggers an alert.

**Vector 4: SignerListSet transaction reordering**

*Attack:* the two SignerListSet transactions (add new, remove old) arrive
out of order or the second validates while the first is still pending.

*Defense:* XRPL enforces strict `Sequence` ordering per account. The
remove-signer tx has Sequence = add-signer Sequence + 1, so it
**cannot** validate before the add-signer tx. If the first tx is dropped,
the second is automatically invalid. This is an XRPL protocol guarantee,
not application logic.

**Vector 5: Orchestrator compromise (single point of trust)**

*Attack:* the orchestrator is the component that constructs SignerListSet
transactions and talks to all enclaves. A compromised orchestrator could
propose adding an attacker's key.

*Defense:* the orchestrator constructs but does not sign — only enclaves
sign. Adding a new signer requires 2-of-3 existing enclaves to co-sign
the transaction. Each enclave verifies the DCAP quote of the new key
before signing (Vector 2 defense). A compromised orchestrator can
propose a malicious key, but the enclaves will reject it because the
DCAP quote won't verify. The orchestrator is a coordinator, not a
trust root.

**Vector 6: Rollback to old enclave after rotation**

*Attack:* after successful rotation, the attacker forces the system to
revert to the old enclave binary (which still has the old sealed keys).
If the old key wasn't properly removed from SignerListSet, the attacker
now controls a valid signer.

*Defense:*
1. Step h explicitly deletes old sealed data (`shred` + `rm` of the old
   enclave's account directory and sealed blobs).
2. The old `enclave.signed.so` binary is removed from disk.
3. Even if the old binary is restored, the sealed data is gone — the
   old MRENCLAVE has no keys to unseal.
4. On-chain: the old address was removed from SignerListSet, so even
   if the old key somehow exists, it cannot participate in signing.

**Vector 7: Parallel rotation on multiple nodes**

*Attack:* two nodes rotate simultaneously, producing conflicting
SignerListSet transactions (e.g. both try to expand from 3→4 signers,
resulting in 5 signers or inconsistent state).

*Defense:* strictly sequential rotation enforced by the orchestrator CLI.
The tool takes a `--node` parameter and rotates exactly one node per
invocation. The second invocation checks on-chain signer count — if it's
not exactly 3 (i.e. a previous rotation is in progress or incomplete),
it refuses to start.

**Vector 8: Denial of service during rotation window**

*Attack:* attacker prevents the rotation from completing (network
partition, XRPL congestion, enclave crash), keeping the system in the
elevated-quorum 4-signer state indefinitely. While not a direct theft,
it degrades the system — quorum=3 means all 3 old signers + the new
one must cooperate, making normal operations harder.

*Defense:* the hard timeout from Vector 3 applies: if the full rotation
(add + remove) doesn't complete within 10 minutes, the system
automatically rolls back by removing the new key and restoring
quorum=2. The rotation can be retried later when conditions are stable.
During the elevated-quorum window, normal operations (withdrawals)
still work — they just need 3 signatures instead of 2, which is
stricter but functional.

### Strategy 1 vs Strategy 4: detailed comparison

Strategies 1 and 4 share the same core mechanism — generate new key in
new enclave, add it to SignerListSet, remove old key. The difference is
not *what* they do but *how many safeguards surround the operation*.

| Property | Strategy 1 (dual-server) | Strategy 4 (pre-planned rotation) |
|----------|--------------------------|-----------------------------------|
| **Execution** | Manual — operator runs commands step by step | Automated — single CLI command |
| **Quorum during rotation** | Unchanged (stays at 2-of-4) | Raised to 3-of-4 during window |
| **New key verification** | None — operator visually confirms the address | DCAP quote: each existing enclave cryptographically verifies the new key belongs to a real enclave with known MRENCLAVE |
| **Timeout / rollback** | None — if step 4 fails, operator must notice and intervene | Hard 10-minute timeout; auto-rollback removes new key, restores quorum=2 |
| **Old key cleanup** | Operator responsibility — may forget | Automated shred+rm of sealed data and old binary |
| **Parallel rotation guard** | None — operator must remember "one at a time" | CLI checks on-chain signer count, refuses to start if ≠ 3 |
| **Transaction ordering** | Operator submits manually — could make mistakes | Automated Sequence numbering + LastLedgerSequence bounds |
| **Orchestrator trust** | Operator IS the trust root — they pick the new key and submit txs | Orchestrator is coordinator only; enclaves verify DCAP before signing |
| **Signer count monitoring** | Manual — operator checks XRPL explorer | Health check alert on signer count ≠ 3 |
| **Time to complete (3 nodes)** | ~15 min (operator-dependent) | ~15 min (automated, same XRPL latency) |
| **Implementation cost** | Low — just a runbook | High — DCAP verification in enclave, CLI tooling, rollback logic |
| **Can operate without DCAP** | Yes — no attestation step | Degraded — without DCAP, Vector 2 (key substitution) defense is weakened |

#### When Strategy 1 is the right choice

Strategy 1 is not a "worse version" of Strategy 4. It is the correct
choice when:

1. **DCAP is unavailable.** On Hetzner (SGX1, no DCAP), Vector 2
   defense (DCAP-based new key verification) cannot work. Strategy 1
   with a trusted operator who manually verifies the new enclave's
   key is the only option. The operator compensates for the lack of
   cryptographic verification with physical access + visual confirmation.

2. **Strategy 4 automation has failed.** If the CLI tool crashes, the
   rollback logic doesn't trigger, or the system is in an unexpected
   state, Strategy 1 is the manual recovery procedure. Every automated
   system needs a manual fallback.

3. **First-time rotation on testnet.** Before trusting the automation,
   run the procedure manually to build operational understanding. You
   cannot debug an automated rotation if you have never done it by hand.

#### When Strategy 4 is required

Strategy 4 is required when:

1. **Real funds are at risk.** Manual procedures have human error rates.
   The quorum elevation (2→3 during window), hard timeout, and automated
   rollback exist specifically because a human operator can forget a step,
   get distracted, or be socially engineered.

2. **Multiple operators exist.** With 3 independent operators, Strategy 1
   requires coordination ("I'm rotating my node, don't rotate yours").
   Strategy 4 enforces this at the protocol level — the CLI checks
   on-chain state.

3. **Audit trail matters.** Strategy 4 produces a machine-verifiable
   audit log: DCAP quote, SignerListSet transactions with Sequence
   numbers, timestamps, rollback events. Strategy 1 produces "the
   operator says they did it correctly."

#### The relationship

Strategy 4 = Strategy 1 + quorum elevation + DCAP verification +
automated timeout/rollback + signer count guards + sealed data cleanup.

Strategy 1 is the *degenerate case* of Strategy 4 where every safeguard
is replaced by "the operator does it right." This is acceptable for
testnet and emergencies. It is not acceptable for mainnet with real funds
and independent operators who do not fully trust each other — which is
the entire point of multisig.

### Recommendation

| Phase | Strategy | Why |
|-------|----------|-----|
| **Now** (testnet) | Strategy 1 (dual-server) | Manual but safe; practice on testnet before mainnet. Build operational muscle memory. |
| **Pre-mainnet** | Build Strategy 4 tooling | Must be ready before mainnet goes live with real funds |
| **Mainnet** | Strategy 4 (rolling key rotation) | Only option that preserves SGX trust model under adversarial conditions |
| **Emergency** | Strategy 1 (dual-server) as fallback | Manual recovery when Strategy 4 automation fails |
| **Hetzner (no DCAP)** | Strategy 1 with trusted operator | DCAP unavailable — operator compensates with physical verification |

### MVP phase: practical setup (current stage)

**Important for collaborators:** Strategy 4 is the long-term target, not
something to implement right now. At the MVP stage — with ~108 XRP in
escrow, near-zero traffic, a single trusted operator, and an LLM
assistant executing commands under operator supervision — Strategy 1 is
the correct and sufficient approach.

#### Dual-instance layout on Hetzner EX44

The deployment dilemma (Options A–E above) is resolved for the MVP phase
by running two fully isolated instances on the existing Hetzner server:

```
Hetzner EX44 (94.130.18.162)
├── MAINNET (production)
│   ├── Enclave:      port 9088, SGX HW mode
│   ├── Orchestrator: port 3000, connected to XRPL mainnet
│   ├── Sealed data:  /tmp/perp-9088/
│   └── Database:     perp_dex (PostgreSQL)
│
└── DEV/STAGING (development)
    ├── Enclave:      port 9089, SGX HW mode
    ├── Orchestrator: port 3001, connected to XRPL testnet
    ├── Sealed data:  /tmp/perp-9089/
    └── Database:     perp_dex_dev (PostgreSQL)
```

This is **not** Option C from the deployment dilemma (which shares one
enclave between mainnet and staging). These are two completely separate
enclave processes with separate sealed data, separate databases, and
separate XRPL networks. The only shared resource is the physical machine.

**Risk:** an OOM or CPU spike on the dev instance could crash mainnet.
Acceptable at current traffic levels (~0 TPS, ~108 XRP). Unacceptable
when funds grow — at that point, move to a second Hetzner (Option A)
or EX130 (Option D).

#### Enclave upgrade workflow at MVP stage

This is Strategy 1 executed with the LLM assistant under operator
control:

1. **Build** new `enclave.signed.so` on Hetzner from git
2. **Deploy to dev** (port 9089) — new enclave gets new MRENCLAVE
3. **Generate new key** in dev enclave → new XRPL testnet address
4. **Test rotation on testnet** — SignerListSet add new signer, then
   remove old signer. Verify on XRPL testnet explorer.
5. **Repeat on mainnet** (port 9088) only after testnet success:
   - Deploy new enclave on a temporary staging port (9090)
   - Generate new key → new mainnet address
   - SignerListSet: add new signer to mainnet escrow
   - Verify on-chain
   - SignerListSet: remove old signer
   - Stop old enclave (9088), promote new enclave to port 9088
   - Delete old sealed data

**The operator sees every command before execution and can abort at any
step.** This is the Strategy 1 trust model: the operator is the root of
trust, not cryptographic verification.

#### When to move beyond this setup

| Signal | Action |
|--------|--------|
| Escrow balance exceeds 10,000 XRP | Move dev to separate server (Option A or D) |
| Second independent operator joins | Begin Strategy 4 tooling |
| Grant audit requires audit trail | Begin Strategy 4 tooling |
| Azure VMs no longer needed by PM | Deallocate Azure, save €300/month |

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
