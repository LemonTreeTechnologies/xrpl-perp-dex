# Environment matrix — five named environments, four orthogonal axes

**Status:** Accepted (2026-05-04).
**Audience:** the internal team and dev-perp; Tom (for environment-pairing references); the AI-Auditor; future external auditors; future operators.
**Companions:** [`development-operating-model.{en,ru}.md`](development-operating-model.en.md) (operating modes), [`build-requirements.{en,ru}.md`](build-requirements.en.md) (Build / Sign / Run policy), [`sdk-version-matrix.{en,ru}.md`](sdk-version-matrix.en.md) (SDK 2.25 vs 2.28 ladder), [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.en.md) (foundation invariants), [`api-environment-policy.{en,ru}.md`](api-environment-policy.en.md) (Tom integration pairing).

---

## 0. Why this document exists

Until 2026-05-04 the project conflated four orthogonal axes into one informal label "mainnet vs testnet":

1. **XRPL network** — testnet (faucet-funded XRP) vs mainnet (real XRP).
2. **Operating mode** (per `development-operating-model.md` §1) — `product-sandbox-single-operator` / `product-sandbox-multi-operator` / `production`.
3. **Host platform** — Hetzner SGX1 (single bare-metal box, OOT `isgx` driver, libsgx-* mostly 2.25) vs Azure DCsv3 (cloud SGX2, in-kernel `/dev/sgx_enclave` driver, libsgx-* clean 2.28).
4. **SGX driver / SDK era** — OOT + SDK 2.25 vs in-kernel + SDK 2.28 (per `sdk-version-matrix.md`).

The conflation produced multiple documented errors during the 2026-05 build-gate work: claims about "currently-deployed mainnet MRENCLAVE" turned out to point at a different environment than actually running mainnet, and confusion about whether Dockerfile.azure-built binaries can run on Hetzner (they cannot — SDK 2.28 dropped OOT driver support, verified empirically with `sgx_create_enclave returned: 0x000a` on 2026-05-03).

This document fixes a four-environment classification with explicit read-out rules so future statements about "where" cannot collapse the axes into ambiguity.

---

## 1. The five named environments

Each row is the unique tuple of all four axes, given a stable name. The combinatorial space is `XRPL × operating-mode = 2 × 3 = 6` cells; we use 5 of them. The unused cell is `testnet × production`, which has no semantic meaning (production is by definition mainnet).

| Environment | XRPL network | Operating mode | Host | SGX driver / SDK | Purpose | MRENCLAVE policy |
|---|---|---|---|---|---|---|
| **dev-hetzner** | testnet | sandbox-single | Hetzner SGX1 (bare metal `94.130.18.162`) | OOT `isgx` + SDK 2.25 | Personal dev playground for Andrey: fast iteration on testnet code without spinning up an Azure VM; experimentation; offline-from-cluster work; smoke-testing changes that do NOT require multi-operator FROST or DCAP | **Local-only.** The MRENCLAVE produced here never enters any cluster's on-chain MRENCLAVE allowlist. Ephemeral by design. |
| **testnet-cluster** | testnet | sandbox-multi | Azure DCsv3 ×3 (`sgx-node-1` `20.71.184.176`, `sgx-node-3` `52.236.130.102`, plus 1 more) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Multi-operator FROST validation on faucet XRP; first place we test cross-machine signing, DKG, peer attestation; Path A migration ceremony testing | **Canonical** (built via committed `Dockerfile.azure`). Reproducible. Auditable. Currently MRENCLAVE `4dfe899771bdb3f3097714013d054c08c7dd6e28f2acd17948f8a08f328c011b` for commit `2c3d31f`. |
| **mainnet-sandbox** | XRPL mainnet | sandbox-single | Azure DCsv3 (new VM, post-migration) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Real XRP small amounts, operator-of-record only; pre-production proof of system on real funds; transient state — bridges to mainnet-sandbox-cluster when a second operator joins | **Canonical** (`Dockerfile.azure`), pinned via on-chain MRENCLAVE allowlist (per REQ-7 §3.4). |
| **mainnet-sandbox-cluster** | XRPL mainnet | sandbox-multi | Azure DCsv3 ×N (≥2 operator topology, but still no real customer state) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | The bridge state between single-operator sandbox and production. Real XRP small amounts, multi-operator architecture activated (foundation invariants 1–4 enforced, 5 + 7 enforced), but no real customer funds. Validation that the multi-operator topology actually works on mainnet before promoting to `production`. | **Canonical** (`Dockerfile.azure`), pinned on-chain. Multi-operator reproducibility cross-check now meaningful (≥2 operators independently rebuild and verify MRENCLAVE per `feedback_reproducible_build_foundation.md`). |
| **production** | XRPL mainnet | production (future, gated on Path A REQ-8 PASS + reproducibility cross-check by ≥N operators) | Azure DCsv3 ×N (multi-operator topology) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Multi-operator real customer funds. Full audit cycle each upgrade. Third-party human audit replaces AI-Auditor as primary gate. | **Canonical**, on-chain allowlist enforced, full audit cycle each upgrade. |

