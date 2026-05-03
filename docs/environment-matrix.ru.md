# Environment matrix — пять named environments, четыре orthogonal оси

**Status:** Принято (2026-05-04).
**Audience:** internal team и dev-perp; Tom (для environment-pairing references); AI-Auditor; future external auditor'ы; future operators.
**Companions:** [`development-operating-model.{en,ru}.md`](development-operating-model.ru.md) (operating modes), [`build-requirements.{en,ru}.md`](build-requirements.ru.md) (Build / Sign / Run policy), [`sdk-version-matrix.{en,ru}.md`](sdk-version-matrix.ru.md) (SDK 2.25 vs 2.28 ladder), [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.ru.md) (foundation invariants), [`api-environment-policy.{en,ru}.md`](api-environment-policy.ru.md) (Tom integration pairing).

---

## 0. Зачем этот документ

До 2026-05-04 проект conflated четыре orthogonal оси в один informal label "mainnet vs testnet":

1. **XRPL network** — testnet (faucet-funded XRP) vs mainnet (real XRP).
2. **Operating mode** (per `development-operating-model.md` §1) — `product-sandbox-single-operator` / `product-sandbox-multi-operator` / `production`.
3. **Host platform** — Hetzner SGX1 (single bare-metal box, OOT `isgx` driver, libsgx-* mostly 2.25) vs Azure DCsv3 (cloud SGX2, in-kernel `/dev/sgx_enclave` driver, libsgx-* clean 2.28).
4. **SGX driver / SDK era** — OOT + SDK 2.25 vs in-kernel + SDK 2.28 (per `sdk-version-matrix.md`).

Conflation produce'нула multiple documented errors во время 2026-05 build-gate работы: claims о "currently-deployed mainnet MRENCLAVE" turned out to point at different environment чем actually running mainnet, и confusion about whether Dockerfile.azure-built binaries can run on Hetzner (не могут — SDK 2.28 dropped OOT driver support, verified empirically с `sgx_create_enclave returned: 0x000a` на 2026-05-03).

Этот документ fix'ит four-environment classification с explicit read-out rules чтобы future statements about "where" не могли collapse оси в ambiguity.

---

## 1. Пять named environments

Каждый row — unique tuple всех четырёх осей, given a stable name. Combinatorial space — `XRPL × operating-mode = 2 × 3 = 6` cells; мы используем 5 из них. Unused cell — `testnet × production`, у которой no semantic meaning (production by definition mainnet).

| Environment | XRPL network | Operating mode | Host | SGX driver / SDK | Purpose | MRENCLAVE policy |
|---|---|---|---|---|---|---|
| **dev-hetzner** | testnet | sandbox-single | Hetzner SGX1 (bare metal `94.130.18.162`) | OOT `isgx` + SDK 2.25 | Personal dev playground для Andrey: fast iteration на testnet code без spinning up Azure VM; experimentation; offline-from-cluster work; smoke-testing changes которые НЕ требуют multi-operator FROST или DCAP | **Local-only.** MRENCLAVE produced здесь никогда не enters любой cluster's on-chain MRENCLAVE allowlist. Ephemeral by design. |
| **testnet-cluster** | testnet | sandbox-multi | Azure DCsv3 ×3 (`sgx-node-1` `20.71.184.176`, `sgx-node-3` `52.236.130.102`, plus 1 more) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Multi-operator FROST validation на faucet XRP; first place мы тестируем cross-machine signing, DKG, peer attestation; Path A migration ceremony testing | **Canonical** (built via committed `Dockerfile.azure`). Reproducible. Auditable. Currently MRENCLAVE `4dfe899771bdb3f3097714013d054c08c7dd6e28f2acd17948f8a08f328c011b` для commit `2c3d31f`. |
| **mainnet-sandbox** | XRPL mainnet | sandbox-single | Azure DCsv3 (new VM, post-migration) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Real XRP small amounts, operator-of-record only; pre-production proof of system на real funds; transient state — bridges к mainnet-sandbox-cluster когда second operator joins | **Canonical** (`Dockerfile.azure`), pinned via on-chain MRENCLAVE allowlist (per REQ-7 §3.4). |
| **mainnet-sandbox-cluster** | XRPL mainnet | sandbox-multi | Azure DCsv3 ×N (≥2 operator topology, но still no real customer state) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Bridge state между single-operator sandbox и production. Real XRP small amounts, multi-operator architecture activated (foundation invariants 1–4 enforced, 5 + 7 enforced), но no real customer funds. Validation что multi-operator topology actually works на mainnet до promoting к `production`. | **Canonical** (`Dockerfile.azure`), pinned on-chain. Multi-operator reproducibility cross-check now meaningful (≥2 operators independently rebuild и verify MRENCLAVE per `feedback_reproducible_build_foundation.md`). |
| **production** | XRPL mainnet | production (future, gated на Path A REQ-8 PASS + reproducibility cross-check by ≥N operators) | Azure DCsv3 ×N (multi-operator topology) | in-kernel `/dev/sgx_enclave` + SDK 2.28 | Multi-operator real customer funds. Full audit cycle each upgrade. Third-party human audit replaces AI-Auditor as primary gate. | **Canonical**, on-chain allowlist enforced, full audit cycle each upgrade. |

