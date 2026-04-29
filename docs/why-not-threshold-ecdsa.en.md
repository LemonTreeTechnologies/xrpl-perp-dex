# ADR — Why we use SGX-FROST for cluster signing, not threshold ECDSA

**Status:** Accepted (matches code in production on testnet, 2026-04-29).
**Audience:** future auditors, investors, technical reviewers asking "why not MPC/TSS instead of SGX?", future maintainers considering replacing the in-enclave signing path.
**Companions:** `docs/cluster-trust-model-decision.md` (DCAP cross-attestation choice), `docs/sgx-enclave-capabilities-and-limits.md` (SGX trust assumptions), `docs/multi-operator-architecture.md` (overall trust model).

## 1. Decision

We use two separate threshold mechanisms in this system; neither is MPC threshold-ECDSA:

1. **XRPL native multisig** for the on-chain escrow account. Each operator's enclave holds its own per-operator ECDSA (secp256k1) key; each operator signs the same unsigned XRPL transaction independently with its own key; the resulting `Signers[]` array is submitted via `submit_multisigned`. The on-chain `SignerQuorum` enforces the `K-of-N` threshold. There is no multi-party computation between the signers — each signature is a complete ECDSA signature under one independent key.

2. **FROST inside SGX** (Flexible Round-Optimized Schnorr Threshold) for any cluster-level Schnorr signature the system needs. Three enclaves run a Pedersen DKG; each enclave seals one share; together they can produce one BIP340-style group signature. The group pubkey is exposed once via `frost_group_id`; share material never leaves any enclave in plaintext. The cross-machine share transport (Path A v2) is ECDH+AES-GCM keyed on a DCAP-attested ECDH identity per `docs/cluster-trust-model-decision.md`.

We did **not** adopt threshold ECDSA (CGGMP, DKLS, or related MPC families) for either layer.

## 2. Context — what threshold-ECDSA-MPC would replace

The architectural alternative would be: replace the SGX enclave with a threshold-ECDSA MPC protocol (CGGMP or DKLS), so the same two security properties (no single party holds the key; no single party can sign) come from MPC math instead of SGX hardware. The April 2026 Sber technical talk "Threshold ECDSA технические аспекты: глубина кроличьей норы или на сколько верна гипотеза Эскобара?" (`https://www.youtube.com/watch?v=ZqHfLjlJBww`, slides in `references/MPC-TSS.pdf`) walks through exactly that protocol family. We summarise our reading below.

### 2.1 Schnorr threshold is trivial

For Schnorr (and GOST), a partial signature is `sig_i = k_i + λ_i · s_i · e`. The aggregate is `Σ sig_i`. Two rounds of communication, no zero-knowledge proofs, batch verification works. This is what FROST does.

### 2.2 ECDSA threshold is not

The ECDSA signature is `(e + r_x · s) / r`. Building it from secret shares requires multiplying two secrets (`s · b` and `r · b`) without revealing either. Multiplication of secret shares without revealing them is the hard problem the talk calls "the rabbit hole." Two protocol families address it:

- **CGGMP** uses Paillier homomorphic encryption plus a stack of zero-knowledge proofs per signature: range, discrete-log knowledge, correctly-chosen primes, correct encryption, discrete-log knowledge again — five distinct ZKs in the version reviewed in the talk, with the count growing as new attacks land patches. The 2018 baseline (GG18) was patched in 2019 (range proofs), patched again in 2021 (CMP — prime selection), and in 2024 a Black Hat presentation showed that an audit assumption ("an attacker can recover at most 1 bit of the key") composes into full-key recovery via 256-fold repetition exploiting modulus games between `Z_q`, `Z_N`, `Z_{N²}`. Paillier-side proofs are now patches-on-patches; the talk's assessment is "we don't know whether it's secure; intuition says it's hard to prove security in these conditions."
- **DKLS** uses a five-layer protocol stack: RVOLE → OT-extension (e.g. SILENT-OT or SoftSpoken-OT) → SubfieldVOLE → MPFSS → DPF. Each layer composes cleanly under the UC framework, which is its strength compared to CGGMP. The cost: understanding the stack requires reading 3–4 dense papers in sequence; per the talk's audience commentary, the KOS protocol (one of the OT-extension candidates) had a 2025 paper showing its security-theorem proof was incomplete and required additional construction to be CD-secure. The audit surface is much larger than the protocol description suggests.

### 2.3 The talk's verdict on both

> «Не известно [как обосновать безопасность], интуиция подсказывает, что сложно в таких условиях обосновать.»
> ("It is not known [how to prove security]; intuition says it is hard to prove security in such conditions.")

The speaker's working position is that threshold ECDSA is currently a research-grade construction, not a ship-ready primitive in the same way Schnorr threshold is.

## 3. Why this leads us to SGX-FROST, not threshold-ECDSA

Three reasons, in priority order.

### 3.1 SGX gives us the trust anchor that makes Schnorr threshold practical

The job of the threshold signing layer is to ensure no single party can forge a signature. SGX gives that property by sealing the share into an enclave the operator cannot peek into. Once the enclave provides the sealed-share property, the threshold signing protocol on top of it can be the simplest available — Schnorr threshold (FROST) — because the security argument no longer rests on the threshold math alone. The enclave handles the "no single party reads the share" half; FROST handles the "no single party signs alone" half via the threshold construction.

