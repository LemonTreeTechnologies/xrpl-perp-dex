# Vault design — follow-up questions and open issues

**Context:** follow-up to `docs/vault-requirements.md` (PR #3 by 8Baller).
**Author:** response from the rest of the team.
**Date:** 2026-04-09
**Status:** discussion — not blocking the merge of PR #3.

This document lists questions and gaps we spotted while reviewing the
vault API spec, plus the database-sync topic that came up in the
follow-up chat. None of this blocks merging `vault-requirements.md` —
we'd rather have the contract captured in the repo and iterate on it
here than argue inside a PR.

Each section is numbered so we can reference items in chat.

---

## Part A — Vault API spec questions

### A1. "Signs with session key" for withdrawals — which key is this?

The spec says:

> `POST /vaults/{vault_id}/withdrawals`: Vault checks user balance,
> creates XRPL transaction, signs with session key, submits to XRPL,
> returns tx hash.

In our current architecture withdrawals are **multisigned 2-of-3 by
three independent SGX enclaves** via XRPL's native `SignerListSet`.
There is no single "session key" that signs a withdrawal by itself.

Two possible readings:

1. **You mean the enclave's per-account `session_key`** (the internal
   auth token returned by `POST /v1/pool/generate`, used by the
   orchestrator to authenticate signing requests to its *own local
   enclave*). In that case the doc's phrasing is fine but misleading
   — the withdrawal actually results in a 2-of-3 multisig on XRPL,
   not a single signature. The "session key" is just one of the inputs
   to one enclave's `POST /v1/pool/sign`.

2. **You mean "the vault has one signing key that signs withdrawals
   on behalf of users"**, i.e. the vault is effectively custodial with
   a single-signer model. That would be a significant downgrade from
   the 2-of-3 model we built and proved in the failure-mode tests.

**Question A1:** which one did you mean? We'd recommend rewording the
spec to explicitly say "the orchestrator coordinates a 2-of-3 multisig
among the operator enclaves and submits the resulting MultiSigned
Payment to XRPL" to avoid future confusion.

### A2. Relationship to the existing user-direct REST API

Our current `orchestrator/src/api.rs` exposes:

- `POST /v1/orders` — user submits an order directly
- `GET /v1/account/balance` — user checks their own margin balance
- `POST /v1/withdraw` — user requests a withdrawal

These are **user-direct** endpoints. Each user has their own margin
state inside the enclave.

Your vault API is **vault-centric**:

- `POST /vaults/{id}/deposits`
- `POST /vaults/{id}/withdrawals`
- `GET /vaults/{id}/balance`

**Question A2:** do vaults *replace* direct user accounts, or are they
an additional product that lives alongside direct trading?

Three possible models:

1. **Pure vault model** — all users deposit into a vault, the vault
   trades on their behalf, users own shares. Direct `/v1/orders`
   goes away for retail users.
2. **Vault as LP layer** — retail users still trade directly via
   `/v1/orders` (counterparty for their own PnL). Vault is a separate
   product where LPs pool capital to provide liquidity / be
   counterparty to retail.
3. **Vault as a *type of user*** — the vault is technically just
   another user_id in the enclave with its own margin balance. The
   orchestrator can create orders "on behalf of" that user. Retail
   still uses `/v1/orders` directly. This is the simplest and keeps
   the existing code.

Model (2) is the HLP pattern from Hyperliquid and is probably what the
project wants long-term. Model (3) is the cheapest to ship first and
leaves the door open to (2).

### A3. Share-accounting invariants are not specified

The spec mentions `price-per-share` and `price-per-share-history` but
does not say how shares are minted/burned or how the NAV is computed.
Questions:

- **A3.1** What goes into NAV? Options:
  a. `cash_balance` only — shares just track cash deposits, easiest
     but hides unrealized PnL from LPs until positions close.
  b. `cash_balance + sum(open_position_unrealized_pnl)` — "mark-to-
     market" NAV, matches Yearn / Morpho / HLP. More correct but
     share price fluctuates every price tick.
  c. `cash_balance + sum(open_position_unrealized_pnl) + sum(funding_accrued)` —
     the fullest accounting, what an institutional LP would expect.
