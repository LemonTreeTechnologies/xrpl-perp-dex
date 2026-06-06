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