Anything that doesn't fit the table is not a defined environment. Examples of things that are **not** environments in this scheme:
- "Tom's local laptop" — see `api-environment-policy.md`; Tom connects to one of the four environments above by URL, he doesn't operate his own environment.
- "AI-Auditor's machine" — the auditor reads artefacts; not an enclave-runtime environment.
- "GHA build runner" — a build environment, not a runtime environment. Output goes to one of the four.

---

## 2. Read-out rules per environment

### 2.1 dev-hetzner

- **What it IS:** Andrey's personal SGX1 playground on Hetzner bare metal. He develops, builds (locally with SDK 2.25), runs an enclave, exercises testnet faucet XRP, breaks things, restarts. No expectation that what he runs here matches anybody else's MRENCLAVE.
- **What it IS NOT:** part of any cluster. Not a multi-operator member. Not bound by foundation invariants 1–4 (multi-operator zero trust) or 7 (reproducibility-N-of-M). Builds here are explicitly NOT canonical.
- **What's at risk if it breaks:** nothing in production sense; only Andrey's iteration speed. Easy to restart (`node-bootstrap` + faucet refill).
- **Build path:** Hetzner local `make` against `/opt/intel/sgxsdk` (SDK 2.25-era). Result links libsgx-urts.so.2 (host runtime SDK 2.25), uses `/dev/isgx` OOT driver. Will not load anywhere else.
- **Why this exists despite being "non-canonical":** because Hetzner SGX1 is fundamentally unsuitable for production multi-operator (no DCAP, no in-kernel driver going forward) but it is excellent for cheap fast iteration. Suppressing dev-hetzner would force every iteration through Azure (cost + latency). dev-hetzner is the "good fit for what Hetzner does well" choice.

### 2.2 testnet-cluster

- **What it IS:** the validation environment for cluster-level changes. 3 Azure DCsv3 VMs running cross-machine FROST 2-of-3, DCAP peer attestation, libp2p mesh. Faucet-funded XRP.
- **What it IS NOT:** dev-hetzner. Anyone confusing the two is one mistake away from accidentally pushing a non-canonical MRENCLAVE into the cluster.
- **What's at risk if it breaks:** validation gate for promoting code to mainnet-sandbox. Faucet-XRP loss is acceptable.
- **Build path:** committed `Dockerfile.azure` → SDK 2.28 → MRENCLAVE matches on all 3 peers. Reproducible-build-N-of-M required for production-mode unlock; for testnet-cluster ≥1 reproducer (GHA) is sufficient today.

### 2.3 mainnet-sandbox

- **What it IS (post-migration target):** real XRP, small amounts (acceptable-loss-as-cost-of-learning per `development-operating-model.md` §1.1), single operator-of-record. Lives on a new Azure DCsv3 VM provisioned specifically for this role.
- **What it IS NOT:** the legacy Hetzner deployment that previously held this name. The legacy deployment is being retired. Mainnet-sandbox has migrated.
- **What's at risk if it breaks:** small real XRP. No customer state because we are still in `product-sandbox-single-operator` mode.
- **Build path:** identical to testnet-cluster. Same `Dockerfile.azure`. Production-mode-grade hardware; sandbox-mode operating semantics during this phase.

### 2.4 mainnet-sandbox-cluster

