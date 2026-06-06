# Architectural pattern: permissioned-chain with code/operator separation

**Status:** internal architectural-vocabulary note, 2026-06-06. **Conceptual only — not a legal, regulatory, or marketing positioning.** Captures the mental model for future technical discussions.

## Premise

At the architectural layer, this system resembles **permissioned-chain pattern** (Hyperledger Fabric / J.P. Morgan Quorum / R3 Corda) rather than either «public-chain DEX» (Uniswap-class smart-contract execution on a permissionless L1) or «centralized DEX» (single operator running everything trustfully). The vocabulary of permissioned chains carries over directly and reduces friction in technical conversations.

## The pattern

| Layer | Public chain (Bitcoin, Ethereum) | Permissioned chain (Fabric, Quorum, Corda) | Our system |
|---|---|---|---|
| Node software | Open-source clients (Geth, Erigon, Lighthouse…) | Fabric peer binaries, Quorum forks | Reproducibly-built MRENCLAVE (dev-perp authored) |
| Validator / operator set | Permissionless (anyone) | Closed consortium, member-curated | XRPL M-of-N SignerList, currently 2-of-3, quorum-gated membership |
| State agreement | PoW / PoS consensus | PBFT / Raft / IBFT | Sealed-state continuity under Path A migration, signed by operator quorum |
| Code-update governance | Hard fork (validators choose to adopt) | Consortium approval of new binary | Path A migration ceremony — old enclave only exports state to a new MRENCLAVE the quorum signed |
| Settlement layer | The chain itself | The chain itself | XRPL (we are not our own chain) |

## The distinguishing axis: code authoring ≠ operator activity

The single most important architectural property is the **separation of the code-authoring entity from the operator entity**, and the fact that this separation is **cryptographically enforced** rather than merely organizational.

Two invariants make the separation real:

1. **Reproducible-build foundation invariant.** Source is published. Any operator can independently rebuild the enclave binary inside a pinned Docker environment and obtain a bit-identical MRENCLAVE. Operators verify what they're about to run; the code-authoring entity cannot ship a binary that doesn't reproduce.

2. **Path A migration ceremony.** Code updates are gated by the operator quorum. The currently-deployed (old) enclave will only export its sealed state to a new MRENCLAVE that the M-of-N quorum has signed off on; the new MRENCLAVE will only import state attested as coming from the previously-recognized old MRENCLAVE. Operators have an explicit veto over what code runs.

In a Bitcoin / Ethereum analog: operators play the role of validators choosing whether to adopt a client release. The separation between «who wrote Geth» and «who runs Geth» is the same shape as «who writes our enclave» and «who runs our cluster».

## Where we extend the pattern: TEE execution privacy

Permissioned chains (Fabric, Quorum, Corda) typically reveal transaction content to all participating peers. Our system uses TEE attestation to make execution **private from operators themselves** — an operator runs the binary but cannot read order content, position state, or pending withdrawals while it's executing. This is closer in spirit to MPC-based confidential compute than to a private blockchain.

The trust shift: operators trust the binary (verified via reproducible-build + DCAP attestation) rather than trusting each other to keep transaction content confidential.

## Where we don't extend: not our own chain

We do not produce blocks; we do not run consensus on transaction history. The settlement layer is XRPL: deposits, withdrawals, signer-list changes, and operator membership all settle on XRPL as ordinary transactions under the M-of-N SignerList. The enclave cluster is an off-chain execution environment with on-chain settlement — closer to a rollup's «execution layer + L1 settlement» split than to a self-contained chain.

This is a deliberate choice: «chain-is-settlement» systems (Bitcoin, Ethereum, Fabric, Corda) own their consensus problem. We delegate it to XRPL, which gains us a battle-tested settlement layer and loses us the optionality to write our own consensus rules. For a perpetuals DEX use case the trade is correct.

## TRUST as a TEE co-processor framing

The stack assembled by the pattern above is sometimes referenced internally as **TRUST** — **Trusted Unchained Settlement**. The acronym is shorthand for the same architecture this document describes; it is included here so future references in technical materials can resolve cleanly.

### Where this sits in the L1 / L2 / L3 vocabulary — it doesn't

L2 (Arbitrum, Optimism, …) means specific things — execution outsourced from L1, L1-verifiable state via fraud or validity proofs, state batched back to L1. We do none of those: the chain (XRPL) does not verify our execution state; we anchor only multisig payments and Domain-field updates. Calling this an L2 sets the wrong expectation. L3 (app-rollup on top of L2) does not fit either.

The closest industry vocabulary is **co-processor** — used by zk teams (Axiom, RISC Zero) for off-chain compute with on-chain anchoring. We are not a zk co-processor; we are a **TEE co-processor**. Structurally a co-processor does **computation**, not block production — which is exactly what TRUST does. Naming it as such avoids the L-tier misframing.

One-line gloss for the section title: **TRUST is a TEE co-processor pattern that adds smart-contract-equivalent programmability to chains without native VMs, settling final state changes via the chain's own multisig primitives.**

### What TRUST adds, in architectural terms

Bringing programmability to chains whose settlement layer does NOT carry a Turing-complete VM (XRPL today; structurally applicable to Bitcoin / Litecoin / other UTXO or amendment-driven chains). The composition is:

- **Native chain primitives** — whatever the underlying chain natively exposes: M-of-N multisig, atomic transactions, an on-chain data field for committing payloads, public verifiable ledger. XRPL gives us `SignerListSet`, escrow accounts, `Payment` with `DestinationTag` / `Memos`, `AccountSet.Domain`. Different on Bitcoin (Script multisig + `OP_RETURN`), different on Cardano, different on Stellar — but the **shape** of «we need these four things» carries.
- **TEE-attested business logic** — the perpetuals matching engine, vault, settlement math, FROST signing. Runs in SGX today; portable to TDX / SEV-SNP / Nitro in principle, see `EthSignerEnclave/docs/contingency/` in the private repo for the substrate-migration analyses.
- **Code/operator separation cryptographically enforced** — reproducible-build invariant + Path A migration ceremony, as described above. This is what makes the pattern composable rather than «just a TEE running code».

The result is smart-contract-**equivalent** business logic without smart contracts — execution does the work that a contract would, settlement anchors the state changes on the chain's own primitives, and operators control which code runs without controlling what each individual operation does. The «equivalent» qualifier matters: this is not «behaves like smart contracts». See § «What it costs» below.

### What TRUST adds vs the permissioned-chain pattern

Two extensions over Fabric / Quorum / Corda:

1. **Execution privacy** — transaction content is private from the operators themselves, not just from outsiders. Fabric/Quorum/Corda peers see transaction payloads; TRUST operators see encrypted ciphertext. This is the part of the design that makes a perpetuals exchange usable: positions, pending orders, and live margin are not legible to the operator running the binary.
2. **Substrate portability in principle** — Fabric is its own chain; Quorum is a Geth fork; Corda has its own network. TRUST's settlement substrate is whatever chain the deployment picks. Today XRPL; the architecture does not require XRPL. See weaknesses below for the gap between «in principle» and «today».

### What it costs (weaknesses, declared honestly)

The pattern is not a strict improvement on either smart-contract DEX or centralized DEX — it is a different point in the trade-off space. The costs are real and should be named.

1. **Substrate portability is aspirational, not factual today.** The current implementation uses XRPL-specific primitives. Porting to Bitcoin requires re-doing the on-chain vocabulary (Script multisig semantics differ from XRPL); porting to other chains requires similar rework. «Any chain» overclaims the current state; the architecture supports it but the code does not yet.
2. **On-chain composability is lost.** Smart contracts compose with each other — a single transaction can touch Uniswap, Aave, and Compound atomically. TRUST does not. The TEE-attested execution layer cannot be called by other on-chain contracts on the settlement chain, and TRUST cannot call them either. Bridges would be required for any cross-protocol composition.
3. **Public state auditability is lost.** Smart-contract DEX state is readable by anyone reading the chain. TRUST state is sealed inside the TEE. Users trust attestation + reproducible build rather than direct chain inspection. Different trust model — not «strictly better», not «strictly worse»; different.
4. **Liveness compound.** Smart contracts on a public chain work as long as the chain works. TRUST works only when **both** the chain is live AND the operator quorum is reachable. Two dependencies in series instead of one.
5. **MEV is not absent, just relocated.** Public chains face on-chain MEV through transaction ordering observable in the mempool. TRUST faces TEE-internal sequencing fairness — controllable inside the enclave, but a different problem, not a solved one. Sequencer fairness becomes an internal architectural concern.
6. **Composability between TRUST instances does not work out of the box.** Two TRUST deployments on two different chains do not compose. Bridges or sequential commitment would be required; this is not an existing primitive of the pattern.

### Why declare these costs explicitly

Because the pattern's positioning otherwise reads as «strict improvement over both smart-contract DEX and centralized DEX», which is not true and would not survive technical due diligence. Declaring the costs up front lets the conversation move to «is this trade-off appropriate for the use case» — which is the conversation we want to be having.

## Current factual state: capability vs deployment

The multi-entity capability is implemented in code; today's three Azure DCsv3 nodes run under a single operator entity for development convenience. The path to onboarding an additional operator entity is the existing operator-onboarding procedure under the existing quorum, plus the new operator independently reproducing the MRENCLAVE before signing. No code change is required to move from one to several distinct operator entities; only the procedural sequence under the quorum and the independent rebuild verification by each new operator.

This distinction — capability vs. current deployment — is the honest answer when a technical conversation reaches «how many distinct entities are operating today.»

## What this document is NOT

- Not a legal or regulatory positioning. Operator-quorum gating distributes architectural control; how that maps to legal liability is a separate analysis.
- Not a marketing or investor narrative. Those use cases need their own framing built on top.
- Not a complete decentralization claim. Decentralization is multi-axis (operator distribution, code authorship, settlement layer, user permissionlessness); this document names the axes, not a verdict.
- Not a comparison with specific blockchain projects beyond the architectural-pattern analogy. We are not «Hyperledger but for perps» — we are «a permissioned-execution + L1-settlement pattern that happens to use TEE for execution privacy.»

## Cross-reference

- `docs/multi-operator-architecture.{en,ru}.md` — the concrete operator-quorum protocol
- `docs/test-env-workflow.{en,ru}.md` — the preflight → staging → acceptance workflow under the model
- Audit-side foundation invariants (referenced via the private audit repo): reproducible-build invariant; upgrade-path invariant (Path A migration ceremony); TEE-thesis invariant (private user state belongs sealed in the enclave, not on chain)
