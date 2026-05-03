# Build, Sign, и Run requirements для SGX enclave

**Status:** Принято (2026-05-03).
**Audience:** internal team и dev-perp; будущие maintainer'ы; AI-Auditor; будущие external auditor'ы; любой новый оператор присоединяющийся к multi-operator кластеру.
**Companions:** [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.ru.md) (foundation invariants 5–7), [`development-operating-model.{en,ru}.md`](development-operating-model.ru.md) (operating modes + production-mode unlock conditions), [`testnet-enclave-bump-procedure.{en,ru}.md`](testnet-enclave-bump-procedure.ru.md) (concrete operator runbook).

Этот документ фиксирует policy и техническую реальность для трёх независимых concerns которые часто confused в одно: где можно **собирать** SGX enclave artefact, где можно его **подписывать**, и где можно его **запускать**. Это разные слои с разными hardware requirements, разными trust assumptions, и разными failure modes. Conflating их производит audit-defeating ошибки.

---

## 0. Зачем этот документ

До сих пор мы implicitly работали с одним оператором (Andrey) который собирает, подписывает, и запускает enclave на инфраструктуре которой он управляет. Multi-operator architecture (`multi-operator-architecture.md`) предполагает что ≥3 операторов независимо производят тот же MRENCLAVE, подписывают своими ключами, и запускают на своём SGX hardware — но implicit assumption что "Hetzner собирает enclave потому что у Hetzner SGX" был неверным по техническим merits и опасным как foundation для trust model.

Этот документ корректирует техническое понимание, фиксирует policy для каждого слоя, устанавливает reproducibility как foundation invariant peer'ом к upgrade-path invariant (per `feedback_upgrade_path_is_foundation.md`), и даёт операторам + аудиторам контракт который не полагается на operator memory.

Корректировки имеют значение потому что: production-mode unlock требует ОБА Path A (state preservation across MRENCLAVE bumps) И reproducible-build-N-of-M (multi-operator MRENCLAVE agreement). Без второго, первый сводится к "trust one operator's build environment" и multi-operator security model вырождается в single-operator trust.

---

## 1. Три слоя разделены

Три операции имеют **независимые** hardware и trust requirements:

| Слой | Назначение | SGX hardware required? | Trust requirement |
|---|---|---|---|
| **Build** | Произвести `enclave.signed.so` artefact из source | НЕТ | committed Dockerfile pinned к точным toolchain versions |
| **Sign** | Применить operator signature к `enclave.so` → `enclave.signed.so` | НЕТ | sign-key custody (operational secret) |
| **Run** | Загрузить `enclave.signed.so` в SGX enclave в runtime | ДА (SGX1 минимум) | runtime host integrity + DCAP attestation chain |

Распространённая ошибка: "build host нужен SGX потому что output для SGX." Не нужен. Build pipeline — это pure software tooling — gcc, SGX SDK toolchain (`sgx_edger8r`, `sgx_sign`), linker подсасывающий `libsgx_*.a` archives с диска, и Docker. Ничего из этого не трогает SGX hardware. Output — это binary file на диске который **идентичен** независимо от того собран ли он на non-SGX laptop, non-SGX cloud VM, или SGX-capable server, при условии что toolchain идентичен.

Runtime layer — это где SGX hardware требуется, потому что там вызывается `sgx_create_enclave()` и enclave реально загружается в SGX enclave-memory region (EPC). Никакая build configuration не может substitute hardware-level enclave instantiation.

---

## 2. Build layer

### 2.1 Где может происходить build — где угодно с Linux x86_64 + Docker

| Host | Подходит для build? | Notes |
|---|---|---|
| Developer laptop (любой non-SGX x86_64 Linux) | ДА | полезно для local iteration; MRENCLAVE может diverge от production если Docker image не используется (см. §2.3) |
| Hetzner non-SGX standard server | ДА | identical capability к laptop; не используется нами сегодня |
| Hetzner SGX1 server (наш текущий build host) | ДА | работает из-за Docker toolchain, НЕ из-за SGX1 hardware. SGX1 capability — runtime side-benefit, не build requirement. |
| Azure DCsv3 (SGX2+DCAP) | ДА | overkill для build но работает |
| GHA standard runner `ubuntu-22.04` | ДА | canonical build-gate target — см. §2.4 |
| GHA self-hosted SGX runner | ДА | unnecessary — добавляет operational complexity без build-time benefit |

Вопрос "где" largely uninteresting. Интересный вопрос — "как" — он в §2.3.