- **What it IS (future, transient bridge state):** the validation environment between mainnet-sandbox (single-op) and production (multi-op real funds). Real XRP small amounts on XRPL mainnet, ≥2 independent operators each running their own Azure DCsv3 VM, FROST 2-of-N + DCAP peer attestation activated. Foundation invariants 1–4 (multi-operator zero trust) + 5 (upgrade-path) + 7 (reproducibility) all enforced.
- **What it IS NOT:** production. No real customer funds yet — only the operators' own seed XRP. AI-Auditor remains the primary gate, not third-party human audit.
- **What's at risk if it breaks:** the operators' own small XRP balances. Customer reputation if third parties were watching, but no customer funds.
- **Build path:** identical to testnet-cluster and mainnet-sandbox. Same `Dockerfile.azure`. Reproducibility cross-check is now actually meaningful (≥2 humans independently rebuild and confirm MRENCLAVE).
- **Why this stage exists:** to validate that the multi-operator architecture works on mainnet before adding real customer funds. Skipping this stage and going directly from single-op sandbox to multi-op production is reckless — multi-operator topology has its own failure modes (operator-vs-operator coordination, signerlist rotation, peer-attestation degradation, etc.) that need real-mainnet validation before customer funds are at stake.

### 2.5 production

- **What it IS (future):** multi-operator, real customer funds, full audit cycle each upgrade.
- **What it IS NOT:** unlocked. Per `multi-operator-architecture.md` §1 invariants 5 + 7, production-mode requires both Path A operational AND reproducibility-N-of-M operational with ≥N independent operators.
- Specification deferred to future REQ cycle (post-REQ-8).

---

## 3. Why Hetzner cannot host mainnet-sandbox or any cluster-class environment

Hetzner SGX1 + OOT `isgx` driver + SDK 2.25 is fundamentally incompatible with the requirements of mainnet-sandbox, mainnet-sandbox-cluster, and production. The reasons compose; even one of them on its own would block, and they hold simultaneously.

### 3.1 No DCAP attestation on Hetzner SGX1

DCAP attestation requires SGX2 + Intel-provisioned Provisioning Certification Keys (PCK). Hetzner SGX1 does not have these. Without DCAP:

- **Peer-to-peer attestation between operators is impossible.** A peer cannot prove to another peer that "I am running MRENCLAVE X on SGX hardware right now" without DCAP. EPID (the older alternative) is deprecated by Intel and not appropriate for new deployments. This breaks foundation invariant 1–2 (no single operator can sign / no single operator can produce a FROST signature), because there is no cryptographic mechanism to verify peer enclave identity.
- **External audit cannot verify deployed MRENCLAVE matches source.** A DCAP quote is the standard mechanism for an outside party to confirm "the enclave running on this host has measurement X." Without DCAP, the only external evidence is operator's signed claim, which is operator-trust-only.

### 3.2 SDK 2.25-era is not future-compatible

- Intel publishes new SDK releases (currently 2.29, soon 2.30+) and progressively drops support for older versions. SDK 2.25 will eventually be removed from the Intel apt repository.
- SDK 2.28 already dropped OOT `/dev/isgx` driver support (verified empirically 2026-05-03: `sgx_create_enclave returned: 0x000a, "Out of tree driver is no longer supported"`). Any newer SDK is even less compatible with Hetzner's driver.
- This means Hetzner SGX1 + OOT is on a deprecation trajectory. Building mainnet-sandbox there would be building on a foundation that Intel is sunsetting.

### 3.3 Reproducibility-N-of-M (foundation invariant 7) cannot include Hetzner

- Canonical builds for cluster-class environments use `Dockerfile.azure` SDK 2.28. The output cannot run on Hetzner OOT (driver mismatch — see §3.2).
- A Hetzner-built artefact (SDK 2.25, OOT-loadable) produces a different MRENCLAVE that cannot be reconciled with the canonical chain.
- Foundation invariant 7 requires ≥N operators to produce bit-identical MRENCLAVE. If one operator runs Hetzner SGX1, their MRENCLAVE will never match the others' — invariant fails by construction.
- For mainnet-sandbox specifically: even if it were single-operator (no peer attestation needed today), the moment a second operator joins (transition to mainnet-sandbox-cluster), invariant 7 must hold. Starting on a host that can never satisfy it is a guaranteed dead-end.

### 3.4 `cf65d92a…` is the cautionary tale

The legacy Hetzner mainnet deployment with MRENCLAVE `cf65d92a60cc059052cc867b8150ed186a13a73dd34a8515fc8b36e611994eab` (April 7, SDK 2.25, local make build) is the empirical example of why mainnet-on-Hetzner does not graduate:

- It cannot be reproduced today (SDK 2.25-era trts no longer available cleanly).
- It cannot accept multi-operator peers (no DCAP).
- It cannot Path-A-migrate to a canonical Dockerfile.azure-built MRENCLAVE on the same host (SDK 2.28 doesn't load on OOT — see §3.2).
- It is operator-trust-only by construction: no auditor or peer can verify it cryptographically.

This is precisely why the mainnet migration plan (§7) retires it rather than upgrading it in place.

### 3.5 Conclusion (the explicit rule)

> **Mainnet-sandbox MUST run on Azure DCsv3 (or equivalent SGX2 + DCAP-certified host with in-kernel SGX driver). It cannot run on Hetzner SGX1.**

This is the rule. It is not a preference; it is a structural consequence of foundation invariants 1–7 and the deprecation trajectory of SGX1 + OOT in the Intel SDK. The same rule applies a fortiori to mainnet-sandbox-cluster and production.

Hetzner remains valuable in the non-runtime roles described in §6.

---

## 4. Canonical build paths

| Build environment | Targets | SDK | Driver | Status |
|---|---|---|---|---|
| GHA `ubuntu-22.04` runner via `Dockerfile.azure` | testnet-cluster, mainnet-sandbox, production | 2.28.100.1 (pinned) | in-kernel target (output binary needs `/dev/sgx_enclave` at runtime) | Canonical (foundation invariant 7). Adopted 2026-05-03 with the build-gate landing. |
| Hetzner Docker via `Dockerfile.azure` | testnet-cluster, mainnet-sandbox, production | 2.28.100.1 (pinned) | in-kernel target | Canonical, equivalent to GHA. Useful when an operator wants to verify reproducibility locally before deploy. |
| Hetzner local `make` against `/opt/intel/sgxsdk` | dev-hetzner ONLY | ~2.25-era (whatever the operator host has) | OOT (`/dev/isgx`) | Operator-local exploration; **NOT** for any cluster environment. The output never enters any allowlist. |

The asymmetry is intentional: the canonical path produces artifacts deployable on testnet-cluster / mainnet-sandbox / production (all in-kernel + SDK 2.28); dev-hetzner runs an artefact only loadable on Hetzner (OOT + SDK 2.25). They are two non-interchangeable pipelines for two non-interchangeable runtimes.

---

## 5. Why this disambiguation matters in practice

Examples of statements that were ambiguous before this matrix and are now precise:

| Old phrasing (ambiguous) | New phrasing (precise) |
|---|---|
| "Mainnet runs on Hetzner" | "The legacy mainnet deployment ran on Hetzner; **mainnet-sandbox** post-migration runs on Azure DCsv3." |
| "Testnet is on Azure" | "**testnet-cluster** is on Azure DCsv3 ×3. **dev-hetzner** also exists for testnet-network experimentation but is operator-local and never joins testnet-cluster." |
| "We have two SDK builds" | "Canonical builds use SDK 2.28 (Dockerfile.azure) for testnet-cluster, mainnet-sandbox, production. SDK 2.25 builds exist only for **dev-hetzner** local experimentation." |
| "Currently-deployed mainnet MRENCLAVE is `cf65d92a…`" | "Legacy Hetzner `mainnet` deployment had MRENCLAVE `cf65d92a…` built before Dockerfile.azure existed. Post-migration **mainnet-sandbox** will have a Dockerfile.azure-canonical MRENCLAVE matching testnet-cluster's." |
| "Tom-testnet client connects to our testnet" | "Tom-testnet client connects to **testnet-cluster** API (Azure DCsv3). Tom does NOT connect to dev-hetzner." (Per `api-environment-policy.md` §1 update.) |

---

## 6. Hetzner's role going forward

Hetzner SGX1 bare metal continues to be valuable in two non-runtime roles plus one runtime role:

1. **Orchestrator-only host (non-runtime):** Hetzner runs the Rust orchestrator daemon, the libp2p mesh peer process, the perp-dex API endpoint when the enclave is local. The orchestrator does not depend on SGX; it can run on any glibc x86_64 Linux. This role is robust regardless of SGX driver state.
2. **Build host (non-runtime):** Hetzner is suitable for `docker build -f Dockerfile.azure` — Docker builds do not need SGX hardware (per `build-requirements.md` §1). The output binary cannot run on Hetzner SGX1 itself (SDK 2.28 vs OOT mismatch) but builds correctly. Useful when an operator wants to verify cross-build reproducibility against GHA.
3. **dev-hetzner enclave runtime (single-operator playground):** Hetzner runs an SGX1 enclave built locally with SDK 2.25, used for personal dev iteration. Operator-private. Not a cluster member.

What Hetzner stops being:
- ❌ The **mainnet** runtime host (post-migration).
- ❌ A **testnet-cluster** peer (it cannot DCAP-attest, so cannot meaningfully participate in cross-machine FROST).
- ❌ A **production** host (foundation invariants 1–7 require SGX2 + DCAP + in-kernel).

---

## 7. Mainnet migration plan (legacy Hetzner → mainnet-sandbox on Azure)

Confirmed direction 2026-05-04: legacy Hetzner mainnet deployment (`cf65d92a…`, built April 7 with SDK 2.25) is retired in favour of a fresh **mainnet-sandbox** on Azure DCsv3 with canonical Dockerfile.azure-built MRENCLAVE.

### 7.1 Steps

| Step | What | When | Dependencies |
|---|---|---|---|
| 1 | Provision new Azure DCsv3 VM dedicated to mainnet-sandbox | Operations task | After Path A REQ-8 PASS (so the migration mechanism is documented and reviewed even if not used for this specific transition) |
| 2 | Bootstrap fresh enclave there via `Dockerfile.azure` → new MRENCLAVE_canonical (will match testnet-cluster's value if source state is the same) | After step 1 | `Dockerfile.azure` build artefact |
| 3 | State migration legacy `cf65d92a…` (Hetzner OOT) → new MRENCLAVE_canonical (Azure in-kernel) | TBD — see §6.2 | Decision on mechanism |
| 4 | XRPL escrow signing authority transfer (via signerlist-update OR account swap if the new enclave has its own bootstrapped XRPL keys) | After step 3 | XRPL multisig from current signers |
| 5 | Hetzner mainnet shutdown + role transition to dev-hetzner only | After step 4 verification | Confirm Azure mainnet-sandbox is healthy and signing |

### 7.2 Cross-host migration mechanism — open question

Path A as currently specified (REQ-7) is **single-host**: it requires OLD enclave and NEW enclave running on the same machine for Local Attestation. The Hetzner-to-Azure transition is cross-host, which is outside Path A's scope.

Two options for resolving this:

**Option α — Extend Path A spec for cross-host migration.** Cross-host Local Attestation does not exist (Local Attestation is by definition same-platform). Cross-host migration would have to use DCAP-attested ECDH between OLD-enclave (Hetzner) and NEW-enclave (Azure), similar to existing `share-v2` transport. This is a non-trivial spec extension that would consume an audit cycle (REQ-7.6 or REQ-8.5).

**Option β — Non-state-preserving fresh-bootstrap on Azure.** Per `project_mainnet_live.md`, the legacy `cf65d92a…` deployment holds only seed/test funds (~108 XRP) belonging to the operator-of-record. There is no real customer state to migrate. Operator-of-record can:
1. On Hetzner: standard XRPL withdrawal of remaining XRP from escrow → to operator's own XRPL address.
2. Bootstrap fresh on Azure with `Dockerfile.azure`-built enclave → new MRENCLAVE_canonical, fresh XRPL escrow address from the new enclave.
3. Re-fund new escrow from operator's address.
4. Update any external references (deployment docs, dashboards) to the new escrow address.
5. Decommission Hetzner mainnet runtime; Hetzner repurposed as dev-hetzner + orchestrator-only.

This is **NOT a Path A migration**. It is a clean shutdown + clean restart, valid because there is nothing to preserve on the legacy side.

### 7.3 Recommendation

Option β (fresh-bootstrap on Azure) is operationally simpler, requires no Path A spec extension, and is acceptable because no real customer state is on the legacy deployment. Option α (cross-host Path A extension) is overkill for sandbox-mode-only legacy state.

Decision still requires explicit operator-of-record sign-off; capturing here as the recommended path pending that.

---

## 8. References

- `docs/development-operating-model.{en,ru}.md` — operating modes (axis 2)
- `docs/build-requirements.{en,ru}.md` §7 — anti-patterns including local `make` for cluster builds
- `docs/sdk-version-matrix.{en,ru}.md` — SDK 2.25 vs 2.28 (axis 4)
- `docs/multi-operator-architecture.{en,ru}.md` §1 invariants 5 + 7 — foundation rules that gate production-mode unlock
- `docs/api-environment-policy.{en,ru}.md` — Tom integration; testnet-pairing references **testnet-cluster**, never dev-hetzner
- `docs/audit/REQ-7.md` (private repo) — Path A spec; single-host scope; cross-host migration is an open extension
- `feedback_reproducible_build_foundation.md` (memory) — foundation invariant 7
- `feedback_upgrade_path_is_foundation.md` (memory) — foundation invariant 5
- `project_mainnet_live.md` (memory) — legacy Hetzner mainnet state including ~108 XRP seed funds
