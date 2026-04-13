# Matching model alignment: CLOB vs AMM

**Audience:** internal team (Andrey, Tom, anyone touching the matching or vault layer after the Paris hackathon).
**Status:** alignment document. This is **not** a decision yet — it exists to force an implicit architectural choice into the open *before* any post-hackathon code is written. Bilingual: Russian counterpart at `clob-vs-amm-alignment-ru.md`.
**Context:** Triggered by a reading of `comparison-arch-network.md` §2a ("Why you cannot simply load a Rust CLOB into ArchVM") alongside the post-hackathon plan documented in the internal memory `project_post_hackathon_architecture.md`. The two documents point at the same architectural choice from opposite sides, and there is a real ambiguity in how "AMM pool" is being used that needs to be resolved before we cut code.

The point of this document is to make the ambiguity precise, describe the two possible interpretations in enough technical detail that both are individually clear, list the consequences of each, and end with one specific question that resolves the choice. There is no loaded framing: both variants are legitimate engineering directions. They just happen to be *different products* built out of the same vocabulary.

---

## 1. The exact words that triggered this document

From `project_post_hackathon_architecture.md`, Tom's proposal for the post-hackathon liquidity layer:

> *"Rip out the current MM vault, replace with AMM-pool-style liquidity."*
> *"The replacement should be an actual AMM pool that quotes continuously, not a bot that places ladder orders."*
> *"Maker rebates for trades against the pool."*
> *"Tom personally will run an arbitrage bot."*

From `comparison-arch-network.md` §2a.1:

> *"vAMM or oracle-priced pools, not a CLOB. GMX (GLP pool + Chainlink), Drift (hybrid AMM on Solana), Gains Network (synthetic with oracle pricing), Synthetix Perps (oracle-priced synths). These avoid the matching problem entirely by replacing the order book with a curve or an external price feed."*

The phrase **"AMM pool that quotes continuously"** and the phrase **"pool instead of an order book"** can describe one of two very different architectures. Tom has not said which one he means — not because he is hiding something, but because the phrase is genuinely ambiguous in common usage, and the difference only matters at the architectural level, not at the product-framing level. We are responsible for making the phrase precise internally before either interpretation is implemented.

---

## 2. Variant A — AMM as a *market maker*, CLOB preserved

### What it is

In Variant A, an "AMM pool" is a **pricing algorithm** used inside a market-making vault that posts limit orders to the CLOB. The pool never interacts with traders directly. What traders see is still the order book: bids, asks, depth, fills, cancels, maker-taker fees. What changes under the hood is *how* the maker vault decides the prices and sizes of its orders.

Concretely, the vault holds inventory (say, some RLUSD and some long/short perp exposure), runs an internal pricing curve — constant-product, constant-mean, Black-Scholes-adjusted, or a custom one — and from that curve it derives a ladder of bids and asks which it posts on the CLOB. When the inventory moves (because someone filled against the vault), the curve re-prices and the vault cancels/re-posts. The CLOB sees a stream of limit orders from a sophisticated maker; it does not see a curve.

### Where this pattern exists in the wild

- **Wintermute and similar professional market makers** use internal curves (often inventory-based) to derive quotes, then post those quotes as limit orders on centralised venues' order books. The CLOB is preserved; the curve is the quote engine.
- **Hummingbot "pure market making"** and **Hummingbot "avellaneda_market_making"** strategies compute bids and asks from an inventory-skew curve and post them as orders. Same pattern.
- **Uniswap v3 as a concentrated maker**: while Uniswap v3 as a DEX is a curve, the *pattern* of "concentrated liquidity around a fair-value price" translates directly into a CLOB-based market maker that posts tight ladders near a dynamically-computed fair value.
- **Our own `vault_mm` already is this, in its simplest form**: it computes a fair value from mark price, applies a fixed half-spread (0.0025), and posts ladders. Upgrading `vault_mm`'s pricing algorithm from "fixed half-spread" to "inventory-adjusted curve" is the natural evolution of this pattern.

### What it changes in our codebase

- **Vault layer only.** `orchestrator/src/vault_mm.rs` gets a new pricing module that replaces the fixed-half-spread logic with a curve. The vault still talks to the enclave through the existing order-placement API.
- **Enclave unchanged.** No new ecalls, no changes to `PerpEngine`, no new state, no oracle dependency. The CLOB matching engine inside the enclave never learns that an "AMM" exists anywhere.
- **Fee structure may gain a maker rebate.** A small addition to the fee logic in the enclave or in the settlement layer. Compatible with Variant B as well but not specific to it.
- **Tom's arb bot operates externally**, just as described: it trades between our CLOB and external venues (CEXes, other perp DEXes), which is standard external arbitrage and does not require any new infrastructure from us.