Anything что doesn't fit table — это не defined environment. Examples что **не** environments в этой scheme:
- "Tom's local laptop" — см. `api-environment-policy.md`; Tom connects к одному из четырёх environments выше by URL, он не оперирует своим own environment.
- "AI-Auditor's machine" — auditor reads artefacts; не enclave-runtime environment.
- "GHA build runner" — это build environment, не runtime environment. Output идёт в один из четырёх.

---

## 2. Read-out rules per environment

### 2.1 dev-hetzner

- **Что это IS:** Andrey's personal SGX1 playground на Hetzner bare metal. Он develops, builds (locally with SDK 2.25), runs enclave, exercises testnet faucet XRP, breaks things, restarts. No expectation что то что он runs здесь matches anybody else's MRENCLAVE.
- **Что это IS NOT:** part of any cluster. Не multi-operator member. Не bound by foundation invariants 1–4 (multi-operator zero trust) или 7 (reproducibility-N-of-M). Builds здесь explicitly NOT canonical.
- **Что at risk если ломается:** ничего in production sense; только Andrey's iteration speed. Easy to restart (`node-bootstrap` + faucet refill).
- **Build path:** Hetzner local `make` против `/opt/intel/sgxsdk` (SDK 2.25-era). Result links libsgx-urts.so.2 (host runtime SDK 2.25), uses `/dev/isgx` OOT driver. Will not load anywhere else.
- **Почему это existует despite "non-canonical":** потому что Hetzner SGX1 fundamentally unsuitable для production multi-operator (no DCAP, no in-kernel driver going forward) но он excellent для cheap fast iteration. Suppressing dev-hetzner would force every iteration через Azure (cost + latency). dev-hetzner — это "good fit для what Hetzner does well" choice.

### 2.2 testnet-cluster

- **Что это IS:** validation environment для cluster-level changes. 3 Azure DCsv3 VMs running cross-machine FROST 2-of-3, DCAP peer attestation, libp2p mesh. Faucet-funded XRP.
- **Что это IS NOT:** dev-hetzner. Anyone confusing the two — это one mistake away from accidentally pushing non-canonical MRENCLAVE в cluster.
- **Что at risk если ломается:** validation gate для promoting code в mainnet-sandbox. Faucet-XRP loss acceptable.
- **Build path:** committed `Dockerfile.azure` → SDK 2.28 → MRENCLAVE matches на всех 3 peers. Reproducible-build-N-of-M required для production-mode unlock; для testnet-cluster ≥1 reproducer (GHA) sufficient today.

### 2.3 mainnet-sandbox

- **Что это IS (post-migration target):** real XRP, small amounts (acceptable-loss-as-cost-of-learning per `development-operating-model.md` §1.1), single operator-of-record. Lives на new Azure DCsv3 VM provisioned specifically для этой роли.
- **Что это IS NOT:** legacy Hetzner deployment которое previously held это name. Legacy deployment retired. Mainnet-sandbox migrated.
- **Что at risk если ломается:** small real XRP. No customer state потому что мы still в `product-sandbox-single-operator` mode.
- **Build path:** identical к testnet-cluster. Same `Dockerfile.azure`. Production-mode-grade hardware; sandbox-mode operating semantics during this phase.

### 2.4 mainnet-sandbox-cluster