### 2.2 Что значит `SGX_MODE=HW` в нашем Makefile

Стандартные Intel SDK sample Makefiles используют `SGX_MODE` для switching какие library variants линкуются: `HW` selects реальные `libsgx_urts.so` / `libsgx_trts.a` (которые требуют SGX hardware в runtime), а `SIM` selects `libsgx_urts_sim.so` / `libsgx_trts_sim.a` (software simulator который позволяет загружать enclave на non-SGX hardware, используется для development и CI smoke-testing).

**Наш `EthSignerEnclave/Makefile` НЕ implements SIM branch.** Все linker invocations хардкодят HW variants (`-lsgx_urts`, `-lsgx_trts`, `-lsgx_tservice`, `-lsgx_uae_service`, `-lsgx_dcap_tvl`). Переменная `SGX_MODE` влияет только на:

- marker filename `.config_<Build_Mode>_<SGX_ARCH>` (purely cosmetic, используется как make dependency для detection mode changes между builds)
- в release builds (`SGX_DEBUG=0`), gates `-Wl,-O2` linker optimisation flags

`SGX_MODE=SIM` не произвёл бы рабочий sim-mode binary в нашем Makefile потому что SIM library variants никогда не линкуются. Если разработчику нужно запустить наш enclave на non-SGX hardware для testing, требуется дополнительная Makefile работа для добавления SIM branch. Это known gap; не было приоритетом потому что у нас всегда есть SGX-capable hardware (Hetzner SGX1 + Azure DCsv3) для runtime testing.

### 2.3 Reproducibility constraint — только committed Dockerfile

Чтобы build производил MRENCLAVE который другие operators могут reproduce, toolchain должен быть byte-identical across builders. Это достигается всегда сборкой через `EthSignerEnclave/Dockerfile.azure`, который pins:

- Base image: `ubuntu:22.04`
- SGX SDK version: `2.28.100.1` (matching Azure runtime)
- libsgx-* package versions: `2.28.100.1-jammy1`
- Build steps invoked from a fixed working directory

Dockerfile committed в private repo и является authoritative build environment. Сборка вне Docker (`make` напрямую на host system) **не authoritative** — host-system gcc version, libc version, system library versions, и embedded build paths будут differ между machines и произведут разный MRENCLAVE.

**Anti-pattern:** "Я склонировал repo и запустил `make` на своём workstation, и MRENCLAVE не матчится с production. Что-то сломано."

**Correct response:** build был выполнен вне committed Dockerfile, поэтому MRENCLAVE expected to differ. Чтобы произвести production-matching MRENCLAVE, запусти `docker build -f EthSignerEnclave/Dockerfile.azure .` из clean clone. Чтобы debug Dockerfile-internal builds, modify Dockerfile и commit изменение.

### 2.4 GHA build-gate

Build-gate работает как GitHub Actions workflow на standard `ubuntu-22.04` runner, в private repo. Он:

1. Checks out source.
2. Provides сгенерированный dev sign-key (random, ephemeral, sandbox-mode only — см. §3.2). Это чтобы GHA build мог завершить `sgx_sign sign` без требования access к operator's production sign-key.
3. Запускает `docker build -f EthSignerEnclave/Dockerfile.azure .` для производства `enclave.signed.so`.
4. Extracts artefact из container.
5. Запускает `sgx_sign dump` (или equivalent SGX SDK tool) на `enclave.signed.so` для печати MRENCLAVE measurement в workflow log.
6. Optionally archives `enclave.signed.so` как workflow artefact для inspection.

Что GHA gate **не** делает:

- Запускает enclave (`./app` или `perp-dex-server`) — GHA `ubuntu-22.04` не SGX-capable, enclave не может быть загружен, smoke-test impossible на этом слое.
- Verifies DCAP attestation — same reason.
- Validates signature against operator production sign-key — gate использует throwaway dev key.

Что GHA gate **доказывает**:

- Build не сломан (Dockerfile parses, dependencies resolve, source compiles, links, signs, produces artefact).
- MRENCLAVE для current commit — published value (в workflow log, addressable by commit SHA).
- Этот MRENCLAVE может быть cross-checked against independent operator's local build для reproducibility verification (§5).

Runtime smoke-tests (load enclave, generate DCAP quote, run orchestrator handshake) deferred к deployment stage, на actual SGX hardware (Azure DCsv3 / Hetzner SGX SKU).

---

## 3. Sign layer

### 3.1 Что делает signing

