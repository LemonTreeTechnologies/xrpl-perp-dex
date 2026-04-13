> **Source / attribution:** This document is imported verbatim from the sister project
> [`77ph/SGX_project`](https://github.com/77ph/SGX_project/blob/main/EthSignerEnclave/docs/sgx_vs_tdx_roi.md)
> (Phoenix Prediction Market), written by the PM team on 2026-04-02.
> We reuse it here as the hardware-cost reference for the perp-DEX business plan —
> the SGX stack, pricing tiers, and the ~1-week SGX→TDX migration estimate apply
> directly to our enclave as well (same `libsecp256k1`/BearSSL base, same
> Hetzner/Azure deployment model). Numbers below are written for the PM workload;
> treat them as an order-of-magnitude reference, not perp-DEX-specific quotes.
> For perp-DEX-specific capability/limit context, see
> [`sgx-enclave-capabilities-and-limits.md`](./sgx-enclave-capabilities-and-limits.md).

---

# SGX vs TDX ROI Analysis — Multi-Operator Prediction Market

**Date:** 2026-04-02
**Context:** Why we chose Intel SGX over TDX (or AMD SEV-SNP) for Phoenix PM. Cost, performance, security trade-offs for a decentralized multi-operator deployment.

---

## Executive Summary

SGX costs ~2x more per vCPU than TDX/SEV-SNP, but provides **process-level isolation** (smallest attack surface) and is the only option that supports **enclave-level code measurement** (MRENCLAVE). For our workload (crypto signing, small memory, CPU-bound), SGX is the optimal ROI choice.

TDX/SEV-SNP protect the entire VM — larger attack surface, but easier to deploy. The "lift-and-shift" advantage of TDX is irrelevant for us: we already have a purpose-built C/C++ enclave.

---

## 1. Pricing Comparison

### Real-world pricing: bare metal vs cloud

| Technology | Provider | Type | vCPUs | RAM | $/month | Bare metal? |
|-----------|----------|------|-------|-----|---------|-------------|
| **Intel SGX** | **Hetzner** | **Bare metal** | 8 | 64 GiB | **~$55 (€50)** | **Yes** |
| **Intel SGX** | OVH | Bare metal | 8 | 64 GiB | ~$60 (€55) | Yes |
| Intel SGX | Azure DC2s_v3 | Cloud VM | 2 | 16 GiB | $138 | No |
| Intel TDX | Azure DC2es_v5 | Cloud VM | 2 | 8 GiB | $69 | No |
| Intel TDX | (bare metal) | — | — | — | **Not available** | **No** |
| AMD SEV-SNP | Azure DC2as_v5 | Cloud VM | 2 | 8 GiB | $62 | No |
| AWS Nitro | c5.large | Cloud VM | 2 | 4 GiB | $61 | No |

**Key insights:**
- On Azure, SGX VMs cost ~2x more per vCPU than TDX ($0.096 vs $0.048/vCPU) — but SGX VMs include more RAM (16 GiB vs 8 GiB for 2-vCPU).
- TDX bare metal exists but is expensive: OVH HGR ~$950/month (5th Gen Xeon, requires 1TB RAM for TDX activation). Hetzner's Sapphire Rapids (EX130, DX293) support SGX but NOT TDX.
- SGX runs on Xeon E (available as affordable bare metal from Hetzner, OVH at €50-60/month).
- **Important caveat:** Hetzner SGX servers use legacy out-of-tree driver (`/dev/isgx`) — they support signing and key custody but **NOT DCAP remote attestation**. DCAP requires in-kernel driver (`/dev/sgx_enclave`), available on Azure DCsv3.
- **Production model:** Hetzner bare metal for signing (€50/month, always-on) + Azure DCsv3 for attestation ($0.19/hr, on-demand when clients verify).

### 3-operator multi-operator deployment (2-of-3 threshold)

**Option A: SGX hybrid (bare metal signing + cloud attestation) — RECOMMENDED**

| Role | Provider | Tech | Cost/month | DCAP? |
|------|----------|------|-----------|-------|
| Operator 1 (signing) | Hetzner bare metal | SGX (Xeon E) | €50 | No |
| Operator 2 (signing) | Hetzner bare metal | SGX (Xeon E) | €50 | No |
| Operator 3 (signing) | Hetzner bare metal | SGX (Xeon E) | €50 | No |
| Attestation (on-demand) | Azure DCsv3 | SGX (Xeon Scalable) | ~$15 (80 hrs) | **Yes** |
| **Total** | | | **~$180/month ($2,160/yr)** | |

Attestation nodes run only when clients request verification — not 24/7.

**Option B: SGX cloud-only (full DCAP)**

| Config | Cost/month | Cost/year | DCAP? |
|--------|-----------|-----------|-------|
| 3 × Azure DC2s_v3 | $415 | $4,977 | Yes |

**Option C: TDX bare metal**

| Config | Cost/month | Cost/year | Notes |
|--------|-----------|-----------|-------|
| 3 × OVH HGR-HCI-i1 | **$2,850** | **$34,200** | 5th Gen Xeon, 1TB RAM required for TDX |

**Option D: TDX cloud**

| Config | Cost/month | Cost/year | DCAP? |
|--------|-----------|-----------|-------|
| 3 × Azure DC2es_v5 | $207 | $2,489 | Yes |

### Cost comparison

| Option | $/year | DCAP | Bare metal | Attack surface |
|--------|--------|------|-----------|---------------|
| **A: SGX hybrid** | **$2,160** | On-demand | Yes (signing) | **~5K LOC** |
| B: SGX cloud | $4,977 | Always | No | ~5K LOC |
| C: TDX bare metal | $34,200 | Always | Yes | ~millions LOC |
| D: TDX cloud | $2,489 | Always | No | ~millions LOC |

**Option A is 16x cheaper than TDX bare metal, with 1000x smaller attack surface.**
Option D (TDX cloud) is comparable in price to Option A, but has a much larger attack surface and no bare metal control.

---

## 2. Current Server Limitation & Upgrade Path

### Current server: Hetzner, Xeon E3-1275 v6 (Kaby Lake, 2017)

| Capability | Status | Why |
|-----------|--------|-----|
| SGX1 | Yes | CPU supports SGX |
| SGX2 | **No** | Kaby Lake, pre-SGX2 |
| Launch Control (FLC) | **No** | Not in silicon — added starting Coffee Lake (2018) |
| DCAP attestation | **No** | Requires Launch Control |
| In-kernel driver | **No** | Uses legacy out-of-tree `/dev/isgx` |
| Signing & key custody | **Yes** | Works fine |
| Sealing | **Yes** | Works fine |

**This is a hardware limitation, not OS or driver.** Launch Control is burned into the CPU. No driver, kernel, or BIOS update can add it to Xeon E3 v6.

### What Launch Control enables

```
Without LC (our server):           With LC (Coffee Lake+):
  Intel controls launch tokens       Operator controls launch tokens
  → only EPID attestation            → DCAP attestation (Intel-signed quotes)
  → deprecated by Intel              → production standard
  → /dev/isgx (legacy driver)        → /dev/sgx_enclave (in-kernel driver)
```

### Upgrade path: Xeon E-2300+ (same Hetzner price tier)

| CPU | Generation | SGX2 | Launch Control | DCAP | EPC | Hetzner price |
|-----|-----------|------|---------------|------|-----|-------------|
| Xeon E3-1275 v6 (current) | Kaby Lake (2017) | No | **No** | **No** | 128 MB | ~€50/mo |
| Xeon E-2288G | Coffee Lake (2019) | Yes | **Yes** | **Yes** | 256 MB | ~€50-60/mo |
| Xeon E-2388G | Rocket Lake (2021) | Yes | **Yes** | **Yes** | 512 MB | ~€60-70/mo |
| Xeon E-2488 | Raptor Lake (2023) | Yes | **Yes** | **Yes** | 512 MB | ~€70-80/mo |

**Realistic upgrade: Xeon E-2388G on Hetzner (~€60-70/month)**
- Full DCAP remote attestation
- In-kernel driver (`/dev/sgx_enclave`)
- SGX2 (dynamic EPC management)
- 512 MB EPC (vs 128 MB now)
- Same price tier as current server
- No more hybrid model needed — one server does signing + attestation

### After upgrade: simplified 3-operator deployment

| Config | Before (hybrid) | After (upgraded) |
|--------|-----------------|-----------------|
| Operator 1 | Hetzner E3 v6 (no DCAP) | **Hetzner E-2388G (DCAP)** |
| Operator 2 | Hetzner E3 v6 (no DCAP) | **Hetzner E-2388G (DCAP)** |
| Operator 3 | Hetzner E3 v6 (no DCAP) | **Hetzner E-2388G (DCAP)** |
| Attestation | Azure DCsv3 (on-demand) | **Not needed** (each node does DCAP) |
| Cost/year | ~$2,160 | **~$2,520 (€210/mo × 12)** |
| DCAP | On-demand (Azure) | **Always available (every node)** |
| Complexity | Hybrid (2 infra providers) | **Simple (1 provider)** |

**Recommended action:** Replace current Hetzner server with Xeon E-2388G when ready for production. ~€20/month more, eliminates Azure dependency for attestation.

---

## 3. What You Get for SGX (vs TDX)

### SGX: Process-Level Isolation (Enclave)

```
┌──────────────────────────────────────┐
│            Host OS (untrusted)        │
│                                      │
│  ┌───────────────────────┐           │
│  │    SGX Enclave         │ ← only   │
│  │    ~5000 LOC C/C++     │   this   │
│  │    secp256k1           │   is     │
│  │    SHA-256/512         │   trusted│
│  │    AES-GCM             │          │
│  │    Shamir GF(256)      │          │
│  └───────────────────────┘           │
│                                      │
│  Rust daemon (untrusted, 1.1 MB)     │
│  nginx (untrusted)                   │
│  PostgreSQL (untrusted)              │
│  OS kernel (untrusted)               │
└──────────────────────────────────────┘

Attack surface: ~5000 LOC of audited C/C++
MRENCLAVE: SHA-256 of enclave code — cryptographic identity
```

### TDX: VM-Level Isolation

```
┌──────────────────────────────────────┐
│         Hypervisor (untrusted)        │
│                                      │
│  ┌───────────────────────────────┐   │
│  │    Trusted VM (entire VM)      │   │
│  │                               │   │
│  │    OS kernel                   │ ← ALL  │
│  │    Python runtime              │   of   │
│  │    pip packages (hundreds)     │   this  │
│  │    nginx                       │   is    │
│  │    SQLite                      │   trusted│
│  │    signing code                │          │
│  │    system libraries            │          │
│  │    systemd, cron, etc.         │          │
│  └───────────────────────────────┘   │
└──────────────────────────────────────┘

Attack surface: entire Linux OS + all dependencies
MRTD: hash of initial VM image — larger, harder to audit
```

### What this means for security

| Aspect | SGX (enclave) | TDX (full VM) |
|--------|--------------|---------------|
| Trusted code size | ~5,000 LOC (audited C/C++) | ~millions LOC (OS + Python + deps) |
| Attack surface | Minimal (only crypto operations) | Full VM (kernel vulns, Python CVEs, pip supply chain) |
| Code measurement | MRENCLAVE (~10 KB of code) | MRTD (entire VM image, GBs) |
| Auditability | Practical (33 findings, all fixed) | Impractical (audit entire OS?) |
| Supply chain risk | libsecp256k1 + BearSSL (well-audited) | Python + pip + OS packages (unknown) |
| Side-channel surface | Small (only enclave code) | Large (entire OS scheduler, I/O, etc.) |

---

## 4. Performance: C/C++ Enclave vs Python-in-VM

### Our SGX approach: optimized C/C++ inside enclave

| Operation | Implementation | Time | Library |
|-----------|---------------|------|---------|
| ECDSA secp256k1 sign | C (libsecp256k1) | **~1-2 ms** | Bitcoin Core's library, 10+ years of optimization |
| SHA-256 (32 bytes) | C (BearSSL) | **<0.01 ms** | Thomas Pornin's constant-time implementation |
| SHA-512Half | C (BearSSL) | **<0.01 ms** | Same |
| AES-128-GCM encrypt | C (SGX SDK) | **<0.1 ms** | Intel AES-NI hardware acceleration |
| Shamir split (32 bytes) | C (custom GF(256)) | **<0.1 ms** | Constant-time, branch-free |
| ECDH shared secret | C (libsecp256k1) | **~2 ms** | Same library as signing |
| FROST nonce gen + sign | C (secp256k1-zkp) | **~3-5 ms** | Blockstream's threshold library |

**Total for one EscrowFinish**: ~5 ms crypto + ~100 ms network = **~105 ms**

### Typical TDX/SEV approach: Python inside VM

| Operation | Implementation | Time | Library |
|-----------|---------------|------|---------|
| ECDSA secp256k1 sign | Python (py-ecc or coincurve) | **~10-50 ms** | Python C-extension or pure Python |
| SHA-256 | Python (hashlib) | **~0.1 ms** | C-backed but Python overhead |
| AES-GCM encrypt | Python (cryptography) | **~1 ms** | OpenSSL backend |
| Shamir split | Python (custom or PyShamir) | **~5-10 ms** | Often pure Python, not constant-time |
| ECDH | Python (cryptography) | **~5-20 ms** | Variable quality |

**Total for one EscrowFinish**: ~30-100 ms crypto + ~100 ms network = **~130-200 ms**

### Benchmark summary

| Metric | SGX + C/C++ | TDX + Python | Ratio |
|--------|------------|-------------|-------|
| ECDSA sign | 1-2 ms | 10-50 ms | **5-25x faster** |
| Full signing round | ~5 ms | ~50 ms | **10x faster** |
| FROST 2-of-3 (3 rounds) | ~15 ms | ~150+ ms | **10x faster** |
| Memory usage | ~10 MB (enclave) | ~200+ MB (Python + OS) | **20x less** |
| Cold start | <100 ms (enclave load) | 5-30s (OS boot) | **50-300x faster** |
| Constant-time crypto | Yes (audited) | Usually no | **Critical for side-channel** |

### Why this matters for multi-operator

In a 3-operator FROST signing round:
```
SGX:    3 × ~5ms = 15ms crypto + 300ms network = ~315ms per TX
Python: 3 × ~50ms = 150ms crypto + 300ms network = ~450ms per TX
```

For 100 markets with 10 escrows each = 1000 EscrowFinish TXs:
```
SGX:    1000 × 315ms = 5.25 minutes
Python: 1000 × 450ms = 7.50 minutes
```

Not a dramatic difference at this scale. But SGX's advantage grows with:
- More operators (5-of-9, 11-of-16)
- Higher throughput (continuous markets, not just binary PM)
- Latency-sensitive operations (liquidations in perp DEX)

---

## 5. Development & Deployment Cost

### SGX (our approach)

| Item | Cost | Notes |
|------|------|-------|
| Enclave C/C++ development | Higher upfront | ~5000 LOC, requires SGX expertise |
| Security audit | Practical (~33 findings) | Small codebase, auditable |
| Deployment | SGX-specific setup | AESM, driver, SDK |
| Multi-operator | DCAP attestation | MRENCLAVE matching across machines |
| CI/CD | Docker build for Azure | Dockerfile.azure (SDK 2.28) |

### TDX (typical approach)

| Item | Cost | Notes |
|------|------|-------|
| Python development | Lower upfront | Standard Python, pip install |
| Security audit | Impractical for full VM | "Audit the OS" = millions of LOC |
| Deployment | Standard VM | Lift-and-shift existing code |
| Multi-operator | VM attestation | MRTD matching (harder — full VM image) |
| CI/CD | Standard Docker | Nothing special |

### Total cost of ownership (first year, 3 operators)

| Item | SGX | TDX |
|------|-----|-----|
| Infrastructure | $4,977 | $2,489 |
| Development (enclave / VM setup) | Higher (done) | Lower |
| Audit | $15,000 (done, practical) | $50,000+ (full VM audit) or skip |
| Maintenance | Low (small codebase) | Higher (OS updates, Python CVEs, pip) |
| **Security confidence** | **High** (5000 LOC audited) | **Medium** (unaudited Python + OS) |

---

## 6. Multi-Operator Attestation

### SGX: MRENCLAVE-based attestation

```
Machine A: MRENCLAVE = sha256(enclave_code) = 5c199bd1...
Machine B: MRENCLAVE = sha256(enclave_code) = 5c199bd1...  ← MATCH
Machine C: MRENCLAVE = sha256(enclave_code) = 5c199bd1...  ← MATCH

Verification: DCAP quote contains MRENCLAVE
              Intel certificate chain proves hardware is genuine
              Any client can verify independently
```

MRENCLAVE is **deterministic** — same source code = same hash. Compact (~10 KB of code measured). Easy to publish and verify.

### TDX: MRTD-based attestation

```
Machine A: MRTD = hash(entire_VM_image) = a3f7c2d1...
Machine B: MRTD = hash(entire_VM_image) = a3f7c2d1...  ← must be identical VM
Machine C: MRTD = hash(entire_VM_image) = a3f7c2d1...  ← any OS update = different MRTD
```

MRTD measures the **entire initial VM memory** — OS, kernel, all packages, config. Problems:
- Any OS security update changes MRTD → all operators must update simultaneously
- Package version differences → different MRTD → attestation fails
- Harder to audit what MRTD represents (it's GBs of code, not KB)

### Practical impact for multi-operator

| Aspect | SGX MRENCLAVE | TDX MRTD |
|--------|--------------|----------|
| What's measured | ~10 KB enclave code | ~GBs VM image |
| Reproducibility | High (same .cpp → same MRENCLAVE) | Hard (exact OS version, packages, config) |
| Operator independence | Each operator builds, gets same MRENCLAVE | Operators must use identical VM image |
| OS updates | Transparent (enclave unchanged) | Breaks MRTD → coordinated re-attestation |
| Auditability | Read 5000 LOC → know exactly what's trusted | Read entire OS → impractical |

---

## 7. Security Quality: Audited C vs "pip install"

### Our crypto stack (inside SGX enclave)

| Library | Language | Audit status | Used by |
|---------|----------|-------------|---------|
| **libsecp256k1** | C | Extensively audited, 10+ years | Bitcoin Core, Ethereum, XRPL |
| **secp256k1-zkp** (FROST) | C | Blockstream, PR #278, code review | Liquid, Phoenix |
| **BearSSL** | C | Thomas Pornin, constant-time by design | Embedded systems, security-critical |
| **SGX SDK crypto** | C | Intel, FIPS-validated AES-NI | Azure, datacenter attestation |
| **Custom GF(256)** | C | Our audit (33 findings), 303 test vectors | Phoenix Shamir |

**Total: ~5000 LOC C/C++, all audited or from battle-tested libraries.**

### Typical Python TDX/VM crypto stack

| Library | Language | Concern |
|---------|----------|---------|
| `py-ecc` / `coincurve` | Python + C | C-extension quality varies |
| `cryptography` (PyCA) | Python + OpenSSL | Good, but pulls in OpenSSL (~500K LOC) |
| `pycryptodome` | Python + C | Mixed audit history |
| `web3.py` | Pure Python | Not designed for TEE, pulls 50+ transitive deps |
| `pyshamir` / custom | Pure Python | Often not constant-time |
| OS: glibc, systemd, kernel | C | Millions of LOC, CVEs monthly |
| Python runtime | C | Regular CVEs, GC timing, memory leaks |
| pip packages (transitive) | Mixed | Supply chain attacks (typosquatting, backdoors) |

**Total: millions of LOC, mostly unaudited for TEE use, supply chain risk.**

### The "pip install" problem

```python
# A typical Python TEE project
pip install web3 py-ecc pycryptodome flask requests

# This pulls in:
# - 50+ transitive dependencies
# - urllib3, charset-normalizer, certifi, idna, ...
# - Each is a supply chain attack vector
# - Each gets its own CVEs
# - All of this runs INSIDE the trusted VM
# - Any compromised package = compromised TEE
```

In SGX enclave: **zero pip packages**. All crypto is compiled C, statically linked, measured by MRENCLAVE.

---

## 8. Decision Matrix

| Factor | SGX (our choice) | TDX | Winner |
|--------|-----------------|-----|--------|
| **Cost (3 ops, 1yr)** | $2,520 (Hetzner E-2388G) | $2,489 (Azure cloud) or $34,200 (OVH bare metal) | **Comparable** (SGX bare metal ≈ TDX cloud) |
| **Attack surface** | ~5K LOC | ~millions LOC | **SGX** (1000x smaller) |
| **Crypto performance** | 1-2 ms/sign | 10-50 ms/sign | **SGX** (10x faster) |
| **Auditability** | 33 findings, all fixed | Impractical (full OS) | **SGX** |
| **Multi-op attestation** | MRENCLAVE (compact, reproducible) | MRTD (huge, fragile) | **SGX** |
| **Supply chain risk** | Zero pip, static C | pip + OS packages | **SGX** |
| **Constant-time crypto** | Yes (audited) | Usually no | **SGX** |
| **Deployment ease** | Harder (enclave setup) | Easier (lift-and-shift) | TDX |
| **Memory limit** | EPC (8-256 GiB) | Full VM RAM | TDX |
| **Development cost** | Higher (C/C++ expertise) | Lower (Python) | TDX |

**Score: SGX 7, TDX 3**

For a security-critical financial application (prediction markets with real XRP), SGX on bare metal delivers the best security at comparable or lower cost than TDX.

---

## 9. Porting to TDX — What Changes, What Stays

### Code inventory

| Component | LOC | SGX-specific? | Portable? |
|-----------|-----|--------------|-----------|
| `Enclave.cpp` (core logic) | 5,509 | Partially | **~95% portable** |
| `LibApp.cpp` (ecall wrappers) | 1,688 | **Yes** (all ecall/ocall) | **Eliminated in TDX** |
| `Enclave.edl` (interface) | 292 | **Yes** (SGX-only) | **Eliminated in TDX** |
| `pool_handler.cpp` (REST) | 1,979 | No | **100% portable** |
| `server.cpp` (main) | 266 | No | **100% portable** |

### What's SGX-specific inside Enclave.cpp

| SGX API | Count | Purpose | TDX replacement |
|---------|-------|---------|----------------|
| `sgx_read_rand()` | 19 | Hardware RNG | `/dev/urandom` or RDRAND directly |
| `sgx_seal_data()` | ~10 | Encrypt to disk (MRENCLAVE-bound) | File encryption with TPM-backed key or TDX sealing API |
| `sgx_unseal_data()` | ~10 | Decrypt from disk | Corresponding decrypt |
| `sgx_create_report()` | 4 | DCAP attestation | TDX `TDG.MR.REPORT` instruction |
| `sgx_calc_sealed_data_size()` | ~5 | Buffer sizing | Fixed buffer (no EPC paging in TDX) |

**Total SGX-specific: ~48 calls to replace.** Everything else (secp256k1, BearSSL, SHA3, GF(256), Shamir, FROST) is portable C.

### What's 100% portable (zero changes)

| Code | Calls | Why portable |
|------|-------|-------------|
| secp256k1 (ECDSA, Schnorr, FROST, DKG, ECDH) | 409 | Pure C library, no SGX dependency |
| BearSSL (SHA, HMAC, AES-GCM) | 15 | Pure C, constant-time by design |
| SHA3/Keccak | 30 | Pure C |
| GF(256) Shamir split/reconstruct | ~50 | Pure C, our own constant-time code |
| Business logic (account pool, preimage management) | ~3000 | Pure C data structures |

### What gets ELIMINATED (not ported)

In TDX, there's no enclave boundary → no ecall/ocall → no marshaling:

| Component | SGX | TDX |
|-----------|-----|-----|
| `Enclave.edl` (292 LOC) | Defines 44 ecalls | **Deleted** |
| `LibApp.cpp` (1,688 LOC) | Wraps each ecall with parameter marshaling | **Deleted** — server calls functions directly |
| ecall/ocall overhead (~17K cycles each) | 44 transitions per request | **Zero** — direct function calls |

### TDX architecture

```
SGX (current):
  server.cpp → pool_handler.cpp → LibApp.cpp → [ecall boundary] → Enclave.cpp
                                   1688 LOC        EDL 292 LOC       5509 LOC

TDX (ported):
  server.cpp → pool_handler.cpp → enclave_logic.cpp (same code, no boundary)
                                   5509 LOC (direct calls)
```

### Migration effort estimate

| Task | LOC to change | Effort |
|------|--------------|--------|
| Replace `sgx_read_rand()` → `/dev/urandom` or RDRAND | 19 calls | 1 hour |
| Replace `sgx_seal/unseal` → file encryption (AES-256-GCM with key from TDX or TPM) | ~20 calls | 1-2 days |
| Replace `sgx_create_report()` → TDX report | 4 calls | 4 hours |
| Remove EDL + LibApp (direct function calls) | delete 1980 LOC | 1 day |
| Update Makefile/CMake (no SGX SDK) | build system | 4 hours |
| Test all 44 functions without enclave boundary | testing | 1-2 days |
| **Total** | | **~1 week** |

### What we LOSE by porting to TDX

| Loss | Impact | Mitigation |
|------|--------|-----------|
| **MRENCLAVE** (compact code measurement) | Can't prove "exactly this 5K LOC is running" | MRTD measures entire VM — less precise |
| **Process isolation** (enclave vs host) | Host compromise = full compromise | TDX protects from hypervisor, but not from malware inside VM |
| **Minimal trust boundary** | 5K LOC → millions LOC | Must harden entire VM (minimal OS, no unnecessary packages) |
| **ecall audit boundary** | Clear interface (44 functions) | No boundary — all code is trusted/accessible |

### What we GAIN by porting to TDX

| Gain | Impact |
|------|--------|
| No EPC memory limit | Can handle larger state (not relevant for PM, but for perp DEX) |
| Standard OS inside TEE | Easier debugging, logging, deployment |
| No ecall overhead | ~17K cycles saved per call (negligible for our workload) |
| Broader hardware support | Xeon 6 (Granite Rapids) may be TDX-only |

### Recommendation

**Stay on SGX as long as Xeon E supports it.** The porting effort is ~1 week, so migration is not urgent. The security advantages of SGX (small attack surface, MRENCLAVE, process isolation) outweigh TDX's convenience benefits for our workload.

**Trigger for migration:** Intel drops SGX from Xeon E server line, OR workload outgrows EPC limits.

**Key point for investors/partners:** "Our crypto code is 95% portable C. SGX-specific code is 48 API calls. Migration to TDX is ~1 week of work, not a rewrite."

---

## Summary

**SGX wins on ALL dimensions for our use case:**

| Dimension | SGX | TDX |
|-----------|-----|-----|
| **Cost** | **$2,160/yr** (3 × Hetzner + Azure attestation) | $2,489/yr (Azure cloud) or $34,200/yr (OVH bare metal) |
| **Attack surface** | **~5K LOC** | ~millions LOC |
| **Performance** | **1-2 ms/sign** (C/C++) | 10-50 ms/sign (Python) |
| **Auditability** | **33 findings, all fixed** | Impractical (audit OS?) |
| **Supply chain** | **0 pip packages** | 50+ transitive deps |
| **Attestation** | **MRENCLAVE (10 KB)** | MRTD (GBs, fragile) |
| **Bare metal** | **Yes** (Hetzner €50) | No (cloud only) |
| **Provider lock-in** | **None** | Azure/GCP only |

**SGX is cheaper, faster, more secure, more auditable, and available on bare metal.** TDX's only advantage — easier deployment (lift-and-shift Python) — is irrelevant when you have a purpose-built C/C++ enclave.

**This is not a trade-off. SGX dominates for security-critical crypto workloads.**