- **A3.2** On deposit, how many shares are minted? Standard formula:
  `new_shares = deposit_amount * total_shares / NAV`. This requires
  a non-zero `total_shares` to avoid 0/0 — how is the vault seeded?
  (Typical answer: first depositor gets shares equal to their deposit
  amount at PPS = 1, and a small amount of shares is dead-locked to
  prevent donation attacks.)
- **A3.3** On withdraw, is the withdrawal immediate at current NAV,
  or is there a **withdrawal queue / epoch**? Immediate withdrawals
  are vulnerable to NAV-spike front-running: an attacker who can see
  an incoming favorable price move can deposit just before and
  withdraw just after, stealing PnL from existing LPs. HLP-style
  vaults use 4-day delayed withdrawals for this reason. Yearn uses
  a "withdrawal fee" which is simpler but less robust.
- **A3.4** If a position gets liquidated, how do we handle the
  resulting loss in NAV? Does the share price drop (pro-rata loss
  socialized) or is there a separate "insurance fund" that absorbs
  some of the loss first?

**Suggested outcome:** add a section `## Accounting` to
`vault-requirements.md` that answers at least A3.1, A3.2, A3.3.

### A4. Orchestrator `POST /create-order` semantics

The Operator API has:

> `POST /vaults/{vault_id}/create-order`: Endpoint for orchestrator
> to create a new order on the XRPL.

This implies the orchestrator places orders **on behalf of the vault**
as a market participant. In the HLP model, this is how the vault earns
— it provides liquidity to the CLOB and takes the other side of
retail trades.

**Question A4.1:** what's the vault's trading strategy? Is it a
naive market-maker (always quote both sides), a delta-neutral
strategy (hedge retail exposure), a passive LP (just provide
liquidity at mid-price), or something else? The spec doesn't say.

**Question A4.2:** does the vault's orderbook participation go
through the **same CLOB** as retail orders (matching against retail),
or is there a separate flow (e.g. RFQ / internalization)? The
cleanest design is the same CLOB — the vault is just another user_id.

**Question A4.3:** how does the orchestrator decide **when** to call
`create-order`? Is there a background strategy loop? Is this
triggered by external signals (price feed) or by retail order flow
(e.g. rebalance after each fill)?

### A5. Missing production fields

Production vaults in DeFi typically include these fields that are not
in the current spec. Listed in order of importance:

1. **Management fee** (annualized, taken from NAV). Example: 2% p.a.
2. **Performance fee** (on profits above high-water mark). Example:
   20% of PnL above HWM.
3. **Capacity limit** (max NAV the vault accepts). Prevents
   strategy degradation when the vault is too large.
4. **Per-vault fee account** for the fee accrual (typically a
   multisig controlled by the protocol).
5. **Withdrawal queue** (see A3.3).
6. **Pagination** on `/transactions`, `/orders`, `/trades`. Without
   it, operators are one 10K-trade user away from OOM.
7. **KYC / whitelist** field on vaults — institutional vaults need
   allowlisted depositors.
8. **Inception date / first-deposit timestamp** — needed for fee
   accrual calculations.

**Suggested outcome:** add a new section `## Fields not in v1` that
explicitly defers these with a short rationale for each. That way we
don't ship something that will fight us at launch.

### A6. Admin auth is not specified

The Admin API has:

- `POST /admin/vaults` — create
- `DELETE /admin/vaults/{id}` — delete
- `POST /admin/vaults/freeze` / `unfreeze` — emergency pause

but the spec is silent on **who** can call these. Three options:

1. **JWT / API token** — simple but a stolen token is catastrophic.
2. **Signed requests** (X-XRPL-Signature from a specific admin XRPL
   address). Matches our existing user auth pattern.
3. **Multisig** between the 3 operators — the admin endpoint only
   succeeds if at least 2 operators sign off. Matches the security
   model of the custody side. Higher friction for routine ops.

**Question A6:** which model? For freeze/unfreeze specifically, we
should use option 3 — a freeze is a high-impact action. For
create/delete in day-one, option 2 (signed requests from a known
admin address) is probably enough.

**A6.1:** Also, audit trail. Freeze/unfreeze must log
`(who, when, why)` — the request body should include a `reason`
string, and the vault should persist it for the post-mortem.

### A7. `DELETE` vs deprecate

> `DELETE /admin/vaults/{vault_id}`: Deletes a vault. Only allowed
> if vault has no users and zero balance.

