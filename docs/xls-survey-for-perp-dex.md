# XLS Proposals Survey — Relevance to the Perp DEX

**Status:** Research note. Not a design document. Captures what was reviewed, what is usable, what is not, and what the competitive landscape looks like as of 2026-04-13.

**Russian version:** `xls-survey-for-perp-dex-ru.md` (per the bilingual docs policy).

**Sources:** XRPL Standards repo `XRPLF/XRPL-Standards` (raw markdown), and `opensource.ripple.com` listings. Where a proposal's specification could not be retrieved (404 or JS-rendered shell), the note says so explicitly.

---

## 0. Executive summary

There is **no XLS proposal that competes with what we are building** — none of the listed standards offer perpetual derivatives, an off-ledger TEE-backed CLOB, threshold-custodied settlement, or anything resembling a margin/funding/liquidation engine. The closest neighbours are spot DEX/AMM primitives (XLS-30 AMM, XLS-81 Permissioned DEX) and a stagnant options proposal (XLS-62). Our market position on XRPL is **first and only** for perps.

Of the proposals reviewed, three are worth integrating into our stack as real building blocks, two are worth keeping on a watch list, and the rest are either irrelevant, the wrong shape, or competing in a different category.

**Useful now (or soon):**
1. **XLS-47 Price Oracles** (Final) — as one input to our mark-price oracle alongside CEX feeds.
2. **XLS-56 Batch** (Final) — for atomic multi-leg XRPL settlement transactions emitted by our orchestrator.
3. **XLS-70 Credentials + XLS-80 Permissioned Domains** (both Final) — as the optional KYC-gating layer if/when we need a regulated tier.

**Watch list:**
4. **XLS-85 Token Escrow** (Final, activated 2026-02-12) — usable for user-side deposit escrows, with caveats; not a fit for our settlement engine.
5. **XLS-100 Smart Escrows** (Draft) — promising future primitive for oracle-settled per-position structures, but not a CLOB engine and currently XRP-only.

**Not useful for us in the short term, but architecturally significant:**
- **XLS-101 Smart Contracts** (Draft, early). Adds *transaction emission* and *persistent contract state* — the two missing primitives that previously made on-chain perps structurally impossible on XRPL. So the trade space changes in theory. In practice it doesn't help us: ledger consensus latency (3–5s) is fundamentally too slow for a CLOB perp venue regardless of the VM, gas economics are unspecified, and the spec is years from activation. Detail in §2.11.
- **XLS-102 WASM VM** — the execution substrate powering both XLS-100 and XLS-101. Foundational, no independent value to us until one of those two ships.

**Not useful for us:**
- **XLS-66 Lending Protocol** — wrong shape (off-chain underwriting, fixed-term, no liquidation).
- **XLS-65 Single Asset Vault** — could be an LP container in theory, but adds complexity over our existing orchestrator-managed MM capital with no clear gain.
- **XLS-62 Options** — stagnant, not active.

---

## 1. Methodology and scope

I reviewed the XLS proposals listed at `opensource.ripple.com/docs` and the corresponding directories under `XRPLF/XRPL-Standards`. The opensource portal is JS-rendered and does not return body content via plain HTTP fetch — all substantive extraction was from the GitHub markdown sources.

Proposals covered (by XLS number): 30, 33, 34, 47, 51, 56, 62, 65, 66, 68, 70, 74, 75, 80, 81, 82, 85, 89, 94, 96, 100, 101, 102. Not every one is discussed in detail below — only the ones that touch our problem space.

What's deliberately out of scope here: NFT proposals (XLS-51), MPT metadata (XLS-89), confidential MPT (XLS-96), payment-channel-token escrow (XLS-34), sponsored fees (XLS-68), and the account permissions / delegation pair (XLS-74/75). They don't intersect the perp DEX architecture in any obvious way and reviewing them would dilute this document.

---

## 2. The key proposals, in priority order for us

### 2.1 XLS-85 Token Escrow

**Status:** Final. Activated on XRPL mainnet 2026-02-12 with >88% validator support.