- **Что это IS (future, transient bridge state):** validation environment между mainnet-sandbox (single-op) и production (multi-op real funds). Real XRP small amounts на XRPL mainnet, ≥2 independent operators each running свой own Azure DCsv3 VM, FROST 2-of-N + DCAP peer attestation activated. Foundation invariants 1–4 (multi-operator zero trust) + 5 (upgrade-path) + 7 (reproducibility) all enforced.
- **Что это IS NOT:** production. No real customer funds yet — только operators' own seed XRP. AI-Auditor remains primary gate, не third-party human audit.
- **Что at risk если ломается:** operators' own small XRP balances. Customer reputation если third parties were watching, но no customer funds.
- **Build path:** identical к testnet-cluster и mainnet-sandbox. Same `Dockerfile.azure`. Reproducibility cross-check now actually meaningful (≥2 humans independently rebuild и confirm MRENCLAVE).
- **Зачем эта стадия existует:** validate что multi-operator architecture works на mainnet до adding real customer funds. Skipping этой стадии и going directly от single-op sandbox к multi-op production reckless — multi-operator topology has its own failure modes (operator-vs-operator coordination, signerlist rotation, peer-attestation degradation, etc.) которые need real-mainnet validation до того как customer funds at stake.

### 2.5 production

- **Что это IS (future):** multi-operator, real customer funds, full audit cycle each upgrade.
- **Что это IS NOT:** unlocked. Per `multi-operator-architecture.md` §1 invariants 5 + 7, production-mode requires both Path A operational AND reproducibility-N-of-M operational с ≥N independent operators.
- Specification deferred к future REQ cycle (post-REQ-8).

---

## 3. Почему Hetzner не может host'ить mainnet-sandbox или любой cluster-class environment

Hetzner SGX1 + OOT `isgx` driver + SDK 2.25 fundamentally incompatible с requirements of mainnet-sandbox, mainnet-sandbox-cluster, и production. Reasons compose; даже один из них в одиночку would block, и они hold simultaneously.

### 3.1 No DCAP attestation на Hetzner SGX1

DCAP attestation requires SGX2 + Intel-provisioned Provisioning Certification Keys (PCK). Hetzner SGX1 не имеет этих. Без DCAP:

- **Peer-to-peer attestation между operators невозможна.** Peer не может prove другому peer что "я running MRENCLAVE X на SGX hardware right now" без DCAP. EPID (older alternative) deprecated by Intel и not appropriate для new deployments. Это breaks foundation invariant 1–2 (no single operator can sign / no single operator can produce FROST signature), потому что нет cryptographic mechanism для verify peer enclave identity.
- **External audit cannot verify deployed MRENCLAVE matches source.** DCAP quote — standard mechanism для outside party confirm "enclave running на этом host имеет measurement X." Без DCAP, only external evidence — operator's signed claim, which is operator-trust-only.

### 3.2 SDK 2.25-era не future-compatible

- Intel publishes new SDK releases (currently 2.29, soon 2.30+) и progressively drops support older versions. SDK 2.25 will eventually be removed from Intel apt repository.
- SDK 2.28 already dropped OOT `/dev/isgx` driver support (verified empirically 2026-05-03: `sgx_create_enclave returned: 0x000a, "Out of tree driver is no longer supported"`). Any newer SDK даже less compatible с Hetzner's driver.
- Это значит Hetzner SGX1 + OOT — на deprecation trajectory. Building mainnet-sandbox там would be building на foundation которую Intel sunsetting.

### 3.3 Reproducibility-N-of-M (foundation invariant 7) не может include Hetzner

- Canonical builds для cluster-class environments use `Dockerfile.azure` SDK 2.28. Output не может run на Hetzner OOT (driver mismatch — см. §3.2).
- Hetzner-built artefact (SDK 2.25, OOT-loadable) produces different MRENCLAVE которая не может be reconciled с canonical chain.
- Foundation invariant 7 requires ≥N operators produce bit-identical MRENCLAVE. Если один operator runs Hetzner SGX1, его MRENCLAVE никогда не match others' — invariant fails by construction.
- Для mainnet-sandbox specifically: даже если бы было single-operator (no peer attestation needed today), the moment second operator joins (transition к mainnet-sandbox-cluster), invariant 7 must hold. Starting на host который никогда can satisfy it — guaranteed dead-end.

### 3.4 `cf65d92a…` — cautionary tale

Legacy Hetzner mainnet deployment с MRENCLAVE `cf65d92a60cc059052cc867b8150ed186a13a73dd34a8515fc8b36e611994eab` (April 7, SDK 2.25, local make build) — empirical example почему mainnet-on-Hetzner does not graduate:

- Не может быть reproduced today (SDK 2.25-era trts no longer available cleanly).
- Не может accept multi-operator peers (no DCAP).
- Не может Path-A-migrate к canonical Dockerfile.azure-built MRENCLAVE на same host (SDK 2.28 не loads на OOT — см. §3.2).
- Operator-trust-only by construction: no auditor or peer cryptographically verify.

