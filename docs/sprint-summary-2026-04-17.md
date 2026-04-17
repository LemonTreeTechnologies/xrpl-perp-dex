# Engineering Sprint Summary — 2026-04-17

**Audience:** Tom (8Baller) + anyone onboarding to the project.

---

## What was done

| # | Task | Repos / commits | What changed | Why it matters |
|---|------|----------------|-------------|----------------|
| 1 | **shard_id plumbing** | enclave `e8baf46`, orchestrator `e3b2f11` | `shard_id` field in PerpState and sealed metadata. Sealed files namespaced as `s{id}_perp_*.sealed`. New `ecall_perp_set_shard_id` + REST endpoint. `ShardRouter` in orchestrator with `shards.toml` config. | Capacity planning requires sharding from Phase 1 — retrofitting later is a rewrite. Code runs `shard_id=0` today; adding shards is a config change, not a code change. |
| 2 | **Multisig flow in Rust** | `orchestrator/src/withdrawal.rs` (already complete) | Full 2-of-N multisig flow: margin check in local enclave -> autofill Sequence/Fee from XRPL -> `multi_signing_hash` per signer -> DER encode -> sort by AccountID -> `submit_multisigned`. | Was ported from Python `tests/multisig_coordinator.py` in a previous sprint. Verified complete — no additional work needed. This is M1 ($22.5K) in the grant application. |
| 3 | **Funding payments persistence** | enclave `622818f`, orchestrator `9297cd8` | `ecall_perp_apply_funding` now returns per-position payment JSON array (user_id, position_id, side, payment). Orchestrator parses it and calls `insert_funding_payment()` for each entry. | Funding was applied to enclave positions but never persisted to Postgres — analytics, user-facing funding history, and audit trail were empty. |
| 4 | **Closes route through CLOB** | orchestrator `978324c` | `POST /v1/positions/close/{id}` now submits a reduce_only IOC market order on the opposite side through the order book. Fill settlement calls `close_position` (not `open_position`) when taker has `close_position_id`. Returns 409 if no liquidity. | Previously bypassed the CLOB and closed at mark price — no price discovery, no interaction with resting orders or your arb bot. **This is a precondition for your arb bot to work on the close path.** |
| 5 | **Partitioned sealing** | enclave `8195d5c` | `MAX_PERP_USERS` 500 -> 5000, `MAX_PERP_POSITIONS` 800 -> 8000, `MAX_TX_HASHES` 500 -> 5000. Generic `seal_array`/`unseal_array` chunking (<=60KB per `sgx_seal_data` call). HeapMaxSize 4MB -> 8MB. | SGX `sgx_seal_data()` has a ~64KB limit per call. Chunked sealing removes this ceiling and supports the shard capacity targets. |

---

## What's blocked on Tom

We've completed all tasks that can be done independently. The remaining work requires a design document from Tom before we can start implementation.

### Required document: Post-hackathon liquidity architecture

**Status:** Not received as of 2026-04-17.

After the Paris hackathon (2026-04-12), Tom indicated the current MM vault is "way too crude" and proposed replacing it with AMM-pool-style liquidity + maker rebates + his external arb bot. We aligned on **Variant A**: the pool posts to the CLOB, the CLOB remains the execution layer (see `docs/clob-vs-amm-alignment.md`).

However, the implementation details are undefined. Short fragments from chat ("rip off the sizing and pricing maths", "so b virtual amm") leave critical ambiguity. Before we write any code, we need a single written document that answers:

| # | Question | Why it blocks us |
|---|----------|-----------------|
| 1 | **(b1) universal-counterparty vAMM, or (b2) curve-quoted orders posted to the CLOB?** | These are completely different architectures. (b1) changes the matching engine, adds vAMM state to the enclave, requires insurance fund math. (b2) is a `vault_mm.rs` rewrite with no enclave changes. Weeks vs months of work. |
| 2 | **Which curve?** Constant-product, concentrated-liquidity, hybrid? Parameters? | Determines the math in the pricing module. |
| 3 | **Reference price source for the curve center** — own mark price, external CEX feed, hybrid? | Determines whether we add an oracle dependency. |
| 4 | **Where does the pool's position state live and how is it margined?** | In (b2) the pool has real margin and can be liquidated. In (b1) the pool is virtual. |
| 5 | **How does the matching engine route a taker order?** Through CLOB, through curve, or both with what precedence? | Changes the hot path of the trading engine. |
| 6 | **Insurance fund: load-bearing or backstop?** Accumulation strategy? Circuit breakers? | In (b1) this is critical — it's what killed Perpetual Protocol v1. |
| 7 | **Rebate model** — protocol-level or pool-internal? Negative-fee path or separate accrual? | Touches fee logic in the enclave. |
| 8 | **Tom's arb bot: assumed always-on?** What's the failure mode if it stops? | Affects risk parameters and monitoring requirements. |
| 9 | **Margin system review** — Tom said he needs to "understand the margin system way more." | If margin params change, it affects every position and all risk calculations. |
| 10 | **Migration plan** from current `vault_mm.rs` ladder bot to the new design. | Determines whether we can deploy incrementally or need a hard cutover. |

### What we can do in the meantime

These items do NOT require Tom's document:

- **Component-level failure testing** (9 scenarios from doc 07, section 2) — HAProxy, orchestrator crash, validator disconnect, P2P partition, etc. Tests run on Azure 3-node cluster.
- **Operational cleanup** — send grant intro email, Hetzner persistent peer_id, Azure VM cost management.
- **Documentation** — capacity planning is done, alignment doc is done, architecture comparisons are done.

But **none of the liquidity/vault/pricing code** can move forward until the design document arrives.