**What it does:** Extends the existing native escrow primitive (which previously held only XRP) to support IOU tokens (trustlines) and Multi-Purpose Tokens (MPTs). `EscrowCreate` now accepts an `Amount` field that is either a string (XRP drops) or an object (issued currency / MPT). Token issuers must explicitly enable the feature: IOU issuers set `lsfAllowTrustLineLocking` on their AccountRoot (`AccountSet` with `SetFlag: 17`); MPT issuers set `lsfMPTCanEscrow` on the `MPTokenIssuance` object.

**Authorization model — important caveat:** *only the source or destination of an escrow may finish or cancel it*. There is no third-party finish path. Issuers cannot be the escrow source. This is the single fact that determines whether XLS-85 can play any role in our system.

**Frozen-token handling:** deep/full freeze blocks `EscrowFinish` (`tecFROZEN`). Both freeze types still allow `EscrowCancel`.

**Usefulness for us:**

- **As a user-side deposit escrow:** *plausible*. A user could escrow RLUSD with the protocol's pseudo-account as destination, with a `FinishAfter` time and a `CancelAfter` deadline. The protocol finishes at margin-credit time. This is mostly equivalent to a direct `Payment` plus a credit ledger entry inside the enclave, with an extra reserve cost and a recovery path if the protocol disappears (user can cancel after `CancelAfter`).
- **As an in-flight settlement primitive (e.g., locking a position's collateral):** *no*. The "only source or destination may finish" rule means the enclave / protocol cannot act as a third-party arbiter releasing funds based on its own internal state. The protocol would have to be a counterparty to every escrow, which is exactly the architecture our memo `project_xrpl_amm_viability.md` documents as fatal — it makes the protocol the AMM and breaks the CLOB invariants.
- **As a withdrawal queue:** *plausible but low-value*. Escrow with `FinishAfter` could implement a delayed withdrawal — but our deploy + custody model already has multi-sig timing controls; adding XLS-85 just doubles bookkeeping.

**Verdict:** keep on a watch list. The realistic use is "user deposits via XLS-85 escrow with `CancelAfter` as a safety hatch", and only if we decide that the safety-hatch property is worth the reserve cost. Not load-bearing.

---

### 2.2 XLS-100 Smart Escrows (with XLS-102 WASM VM)

**Status:** Draft (XLS-100 last updated 2025-11-20; XLS-102 updated 2026-02). Awaiting community review and validator vote. Not activated.

**What it does:** Adds a `FinishFunction` field to `Escrow` objects holding compiled WebAssembly. The WASM exports a single `finish() -> i32` function; if it returns `> 0`, the escrow can be released. Execution capped at 100,000 gas units (UNL-votable), code capped at 100KB, with a strictly fixed Wasmi runtime to keep consensus deterministic.

**What the WASM can read:** ledger objects (read-only), oracle data (`PriceDataSeries`, the XLS-47 output), credential objects (the XLS-70 output), the current escrow's own fields, and time-derived state. Roughly 70 host functions across general ledger access, NFT lookup, cryptography, and float ops.

**What it cannot do:**
- **No transaction emission.** It can decide "release / don't release", and it can write to its own escrow's 4KB `Data` field. Nothing else.
- **No write access to other ledger objects.**
- **No iteration over arbitrary directories** — all access via bounded keylets.
- **No XLS-85 token support yet.** The spec footnote says token support is "currently up for voting as part of the TokenEscrow amendment", but as of the spec's last revision it's XRP-only.
- **Must have a `CancelAfter`** to mitigate the stuck-funds risk from buggy WASM.

**The use case list explicitly includes derivatives:** "Oracle-Driven P2P Bets", "Options Contracts", and "Vesting Schedules". This is the closest thing in the XLS catalogue to native programmable settlement.

**Usefulness for us:**

- **As a per-position settlement structure for *very* simple products** (e.g., a single binary oracle-settled bet between two named accounts): yes, in principle.
- **As a CLOB engine:** no. There is no transaction emission, no aggregate state machine across positions, no way to express "match this taker against the best resting maker, update both positions, charge fees, route rebates". A CLOB needs all those things.
- **As a liquidation engine for our existing positions:** also no. The escrow can read a price oracle and decide to release, but it can't *force* the position out — there's no execution-on-trigger model, only "the loser of the bet won't bother claiming".
- **Once token escrow lands inside smart escrows** (the next amendment cycle, presumably), the same conclusion holds — the missing piece isn't tokens, it's emit and aggregate state.

**Verdict:** watch list. Genuinely interesting as a primitive and we should track it, especially if XLS-101 Smart Contracts (the broader programmability proposal) eventually gets transaction-emission semantics. But it does not change our short-term architecture, and "we should reread XLS-100 every six months" is the right cadence.

---

### 2.3 XLS-47 Price Oracles

**Status:** Final (created Aug 2023).

**What it does:** Adds a `PriceOracle` ledger object holding up to ten price pairs per instance. Updates via `OracleSet`, deleted via `OracleDelete`. Only the owning account can write. A `LastUpdateTime` field tracks freshness. The native `get_aggregate_price` API computes mean, median, and trimmed-mean across multiple oracle instances with optional staleness filtering.

**Usefulness for us:**

- **As one input to our mark price:** clean fit. Our mark price needs multi-source robustness anyway (CEX feeds, our own EWMA of recent fills, and a sanity-check signal). XLS-47 + `get_aggregate_price` is exactly the kind of "trimmed-mean across N oracle writers" you want as the sanity-check tier.
- **Does *not* replace CEX feeds.** Each oracle is owned by one account, so trust devolves to whoever runs the writers. The aggregator helps if there are several independent writers; on XRPL today the writer set is small.
- **Useful as a public commitment.** If our enclave signs and writes its own mark price to an XLS-47 oracle on every settlement boundary, that gives us a public, on-ledger, timestamped trail of what mark price our protocol used at any given moment. Cheap to add, valuable for audit and disputes.

**Verdict:** integrate. This is the simplest "good thing we should be using anyway" on the list. Action item: add a one-line "publish mark price to XLS-47 oracle on each funding interval" task to the post-hackathon backlog. Cost is small, value is non-trivial.

---

### 2.4 XLS-56 Batch

**Status:** Final (last updated 2026-02-10).

**What it does:** Wraps 2–8 inner transactions inside a single outer `Batch` transaction. Four atomicity modes:

- `ALLORNOTHING` — every inner tx must succeed or none execute
- `ONLYONE` — first success commits, rest are blocked
- `UNTILFAILURE` — sequential, stop on first failure
- `INDEPENDENT` — execute all regardless of individual results

Single-account batches sign only the outer tx. Multi-account batches use a `BatchSigners` array — every participating account signs the outer tx.

**Usefulness for us:**

- **Atomic settlement events.** When our orchestrator emits XRPL-side settlement (crediting winners + debiting losers + paying funding + transferring fees), `ALLORNOTHING` is the right primitive. Today we'd have to either issue them serially and handle partial failure manually, or pack them into a single business-logic transaction inside the enclave and rely on the receiver. With XLS-56 we get atomicity for free.
- **Multi-leg deposits/withdrawals.** A user opening or closing a position can be expressed as a batched (margin-update + position-credit + fee-payment) tx.
- **Limit of 8 inner tx is fine** for our settlement granularity — funding intervals or per-batch settlement events fit comfortably.

**Verdict:** integrate. This is the second "good thing we should be using anyway". Action item: when we wire periodic settlement from the enclave to XRPL (currently a TODO outside of liquidation), build it on `Batch ALLORNOTHING` from day one rather than retrofitting later.

---

### 2.5 XLS-70 Credentials + XLS-80 Permissioned Domains

**Status:** Both Final. XLS-70 created June 2024, XLS-80 finalised September 2024.

**What they do:**
- **XLS-70:** introduces a `Credential` ledger object (subject, issuer, type, expiration, optional URI). Three transactions: `CredentialCreate`, `CredentialAccept`, `CredentialDelete`. KYC documents stay off-chain; only the attestation lives on-chain.
- **XLS-80:** introduces a "permissioned domain" — a collection of accepted credential types. Membership is *implicit*: an account is a member iff it currently holds at least one accepted credential. No explicit join/leave, no behavioural restrictions, just a membership predicate.

**Important caveat:** XLS-80 itself does *not* gate features. It's a membership primitive. Feature gating happens in proposals that consume domains (e.g., XLS-81 Permissioned DEX adds a `DomainID` field to native DEX offers).

**Usefulness for us:**

- **As a KYC tier flag.** If we ever want to offer a "KYC-only market" (institutional pool) alongside the open market, XLS-70 + XLS-80 is the right way to identify which users have which credentials. Our orchestrator reads the user's credentials when they connect, gates entry, and tags trades by domain.
- **As access control for the orchestrator's REST API:** also clean. The enclave can require a credential signature on `POST /v1/orders` for the regulated tier.
- **Privacy:** the on-chain `Credential` object only commits to the attestation, not the underlying KYC data. Acceptable.

**Verdict:** integrate when needed. Not on the critical path for the permissionless launch, but the right primitive for a future regulated tier. Action item: keep these in mind when designing the API auth model so we don't need to retrofit later.

---

### 2.6 XLS-30 Automated Market Maker

**Status:** Final, activated.

**What it does:** Native AMM, weighted geometric mean (currently restricted to 50/50). LP tokens are real XRPL tokens. Fee is governed by LP voting (up to 8 active vote slots), 0–1000 bps. Distinctive feature: a 24-hour continuous auction slot lets arbitrageurs bid for fee-discounted access (1/10 of standard fee), with the bid going to LP holders.

**Usefulness for us:** already covered in `project_xrpl_amm_viability.md` — five blockers make it infeasible as our liquidity engine. Briefly: spot not derivative, wrong curve shape for perps (constant-product gives infinite slippage at low TVL), settlement collision with our off-ledger model, low TVL on relevant pairs, no native USD on XRPL. Possible niche use: read its mid-price as one signal in our oracle, nothing more.

**Verdict:** not competition, not infrastructure. Already analysed elsewhere.

---

### 2.7 XLS-66 Lending Protocol

**Status:** Draft (2026-01-14). Depends on XLS-65 (Vault) and XLS-64.

**What it does:** Off-chain-underwritten, fixed-term loans. New ledger objects `LoanBroker` (type 0x0088) and `Loan` (type 0x0089). Interest rates in 1/10 bps with four tiers (base / late / close / overpayment). First-Loss Capital pool absorbs initial defaults. No automated liquidation, no leverage, no oracle integration.

**Usefulness for us:** *zero*. The shape is wrong on every axis we care about:
- Loans are fixed-term, not perpetual.
- No automated liquidation.
- No leverage / margin.
- No oracle / mark-to-market.
- Risk assessment is explicitly off-chain.

There is no path from XLS-66 to a perp margin engine. The spec author chose a deliberately different design space (institutional credit origination, not DeFi leverage).

**Verdict:** ignore. Useful piece of XRPL DeFi furniture, but not for us.

---

### 2.8 XLS-65 Single Asset Vault

**Status:** Draft (2025-11-17). Requires XLS-33 (MPT).

**What it does:** A vault holding one asset (XRP / IOU / MPT) and issuing share tokens (MPTs from the vault's pseudo-account). Two-step deposit/withdraw with rounding designed to prevent arb of unrealised losses. Public or domain-permissioned. **No timelocks, no withdrawal queues** — first-come, first-served. Owner cannot lock out shareholders. Yield comes from external protocols, not from the vault itself; debt tracking is the consuming protocol's responsibility.

**Usefulness for us:**

- **As an LP container for the market-making side** (whoever ends up doing it, if Tom's post-hackathon plan goes the LP route): plausible. Vault holds XRP or RLUSD, issues shares, our orchestrator borrows from it as the protocol "MM treasury". External debt tracking matches our enclave-managed model.
- **Caveat — first-come-first-served withdrawals are bad for a trading vault.** When the MM side is losing money, withdrawals would race, and the late shareholders eat the losses. A perp MM vault really needs withdrawal delays / cooldowns / share-price-at-T+N semantics. XLS-65 doesn't provide that.
- **Versus rolling our own:** for a hackathon-grade product the existing direct-orchestrator-managed MM capital is simpler. XLS-65 starts paying off only when we want third-party LPs.

**Verdict:** keep in mind for the LP-onboarding phase (Tom's option (c) in the AMM viability memo). Not useful before then. If/when we go that route, we will need to either accept the FCFS withdrawal limitation or wrap the vault in our own queue logic.

---

### 2.9 XLS-81 Permissioned DEX

**Status:** Final.

**What it does:** Adds a `DomainID` field to Offers and Payments on the native DEX, segregating orderbooks by domain. A permissioned offer can only cross another permissioned offer in the same domain; it cannot cross open offers and vice versa. Built on XLS-80 + XLS-70.

**Usefulness for us:** none direct. Our matching is off-ledger inside the enclave, not on the native DEX. We don't post offers to the native book at all (other than our cross-pair hedge legs, which would be open offers anyway). Mentioning it because if we ever wanted a "KYC-only orderbook" we could either build it ourselves on top of XLS-70/80, or post offers into a permissioned native domain — the second approach loses our atomic margin enforcement, so the first is the right call.

**Verdict:** ignore for matching. Read for context only.

---

### 2.10 XLS-62 Options

**Status:** Stagnant (last touched April 2025).

**What it does:** Physically-settled call/put options. Sellers lock collateral. Strike and expiration set by participants. No oracle dependency.

**Usefulness for us:** none. Stagnant means no momentum. If it ever ships it would be complementary, not competing — options and perps serve different strategies.

**Verdict:** ignore.

---

### 2.11 XLS-101 Smart Contracts

**Status:** Draft / Proposal. Created 2025-07-28 by Mayukha Vadari. The spec itself describes itself as "a fairly early draft" with TODOs throughout.

**What it adds — three new ledger object types:**
- **`ContractSource`** — stores WASM bytecode with reference counting, so identical code shared across deployments doesn't multiply storage cost.
- **`Contract`** — the deployed instance, lives on its own pseudo-account, holds owner, code hash, and instance parameters.
- **`ContractData`** — *persistent storage* for contract state. Supports both contract-level and per-user data. This is a real state model, not the 4KB scratch field that XLS-100 escrows have.

**Six new transaction types:** `ContractCreate`, `ContractCall`, `ContractModify`, `ContractDelete`, `ContractUserDelete`, `ContractClawback`. Plus two RPCs: `contract_info`, `event_history`.

**The two changes that matter for our trade space:**

1. **Transaction emission — YES.** A contract can submit its own XRPL transactions through its pseudo-account. This is the single capability that XLS-100 explicitly forbids and XLS-101 explicitly grants.

2. **Persistent contract state — YES.** Beyond a tiny data field. `ContractData` is designed as the contract's actual state store, not a scratch buffer.

Together those two changes mean that in the XLS-101 era, a perp DEX expressed as on-chain contract logic stops being structurally impossible. The contract could hold positions in per-user `ContractData`, accept deposits via standard payments, process orders via `ContractCall`, and emit settlement transactions through its pseudo-account. The fully-on-XRPL design that we ruled out in `project_xrpl_amm_viability.md` would no longer be ruled out *by missing primitives*.

**Why the practical trade space barely moves anyway:**

- **Latency.** XRPL consensus is 3–5 seconds per ledger close. Every order, every match, every cancel becomes a ledger transaction. CLOB perps for any serious trader require millisecond fills, sub-second cancel-and-replace, and tight maker spreads that depend on being able to update quotes faster than the next market move. This is a property of consensus latency, not of the VM, and no XLS proposal closes that gap. The on-chain perp DEX you can build with XLS-101 is structurally a 3-second-tick batch auction, not a CLOB.

- **No directory iteration.** XLS-101 inherits from XLS-102 the design that ledger access must go through bounded keylets — no walking arbitrary directories. Building an order book (which is fundamentally iteration over a sorted directory) becomes an ergonomics problem. Workable, but not what the primitive is shaped for.

- **No background scheduling.** Contracts run only when called via `ContractCall`. Liquidations and funding-rate ticks need an external poker. The same limitation that hurts XLS-100 hurts XLS-101 — you need a permissionless keeper bot, and you need to design fees to compensate it.

- **Read-only ledger access from the contract's own code path.** State modifications happen through emitted transactions, which are normal XRPL transactions and have to go through the consensus pipeline. So even within a single `ContractCall`, you can't atomically read-modify-write multiple ledger entries — you're in the same transactional model as a normal transaction submitter.

- **Gas economics unknown.** The spec says limits and fees will be UNL-votable. A perp engine touching many state slots per fill (mark price update, position update, margin update, fee accrual, funding accrual) is going to be expensive. Until there's a published gas table, the "is it cost-competitive with off-ledger" question can't even be priced.

- **Status: early Draft.** XLS-100 is at Draft and presumably ahead of XLS-101 in the queue. Realistic time horizon to mainnet activation is years, not quarters.

**What this means for our existing recommendations:**

The "TEE-backed off-ledger CLOB" advantage in section 4 is still real, but the framing has to be honest: it's not that other approaches are *impossible*, it's that other approaches are *latency-bound to ~3s ticks and cost-bound by per-fill ledger fees*. For derivatives traders who expect millisecond execution and tight spreads, those constraints are not "minor inconveniences", they are disqualifying. So our advantage is "the only design that gives you trader-grade execution semantics on XRPL", not "the only possible design".

The "First-mover for perps" advantage is unchanged — XLS-101 is years away and even when it lands the most likely first products are slow batch-auction primitives, not CLOB perps.

**Verdict:** watch closely, re-read every six months. If a published gas table makes per-fill cost competitive (it almost certainly won't), or if a future XLS proposal adds anything resembling sub-ledger-close execution scheduling (it almost certainly won't), revisit. Otherwise the verdict is unchanged: TEE off-ledger CLOB is the right design for the next several years.

### 2.12 XLS-102 WASM VM

**Status:** Draft. The execution substrate that powers both XLS-100 Smart Escrows and XLS-101 Smart Contracts.

**What it establishes:** a deterministic Wasmi-based execution layer with ~70 host functions, gas metering across instructions / memory / host calls, code-size and computation limits voted by the UNL. Read-only ledger access, no traversal of arbitrary directories, no temporal scheduling (only runs during transaction processing), modifications routed through whichever calling proposal (XLS-100 or XLS-101) defines them.

**Verdict:** foundational, no independent action item — its impact is fully captured by what XLS-100 and XLS-101 do with it.

---

## 3. Competitive landscape on XRPL

The honest summary: **there is no perp DEX on XRPL today, and nothing in the XLS pipeline points at one.**

| Proposal | Category | Competes with us? | Why / why not |
|---|---|---|---|
| XLS-30 AMM | Spot AMM | No | Spot only. No leverage, no funding, no liquidations. |
| XLS-62 Options | Derivatives | No | Stagnant. Different product. |
| XLS-66 Lending | Credit | No | Fixed-term, no automation, no leverage. |
| XLS-81 Permissioned DEX | Spot CLOB | No | Spot only. Ours is off-ledger and perpetual. |
| XLS-100 Smart Escrows | Programmable settlement | Adjacent | Could in principle express a single oracle-settled bet, not a CLOB perp venue. |

**Off-XRPL competition** (not catalogued here in depth): standard perp DEXes on other chains — dYdX (Cosmos app-chain), Hyperliquid (own L1), GMX (Arbitrum), Drift (Solana), Vertex (Arbitrum + own sequencer). None of these are on XRPL. The closest architectural relative to us is Hyperliquid (off-chain matching, on-chain settlement, custom infrastructure for low-latency matching) — but they run on their own L1 with consensus tuned for trading, while we get XRPL settlement and asset issuance for free without paying to bootstrap a chain.

---

## 4. Our advantages, stated plainly

These are the things we have that no XLS proposal provides and no on-XRPL competitor offers today:

1. **Real perpetual mechanics** — funding rate, mark price, isolated/cross margin, automated liquidation. None of these are native to XRPL and none of the activated proposals add them. The closest is XLS-100 + XLS-47 + XLS-85 stitched together, and even that gives you single-position oracle-settled structures, not a perp venue. XLS-101 Smart Contracts (still Draft, years away) would in principle make a fully-on-chain perp design *expressible*, but not *practical* — see point 2.

2. **TEE-backed off-ledger CLOB with trader-grade execution semantics.** Sub-millisecond matching, real maker/taker dynamics, atomic margin enforcement inside the enclave. The honest framing: it's not that other approaches are impossible on XRPL, it's that they're latency-bound to ledger consensus (3–5 seconds per close) and cost-bound by per-fill ledger fees. For derivatives traders who expect millisecond execution and tight maker spreads, those constraints are disqualifying. Our design is the only one on XRPL that gives you trader-grade execution semantics — and that property is structural, not a temporary lead.

3. **2-of-3 distributed custody (XRPL SignerList — independent per-operator ECDSA, not an aggregated threshold signature).** Better trust model than the issuer-controlled escrow / vault model the XLS catalogue assumes. No single operator can sign a malicious settlement, no single operator can rug the protocol. The deployment-procedure document we just wrote extends this property to the deploy path.

4. **XRPL settlement without on-ledger-per-fill cost.** Batched settlement (XLS-56 once we wire it) gives us the best of both worlds: XRPL finality and asset ecosystem, off-ledger trading speed and economics. No on-XRPL competitor splits these layers this cleanly.

5. **Asset agnosticism through MPT/IOU.** Once XLS-85 token escrow is active (it is, since 2026-02-12), and once token support lands in smart escrows, we can collateralise positions in any RLUSD-class IOU or MPT, not just XRP. Our enclave already speaks the IOU/MPT vocabulary.

6. **First-mover for perps on the network.** Not a technical advantage but worth stating: there is no incumbent to displace. Whatever we ship is the reference implementation by default.

What we *don't* have (honest version):
- Audited production deployment of the multisig + enclave stack — that's the deploy procedure work in `deployment-procedure.md`, currently a draft.
- Liquidity. Day-one liquidity is the unsolved problem regardless of which architecture we pick. Tom's plan (post-hackathon vAMM + arb bot) is the working answer; this XLS survey doesn't change that calculus.
- Network effects. The whole XRPL DeFi ecosystem is small; this is a market-making and BD problem, not a technical one.

---

## 5. Concrete integration recommendations

In rough order of impact-per-effort:

1. **XLS-56 Batch — wire in for settlement emission.** When the post-hackathon work starts on periodic settlement from the enclave to XRPL, build it on `Batch ALLORNOTHING` from day one. Don't retrofit. Effort: small (it's a new XRPL tx type our signer needs to support). Value: large (atomic multi-leg settlement is the difference between "occasionally torn settlements that need reconciliation" and "transactionally clean").

2. **XLS-47 Oracles — publish our mark price.** On every funding interval, the enclave signs and submits an `OracleSet` writing the current mark price to a protocol-owned `PriceOracle`. Public, timestamped, audit trail. Cost: trivial. Value: cheap insurance against future "what mark price did you use at T?" disputes.

3. **XLS-47 Oracles — read external sources.** When other parties (Pyth on XRPL, RippleX feeds, Band) are publishing XLS-47 oracles for the assets we trade, fold them into our mark-price aggregator alongside the CEX feed. Use `get_aggregate_price` with trimmed-mean. Effort: moderate (oracle adapter inside the enclave). Value: moderate (resilience to single-source failures).

4. **XLS-70 + XLS-80 — design API auth so credential gating slots in cleanly.** Don't implement KYC now. But don't paint ourselves into a corner: when designing `/v1/auth` and the user identity model, leave a hook for "this account is in domain X" so adding the regulated tier later is one new module, not a rewrite.

5. **XLS-85 Token Escrow — keep on the watch list, build only if a use case forces it.** The user-side "deposit with safety hatch" pattern is plausible but not load-bearing; default to direct payments + enclave bookkeeping unless someone (Tom, an investor, a regulator) names a concrete reason to add the escrow indirection.

6. **XLS-65 Vault — revisit when LP onboarding becomes a real product line** (i.e., post-vAMM, when the question "where does the MM capital come from" needs a public answer). Not now.

7. **XLS-100 Smart Escrows — re-read every six months.** Especially when token escrow lands inside smart escrows, and especially when XLS-101 clarifies whether transaction emission or persistent contract state is on the table. Either of those changes the architecture trade space.

---

## 6. Open questions worth pursuing later

1. **XLS-101 published gas table.** Currently the spec says limits and fees are UNL-votable but does not commit to numbers. Once a draft gas table appears, re-price the cost of a per-fill state update under XLS-101 and verify the "structurally too expensive for CLOB perps" claim with real numbers rather than reasoning. ~~Couldn't retrieve XLS-101 in the previous pass~~ — done in this revision, see §2.11.

2. **XLS-94 Dynamic MPT and XLS-96 Confidential MPT** — both touch the asset model we'd be collateralising in. Skipped this pass; worth a one-paragraph review each.

3. **XLS-74 Account Permissions and XLS-75 Permission Delegation** — could be relevant for the operator/treasury model, especially the delegation piece. Worth a follow-up.

4. **Whether there's an XLS proposal for funding-rate-style periodic transfers.** I didn't find one. If one exists, it would directly affect how we model funding payment settlement on-ledger.

5. **What other teams are doing with XLS-100 or with XLS-101 prototypes.** If anyone publishes a smart-escrow-based or smart-contract-based options/futures product, that's the closest thing to a peer in the space and we should read their code.

6. **XLS-101 sub-ledger-close execution scheduling.** If a future revision of XLS-101 (or a separate proposal) ever introduces something resembling intra-ledger execution, scheduled callbacks, or sub-second contract triggering, the trade space changes materially. Currently no such proposal exists. Worth re-checking annually.

---

## Appendix A — Status snapshot of every XLS reviewed

| XLS | Title | Status | Relevance to us |
|---|---|---|---|
| 30 | AMM | Final, activated | Already analysed (`project_xrpl_amm_viability.md`) — not viable as our liquidity engine |
| 33 | Multi-Purpose Tokens | Final | Asset model for collateral; integrated as needed |
| 47 | Price Oracles | Final | **Integrate** as one input + publish mark price |
| 56 | Batch | Final, 2026-02 | **Integrate** for atomic settlement emission |
| 62 | Options | Stagnant | Ignore |
| 65 | Single Asset Vault | Draft | Watch — possible LP container later |
| 66 | Lending Protocol | Draft | Ignore — wrong shape |
| 70 | Credentials | Final | Integrate when KYC tier needed |
| 80 | Permissioned Domains | Final | Integrate when KYC tier needed |
| 81 | Permissioned DEX | Final | Ignore — spot only |
| 85 | Token Escrow | Final, activated 2026-02-12 | Watch — not load-bearing for us |
| 100 | Smart Escrows | Draft | Watch — re-read every 6mo |
| 101 | Smart Contracts | Draft (early Proposal, 2025-07) | Watch — adds tx emission + persistent state, but latency/economics still rule out CLOB perps |
| 102 | WASM VM | Draft | Substrate for 100/101 only |

## Appendix B — Cross-references to project memory

- `project_xrpl_amm_viability.md` — covers XLS-30 AMM and the vAMM design space; this document complements rather than replaces it.
- `project_post_hackathon_architecture.md` — Tom's plan; the recommendations here (XLS-56, XLS-47) are intended to slot into that plan, not replace it.
- `feedback_closes_must_route_clob.md` — the close-routing constraint; it's the reason XLS-85 cannot be our settlement primitive (third-party finish would be the only acceptable model and XLS-85 doesn't allow it).
- `project_perp_dex_xrpl.md` — the parent feasibility memo.