Это precisely why mainnet migration plan (§7) retires it rather than upgrading in place.

### 3.5 Conclusion (the explicit rule)

> **Mainnet-sandbox MUST run на Azure DCsv3 (или equivalent SGX2 + DCAP-certified host с in-kernel SGX driver). Не может run на Hetzner SGX1.**

Это правило. Это не preference; это structural consequence foundation invariants 1–7 и deprecation trajectory of SGX1 + OOT в Intel SDK. Same rule applies a fortiori к mainnet-sandbox-cluster и production.

Hetzner remains valuable в non-runtime roles described в §6.

---

## 4. Canonical build paths

| Build environment | Targets | SDK | Driver | Status |
|---|---|---|---|---|
| GHA `ubuntu-22.04` runner via `Dockerfile.azure` | testnet-cluster, mainnet-sandbox, production | 2.28.100.1 (pinned) | in-kernel target (output binary needs `/dev/sgx_enclave` at runtime) | Canonical (foundation invariant 7). Adopted 2026-05-03 с build-gate landing. |
| Hetzner Docker via `Dockerfile.azure` | testnet-cluster, mainnet-sandbox, production | 2.28.100.1 (pinned) | in-kernel target | Canonical, equivalent к GHA. Useful когда operator wants verify reproducibility locally before deploy. |
| Hetzner local `make` против `/opt/intel/sgxsdk` | dev-hetzner ONLY | ~2.25-era (whatever operator host has) | OOT (`/dev/isgx`) | Operator-local exploration; **NOT** для any cluster environment. Output никогда не enters any allowlist. |

Asymmetry intentional: canonical path produces artifacts deployable на testnet-cluster / mainnet-sandbox / production (all in-kernel + SDK 2.28); dev-hetzner runs artefact only loadable на Hetzner (OOT + SDK 2.25). They are two non-interchangeable pipelines для two non-interchangeable runtimes.

---

## 5. Почему это disambiguation matters в practice

Examples statements которые были ambiguous до этой matrix и теперь precise:

| Old phrasing (ambiguous) | New phrasing (precise) |
|---|---|
| "Mainnet runs on Hetzner" | "Legacy mainnet deployment ran на Hetzner; **mainnet-sandbox** post-migration runs на Azure DCsv3." |
| "Testnet is on Azure" | "**testnet-cluster** is на Azure DCsv3 ×3. **dev-hetzner** также existует для testnet-network experimentation но operator-local и никогда не joins testnet-cluster." |
| "We have two SDK builds" | "Canonical builds use SDK 2.28 (Dockerfile.azure) для testnet-cluster, mainnet-sandbox, production. SDK 2.25 builds existуют только для **dev-hetzner** local experimentation." |
| "Currently-deployed mainnet MRENCLAVE — `cf65d92a…`" | "Legacy Hetzner `mainnet` deployment имел MRENCLAVE `cf65d92a…` built before Dockerfile.azure existed. Post-migration **mainnet-sandbox** будет иметь Dockerfile.azure-canonical MRENCLAVE matching testnet-cluster's." |
| "Tom-testnet client connects to our testnet" | "Tom-testnet client connects к **testnet-cluster** API (Azure DCsv3). Tom does NOT connect к dev-hetzner." (Per `api-environment-policy.md` §1 update.) |

---

## 6. Hetzner's role going forward

Hetzner SGX1 bare metal continues to be valuable в two non-runtime roles plus one runtime role:

1. **Orchestrator-only host (non-runtime):** Hetzner runs Rust orchestrator daemon, libp2p mesh peer process, perp-dex API endpoint когда enclave local. Orchestrator не depends on SGX; can run на любом glibc x86_64 Linux. Эта роль robust regardless of SGX driver state.
2. **Build host (non-runtime):** Hetzner suitable для `docker build -f Dockerfile.azure` — Docker builds не need SGX hardware (per `build-requirements.md` §1). Output binary cannot run на Hetzner SGX1 itself (SDK 2.28 vs OOT mismatch) но builds correctly. Useful когда operator wants verify cross-build reproducibility против GHA.
3. **dev-hetzner enclave runtime (single-operator playground):** Hetzner runs SGX1 enclave built locally с SDK 2.25, used для personal dev iteration. Operator-private. Не cluster member.

Что Hetzner stops being:
- ❌ **mainnet** runtime host (post-migration).
- ❌ **testnet-cluster** peer (он не может DCAP-attest, поэтому не может meaningfully participate в cross-machine FROST).
- ❌ **production** host (foundation invariants 1–7 require SGX2 + DCAP + in-kernel).

