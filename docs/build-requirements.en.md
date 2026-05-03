# Build, Sign, and Run requirements for the SGX enclave

**Status:** Accepted (2026-05-03).
**Audience:** the internal team and dev-perp; future maintainers; the AI-Auditor; future external auditors; any new operator joining the multi-operator cluster.
**Companions:** [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.en.md) (foundation invariants 5–7), [`development-operating-model.{en,ru}.md`](development-operating-model.en.md) (operating modes + production-mode unlock conditions), [`testnet-enclave-bump-procedure.{en,ru}.md`](testnet-enclave-bump-procedure.en.md) (concrete operator runbook).

This document fixes the policy and the technical reality for three independent concerns that are often confused into one: where the SGX enclave artefact can be **built**, where it can be **signed**, and where it can be **run**. These are different layers with different hardware requirements, different trust assumptions, and different failure modes. Conflating them produces audit-defeating mistakes.

---

## 0. Why this document exists

Until now we have implicitly run with one operator (Andrey) building, signing, and running the enclave on infrastructure he controls. Multi-operator architecture (`multi-operator-architecture.md`) presumes ≥3 operators independently produce the same MRENCLAVE, sign with their own keys, and run on their own SGX hardware — but the implicit assumption that "Hetzner builds the enclave because Hetzner has SGX" was wrong on the technical merits and dangerous as a foundation for the trust model.

This document corrects the technical understanding, fixes the policy for each layer, establishes reproducibility as a foundation invariant peer to the upgrade-path invariant (per `feedback_upgrade_path_is_foundation.md`), and gives operators + auditors a contract that does not rely on operator memory.

The corrections matter because: production-mode unlock requires both Path A (state preservation across MRENCLAVE bumps) AND reproducible-build-N-of-M (multi-operator MRENCLAVE agreement). Without the second, the first reduces to "trust one operator's build environment" and the multi-operator security model degenerates to single-operator trust.

---

## 1. The three layers separated

The three operations have **independent** hardware and trust requirements:

| Layer | Purpose | SGX hardware required? | Trust requirement |
|---|---|---|---|
| **Build** | Produce `enclave.signed.so` artefact from source | NO | committed Dockerfile pinned to exact toolchain versions |
| **Sign** | Apply operator signature to `enclave.so` → `enclave.signed.so` | NO | sign-key custody (operational secret) |
| **Run** | Load `enclave.signed.so` into an SGX enclave at runtime | YES (SGX1 minimum) | runtime host integrity + DCAP attestation chain |

A common mistake is to assume "build host needs SGX because the output is for SGX." It does not. The build pipeline is pure software tooling — gcc, the SGX SDK toolchain (`sgx_edger8r`, `sgx_sign`), the linker pulling `libsgx_*.a` archives from disk, and Docker. None of these touch SGX hardware. The output is a binary file on disk that is **identical** whether built on a non-SGX laptop, a non-SGX cloud VM, or an SGX-capable server, provided the toolchain is identical.

The runtime layer is where SGX hardware is required, because that is where `sgx_create_enclave()` is called and the enclave is actually loaded into the SGX enclave-memory region (EPC). No amount of build configuration can substitute hardware-level enclave instantiation.

---

## 2. Build layer

### 2.1 Where build can happen — anywhere with Linux x86_64 + Docker

| Host | Suitable for build? | Notes |
|---|---|---|
| Developer laptop (any non-SGX x86_64 Linux) | YES | useful for local iteration; MRENCLAVE may diverge from production if Docker image is not used (see §2.3) |
| Hetzner non-SGX standard server | YES | identical capability to laptop; not used by us today |
| Hetzner SGX1 server (our current build host) | YES | works because of the toolchain in Docker, NOT because of SGX1 hardware. The SGX1 capability is a runtime side-benefit, not a build requirement. |
| Azure DCsv3 (SGX2+DCAP) | YES | overkill for build but works |
| GHA standard runner `ubuntu-22.04` | YES | the canonical build-gate target — see §2.4 |
| GHA self-hosted SGX runner | YES | unnecessary — adds operational complexity for no build-time benefit |

The "where" question is largely uninteresting. The interesting question is "how" — which is in §2.3.

### 2.2 What `SGX_MODE=HW` means in our Makefile