`sgx_sign sign` применяет operator-controlled RSA private key (`Enclave/Enclave_private.pem`) к unsigned enclave binary, производя `enclave.signed.so`. Signature определяет MRSIGNER (SHA-256 от public key derived from private key); enclave content определяет MRENCLAVE (SHA-256 от enclave's pages и initial state).

**Critical fact:** MRENCLAVE независим от sign-key. Два оператора с разными sign-keys собирающие тот же source через тот же Dockerfile производят тот же MRENCLAVE. Они производят разные MRSIGNER values, но наша policy — MRENCLAVE-sealing (см. `feedback_read_our_docs_first.md`), поэтому MRSIGNER не используется для sealed-state identity в этом проекте.

### 3.2 Sign-key custody policy

Это policy-dependent и varies by operating mode (per `development-operating-model.md` §1):

| Operating mode | Sign-key policy | Rationale |
|---|---|---|
| `product-sandbox-single-operator` (current) | Один оператор (Andrey) держит sign-key, подписывает все release builds | Нет multi-operator trust assumption который нужно honour |
| `product-sandbox-multi-operator` (transitional) | Каждый оператор независимо подписывает свой deployment своим sign-key; MRENCLAVE-match across all operators — cross-check (sign-keys могут differ) | MRENCLAVE-policy значит signature только для runtime acceptance, не identity |
| `production` (final) | ADR-required, deferred. Options: SGX 2-step sign с offline N-of-M ceremony; HSM-stored production key; или per-operator-signed enclaves с on-chain MRENCLAVE allowlist | Open question — см. §3.3 |
| GHA build-gate (CI) | Throwaway random key generated per build; sandbox-mode only | CI не может иметь access к operator production keys; acceptable потому что MRENCLAVE — что важно |

### 3.3 Open ADR — production sign-key custody

Отдельное architectural decision требуется до того как production-mode reached. Decision **не** часть этого документа; этот документ только fixes framing:

- Decision независим от MRENCLAVE-reproducibility (MRENCLAVE не affected sign-key choice).
- Decision affects MRSIGNER, который не используется нашей project policy — но future audit может спросить почему мы не используем его как additional defence-in-depth, и ADR должен ответить.
- Decision affects operational ceremony: signing в production не должен concentrate authority в single operator's filesystem.

Это captured как foundation gap от которого зависит production-mode unlock (наряду с Path A и reproducibility).

### 3.4 Где может происходить signing

Signing требует только sign-key file и SGX SDK `sgx_sign` tool. Оба доступны на любом x86_64 Linux. **Где живёт sign-key** — operational question — обычно на hardened operator workstation, ключ никогда не копируется на build host.

Для нашей текущей sandbox-mode reality: sign-key на Andrey's controlled host; Hetzner build pipeline reads его из mount path который не committed в repo. Для GHA: sign-key generated fresh per build внутри workflow (no operator key exposure).

---

## 4. Runtime layer

### 4.1 SGX hardware level matrix

Три relevant levels:

| Level | Hardware example | Поддерживает |
|---|---|---|
| No SGX | большинство VPS, ARM laptops, Intel CPUs без SGX feature | No SGX runtime at all |
| SGX1 | Hetzner EX line (Coffee Lake / Skylake era), older Intel Xeon | Enclave loading, MRENCLAVE-sealing, **Local Attestation**, EPID attestation (Intel-managed, deprecated для new deployments) |
| SGX2 + DCAP-certified CPU | Azure DCsv3 / DCsv2, IBM Cloud SGX, Equinix SGX | All of SGX1 + **DCAP attestation** + Flexible Launch Control + Dynamic Memory Management |

**DCAP availability — это не то же что SGX2.** Дополнительно требует чтобы CPU был на Intel's "DCAP supported" Provisioning Certification list И имел provisioned Provisioning Certification Keys. Cloud providers handle это transparently для supported SKUs.

### 4.2 Hetzner SGX1 capability table

Поскольку Hetzner — наш текущий build host AND наш orchestrator host, и проект исторически conflated эти uses, эта таблица fixes что Hetzner SGX1 может и не может:

| Operation | Hetzner SGX1 support | Notes |
|---|---|---|
| Build enclave | ДА | работает из-за Docker toolchain, не из-за SGX1 |
| Run enclave (`sgx_create_enclave`) | ДА | SGX1 поддерживает enclave loading |
| MRENCLAVE-sealing | ДА | platform sealing primitives существуют на SGX1 |
| **Local Attestation** (между двумя enclaves на той же машине) | ДА | не требует DCAP — использует platform report key; работает на любом SGX-capable host |
| Path A migration ceremony participant | ДА | Path A использует Local Attestation, не DCAP — см. REQ-7 §3 |
| EPID attestation | ДА (deprecated) | Intel-managed; не используется в этом проекте |
| **DCAP attestation quote generation** | НЕТ | требует SGX2 + Provisioning Certification |
| ECDH-over-DCAP cross-machine peer authentication | НЕТ | depends on DCAP |
| Multi-operator FROST signing peer (production) | НЕТ | требует DCAP для cross-machine peer attestation |
| Test-only multi-machine peering с attestation disabled | ДА | only valid для development/testnet, не production |

Implication для Path A testing: один Hetzner host может запустить OLD enclave + NEW enclave side-by-side и exercise полную Path A migration ceremony, потому что Local Attestation не требует DCAP. Это делает Hetzner полезным single-machine Path A test environment, отдельным от multi-machine Azure DCsv3 testnet кластера.

### 4.3 Production runtime requirements

Для production-mode (per `development-operating-model.md` §1.3), каждый operator's runtime host должен:

- Быть SGX2 + DCAP-certified (Azure DCsv3 или equivalent).
- Иметь DCAP libraries и PCS-cache configuration validated (см. `feedback_azure_dcap_findings.md` для known PCS endpoint issues).
- Быть на on-chain MRENCLAVE allowlist (per REQ-7 §3.4) для MRENCLAVE который он загружает.
- Иметь enclave loaded с `DisableDebug=1` per `Enclave.config.xml` (current `DisableDebug=0` — sandbox-only).
- Иметь sign-key custody arrangement aligned с §3.3 ADR decision (всё ещё pending).

---

## 5. Reproducibility — N-of-M procedure

Это foundation invariant introduced 2026-05-03 (per `feedback_reproducible_build_foundation.md`).

### 5.1 Rule

> **No production deployment с MRENCLAVE который не был reproducibly built ≥N independent operators с bit-identical result.**

Для нашего 2-of-3 кластера, N = 3 (все operators независимо собирают). Для нашего текущего sandbox-single-operator mode, N = 1 (правило suspended along с `multi-operator-architecture.md` §1 invariants 1–4 per `development-operating-model.md` §1.1).

### 5.2 Procedure для production builds

1. Каждый из N operators clones private repo на specific commit SHA на clean working host.
2. Каждый запускает `docker build -f EthSignerEnclave/Dockerfile.azure .` (no cache, no host-system contamination).
3. Каждый extracts `enclave.signed.so` из resulting container и запускает `sgx_sign dump -enclave enclave.signed.so` для obtain MRENCLAVE.
4. Каждый publishes signed message containing `(commit_sha, MRENCLAVE, build_timestamp, operator_id)`. Signature через operator's XRPL operator key (sealed внутри его enclave — но это bootstrap так что done off-enclave с documented key).
5. N messages aggregated; если все MRENCLAVE values identical → bit-identical reproducibility attested.
6. Agreed MRENCLAVE добавлен в on-chain allowlist (per REQ-7) как next-authorised MRENCLAVE_new.

Если any MRENCLAVE diverges → **STOP**. Это architectural bug per `feedback_workarounds_are_arch_bugs.md`, не deployment issue. См. §6.

### 5.3 GHA + Hetzner cross-check

Минимальный reproducibility test который мы можем запустить сегодня (без второго human operator) — это:

- GHA workflow собирает в `ubuntu-22.04` runner → published MRENCLAVE в workflow log.
- Hetzner local `docker build` → MRENCLAVE captured в operator log.
- Compare. Если identical, reproducibility демонстрирована для **N=2 build environments** (same repo, different hosts, same Docker image).

Это sufficient evidence для sandbox-multi-operator readiness (procedure works) но не satisfies production-mode N=3 (которое needs three independent humans, не two environments controlled by one human).

---

## 6. Когда MRENCLAVE diverges между builds

Это real и recurring проблема в SGX reproducible builds. Common causes и remediations:

| Source of non-determinism | Remediation |
|---|---|
| Разные Intel SGX SDK versions | Pin в Dockerfile (`libsgx-*=2.28.100.1-jammy1`) — already done |
| Разные gcc / glibc versions на build host | Use Docker only; никогда `make` напрямую на host |
| Embedded build paths в debug info (`/build/...` vs `/home/andrey/...`) | Add `-fdebug-prefix-map=$(PWD)=/build` к CFLAGS; или strip debug info из release builds |
| Embedded build timestamps в object files | Set `SOURCE_DATE_EPOCH=0` env в build script |
| Random stack canaries в object files | Обычно consistent across SDK versions; investigate если observed |
| Symbol table ordering depending on filesystem ordering | Add `find ... | sort` для source file enumeration; или `--sort-section=name` linker flag |
| Docker BuildKit cache mounts retaining timestamps | Use `--no-cache` для release builds; или fix BuildKit reproducibility flags |
| Differing `Enclave.config.xml` (e.g. local dev edits) | Ensure `Enclave.config.xml` committed; CI uses committed version only |

### 6.1 Diagnostic procedure when divergence observed

1. **Collect both `enclave.signed.so` artefacts** из diverging builds.
2. Run `objcopy --strip-all` на both для удаления debug info; compare. Если MRENCLAVE now matches → divergence в debug info embedding (apply `-fdebug-prefix-map` fix).
3. Если still differs: run `diffoscope enclave_a.signed.so enclave_b.signed.so` для identify byte-level differences.
4. Если разница в section ordering: linker / `find` ordering issue (apply sort fix).
5. Если разница в section content: SDK / library / source-version drift (compare Dockerfile pinned versions; check если any `apt-get install` missing version pin).

### 6.2 Что НЕ делать когда divergence persists

- **Не** accept "one operator's build is canonical" — это nullifies multi-operator security model.
- **Не** add per-operator MRENCLAVE allowlist — это превращает reproducibility из binary check в growing compatibility surface.
- **Не** disable MRENCLAVE check в каком-нибудь "trust DCAP only" mode — DCAP только proves "этот enclave запущен на SGX hardware right now", не "этот enclave — то что мы audited."
- **Не** ship в production с unresolved divergence — defer пока root cause не fixed в Dockerfile.

---

## 7. Anti-patterns

Следующее запрещено в production-mode (и discouraged в sandbox modes):

1. **`make` напрямую на host system** — производит non-canonical MRENCLAVE. Use `docker build -f Dockerfile.azure` only.
2. **Build с `SGX_MODE=SIM`** — не работает в нашем Makefile и произвёл бы non-loadable artefact даже если бы работал. SIM — known gap; не workaround.
3. **Share operator's production sign-key с CI или другими operators** — sign-keys — operational secrets. CI uses throwaway dev keys.
4. **Skip GHA build-gate when shipping to mainnet** — production deployments должны trace back к GHA-built MRENCLAVE который был reproducibly verified.
5. **Run enclave на non-SGX hardware** — даже для testing, это meaningless: enclave не может load, sealed state не может unsealed, attestation не produces quote.
6. **Run enclave на SGX1 в production-mode** — DCAP required для cross-machine attestation. SGX1 acceptable для build host и single-machine Path A testing; не для production multi-operator runtime.
7. **Build для production без `DisableDebug=1`** — current sandbox `Enclave.config.xml` has `DisableDebug=0`, который loads только на debug-enabled hosts. Production должен flip это и re-attest MRENCLAVE.

---

## 8. References

- [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.ru.md) §1 invariants 5 (upgrade-path-foundation), 6 (Pattern 318), 7 (reproducibility — to be added in this document's wake)
- [`development-operating-model.{en,ru}.md`](development-operating-model.ru.md) §1 (operating modes), §3 (Mode S sync — currently postponed pending Path A + reproducibility)
- [`testnet-enclave-bump-procedure.{en,ru}.md`](testnet-enclave-bump-procedure.ru.md) — concrete operator runbook для testnet today (will be updated post-Path-A для use migration ceremony)
- `docs/audit/REQ-7.md` (private repo) — Path A spec including on-chain MRENCLAVE allowlist mechanism который consumes outputs §5 procedure
- `feedback_reproducible_build_foundation.md` (memory) — foundation rule
- `feedback_upgrade_path_is_foundation.md` (memory) — peer foundation rule (Path A)
- `feedback_read_our_docs_first.md` (memory) — MRENCLAVE-sealing policy (rejecting MRSIGNER-sealing permanently)
- `feedback_azure_dcap_findings.md` (memory) — Azure DCAP runtime quirks
- `EthSignerEnclave/Dockerfile.azure` (private repo) — authoritative build environment
- `EthSignerEnclave/Makefile` (private repo) — build orchestration; SIM branch — known gap