Threshold ECDSA without SGX would push the entire trust burden onto the MPC math. The math currently has the audit-surface and patches-on-patches issues §2.2 describes. Adopting it would mean trading a known, audited, deployed trust anchor (SGX/DCAP) for a research-grade one — for a system where the SGX trust anchor is already paid for by the orderbook, margin engine, and per-user state living inside the same enclave.

### 3.2 The audit surface is much larger

We track these audit-quality concerns the talk surfaces about CGGMP/DKLS:

- **CGGMP requires re-verifying ZK proofs every signature.** Range proofs, correctly-chosen-primes proofs, correct-encryption proofs run on every signing round. Latency is paid per signature, not amortised at setup. Per the talk: "you have to do all of these inside one signature, and practically every time."
- **CGGMP's ZK stack is moving.** The 2018 → 2019 → 2021 → 2024 history of "patch found, new ZK added" is exactly the structural pattern audit guidance warns against. Adding a new tx-type or weight semantic might invalidate a previously-proven property without anyone noticing.
- **DKLS's audit surface is wide.** Reading the protocol means reading RVOLE + OT-extension + SubfieldVOLE + MPFSS + DPF in sequence. Auditing it means auditing each layer plus the composition. The 2025 KOS theorem-proof issue is exactly the kind of finding that surfaces when the surface is this wide.
- **Both involve O(N²) point-to-point messages between operator pairs.** Latency and bandwidth scale poorly relative to FROST's broadcast-friendly aggregation.

We have an existing per-tx-type signing-policy hardening (`p2p::validate_signing_policy`, audit-shipped per re-audit-3 X-C1 hardening) that is six lines of business validation per allowed tx type. Adding ECDSA-MPC protocol code to the threat model would add four orders of magnitude more code to keep correct.

### 3.3 XRPL multisig does not need MPC at all

The on-chain escrow's signing requirement is satisfied by XRPL's native `SignerListSet` primitive: `K-of-N` operator addresses, each signs independently with its own ECDSA key, the chain enforces the quorum. This is a threshold scheme implemented by the chain protocol, not by us. There is nothing for MPC to replace at this layer. (See `orchestrator/src/signerlist_update.rs` and Phase 2.2 for how membership changes propagate.)

The only place MPC threshold-ECDSA would apply is the FROST layer (a future Schnorr-incompatible chain that doesn't have on-chain multisig). For that, we use Schnorr inside SGX.

## 4. When we would reconsider

Two scenarios would re-open this decision:

1. **SGX trust assumptions weaken substantively.** Examples that would matter: a side-channel attack that retrieves sealed material at scale on production-supported hardware, an Intel decision to retire DCAP without a TDX migration path on a usable timeline, or a regulatory ruling that disallows SGX as a custody primitive in our jurisdiction. The roadmap response in `docs/sgx-vs-tdx-roi.md` is TDX migration first; threshold-ECDSA-MPC is the second-tier fallback if no TEE option remains usable.
2. **A non-Schnorr-supporting chain becomes a hard product requirement and the on-chain native multisig is unavailable.** Most ECDSA chains we care about have multi-party signing primitives that work without MPC (XRPL `SignerListSet`, EVM gnosis-safe-style multisig contracts, BTC native multisig + Taproot Schnorr after Tapscript). MPC ECDSA becomes load-bearing only where none of those work.

In both scenarios the effort estimate from the talk is "research-grade, 4–6 months to ship reliably for the first time, longer to defend against the next round of patches." This is a project work item, not an in-flight refactor.

## 5. References

- Sber technical talk, 2026-04-22. Slides: `references/MPC-TSS.pdf` (27-slide Beamer deck, Russian, with Strugatsky chapter epigraphs). Recording: <https://www.youtube.com/watch?v=ZqHfLjlJBww>. The talk is presented as research-direction overview; the speaker's `Σ ZK` count and "rabbit hole" framing inform §2.2 above.
- CGGMP family — Gennaro, Goldfeder (2018) "Fast Multiparty Threshold ECDSA with Fast Trustless Setup" (GG18); Canetti, Gennaro, Goldfeder, Makriyannis, Peled (2020+) "UC Non-Interactive, Proactive, Threshold ECDSA with Identifiable Aborts" (CGGMP).
- DKLS family — Doerner, Kondi, Lee, Shelat (2018, 2019, 2023) — "Secure Two-party Threshold ECDSA from ECDSA Assumptions" and follow-ups; the 2023 paper consolidates the protocol used in current DKLS implementations.
- KOS protocol — Keller, Orsini, Scholl (2015) for OT-extension; the 2025 paper revising KOS's CD-security argument was referenced by the talk's audience (Mike Voronov, in chat).
- `docs/cluster-trust-model-decision.md` — DCAP cross-attestation choice and rejection of operator-signed roster.
- `docs/sgx-enclave-capabilities-and-limits.md` — SGX trust model + FROST 2-of-3 framing.
- `docs/multi-operator-architecture.md` §1 (trust model), §10 (subcommand classes).
- `SECURITY-REAUDIT-4.md` X-C1 hardening — the per-tx-type signing-policy pattern that bounds this layer's audit surface.