---

## 7. Mainnet migration plan (legacy Hetzner → mainnet-sandbox на Azure)

Confirmed direction 2026-05-04: legacy Hetzner mainnet deployment (`cf65d92a…`, built April 7 с SDK 2.25) retired в favour of fresh **mainnet-sandbox** на Azure DCsv3 с canonical Dockerfile.azure-built MRENCLAVE.

### 7.1 Steps

| Step | What | When | Dependencies |
|---|---|---|---|
| 1 | Provision new Azure DCsv3 VM dedicated to mainnet-sandbox | Operations task | After Path A REQ-8 PASS (so migration mechanism documented and reviewed even если not used для этого specific transition) |
| 2 | Bootstrap fresh enclave там via `Dockerfile.azure` → new MRENCLAVE_canonical (will match testnet-cluster's value если source state same) | After step 1 | `Dockerfile.azure` build artefact |
| 3 | State migration legacy `cf65d92a…` (Hetzner OOT) → new MRENCLAVE_canonical (Azure in-kernel) | TBD — см. §6.2 | Decision on mechanism |
| 4 | XRPL escrow signing authority transfer (via signerlist-update OR account swap если new enclave имеет own bootstrapped XRPL keys) | After step 3 | XRPL multisig от current signers |
| 5 | Hetzner mainnet shutdown + role transition к dev-hetzner only | After step 4 verification | Confirm Azure mainnet-sandbox healthy и signing |

### 7.2 Cross-host migration mechanism — open question

Path A as currently specified (REQ-7) — **single-host**: it requires OLD enclave и NEW enclave running на same machine для Local Attestation. Hetzner-to-Azure transition cross-host, что outside Path A's scope.

Two options для resolving:

**Option α — Extend Path A spec для cross-host migration.** Cross-host Local Attestation does not exist (Local Attestation by definition same-platform). Cross-host migration would have to use DCAP-attested ECDH между OLD-enclave (Hetzner) и NEW-enclave (Azure), similar к existing `share-v2` transport. Это non-trivial spec extension которая would consume audit cycle (REQ-7.6 или REQ-8.5).

**Option β — Non-state-preserving fresh-bootstrap на Azure.** Per `project_mainnet_live.md`, legacy `cf65d92a…` deployment holds только seed/test funds (~108 XRP) belonging к operator-of-record. There is no real customer state to migrate. Operator-of-record can:
1. На Hetzner: standard XRPL withdrawal оставшихся XRP from escrow → к operator's own XRPL address.
2. Bootstrap fresh на Azure с `Dockerfile.azure`-built enclave → new MRENCLAVE_canonical, fresh XRPL escrow address from new enclave.
3. Re-fund new escrow от operator's address.
4. Update any external references (deployment docs, dashboards) к new escrow address.
5. Decommission Hetzner mainnet runtime; Hetzner repurposed как dev-hetzner + orchestrator-only.

Это **NOT a Path A migration**. Это clean shutdown + clean restart, valid потому что nothing to preserve на legacy side.

### 7.3 Recommendation

Option β (fresh-bootstrap на Azure) operationally simpler, requires no Path A spec extension, и acceptable потому что no real customer state на legacy deployment. Option α (cross-host Path A extension) overkill для sandbox-mode-only legacy state.

Decision still requires explicit operator-of-record sign-off; capturing здесь как recommended path pending that.

---

## 8. References

- `docs/development-operating-model.{en,ru}.md` — operating modes (axis 2)
- `docs/build-requirements.{en,ru}.md` §7 — anti-patterns including local `make` для cluster builds
- `docs/sdk-version-matrix.{en,ru}.md` — SDK 2.25 vs 2.28 (axis 4)
- `docs/multi-operator-architecture.{en,ru}.md` §1 invariants 5 + 7 — foundation rules которые gate production-mode unlock
- `docs/api-environment-policy.{en,ru}.md` — Tom integration; testnet-pairing references **testnet-cluster**, никогда dev-hetzner
- `docs/audit/REQ-7.md` (private repo) — Path A spec; single-host scope; cross-host migration — open extension
- `feedback_reproducible_build_foundation.md` (memory) — foundation invariant 7
- `feedback_upgrade_path_is_foundation.md` (memory) — foundation invariant 5
- `project_mainnet_live.md` (memory) — legacy Hetzner mainnet state including ~108 XRP seed funds