### What it preserves

- **The CLOB.** Microsecond matching, cancel-reprice cycles at memory speed, maker-taker fee structure, deterministic fill semantics.
- **Our enclave differentiator.** Every argument in `comparison-arch-network.md` §2a.4 and `btc-perp-dex-feasibility.md` that says "we are the only project with a real in-enclave CLOB" still holds word for word.
- **The investor pitch.** The BTC-feasibility document, the Arch comparison, the "hardware-attested execution + microsecond CLOB" story all remain intact.
- **Chain-agnostic path.** Because nothing about matching or settlement changes, the ChainAdapter story for eventual BTC deployment is unaffected.

### What it costs

A moderate refactor of `vault_mm.rs`. Probably a few weeks of focused work plus tuning. No touching of the enclave, no touching of margin, no touching of matching, no touching of settlement.

---

## 3. Variant B — AMM as a *counterparty*, CLOB replaced

### What it is

In Variant B, an "AMM pool" is **the counterparty to every trade**. There is no order book. A trader who wants to open a long perp position hits the pool directly, the pool's curve computes the execution price based on current reserves (or a virtual reserve, i.e. vAMM), the pool takes the other side, and a position is recorded. Mark price for funding, liquidations, and PnL accounting comes from an external oracle, not from a crossing book.

This is the architecture of **GMX, Drift, Gains Network, Synthetix Perps**, and — almost certainly — **VoltFi on Arch**. It is the only architecture that fits into a general-purpose smart-contract VM, which is why those projects picked it: they had no other choice, because their underlying platform could not host a CLOB. (Refer to `comparison-arch-network.md` §2a.2 for the six structural reasons why.)

### What it changes in our codebase

This is architectural surgery, not a refactor:

- **New ecalls in the enclave.** At minimum: `ecall_pool_open_position`, `ecall_pool_close_position`, `ecall_pool_update_oracle_price`, `ecall_pool_liquidate`. The existing `ecall_perp_open_position` / `_close_position` / `_liquidate` either go away or live alongside for a transition period.
- **New state in `PerpEngine`.** Pool reserves (or virtual reserves), curve parameters (k, leverage cap per side, utilization ratio), per-side skew caps, a backstop fund.
- **The matching engine as we know it becomes unused by the main trading path.** It may still exist for historical orders or for a parallel CLOB-for-whales design, but the primary path no longer goes through it.
- **Oracle integration becomes critical.** Funding, liquidations, and mark price all depend on an external BTC/USD (or XRP/USD) price feed posted into the enclave at each block. A new ecall for oracle updates, a new trust assumption on the oracle operator, rate-limit logic to prevent oracle-driven manipulation.
- **Liquidation mechanics change.** In a CLOB, liquidations are reduce-only IOC orders routed through the book (see `feedback_closes_must_route_clob.md`). In a pool model, liquidations are forced unwinds against the pool, at the oracle-derived mark price, with a keeper incentive. Different code, different risk profile.
- **Margin system gets rewritten.** Because the counterparty is the pool, margin no longer protects one user from another; it protects the pool from insolvency. Different formulas, different worst-case analysis. (Tom's own note — *"need to do some work on the margin system I need to understand that way more"* — is consistent with this being a major piece of work.)
- **LP token or vault share.** Liquidity providers need a way to deposit into the pool and receive a share of its PnL. New token-like concept, new accounting, new exit-queue logic when LPs withdraw.
- **Vault architecture is not upgraded, it is replaced.** `vault_mm` and `vault_dn` disappear entirely; they become the *pool* itself in a different shape.

### What it destroys

- **The CLOB.** Microsecond matching, cancel-reprice cycles, maker-taker fees as we know them. These concepts no longer apply to the main trading path.
- **The SGX-enclave rationale.** The core argument for why we chose SGX in the first place is that "you cannot run a real CLOB anywhere except inside an enclave-protected process" (see `comparison-arch-network.md` §2a.4). If we are no longer running a CLOB, then the enclave is still useful for *something* — signing, custody, risk bookkeeping — but its central architectural reason evaporates. We could, in principle, run Variant B on a regular server without an enclave and lose very little security, because the pool's math is public and the pool's state is reproducible from an oracle feed. This matters because it weakens our answer to the question "why do you need SGX at all?".
- **The §2a argument in the Arch comparison.** Every paragraph in that section becomes self-incriminating. If we build Variant B, we are doing exactly what we said nobody else should do — replacing a CLOB with a curve because it's easier. That is fine as an engineering decision, but it cannot coexist with a public document that says "anyone who builds a curve-based perp is giving up the differentiator". One of the two has to change.
- **The "we are architecturally different from VoltFi" framing.** Under Variant B, we become structurally similar to VoltFi, just running on different infrastructure (SGX enclave vs. ArchVM). The competitive comparison changes from "CLOB vs. curve, different products" to "curve vs. curve, same product on different platforms". That is a harder pitch because now we have to explain why SGX is a better substrate for a curve than ArchVM is, and the answer ("smaller TCB, attested binary") is quantitatively true but qualitatively less compelling.
- **Part of the BTC-feasibility argument.** `btc-perp-dex-feasibility.md` §5 describes a CLOB-based BTC perp, and much of the "why only us" language assumes the in-enclave matching engine is the differentiator. A curve-based BTC perp is still feasible but needs a different pitch.

### What it preserves

To be fair, Variant B is not without upside:
- **Simpler user flow.** Traders click "long 10x", instantly get a position, no waiting for a fill, no partial fills, no cancel-reprice latency issues from the user's perspective.
- **Lower inventory risk for LPs in some regimes.** A well-designed pool can be more capital-efficient than a ladder of limit orders in certain volatility regimes.
- **Easier to bootstrap liquidity.** One pool with one pricing curve is conceptually simpler to seed than a two-sided book. This is why every "we don't have enough MMs yet" team eventually gravitates toward this model.
- **Fits common retail mental models.** The GMX/Drift UX is familiar to crypto-native retail users in a way that a CLOB (BitMEX-style) is not.

These are real advantages. They are the reason GMX has real users. Variant B is not stupid; it is just *a different product than the one we built our architecture for*.

### What it costs

This is multi-month architectural work inside the enclave, plus audit, plus oracle infrastructure, plus new LP mechanics, plus rewriting the investor pitch, plus rewriting §2a of the Arch comparison, plus taking on a dependency on an oracle operator, plus a fundamental review of the margin system. It is not a refactor; it is a new product.

---

## 4. Side-by-side

| Axis | Variant A (AMM-as-MM) | Variant B (AMM-as-counterparty) |
|---|---|---|
| **What is the trader matched against?** | Other traders' orders on the CLOB. The "AMM vault" is just one more maker. | The pool itself. Every trade is trader-vs-pool. |
| **Does a CLOB exist?** | Yes — unchanged. | No. Replaced by a curve. |
| **Where does "AMM" live?** | In the vault's pricing algorithm (off-CLOB). | In the enclave's state machine (replaces matching). |
| **Enclave code changes** | None. | New ecalls, new state, new liquidation path, new margin logic. |
| **Oracle dependency** | None (mark price is derived from book mid as today). | Hard — oracle is critical for funding, liquidations, and trade pricing. |
| **Maker-taker fees** | Preserved; rebates to CLOB makers. | Gone in current form. Fees now go to the pool LPs. |
| **Liquidations** | Reduce-only IOC orders through the CLOB (`feedback_closes_must_route_clob.md`). | Forced unwinds against the pool at oracle mark price, with keeper incentive. |
| **Tom's arb bot** | Arbs the CLOB against external venues. Standard. | Arbs the pool curve against external venues. Critical to price discovery. |
| **`vault_mm` / `vault_dn` fate** | Upgraded (new pricing algorithm, same vault shape). | Deleted and replaced by the pool itself. |
| **SGX rationale** | Intact — in-enclave CLOB is the reason. | Weakened — pool math doesn't need an enclave as much as CLOB does. |
| **`comparison-arch-network.md` §2a** | Intact. | Self-incriminating; has to be rewritten. |
| **`btc-perp-dex-feasibility.md`** | Intact. | Needs substantial rewrite; curve-based pitch is different. |
| **Competitor category** | CLOB perp DEX — competes with BitMEX, Deribit, Hyperliquid. | Curve/oracle perp DEX — competes with GMX, Drift, VoltFi. |
| **Scope of work** | Refactor `vault_mm.rs` pricing. Weeks. | Multi-month architectural overhaul including audit. |
| **Who needs to sign off** | Tom + engineering lead. Normal vault-layer change. | Andrey (product scope) + Tom (implementation) + audit budget approval. |

---

## 5. Consequences, spelled out

### 5.1 If we pick Variant A

- **Nothing in our current documentation needs to change.** `comparison-arch-network.md`, `btc-perp-dex-feasibility.md`, `sgx-enclave-capabilities-and-limits.md` remain accurate. The investor pitch remains "in-enclave CLOB with microsecond matching, hardware-attested, FROST-settled".
- **`vault_mm.rs` gets a new pricing module.** This is the post-hackathon Phase 1 work. Clear owner: Tom. Clear scope: bounded. Clear review surface: vault layer only.
- **Maker rebates get added** to the fee logic, either in the enclave or in a settlement wrapper. Small change. Compatible with Variant A (and in fact more natural under it, because the maker/taker distinction still exists).
- **Tom's arb bot runs externally** as described. No new enclave work to support it.
- **The margin system review Tom asked for** happens independently, as a knowledge-sharing pass. Not coupled to the vault redesign.

### 5.2 If we pick Variant B

- **A product-scope decision is required, not just an engineering decision.** Andrey has to explicitly accept that we are changing what we sell: from "in-enclave CLOB perp" to "in-enclave curve perp". This is not a question Tom can answer on his own, and it is not a question that should be answered implicitly by writing code.
- **`comparison-arch-network.md` §2a has to be rewritten.** Specifically, §2a.4 and §2a.5 become indefensible as-written, because they explicitly argue that building a curve instead of a CLOB is "voluntarily giving up our differentiator". If we *are* building a curve, we have to either (a) accept that framing and explain why we do it anyway, or (b) rewrite the framing to not rest on CLOB-ness as the key differentiator.
- **`btc-perp-dex-feasibility.md` needs substantial rework.** Several sections (§3 "what ports unchanged", §5 "architecture sketch", §7 "unique wedge") rest on the CLOB assumption.
- **The SGX rationale needs a new foundation.** What does the enclave give us, if not the ability to run a CLOB that no generic VM can host? Plausible answers: attested execution of the curve itself (so users can verify we are not rug-pulling the curve parameters), auditable custody, minimal TCB compared to a smart-contract deployment. These are real benefits but they are *different* benefits from the current ones.
- **A new oracle trust model enters the picture.** We have to decide whose price feed we use, how we attest its freshness, how we handle oracle downtime, how we prevent oracle-based manipulation of the pool. This was not a concern under the CLOB model because mark price was derived from the book mid.
- **The margin system rewrite is no longer optional or a knowledge-sharing pass.** It becomes a first-class dependency of the launch, because pool-based margin is fundamentally different from user-vs-user margin. Tom's note that he "needs to understand the margin system more" is exactly the right instinct for this path but it becomes critical, not nice-to-have.
- **Audit cost and timeline expand significantly.** A CLOB refactor inside a vault is a small-audit item. A new pool-based matching core with new margin and new oracle trust is a full re-audit of the enclave.
- **`feedback_closes_must_route_clob.md` becomes obsolete** under Variant B. The closes-must-go-through-CLOB feedback is specifically a CLOB-world guarantee. In pool world, closes go through the pool, and the concern that motivated that feedback (preventing closes at an arbitrary mark price bypassing the book) has to be re-expressed in pool-world terms.

---

## 6. The one question that resolves this

To Tom, verbatim:

> *"When you say 'AMM pool that quotes continuously', do you mean:*
>
> *(A) a vault that uses a curve-based algorithm to compute bid/ask prices and then posts those as limit orders on the existing CLOB (CLOB is still the execution layer, the pool is a smarter market maker), or*
>
> *(B) a pool that is the counterparty to every trade, so traders hit the curve directly and the CLOB is no longer the execution layer?"*

This question is designed to be answerable in one word. There is no third option that is meaningfully different from these two. Any answer Tom gives will either map onto A, map onto B, or reveal that the concept was not yet precise in his own head, in which case we have resolved the ambiguity by getting both of us to think about the question at the same level of precision.

### Why not just "do you want a CLOB or not?"

Because that phrasing is loaded and implies one answer is the right one. The A/B phrasing above is neutral: both are described as legitimate engineering choices, and the difference is framed as a matter of where execution happens, not as a matter of who is right.

### What if Tom answers "I don't know, what do you think?"

Then we have surfaced the real situation: the post-hackathon plan had not yet distinguished these two variants, and the distinction needs to be made collaboratively. In that case, Andrey decides (because it is a product-scope question) and Tom implements whichever variant Andrey picks.

---

## 7. Decision matrix

### If Variant A is chosen

1. Tom owns the `vault_mm` pricing upgrade. Scope: `orchestrator/src/vault_mm.rs` and adjacent. No enclave changes.
2. Maker rebate logic added in parallel. Owner: TBD but small scope.
3. Margin system review scheduled as a standalone knowledge-sharing session. Not blocking the vault work.
4. No documentation changes required beyond normal implementation notes.
5. Tom's external arb bot proceeds independently; no coordination needed beyond shared API knowledge.
6. This document gets a closing note: "Alignment resolved in favor of Variant A on YYYY-MM-DD."

### If Variant B is chosen

1. Andrey signs off explicitly on the product-scope change in writing (this document or a follow-up).
2. An ADR (architecture decision record) is written documenting the decision, linked from this document and from the Arch comparison.
3. `comparison-arch-network.md` §2a is rewritten or marked as historical context, with a new argument about why SGX is still the right substrate for a curve-based design.
4. `btc-perp-dex-feasibility.md` is rewritten to reflect a curve-based architecture.
5. Oracle trust model is designed before any enclave code is written. This is a design deliverable, not an implementation deliverable.
6. Margin system is redesigned from first principles for pool-based counterparty. Tom is the owner, but it is no longer "learn the existing system" — it is "design the new system".
7. Audit budget is re-scoped.
8. `vault_mm.rs` and `vault_dn.rs` are marked for removal after the pool path is live.
9. Timeline is reset. Variant B is not a hackathon sprint; it is a new product line.
10. This document gets a closing note: "Alignment resolved in favor of Variant B on YYYY-MM-DD. Product scope formally changed; see ADR-NNN."

---

## 8. What this document does NOT decide

To prevent scope creep in the conversation it triggers:

- **It does not reject maker rebates.** Rebates are compatible with both variants. Whether we add them is a separate decision.
- **It does not reject Tom's arb bot.** The arb bot is compatible with both variants and is a good idea in either.
- **It does not reject the margin system review.** That review is needed regardless and is a separate workstream.
- **It does not pick between Variant A and Variant B.** It forces the choice to be made explicitly, by the right person (Andrey for B, Tom for A), at the right time (before post-hackathon coding starts).
- **It does not pass judgment on GMX, Drift, VoltFi, or any other curve-based perp DEX.** Those are legitimate products. The question is whether *we* should become one of them.
- **It does not decide anything about XRPL AMM integration** (see `project_xrpl_amm_viability.md` for that separate discussion). The XRPL-native AMM question is orthogonal to this document's question.

---

## 9. Who needs to read this and when

- **Tom** — before any code in the post-hackathon vault-redesign branch is written. He answers the §6 question and this document's conclusion is recorded.
- **Andrey** — before giving Tom the green light to start implementation, and definitely before any investor conversation that describes the post-hackathon direction. If Variant B is the answer, Andrey has to approve the product-scope change explicitly.
- **Anyone else on the enclave or vault layer** — should read this before participating in design discussions about liquidity, matching, or pool mechanics after Paris.
- **Future contributors** — this document is the canonical reference for "why did we end up with the current vault/pool design?". Whichever variant we pick, the reasoning is captured here so we don't re-litigate it in six months.

---

## 10. Cross-references

- `comparison-arch-network.md` §2a — the argument that no fully on-chain CLOB exists in any generic smart-contract VM; the technical background for why Variant B is what you are *forced* to build when you do not control the matching layer, and why we built the enclave specifically to avoid that forcing.
- `btc-perp-dex-feasibility.md` — the chain-agnostic pitch that currently assumes a CLOB; affected under Variant B.
- `sgx-enclave-capabilities-and-limits.md` — the enclave trust model; unaffected by variant choice but the *rationale* for using it shifts under Variant B.
- `feedback_closes_must_route_clob.md` — internal memory that presumes CLOB-based execution; becomes obsolete under Variant B and must be replaced.
- `project_post_hackathon_architecture.md` — internal memory capturing Tom's post-hackathon plan; this document is a direct continuation of that one, forcing the implicit choice in it into the open.
- `project_xrpl_amm_viability.md` — separate discussion about XRPL's native AMM; not the subject here but related enough to worth linking.
- `feedback_clob_is_the_product.md` — internal memory that the CLOB is the product, not an implementation detail; this document is the concrete application of that feedback to the specific post-hackathon proposal.
