# Re: Mono-repo proposal — appreciation, and a sequencing decision

**Author**: Andrey (project lead, architect of Path A and the audit protocol)
**In response to**: [`docs/mono-repo-proposal.md`](mono-repo-proposal.md) (Tom, merged 2026-05-03)
**Status**: decision document; supersedes any implicit acceptance implied by the merge of the proposal doc.

---

Tom — thanks for putting this together. The proposal is substantive and the goals it points at (deployment hygiene across three repos, explicit governance for production deploys, environment separation, post-deploy verification, monitoring) are all real and worth solving. I want to acknowledge that before getting to the part where I'm asking for a sequencing decision.

This document is the response. It is intentionally explicit because deployment architecture is one of those things where unspoken disagreements compound.

## Before the technical part — the human one

I value your participation in this project and I want that on the record before the rest of the document, because the rest of the document is going to ask you to step back from one specific surface for a while, and I don't want that ask to read as "your contribution is unwelcome." It isn't. I read your proposal carefully, your pull request answering the v1 blocking-questions on `xrpl-perp-dex` ([PR #9](https://github.com/LemonTreeTechnologies/xrpl-perp-dex/pull/9)) is exactly the kind of input the project needs from you, and your work in `8ball030/data_collection` PR #412 fits the same pattern: domain expertise applied where it has high leverage. The ask in this document is a sequencing ask, not a "no" — and the reason I'm putting it on paper rather than handling it informally is precisely because I respect the work enough to be specific about it.

## The shape of the problem we are actually solving right now

Enclave deployment under SGX is a trilemma. Pick **two of three**:

- **simple** — operationally light, low coordination cost, fast iteration
- **secure** — adversary cannot induce a malicious upgrade or extract sealed state
- **does not lose customer funds** — escrow accessibility and customer state survive every upgrade and every failure mode

You can have any two. You cannot have all three at once with a generic deployment substrate. We are currently at the very start of figuring out *which* two-of-three combination this project will actually live with, and *how* we get the third property as close as possible without sacrificing the other two. Path A is our current candidate path through this trilemma — and it has only just been specified. The implementation has just started. We have not yet seen Path A run end-to-end on real state. Until we have, the trilemma is unresolved in practice, only on paper.

The audit-protocol cycle (REQ/RESP between dev and audit) is itself only a few weeks old. We just developed the request-response discipline that lets us reason about these trade-offs incrementally rather than in one big-bang design. That foundation is not yet solid; it is solid *enough* to design Path A, but not solid enough to absorb a parallel architectural workstream of comparable weight without strain.

The single most important thing for this project right now is being able to work through the trilemma calmly, with one architectural surface in flight at a time. We can revisit deployment-substrate ideas — including yours — once we have **150% confidence** that the upgrade procedure (a) works mechanically, (b) is secure under our threat model, and (c) does not lose funds. Three independent properties, each demonstrated under load, before we layer additional governance / automation / mono-repo workflow on top.

Your proposal sits on top of that future layer. It is too early to sit it down well.

## Where I agree

- Three repos with no shared deployment story is real friction.
- Production deploys benefit from explicit multi-party governance and an audit trail.
- Post-deploy verification (integration tests, smoke checks) matters and ad-hoc smoke testing isn't enough.
- Monitoring with Prometheus/Grafana is the right toolchain.
- Environment separation (dev/staging/prod) is the right structural shape.
- Ansible is a reasonable deployment substrate for stateless components.

These are not in dispute.

## Where I am asking to defer, and why

We are currently mid-flight on a single load-bearing architectural design: **Path A** (state migration across enclave upgrades). The protocol spec was just frozen at PASS via the audit cycle; the implementation has just started. Path A is what makes "deploy a new enclave version without losing customer state" a defined operation rather than an unsolved problem. Until Path A is **operationally proven** (spec done, implementation done, end-to-end customer-state migration succeeds in a real environment), every other deployment-architecture decision sits on top of an unfinished foundation.

Concretely, the proposal as written touches several places where Path A specifics dominate:

### 1. Enclave deploy is not stateless deploy

A new enclave version means a new `MRENCLAVE`, which means a Path A ceremony with operator quorum delegation. This is a coordinated operation between humans, not a `make deploy-production` invocation. The proposal's forward-looking rollback (merge previous-stable as new commit, deploy) would itself trigger a full Path A ceremony — not a quick revert. That is fine if the deployment spec accounts for it, but the deployment spec hasn't been written against Path A's actual mechanics yet, because Path A's mechanics were only finalized days ago.

### 2. "2/3 of repo owners" intersects an authority surface that was just locked down

The audit cycle (RESP-7, M1) just finalized: **software upgrade authorization equals on-chain `SignerList.SignerQuorum`** — the same threshold that governs every withdrawal from the escrow. If "2/3 of repo owners" reduces to the same authority set, fine. If it doesn't, the proposal introduces a *parallel governance layer* with a different identity surface than the cryptographic upgrade authority. That parallel layer would be a subtle bypass: an attacker who compromised 2/3 of repo-owner GitHub accounts (which is a substantially smaller threshold to attack than 2/3 of XRPL signing infrastructure) could trigger an upgrade ceremony that the cryptographic layer believes was authorized. I don't have a clean answer for how this should compose, and I don't want to design a parallel governance layer at the same time we're finalizing the primary one.

### 3. GHA + Ansible vault is a new build-system trust footprint

Today the deploy substrate is "operator's laptop + SSH to host." Threat model: small, well-understood, manual. Moving to GitHub Actions + Ansible vault adds: runner pool identity, secret storage, action-publishing trust, automated invocation paths. None of this has been analyzed against the SGX trust boundary. The audit envelope explicitly carries build-system compromise as out-of-scope today (REQ-7 §3.1 / I1). Bringing GHA + Ansible in changes that. We can analyze it — but adding it concurrently with finishing Path A means stacking two architectural changes on top of each other, each non-trivial alone.

### 4. dev/staging/prod cannot be uniform for the enclave component

The proposal treats `make deploy-X` as the same shape across environments. For frontend and backend that's basically true. For enclave it isn't — dev and staging would run with `SGX_DEBUG=1` (no production attestation), and Mode S (state-loss-on-upgrade) is acceptable in those environments but never on production. The deployment mechanism needs per-environment behavior specific to the enclave component, and that specification depends on Path A details that aren't yet stable in code form.

### 5. SGX-specific deploy considerations the proposal doesn't address

Production deploy of an SGX enclave is not "copy binary, restart service." It is: build with reproducible-build discipline → measure `MRENCLAVE` → operators sign delegation messages with their per-operator keys → ceremony with `ceremony_nonce` → state migration → confirmation → atomic switchover. Each step has failure modes that are specific to SGX. Wrapping this in `make deploy-production` without internalizing those mechanics would either (a) lose the security properties we're paying SGX cost for, or (b) be a thin wrapper that doesn't actually simplify anything. Either outcome is worse than what we have.

### 6. The multi-operator model is itself unfinished

Path A solves *upgrade* — it does not by itself solve the broader question of what an *operator* is in the production multi-operator topology. We still need to design and audit, in detail:

- What it means for each operator to be an **independent subject** with their own physical infra, their own keys, their own organizational boundary.
- **What proves an operator is an operator** — what attestation chain, what registration mechanism, what revocation path, what does the rest of the cluster check before accepting a peer as a legitimate signer.
- **To whom an operator presents those proofs** — peer-to-peer? Through an on-chain registry? Through a quorum vote? Through an out-of-band ceremony?
- How operator turnover (adding a new operator, removing a compromised one) interacts with sealed state, FROST shares, and on-chain SignerList rotations.

This is a substantial architectural surface that has barely been sketched. It will need to be designed and audited by **one developer who understands TEE deployment specifics** — either someone with prior production TEE experience, or someone who acquires that experience through the trial-and-error of getting Path A to actually work end-to-end. (The latter is what's happening now, deliberately.) Until that operator-model design exists in concrete form, deployment automation that assumes a fixed operator topology is automating against an undefined target.

The mono-repo + 2/3-owners + Ansible proposal implicitly assumes a particular operator model (repo owners ≈ operators, GitHub identity ≈ operator identity, Ansible vault keys ≈ operator authority). That implicit model is not the one I expect us to land on, and locking in any deployment substrate before the operator model is settled creates rework debt later. Better to wait until the operator model is concrete, and then build the deployment substrate to match.

### The general shape

The proposal introduces *new unexplored entities* (Ansible, GHA, mono-repo workflow, "2/3 owners" governance) on top of an *unfinished architectural foundation* (Path A). Compounding unknowns this way tends to produce hard-to-debug cross-cutting issues later — not because any individual piece is wrong, but because the dependency graph between them isn't stable yet. I want to stabilize the foundation first.

## How I want to draw the ownership line

### Where your input is welcome and important right now

- **Business logic of perp-DEX** — order matching mechanics, perp pricing, fee structures, liquidation logic, maker/taker incentives.
- **Customer-facing UX** — perp-dex-ui design, frontend integration patterns, API ergonomics, how the product feels to traders.
- **Backend logic that doesn't cross the enclave trust boundary** — non-custodial features, public-data flows, indexing, analytics.
- **Specifically `LemonTreeTechnologies/perp-dex-ui`** — you own this; I'm not in a position to design it well, and I rely on you for it.

PRs in any of these areas remain welcome on their own merits, on each repo's own branch flow.

### Where I am asking for single-owner design discipline

- **Deployment architecture for the enclave**, until Path A is operationally proven.
- **Trust topology** — which keys live where, who has shell on which host, who can invoke which command, how authority surfaces compose.
- **The multi-operator model** — what an operator is, what proves it, to whom, how the topology changes over time.
- **The audit-cycle workflow itself** — REQ/RESP cadence, branch model, ownership of audit artifacts.

This isn't a permanent restriction on what you can contribute. It is a sequencing decision: **architecture first, your feedback once architecture is on stable ground.** After Path A is operationally proven and the multi-operator model is concrete, the deployment-infrastructure conversation reopens — and at that point your proposal becomes a *materially better* starting point because the substrate it builds on will be solid.

The reason for the sequencing isn't gatekeeping. It's that I'm currently coordinating between my own architectural judgment and AI-assisted analysis, and adding a third coordinating party on the same architectural surface — while that surface is still being finalized — produces more friction than throughput. Once the surface is stable, parallel input is healthy. While it's still being shaped, single-owner discipline keeps the design coherent.

## What I want to actively unblock for you

The strongest practical argument for deferring the deployment-architecture work is that doing so **directly unblocks** the work you've already opened.

[`xrpl-perp-dex` PR #9](https://github.com/LemonTreeTechnologies/xrpl-perp-dex/pull/9) is exactly the kind of contribution where you have outsized leverage: order-size discretization, rounding semantics, cross-margin liquidation policy, funding accrual, vAMM curve semantics, market-making ladder design, delta-band hysteresis, per-vault risk limits. These answers are domain calls that the project genuinely needs your input on, and merging them gives the v1 implementation a concrete target. I want that PR to land. I want it to land **soon**, not after we have spent weeks arguing about deployment infrastructure.

Your `8ball030/data_collection` PR #412 is in the same category — domain-specific data work that makes the broader system tractable.

Both of these PRs depend on a stable foundation underneath them — the perp-DEX behavior they define needs to actually run somewhere, on an enclave that can be upgraded without losing the customer state your business logic operates on. If we destabilize the deployment substrate now, the implementation work that consumes your PR #9 answers is at risk: a foundation in flux means downstream code keeps having to change to match. The cleanest way to honor your business-logic input is to **keep the substrate stable** while we wire up Path A, and to merge your business-logic PRs into a foundation that isn't shifting under them.

So the practical effect of this sequencing decision is:

- **PR #9 (xrpl-perp-dex blocking-questions answers)**: I want to merge the answers as the agreed v1 contract. Path A implementation will encode them. This is high-leverage work that should land soon.
- **PR #412 (data_collection)**: same posture — domain work, lands on its own merits, doesn't intersect the deployment substrate.
- **Mono-repo / Ansible / GHA proposal**: held until Path A is operationally proven and the operator model is concrete.

In other words: the best thing we can do for the project right now, including for *your* throughput on it, is to **do nothing** on the deployment-architecture surface, and to **do as much as possible** on the business-logic and data surfaces where your input has clear leverage.

## What I propose concretely

1. **Keep `docs/mono-repo-proposal.md` as-is.** It's a good record of intent. It doesn't need to be reverted. We'll come back to it.
2. **Hold on implementation.** No master meta-repo, no Ansible playbook, no GHA workflow until Path A REQ-8 (implementation review) lands PASS.
3. **Continue the current pattern.** Each repo independently maintained; code-level proposals (PRs) welcome on each repo's own merits; cross-cutting architecture decisions sequenced through me until Path A is operationally validated.
4. **After Path A is operationally proven**, we open a deployment-infrastructure cycle (call it REQ-9 in the audit-protocol terminology) where this proposal is the **starting point** and we work through specifics together: governance mechanism, per-environment behavior, build-system trust scope, rollback semantics for the enclave component, master repo visibility decision, and your role in the trust topology.

The expected sequence:

```
NOW          → Path A implementation (dev-perp + Claude Code + audit cycle)
             ↓
REQ-8 PASS   → Path A operationally proven
             ↓
REQ-9        → deployment infrastructure cycle, your proposal as starting point
             ↓
adoption     → mono-repo + Ansible + GHA workflow + governance, designed jointly
```

This is the ordering. It is the order *because of* the work involved, not despite Tom's contribution — once Path A is solid, your input on the layer above it becomes much more useful than it would be now.

## What this means day-to-day

- **PRs to `perp-dex-ui`**: business as usual; you own this.
- **PRs to `xrpl-perp-dex` business-logic code**: welcome, reviewed on their merits.
- **PRs to `xrpl-perp-dex` adding Ansible / GHA / deployment automation**: please hold until REQ-9 opens.
- **Architectural proposal docs**: welcome any time, but please understand that merging a proposal doc is not the same as accepting its implementation — I will respond explicitly (like this document) when an architectural proposal is ready to enter the design pipeline vs. when it's queued behind in-flight work.

## Closing

I've been deliberate about being specific in this response, because in my experience deployment-architecture disagreements that go unspoken compound silently and surface later as friction. I'd rather we both have a precise shared understanding of *why* this is "later, not now," than have the proposal sit in an ambiguous state.

The ask is concrete: hold on the implementation parts of this proposal until Path A REQ-8 lands; continue collaborating on every other layer; we'll re-open the deployment-architecture conversation with this proposal as the starting point once the foundation is solid.

— Andrey