The standard Intel SDK sample Makefiles use `SGX_MODE` to switch which library variants are linked: `HW` selects the real `libsgx_urts.so` / `libsgx_trts.a` (which require SGX hardware at runtime), while `SIM` selects `libsgx_urts_sim.so` / `libsgx_trts_sim.a` (a software simulator that lets the enclave be loaded on non-SGX hardware, used for development and CI smoke-testing).

**Our `EthSignerEnclave/Makefile` does not implement the SIM branch.** All linker invocations hardcode the HW variants (`-lsgx_urts`, `-lsgx_trts`, `-lsgx_tservice`, `-lsgx_uae_service`, `-lsgx_dcap_tvl`). The `SGX_MODE` variable affects only:

- the marker filename `.config_<Build_Mode>_<SGX_ARCH>` (purely cosmetic, used as a make dependency to detect mode changes between builds)
- in release builds (`SGX_DEBUG=0`), it gates `-Wl,-O2` linker optimisation flags

`SGX_MODE=SIM` would not produce a working sim-mode binary in our Makefile because the SIM library variants are never linked. If a developer needs to run our enclave on non-SGX hardware for testing, additional Makefile work is required to add the SIM branch. This is a known gap; it has not been a priority because we always have SGX-capable hardware available (Hetzner SGX1 + Azure DCsv3) for runtime testing.

### 2.3 The reproducibility constraint — committed Dockerfile only

For a build to produce a MRENCLAVE that other operators can reproduce, the toolchain must be byte-identical across builders. This is achieved by always building through `EthSignerEnclave/Dockerfile.azure`, which pins:

- Base image: `ubuntu:22.04`
- SGX SDK version: `2.28.100.1` (matching Azure runtime)
- libsgx-* package versions: `2.28.100.1-jammy1`
- Build steps invoked from a fixed working directory

The Dockerfile is committed to the private repo and is the authoritative build environment. Building outside Docker (`make` directly on a host system) is **not authoritative** — host-system gcc version, libc version, system library versions, and embedded build paths will differ between machines and produce a different MRENCLAVE.

**Anti-pattern:** "I cloned the repo and ran `make` on my workstation, and the MRENCLAVE doesn't match production. Something must be broken."

**Correct response:** the build was performed outside the committed Dockerfile, so the MRENCLAVE is expected to differ. To produce a production-matching MRENCLAVE, run `docker build -f EthSignerEnclave/Dockerfile.azure .` from a clean clone. To debug Dockerfile-internal builds, modify the Dockerfile and commit the change.

### 2.4 GHA build-gate

The build-gate runs as a GitHub Actions workflow on the standard `ubuntu-22.04` runner, in the private repo. It:

1. Checks out the source.
2. Provides a generated dev sign-key (random, ephemeral, sandbox-mode only — see §3.2). This is so the GHA build can complete `sgx_sign sign` without requiring access to the operator's production sign-key.
3. Runs `docker build -f EthSignerEnclave/Dockerfile.azure .` to produce `enclave.signed.so`.
4. Extracts the artefact from the container.
5. Runs `sgx_sign dump` (or equivalent SGX SDK tool) on `enclave.signed.so` to print the MRENCLAVE measurement to the workflow log.
6. Optionally archives `enclave.signed.so` as a workflow artefact for inspection.

What the GHA gate does **not** do:

- Run the enclave (`./app` or `perp-dex-server`) — GHA `ubuntu-22.04` is not SGX-capable, the enclave cannot be loaded, smoke-test is impossible at this layer.
- Verify DCAP attestation — same reason.
- Validate signature against operator production sign-key — the gate uses a throwaway dev key.

What the GHA gate **does** prove:

- The build is not broken (Dockerfile parses, dependencies resolve, source compiles, links, signs, produces an artefact).
- The MRENCLAVE for the current commit is a published value (in the workflow log, addressable by commit SHA).
- This MRENCLAVE can be cross-checked against an independent operator's local build for reproducibility verification (§5).

Runtime smoke-tests (load enclave, generate DCAP quote, run the orchestrator handshake) are deferred to the deployment stage, on actual SGX hardware (Azure DCsv3 / Hetzner SGX SKU).

---

## 3. Sign layer

### 3.1 What signing does