In practice, vaults are **deprecated**, not deleted. Reasons:

- Historical tx references the vault_id — if we delete it, old
  `GET /vaults/{id}/transactions` becomes a 404 for legitimate
  historical queries.
- Audit trail — "what happened to vault X" should be a queryable
  record forever.
- Share-holder repayment — even a "zero balance" vault may have had
  share-holders whose history must stay accessible.

**Suggested change:** rename to
`POST /admin/vaults/{id}/deprecate` (soft delete, still queryable,
blocks new deposits/withdrawals). Keep DELETE out of the spec
entirely, or reserve it for vaults that never received a single
deposit.

---

## Part B — Database synchronization

### B1. Short answer to "are we syncing the databases?"

**No, we are not.** The current architecture treats PostgreSQL as a
**per-operator local cache of historical data**, not as authoritative
state. Authoritative state lives in:

1. **SGX enclave sealed state** — margin balances, open positions,
   funding accruals. This is the source of truth for "what does a
   user own right now."
2. **In-memory orderbook** inside the Rust orchestrator — sequencer
   owns it, validators replay it from P2P batches.

PostgreSQL stores derivative data: `trades`, `funding_payments`,
`deposits`, `liquidations`. These are written **only on the operator
that handled the originating event**, which means:

| Event | Where it's written to PG |
|---|---|
| Trade executed | `api.rs::submit_order` — **sequencer only** |
| Liquidation | `main.rs::run_liquidation_scan` — **wherever the scan runs (sequencer)** |
| Deposit observed on XRPL | `main.rs::deposit monitor` — **wherever the monitor runs** |
| Funding payment applied | `main.rs::funding loop` — **wherever the loop runs** |

The validator batch replay path in `main.rs` replays fills into its
local enclave via `validator_perp.open_position()` **but does NOT
write to PostgreSQL**. So validators have a gap in PG for any period
they were the non-sequencer.

### B2. Why we're not syncing — intentional or accidental?

Honestly, partly both:

- **Intentional:** we didn't want PG to be in the critical path for
  consensus. If two operators disagree on a trade history row, we
  didn't want that to block settlement.
- **Accidental:** we simply didn't wire the write paths into the
  validator replay loop. It was trivially easier to write PG from
  `submit_order` only, since the sequencer is always the one with
  the request in hand.

The result is correct for the current usage (single live sequencer on
Hetzner, three Azure validators, nobody queries the validators for
trade history) but **wrong for the vault work** — share-price history
and transaction history need to be consistent regardless of which
operator a user is reading from, because vault users don't know and
shouldn't have to know which operator is the current sequencer.

### B3. Options to fix this

Three clean options, in order of how invasive they are:

#### Option B3.1 — Passive replication via validator replay (recommended)

Wire the PG writes into the validator batch replay loop, the same
way they're wired into `submit_order`. Every operator receives the
same batches from the sequencer and writes the same rows locally.
No external replication primitive. Each PG is independent but
convergent.

Pros:
- ~20 lines of Rust code
- No external tooling (no Postgres streaming replication, no Debezium)
- Reuses the existing P2P batch protocol
- If an operator becomes sequencer after a failover, its local PG
  already has the full history it needs.

Cons:
- Each operator's PG is independent — if one operator's disk fills
  up, it drops behind the others. Needs monitoring.
- Deduplication concern: if a batch is replayed twice (e.g. restart
  after partial write), we'd insert duplicate rows. Solved by making
  `trade_id` the primary key with `ON CONFLICT DO NOTHING` in the
  `insert_trade` SQL.

#### Option B3.2 — PostgreSQL streaming replication

Make one operator's PG the primary, others replicate via built-in
Postgres streaming replication. Applications only talk to the primary
for writes; reads can fan out.

Pros:
- Postgres-native, battle-tested, well-documented
- Failover between primaries is a standard problem with known
  solutions (Patroni, repmgr)
- Perfect consistency

Cons:
- Requires an external tool (Patroni or similar) for primary-election
- If the primary dies, writes block until a new primary is elected —
  conflicts with our design of "sequencer can die, cluster keeps
  running"
- Primary is a single point of failure for writes
- Cross-datacenter latency (Hetzner ↔ Azure ↔ Azure) is non-trivial,
  streaming replication adds a round-trip
- Operational burden: backup, monitoring, WAL archives, etc.

#### Option B3.3 — XRPL-as-source-of-truth

Derive all historical data from XRPL `account_tx` history plus
deterministic replay of enclave events. Each operator rebuilds its PG
on demand from XRPL + local enclave state.

Pros:
- Theoretically purest — no trust in operator PGs at all
- Works even after catastrophic loss of all operators (scenario 3.9
  already tested)

Cons:
- Slow — `account_tx` pagination is ~200 tx/request
- XRPL only records settlement (deposits, withdrawals, SignerListSet
  updates), not individual trades inside our CLOB. So this only
  covers deposits/withdrawals, not trade/funding history
- Would need a second "indexer" service that subscribes to XRPL
  stream and rebuilds PG on demand
- Complicates the architecture for marginal benefit

### B4. Our recommendation

**Go with B3.1 (passive replication via validator replay).** Here's
the concrete change:

1. In `orchestrator/src/main.rs`, inside the validator batch replay
   loop (around line 400), after each fill is replayed into the
   enclave, also call `db.insert_trade(...)` on the local PG.
2. In `orchestrator/src/db.rs`, change `insert_trade` to use
   `INSERT INTO trades (...) VALUES (...) ON CONFLICT (trade_id) DO NOTHING`
   so duplicates are no-ops (needed because the sequencer also calls
   `insert_trade` from `submit_order` and we don't want double-writes
   when the sequencer receives its own batch back from gossipsub).
3. Do the same for `insert_liquidation` and `insert_deposit` if we
   want those to be consistent across operators as well.

This can ship as a single PR. Size: ~30 lines changed, 0 new
dependencies. Once merged, all 3 Azure operators will have
identical trade/liquidation history and vault-level views will work
regardless of which operator the user hits.

**Caveats for B3.1:**

- New operators (like when we add a 4th for scaling) need to
  **backfill** their PG from somewhere. Two options:
  - From the old XRPL `account_tx` stream (slow but trustless)
  - From another operator's `pg_dump` (fast but trusted)
  - Accept the gap and treat the new operator's history as
    "available from <its join time>" — probably OK for v1.
- If the orchestrator crashes mid-batch-replay after writing some PG
  rows but before crashing, the `ON CONFLICT DO NOTHING` makes the
  restart safe: the same batch comes back over gossipsub (or from
  the sequencer's retransmit) and is idempotently re-applied.

### B5. What this means for the vault spec

For the vault API to work correctly in a multi-operator deployment:

- **Deposits and withdrawals** already hit XRPL and are observable
  by every operator via the deposit monitor. These will be consistent
  after B3.1 because `insert_deposit` will also be called from the
  validator path.
- **Price-per-share history** can be computed deterministically from
  NAV at each block. If every operator has the same trade history
  and the same enclave state, they'll all compute the same PPS. So
  `GET /vaults/{id}/price-per-share-history` is safe to answer from
  the local PG on any operator.
- **User transactions in vault** — the `GET /vaults/{id}/transactions`
  endpoint needs user deposits and withdrawals tagged with the vault
  they came from. That requires a new `vault_id` column on the
  `deposits` and `withdrawals` tables, which is a schema migration.
  Not hard, just needs to happen before the vault code ships.

---

## Summary for 8Baller

If you'd rather skip to the punchline:

1. **Vault API spec** is fine to merge as-is. Good structure, right
   patterns (share-based accounting, User/Operator/Admin split,
   freeze mechanism). Questions A1–A7 above are not blockers but
   would be good to answer before we start coding the vault module.
2. **"Signs with session key"** phrasing in A1 is the one thing
   worth fixing in the spec itself — other readers may take it to
   mean single-signer withdrawal which isn't what we do.
3. **DB sync**: not done today, but cheap to fix (option B3.1,
   ~30 lines of Rust). Before you ship the vault code, please ping
   us to land that first, or we'll have to build the vault on top of
   an inconsistent foundation.
4. **Nothing here is urgent.** The grant application, hackathon pitch,
   and failure mode tests don't depend on vault work. This is
   infrastructure for the post-grant roadmap.

Feel free to reply inline in this file (it's in the repo, just
commit on top), or in chat. Whatever is easier.