`sgx_sign sign` applies an operator-controlled RSA private key (`Enclave/Enclave_private.pem`) to the unsigned enclave binary, producing `enclave.signed.so`. The signature determines MRSIGNER (the SHA-256 of the public key derived from the private key); the enclave content determines MRENCLAVE (the SHA-256 of the enclave's pages and initial state).

**Critical fact:** MRENCLAVE is independent of the sign-key. Two operators with different sign-keys building the same source through the same Dockerfile produce the same MRENCLAVE. They produce different MRSIGNER values, but our policy is MRENCLAVE-sealing (see `feedback_read_our_docs_first.md`), so MRSIGNER is not used for sealed-state identity in this project.

### 3.2 Sign-key custody policy

This is policy-dependent and varies by operating mode (per `development-operating-model.md` §1):

| Operating mode | Sign-key policy | Rationale |
|---|---|---|
| `product-sandbox-single-operator` (current) | One operator (Andrey) holds the sign-key, signs all release builds | No multi-operator trust assumption to honour |
| `product-sandbox-multi-operator` (transitional) | Each operator independently signs their own deployment with their own sign-key; MRENCLAVE-match across all operators is the cross-check (sign-keys may differ) | MRENCLAVE-policy means signature is for runtime acceptance only, not identity |
| `production` (final) | ADR-required, deferred. Options: SGX 2-step sign with offline N-of-M ceremony; HSM-stored production key; or per-operator-signed enclaves with on-chain MRENCLAVE allowlist | Open question — see §3.3 |
| GHA build-gate (CI) | Throwaway random key generated per build; sandbox-mode only | CI cannot have access to operator production keys; acceptable because MRENCLAVE is what matters |

### 3.3 Open ADR — production sign-key custody

A separate architectural decision is required before production-mode is reached. The decision is **not** part of this document; this document only fixes the framing:

- The decision is independent of MRENCLAVE-reproducibility (MRENCLAVE is not affected by sign-key choice).
- The decision affects MRSIGNER, which is not used by our project policy — but a future audit may ask why we don't use it as an additional defence-in-depth, and the ADR must answer.
- The decision affects operational ceremony: signing in production must not concentrate authority in a single operator's filesystem.

This is captured as a foundation gap that production-mode unlock depends on (alongside Path A and reproducibility).

### 3.4 Where signing can happen

Signing requires only the sign-key file and the SGX SDK `sgx_sign` tool. Both are available on any x86_64 Linux. **Where the sign-key lives** is the operational question — usually on a hardened operator workstation, with the key never copied to the build host.

For our current sandbox-mode reality: the sign-key is on Andrey's controlled host; Hetzner build pipeline reads it from a mount path that is not committed to the repo. For GHA: the sign-key is generated fresh per build inside the workflow (no operator key exposure).

---

## 4. Runtime layer

### 4.1 SGX hardware level matrix

Three relevant levels:

| Level | Hardware example | Supports |
|---|---|---|
| No SGX | most VPS, ARM laptops, Intel CPUs without SGX feature | No SGX runtime at all |
| SGX1 | Hetzner EX line (Coffee Lake / Skylake era), older Intel Xeon | Enclave loading, MRENCLAVE-sealing, **Local Attestation**, EPID attestation (Intel-managed, deprecated for new deployments) |
| SGX2 + DCAP-certified CPU | Azure DCsv3 / DCsv2, IBM Cloud SGX, Equinix SGX | All of SGX1 + **DCAP attestation** + Flexible Launch Control + Dynamic Memory Management |

**DCAP availability is not the same as SGX2.** It additionally requires the CPU to be on Intel's "DCAP supported" Provisioning Certification list AND have provisioned Provisioning Certification Keys. Cloud providers handle this transparently for supported SKUs.

### 4.2 Hetzner SGX1 capability table

Because Hetzner is our current build host AND our orchestrator host, and the project has historically conflated these uses, this table fixes what Hetzner SGX1 can and cannot do:

| Operation | Hetzner SGX1 support | Notes |
|---|---|---|
| Build enclave | YES | works because of Docker toolchain, not because of SGX1 |
| Run enclave (`sgx_create_enclave`) | YES | SGX1 supports enclave loading |
| MRENCLAVE-sealing | YES | platform sealing primitives exist on SGX1 |
| **Local Attestation** (between two enclaves on same machine) | YES | does not require DCAP — uses platform report key; works on any SGX-capable host |
| Path A migration ceremony participant | YES | Path A uses Local Attestation, not DCAP — see REQ-7 §3 |
| EPID attestation | YES (deprecated) | Intel-managed; not used in this project |
| **DCAP attestation quote generation** | NO | requires SGX2 + Provisioning Certification |
| ECDH-over-DCAP cross-machine peer authentication | NO | depends on DCAP |
| Multi-operator FROST signing peer (production) | NO | requires DCAP for cross-machine peer attestation |
| Test-only multi-machine peering with attestation disabled | YES | only valid for development/testnet, not production |

The implication for Path A testing: a single Hetzner host can run an OLD enclave + a NEW enclave side-by-side and exercise the full Path A migration ceremony, because Local Attestation does not require DCAP. This makes Hetzner a useful single-machine Path A test environment, separate from the multi-machine Azure DCsv3 testnet cluster.

### 4.3 Production runtime requirements

For production-mode (per `development-operating-model.md` §1.3), every operator's runtime host must:

- Be SGX2 + DCAP-certified (Azure DCsv3 or equivalent).
- Have DCAP libraries and PCS-cache configuration validated (see `feedback_azure_dcap_findings.md` for known PCS endpoint issues).
- Be on the on-chain MRENCLAVE allowlist (per REQ-7 §3.4) for the MRENCLAVE it loads.
- Have its enclave loaded with `DisableDebug=1` per `Enclave.config.xml` (current `DisableDebug=0` is sandbox-only).
- Have its sign-key custody arrangement aligned with §3.3 ADR decision (still pending).

---

## 5. Reproducibility — N-of-M procedure

This is the foundation invariant introduced 2026-05-03 (per `feedback_reproducible_build_foundation.md`).

### 5.1 Rule

> **No production deployment with a MRENCLAVE that has not been reproducibly built by ≥N independent operators with bit-identical result.**

For our 2-of-3 cluster, N = 3 (all operators independently build). For our current sandbox-single-operator mode, N = 1 (the rule is suspended along with `multi-operator-architecture.md` §1 invariants 1–4 per `development-operating-model.md` §1.1).

### 5.2 Procedure for production builds

1. Each of N operators clones the private repo at a specific commit SHA on a clean working host.
2. Each runs `docker build -f EthSignerEnclave/Dockerfile.azure .` (no cache, no host-system contamination).
3. Each extracts `enclave.signed.so` from the resulting container and runs `sgx_sign dump -enclave enclave.signed.so` to obtain MRENCLAVE.
4. Each publishes a signed message containing `(commit_sha, MRENCLAVE, build_timestamp, operator_id)`. Signature is via the operator's XRPL operator key (sealed inside their enclave — but this is bootstrap so done off-enclave with a documented key).
5. The N messages are aggregated; if all MRENCLAVE values are identical → bit-identical reproducibility attested.
6. The agreed MRENCLAVE is added to the on-chain allowlist (per REQ-7) as the next-authorised MRENCLAVE_new.

If any MRENCLAVE diverges → **STOP**. This is an architectural bug per `feedback_workarounds_are_arch_bugs.md`, not a deployment issue. See §6.

### 5.3 GHA + Hetzner cross-check

The minimum reproducibility test we can run today (without a second human operator) is:

- GHA workflow builds in `ubuntu-22.04` runner → published MRENCLAVE in workflow log.
- Hetzner local `docker build` → MRENCLAVE captured in operator log.
- Compare. If identical, reproducibility is demonstrated for **N=2 build environments** (same repo, different hosts, same Docker image).

This is sufficient evidence for sandbox-multi-operator readiness (the procedure works) but does not satisfy production-mode N=3 (which needs three independent humans, not two environments controlled by one human).

---

## 6. When MRENCLAVE diverges between builds

This is a real and recurring problem in SGX reproducible builds. Common causes and remediations:

| Source of non-determinism | Remediation |
|---|---|
| Different Intel SGX SDK versions | Pin in Dockerfile (`libsgx-*=2.28.100.1-jammy1`) — already done |
| Different gcc / glibc versions on build host | Use Docker only; never `make` directly on host |
| Embedded build paths in debug info (`/build/...` vs `/home/andrey/...`) | Add `-fdebug-prefix-map=$(PWD)=/build` to CFLAGS; or strip debug info from release builds |
| Embedded build timestamps in object files | Set `SOURCE_DATE_EPOCH=0` env in build script |
| Random stack canaries in object files | Usually consistent across SDK versions; investigate if observed |
| Symbol table ordering depending on filesystem ordering | Add `find ... | sort` for source file enumeration; or `--sort-section=name` linker flag |
| Docker BuildKit cache mounts retaining timestamps | Use `--no-cache` for release builds; or fix BuildKit reproducibility flags |
| Differing `Enclave.config.xml` (e.g. local dev edits) | Ensure `Enclave.config.xml` is committed; CI uses committed version only |

### 6.1 Diagnostic procedure when divergence is observed

1. **Collect both `enclave.signed.so` artefacts** from the diverging builds.
2. Run `objcopy --strip-all` on both to remove debug info; compare. If MRENCLAVE now matches → divergence is in debug info embedding (apply `-fdebug-prefix-map` fix).
3. If still differs: run `diffoscope enclave_a.signed.so enclave_b.signed.so` to identify byte-level differences.
4. If the difference is in section ordering: linker / `find` ordering issue (apply sort fix).
5. If the difference is in section content: SDK / library / source-version drift (compare Dockerfile pinned versions; check if any `apt-get install` is missing version pin).

### 6.2 What NOT to do when divergence persists

- **Do not** accept "one operator's build is canonical" — this nullifies the multi-operator security model.
- **Do not** add a per-operator MRENCLAVE allowlist — this turns reproducibility from a binary check into a growing compatibility surface.
- **Do not** disable the MRENCLAVE check in some "trust DCAP only" mode — DCAP only proves "this enclave is running on SGX hardware right now", not "this enclave is what we audited."
- **Do not** ship to production with unresolved divergence — defer until the root cause is fixed in the Dockerfile.

---

## 7. Anti-patterns

The following are forbidden in production-mode (and discouraged in sandbox modes):

1. **`make` directly on host system** — produces a non-canonical MRENCLAVE. Use `docker build -f Dockerfile.azure` only.
2. **Build with `SGX_MODE=SIM`** — does not work in our Makefile and would produce a non-loadable artefact even if it did. SIM is a known gap; not a workaround.
3. **Share the operator's production sign-key with CI or other operators** — sign-keys are operational secrets. CI uses throwaway dev keys.
4. **Skip the GHA build-gate when shipping to mainnet** — production deployments must trace back to a GHA-built MRENCLAVE that was reproducibly verified.
5. **Run the enclave on non-SGX hardware** — even for testing, this is meaningless: the enclave cannot load, sealed state cannot be unsealed, attestation does not produce a quote.
6. **Run the enclave on SGX1 in production-mode** — DCAP is required for cross-machine attestation. SGX1 is acceptable for build host and single-machine Path A testing; not for production multi-operator runtime.
7. **Build for production without `DisableDebug=1`** — current sandbox `Enclave.config.xml` has `DisableDebug=0`, which loads only on debug-enabled hosts. Production must flip this and re-attest the MRENCLAVE.

---

## 8. References

- [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.en.md) §1 invariants 5 (upgrade-path-foundation), 6 (Pattern 318), 7 (reproducibility — to be added in this document's wake)
- [`development-operating-model.{en,ru}.md`](development-operating-model.en.md) §1 (operating modes), §3 (Mode S sync — currently postponed pending Path A + reproducibility)
- [`testnet-enclave-bump-procedure.{en,ru}.md`](testnet-enclave-bump-procedure.en.md) — concrete operator runbook for testnet today (will be updated post-Path-A to use migration ceremony)
- `docs/audit/REQ-7.md` (private repo) — Path A spec including on-chain MRENCLAVE allowlist mechanism that consumes outputs of §5 procedure
- `feedback_reproducible_build_foundation.md` (memory) — foundation rule
- `feedback_upgrade_path_is_foundation.md` (memory) — peer foundation rule (Path A)
- `feedback_read_our_docs_first.md` (memory) — MRENCLAVE-sealing policy (rejecting MRSIGNER-sealing permanently)
- `feedback_azure_dcap_findings.md` (memory) — Azure DCAP runtime quirks
- `EthSignerEnclave/Dockerfile.azure` (private repo) — authoritative build environment
- `EthSignerEnclave/Makefile` (private repo) — build orchestration; SIM branch is a known gap
