# Capacity Planning — User Count → Hardware Cost

**Status:** Forecast document for business-plan hardware budgeting.
**Date:** 2026-04-13
**Audience:** PM / finance / ops colleagues sizing the production deployment.
**Russian version:** [`capacity-planning-ru.md`](./capacity-planning-ru.md)

**Scope.** How many users can the current perp DEX architecture serve per box, how does that scale up, and what does it cost on Hetzner hardware at each tier.

**Companion documents:**
- [`sgx-vs-tdx-roi.md`](./sgx-vs-tdx-roi.md) — per-node price reference (Hetzner vs Azure vs OVH, SGX vs TDX)
- [`sgx-enclave-capabilities-and-limits.md`](./sgx-enclave-capabilities-and-limits.md) — Part 8 on EPC size and what actually lives in the enclave
- [`comparison-arch-network.md`](./comparison-arch-network.md) — why we run a real CLOB in the enclave instead of a vAMM

---

## TL;DR — user count to hardware budget

**Architectural constraint: the design is sharded from day one.** A single cluster of 3 SGX nodes is the *smallest unit* of capacity, not the *only* unit. Phase 1 runs one shard (one cluster of 3 nodes); growth means adding more shards, not buying bigger boxes. The code must know about `shard_id` from the beginning — retrofitting sharding into a live DEX is a rewrite, so we pay the architectural cost up front.

| Phase | Target users | Active traders | Shards × nodes/shard | Hetzner hardware per node | Cost/year | Code work |
|---|---|---|---|---|---|---|
| **MVP / demo** | ≤ 500 | ≤ 100 | 1 × 1 (no multisig yet) | EX44 Xeon E-2388G | ~€600 | none — fits current code |
| **Early production** | 2 000 – 5 000 | 200 – 500 | **1 × 3** | EX44 Xeon E-2388G | ~€2 500 | `shard_id` plumbing + partitioned sealing |
| **Growth** | 10 000 – 50 000 | 1 000 – 5 000 | **1 × 3** (single shard, multi-threaded) | EX44 Xeon E-2388G | ~€2 500 | multi-threaded enclave (TCSNum ≥ 4) |
| **Scale** | 50 000 – 200 000 | 5 000 – 20 000 | **2–4 × 3** (multiple shards) | EX44 Xeon E-2388G | ~€5 000 – 10 000 | cross-shard router; shard-aware vaults |
| **Hyperscale** | 200 000 – 1 M | 20 000 – 100 000 | **5–20 × 3** | EX44 or EX101 | ~€15 000 – 60 000 | cross-shard settlement, operational tooling |
| **Ceiling check** | 1 M+ | 100 000+ | 20+ × 3 | EX101 / EX130 tier | €60 000+ | global funding coordinator, shard rebalancing |

All Hetzner prices are monthly rentals, 2026 price levels, Euro → rounded to USD at ~1.08. Numbers are for the SGX-signing plane only (orchestrator + enclave); they exclude database, nginx frontend, monitoring, and ops labour.

**Key insight:** every phase above Phase 1 uses the same cheap EX44 hardware. Scaling happens by adding shards (horizontal, linear), not by renting more expensive boxes. The hardware line stays flat; the shard count grows.

---

## Part 1 — What "a user" actually costs

### 1.1 State per user (from code)

All numbers below are read directly from `Enclave/PerpState.h` in the enclave repo. The enclave state is a fixed-size C struct — there's no heap, no dynamic allocation.

**Per-user record (`PerpUser`), PerpState.h:53–60:**

```c
typedef struct {
    char user_id[USER_ID_SIZE];  // 36 B
    int64_t margin_balance;      //  8 B
    int64_t xrp_balance;         //  8 B
    int64_t staked_xrp;          //  8 B
    int64_t points;              //  8 B
    bool is_active;              //  1 B
} PerpUser;                      // ~72 B raw → ~80 B with alignment
```

**Per-position record (`PerpPosition`), PerpState.h:62–71:**

```c
typedef struct {
    uint32_t position_id;        //  4 B
    char user_id[USER_ID_SIZE];  // 36 B
    uint8_t side;                //  1 B
    uint8_t status;              //  1 B
    int64_t size;                //  8 B
    int64_t entry_price;         //  8 B
    int64_t margin;              //  8 B
    int64_t realized_pnl;        //  8 B
} PerpPosition;                  // ~74 B raw → ~80 B aligned
```

The comment at PerpState.h:18 is authoritative: `800 × ~74B = ~58KB < 64KB seal limit`.

**Derived per-user memory cost:**

| Assumption | Value |
|---|---|
| Avg open positions per active user | 1.6 |
| PerpUser + 1.6 × PerpPosition | 80 + 128 = **~208 B** |
| 10 000 users | ~2 MB |
| 100 000 users | ~20 MB |

Compared to the 128 MB EPC on current Hetzner E3 hardware (Part 8 of `sgx-enclave-capabilities-and-limits.md`), **memory is not the bottleneck**. Even a 100× user-base increase stays comfortably inside EPC.

### 1.2 The real limit: 64 KB seal partitions

The binding constraint on `MAX_PERP_USERS` / `MAX_PERP_POSITIONS` is **not EPC size**, it's the **64 KB limit per sealed partition** that the current code is built around (comment at PerpState.h:16: *"Capacity limits — partitioned sealing (each part <64KB) allows larger limits"*).

The current hard-coded ceilings are set to sit just under that 64 KB boundary per part:

| Array | Entry size | Count | Part size |
|---|---|---|---|
| `users[]` | ~80 B | 500 | 40 KB |
| `positions[]` | ~80 B | 800 | 64 KB |
| `processed_tx[]` | ~40 B | 500 | 20 KB |

To raise the ceilings you do not need a bigger CPU — you need **more partitions** (N × 64 KB parts sealed separately), which is a code change inside the enclave, not hardware. The sealing API is partitioned precisely to allow this.

### 1.3 Throughput per box

From the Phoenix PM benchmark numbers in `sgx-vs-tdx-roi.md` §4, reconfirmed by the current code path:

| Operation | Cost |
|---|---|
| ECDSA secp256k1 sign (libsecp256k1) | 1 – 5 ms |
| Margin check (fixed-point arithmetic) | < 1 ms |
| Seal sub-partition to disk | 5 – 10 ms |
| **Total `open_position` / `close_position` round-trip** | **~10 ms** |

Enclave is single-threaded today (`TCSNum = 1`). Per box:

**~100 enclave operations per second, sustained.**

Order matching (CLOB) runs **outside** the enclave in the Rust orchestrator — the enclave is not on the matching hot path, it only validates margin and signs withdrawals. So the 100 ops/sec ceiling only binds `open_position`, `close_position`, `withdraw`, `liquidate` — roughly one call per trade fill plus one per withdrawal.

### 1.4 Converting ops/sec to user count

```
100 ops/sec × 86 400 s/day = ~8.6 M enclave ops/day
```

If the average active trader generates 10 enclave ops/day (open, close, adjust, withdraw), one single-threaded enclave box sustains **~860 000 trade-ops/day** at 100 % utilisation.

At realistic utilisation (account for peak hours, news spikes, liquidation cascades — typical factor of 5–10× headroom), the planning number is:

> **One current-generation box ≈ 50 000 – 100 000 active traders/day.**

Beyond that, you have to either multi-thread the enclave, replace the CPU with something faster, or shard state across multiple enclaves.

---

## Part 2 — What is *not* the bottleneck

It's worth being explicit about what scaling work is **not** needed, because these are the obvious things people assume are problems:

| Resource | Status | Reason |
|---|---|---|
| EPC memory | Not limiting | 128 MB on E3 fits 100 k+ users; upgraded boxes have 256–512 MB |
| Disk | Not limiting | Sealed state is < 1 MB total; sealing is the slow op, not the I/O |
| Order matching | Not limiting | Happens in orchestrator (Rust), plain in-memory CLOB, ~10 µs per match |
| Network | Not limiting | XRPL submit + WebSocket fanout at user rates, well under 1 Gbit |
| FROST signing | Not used on the signing hot path | Withdrawals use XRPL-native 2-of-3 multisig (three independent ECDSA signers), not FROST aggregation. No 3-round crypto to orchestrate per trade. |
| Database | Not limiting for enclave | Postgres holds off-enclave history only |

**The only binding constraint is single-threaded enclave ECDSA+seal throughput.** Hardware sizing follows directly from that.

---

## Part 3 — Hetzner hardware reference

Hetzner is the bare-metal reference because (a) the existing deployment is already there, (b) bare metal gives us the smallest monthly cost for SGX, and (c) the hardware is controllable (CPU choice, no cloud-tax).

### 3.1 Hetzner dedicated server lines (2026, SGX-relevant only)

Hetzner sells several dedicated server families. Only the Intel Xeon–based ones matter for SGX; the AMD AX line (Ryzen/EPYC) has no SGX at all. Prices below are reference rentals from Hetzner's 2026 dedicated-server catalogue — actual availability and price tiers change, so treat these as planning numbers, not quotes.

| Server line | CPU | SGX gen | Launch Control / DCAP | EPC | RAM | ~Rental | Notes |
|---|---|---|---|---|---|---|---|
| **EX44** (current) | Xeon E3-1275 v6 (Kaby Lake) | SGX1 | **No** (pre-Coffee Lake) | 128 MB | 64 GB | ~€50/mo | Current Hetzner box. Signing works, DCAP does not. |
| **EX44** (new order) | Xeon E-2388G (Rocket Lake 2021) | SGX2 | **Yes** | 512 MB | 64 GB | ~€60–70/mo | Full DCAP, in-kernel driver. Minimal upgrade path. |
| **EX64-NVMe / EX52** | Xeon E-2486 (Alder Lake 2022) | SGX2 | Yes | 512 MB | 64–128 GB | ~€70–90/mo | Newer core, similar SGX capabilities. |
| **EX101** | Xeon Gold 5412U (Sapphire Rapids 2023) | SGX2 (server) | Yes | up to 512 MB per socket | 128 GB | ~€180–220/mo | Xeon Scalable: bigger EPC, more cores, much higher sustained throughput. |
| **EX130** | Xeon Gold 6526Y or similar | SGX2 (server) | Yes | up to 1 GB per socket | 256 GB | ~€280–350/mo | Top-tier Intel dedicated; SGX on the Xeon Scalable line with large EPC. |
| **EX130-S** / AX103 / DX293 | Newer Sapphire/Emerald Rapids | SGX2 (server) | Yes | multi-GB EPC | 256–512 GB | ~€400–600/mo | For capacity headroom well beyond anything the code currently exploits. |
| **Server Auction** (used) | mixed Xeon E | depends | depends | depends | depends | €30–50/mo | Discounted used hardware; use only for test/staging, not production. |
| **AX line** (AMD) | Ryzen / EPYC | **none** | n/a | n/a | n/a | n/a | **No SGX — not usable for the enclave.** Listed only so no one orders one by mistake. |

**Confidence note.** The Xeon E-class rows (EX44 tier) are the ones the sister project has actually deployed and benchmarked — those numbers are solid. The Xeon Scalable rows (EX101/EX130) are reasonable planning numbers based on Hetzner's general Intel dedicated-server catalogue; before committing budget for those tiers, someone should check Hetzner's current offers page and confirm that the specific SKU advertises SGX enabled and Launch Control supported (not all Xeon Scalable servers ship with SGX activated in BIOS).

### 3.2 Hetzner cloud (CCX / CX)

Hetzner Cloud is not an option for the signing plane — SGX is not exposed on any Hetzner Cloud instance type. It remains useful for:

- Orchestrator / order router only (no signing) — cheap CCX instances are fine.
- Monitoring, Grafana, logs — small CX instances.
- Public-facing WebSocket/REST endpoints offloaded from the enclave box.

Don't budget Hetzner Cloud lines for capacity planning against the enclave; budget them as auxiliary infrastructure (roughly €30–100/mo total for the whole deployment).

### 3.3 Why Hetzner is strictly cheaper than the alternatives

For a given number of SGX-attested operator nodes:

| Provider | 3-node yearly cost | Bare metal? | DCAP |
|---|---|---|---|
| **Hetzner EX44 (E-2388G)** | **~€2 500** | Yes | Yes (post-upgrade) |
| Azure DCsv3 | ~$4 977 | No | Yes |
| OVH HGR-HCI-i1 (TDX) | ~$34 200 | Yes | Yes (TDX, not SGX) |
| Azure DC2es_v5 (TDX cloud) | ~$2 489 | No | Yes (TDX) |

Numbers reproduced from `sgx-vs-tdx-roi.md` §1. For **SGX specifically** on bare metal, Hetzner is the cheapest option by a factor of ~2× vs Azure cloud SGX and ~14× vs OVH TDX bare metal. There is no cheaper SGX-capable dedicated-server provider currently known to the team.

---

## Part 4 — Shard-first architecture

**Core principle: a 3-node cluster is the unit of capacity, not the whole system.** The system is a set of `N` independent 2-of-3 SGX clusters (shards), coordinated by an off-enclave router. Phase 1 ships with `N = 1`. Every subsequent phase increases `N`. No phase ever requires a larger-than-EX44-class box to reach its capacity target.

This is a hard constraint: **any implementation detail that makes `N = 1` easier but makes `N = k > 1` harder is rejected.** We pay the sharding design cost at Phase 1, even when there's only one shard live, so that Phase 3+ is an operations problem (deploy more clusters) rather than an engineering problem (rewrite the state layer).

### 4.1 Why not just buy bigger boxes

The obvious alternative — keep one cluster, rent an EX130 Xeon Gold, multi-thread the enclave, and serve everybody from it — fails for three reasons:

1. **It caps out.** Even a fully multi-threaded 16-core Xeon Scalable enclave running at ~1500 ops/sec sustained hits a wall around ~150 k active traders. That's a ceiling, not a runway.
2. **It concentrates blast radius.** One cluster means one sealed-state blob per operator. If that state gets corrupted (hardware failure, sealing bug, operator mistake), every user on the exchange is affected simultaneously. Sharded deployments isolate failures to one shard's users.
3. **It prices out decentralisation.** A Phase-1 operator running three EX44 boxes at €210/month total can afford to be a real node. A Phase-3 operator forced onto EX130 boxes at €1 050/month/cluster starts pricing out smaller operators, which degrades the multisig's independence property.

Horizontal sharding on cheap hardware beats vertical scaling on expensive hardware on all three axes.

### 4.2 What gets sharded

The shard key is **user_id** (XRPL r-address, hashed). Every user lives in exactly one shard. Every position the user opens lives in the same shard. Every margin calculation for that user is done inside that shard's enclave.

What is **not** sharded:

- **Markets** (the perp pair — XRP/RLUSD today, BTC/RLUSD later). A single market's CLOB matching engine is global, in the orchestrator layer, outside any enclave. All shards see the same mark price, the same funding rate, the same index.
- **Vaults** (Liquidation, HLP, Delta0, Delta1). These are logically global, but their state is owned by a designated "vault shard" (shard 0 by convention). Cross-shard vault access goes through the router.
- **Withdrawals.** XRPL-native 2-of-3 multisig is per-shard. Each shard holds its own multisig key set; withdrawal signing happens inside the shard that owns the user.

The split is chosen so that **the 99 % hot path — `open_position` / `close_position` / `margin_check` — is entirely local to one shard.** Cross-shard coordination is needed only for global funding application and vault rebalancing, which run on a slow clock (hours, not milliseconds).

### 4.3 The router layer

In front of the shards is a thin stateless **shard router** in the orchestrator:

```
          ┌─────────────┐
User ──▶  │   Router    │ ──▶ shard_id = hash(user_id) % N
          └─────────────┘             │
                                      ▼
                         ┌────────┬────────┬────────┐
                         │Shard 0 │Shard 1 │Shard N │
                         │3 nodes │3 nodes │3 nodes │
                         │SGX ENC │SGX ENC │SGX ENC │
                         └────────┴────────┴────────┘
```

The router:
- holds no state (the shard map is a static config, reloaded on shard add/remove)
- does not see keys, does not see positions — it only dispatches
- is horizontally replicable without concerns

Adding a shard = adding three rows to the shard map and deploying three new EX44 boxes. That's the entire operational cost of +50 k active users.

### 4.4 How shards get added without downtime

Adding a shard is the one cross-shard operation that must be handled carefully. The supported model:

1. **New users only on new shards.** Rebalancing existing users across shards is not supported — once a user is assigned to shard `k`, they stay on shard `k` for the account's lifetime. Adding a new shard only changes the hash function's output range for *newly onboarded* users.
2. **Old shards drain naturally.** If an old shard fills up and you want to relieve it, stop onboarding new users to it (weight its hash bucket to zero); existing users churn out over time; eventually the shard is small enough to retire or merge.
3. **No shard splits.** We don't support "take shard 0 and split it into 0a and 0b". That operation would require moving sealed state between enclaves, which forces a re-seal under a new MRENCLAVE identity — doable, but complex enough that it's explicitly out of scope for this plan.

This makes the shard count **monotonically non-decreasing**, which is the simplest lifecycle and matches the growth model (users arrive faster than they leave).

### 4.5 Within-shard scaling axes

Inside each shard, all three levers from the previous draft still apply — they just now determine **how much one shard can do**, not how much the whole system can do:

- **Vertical (faster CPU):** E3 v6 → E-2388G gives ~1.5× per-op speedup and SGX2/DCAP.
- **Multi-threading (`TCSNum > 1`):** the highest-leverage single change — 1–2 weeks of enclave work buys 4–8× throughput per box. **Mandatory before Phase 2.**
- **Within-shard replication (2-of-3 multisig):** gives fault tolerance and decentralisation, not throughput.

Per-shard throughput ceiling with multi-threading on EX44 E-2388G: **~400–800 ops/sec sustained**, translating to **~40 k – 80 k active traders/shard at realistic utilisation**. That's the working planning number: **one shard ≈ 50 k active traders**.

### 4.6 What sharding does *not* solve

Some things are global by construction and cannot be sharded away:

- **Global mark price and index.** A single XRP/RLUSD mark price feeds all shards. If the price oracle or the CLOB mid is wrong, every shard is wrong simultaneously.
- **Global funding rate.** Computed once per funding interval (typically 1h or 8h), then applied inside each shard to that shard's open positions. Cross-shard coordination point.
- **Insurance fund / liquidation vault.** Global liquidity pool backing all shards. Cross-shard settlement at liquidation events.
- **Cross-shard transfers.** Not supported in the hot path. A user moves funds between shards only by withdrawing from one and depositing into another as a new account — this is explicit, not a DEX-internal operation.

The hot path stays shard-local. Cross-shard is reserved for the slow infrequent operations where the extra coordination cost is amortised over many users.

---

## Part 5 — Growth phases and recommendations

Every phase below deploys the same shape: `N` shards × 3 nodes. `N` grows over time, per-node hardware mostly does not. The per-node box stays on the cheap Xeon E line (EX44 tier) until well into hyperscale.

### Phase 0 — Current (demo / hackathon)

- **Shards:** 0 (no multisig; single-node deployment).
- **Hardware:** 1 × Hetzner EX44 (Xeon E3-1275 v6), current box.
- **Capacity:** 500 users hard-coded, ~50 active traders tested.
- **Cost:** ~€50/mo = **~€600/year**.
- **DCAP:** no (Kaby Lake has no Launch Control).
- **Status:** sufficient for the Paris demo (2026-04-12) and the grant deliverable. Not suitable for real production traffic — no remote attestation means clients cannot verify the operator is running the audited binary.

### Phase 1 — First shard in production (up to ~5 k users, ~500 active)

- **Shards:** `N = 1`. Three SGX nodes, 2-of-3 XRPL multisig.
- **Hardware per node:** Hetzner EX44 with Xeon E-2388G (DCAP-capable, SGX2).
- **Cost:** ~€60–70/mo × 3 = **~€2 500/year**.
- **Code work required — and this is the load-bearing phase for sharding:**
  1. Add `shard_id` to every enclave data structure, every REST endpoint, every sealed state partition. Even with one shard live, the code must route through the router and stamp `shard_id = 0` everywhere. Retrofitting this later is a rewrite; doing it now is ~1 week. **This is not optional.**
  2. Build the **shard router** in the orchestrator. Static config file `shards.toml` listing shard IDs → node endpoints. One entry today, arbitrary number tomorrow.
  3. Raise `MAX_PERP_USERS` from 500 to 5 000 via partitioned sealing (N × 64 KB seal parts).
  4. Same for `MAX_PERP_POSITIONS` → 8 000 and `MAX_TX_HASHES` → 5 000.
  5. Vault-shard convention: vaults live on shard 0. Even though there's only one shard, the vault access path goes through "fetch vault from shard 0" logic, so Phase 3 doesn't have to retrofit it.
- **Attestation:** DCAP working on every node; Azure dependency from the hybrid model is dropped.

**This is the most important phase in the whole plan.** Every architectural decision made here propagates forward. If sharding is skipped here and postponed to Phase 3, it becomes a rewrite; if it's built in now, Phase 2 and beyond are operational scale-ups, not engineering problems.

### Phase 2 — Single shard, multi-threaded (5 k – 50 k users, 500 – 5 000 active)

- **Shards:** still `N = 1`. One shard suffices.
- **Hardware per node:** unchanged — EX44 E-2388G.
- **Cost:** unchanged — **~€2 500/year.**
- **Code work:**
  1. **Multi-thread the enclave** (`TCSNum = 4` or `8`). Audit every `PerpState` access path for thread safety — per-user rwlock, atomic margin updates, serialised seal operations. **1–2 weeks of enclave work.**
  2. Raise `MAX_PERP_USERS` further — 50 000 × 208 B = 10 MB, well inside the 512 MB EPC on E-2388G. More partitioned seal parts.
  3. No router changes — shard router already routes to shard 0, same as Phase 1.

**Phase 1 → Phase 2 is the step where one-time engineering buys an order of magnitude of capacity on the same hardware.** Multi-threading is the single highest-leverage change in the whole plan. Any post-hackathon roadmap that doesn't include it is leaving ~5–8× throughput on the table.

### Phase 3 — Multi-shard (50 k – 200 k users, 5 k – 20 k active)

- **Shards:** `N = 2` to `N = 4`. Each shard is still 3 × EX44 E-2388G.
- **Hardware:** 2–4 × (3 × EX44) = **6–12 total boxes**, all EX44 class.
- **Cost:** ~€210/mo × N_shards × (12 months) = **~€5 000 – 10 000/year.**
- **Code work:**
  1. Turn on real sharding in the router (hash(user_id) % N instead of constant 0). The heavy lifting is already done in Phase 1, so this is a config flip plus the shard-rollout procedure.
  2. **Shard rollout procedure.** Stand up new shard, flip router config, new users onboarded to new shard. Existing users stay on their current shard forever (see §4.4).
  3. **Cross-shard funding application.** At each funding tick, the global funding rate is computed once in the orchestrator and then passed to every shard's `apply_funding` ecall. Straightforward — each shard only touches its own positions.
  4. **Cross-shard vault access.** Vault shard (shard 0) remains the owner; non-vault-shard users calling `vault_deposit` / `vault_withdraw` round-trip through the router to shard 0. This is the slowest path, but it's OK — vault operations are not latency-critical.
- **Monitoring:** proper per-shard observability — sealed-state sizes per shard, ops/sec per shard, p99 latency per shard. Budget ~€100–200/month for monitoring infrastructure (Grafana on a Hetzner cloud VM, Prometheus per shard).

### Phase 4 — Hyperscale (200 k – 1 M users, 20 k – 100 k active)

- **Shards:** `N = 5` to `N = 20`. Still EX44-class hardware per node.
- **Hardware:** 15–60 boxes, still all EX44 tier.
- **Cost:** **~€15 000 – 60 000/year** for hardware. At 1 M users this is roughly **€0.06/user/year** — trivial relative to any realistic per-user revenue.
- **Code work:**
  1. **Shard-aware liquidation vault.** At this scale, a single global liquidation vault on shard 0 may become a contention point. Options: shard the liquidation vault itself (one per shard, with a global backstop), or keep it global with a slower rebalancing loop. Pick based on measured contention.
  2. **Shard rebalancing tooling.** Monitoring to detect hot shards (one shard with way more volume than others) and route new users away from them.
  3. **Operational automation.** Adding a shard should be scriptable end-to-end: provision 3 Hetzner boxes, deploy enclave, seal MRENCLAVE, publish attestation, update router config, announce.

### Phase 5 — Ceiling check (1 M+ users)

- **Shards:** `N = 20+`. At this point, upgrading per-node hardware from EX44 to EX101/EX130 starts to make sense as an alternative to adding more shards, because the operational cost of managing 60+ boxes begins to outweigh the rental cost difference.
- **Hardware:** mixed. Possibly 5–10 shards on EX101-class nodes (each EX101 shard holds more capacity per node).
- **Cost:** **€60 000+/year.** Still small relative to DEX revenue at this scale.
- **This is where the shard-first architecture pays off.** If the system had been built single-cluster and then needed to shard at 1 M users, that would be a complete rewrite under traffic. Because sharding was built in at Phase 1, reaching 1 M users is an operational scale-up, not an engineering project.

---

## Part 6 — Code work per tier (summary)

| Change | Effort | Phase it unblocks | Pay now or later? |
|---|---|---|---|
| Migrate to Xeon E-2388G box + DCAP enabled | ~1 week (deploy + test) | Phase 1 production readiness | Now (hardware swap, not code) |
| Raise `MAX_PERP_*` ceilings via partitioned sealing | 2–3 days | Phase 1 (5 k users) | Now |
| **Add `shard_id` throughout enclave + REST + sealed state** | **~1 week** | Phase 1 (prepays Phase 3) | **Now — retrofit cost is 5–10× worse** |
| **Build shard router in orchestrator (static config, 1 shard to start)** | ~3 days | Phase 1 (prepays Phase 3) | **Now** |
| Vault-shard convention (vaults on shard 0, cross-shard access path) | ~3 days | Phase 1 (prepays Phase 3) | Now |
| Multi-thread enclave (`TCSNum` ≥ 4) + state thread-safety audit | **1–2 weeks** (the hard one) | Phase 2 (50 k users) | Between Phase 1 and Phase 2 |
| Turn on real shard routing (`hash(user_id) % N`) | 1 day | Phase 3 | When adding shard #2 |
| Cross-shard funding application | ~1 week | Phase 3 | With Phase 3 |
| Cross-shard vault access path | ~1 week | Phase 3 | With Phase 3 |
| Shard-aware liquidation vault or global backstop | 2–4 weeks | Phase 4 | With Phase 4 |
| Shard rollout automation (provision → deploy → attest → route) | 2 weeks | Phase 4 | With Phase 4 |

**Two highest-leverage items, both at Phase 1:**

1. **Shard-id plumbing** — because retrofitting it later costs 5–10× more and forces a painful migration under live traffic. Even when `N = 1`, the code must treat the shard ID as a first-class parameter. This is the single biggest architectural commitment in the whole plan.
2. **Enclave multi-threading** — the single highest-throughput-per-week-of-work change, worth 5–8× per box on the same hardware. Mandatory before Phase 2.

**Sharding itself (Phase 3 onwards) is then an operational task, not an engineering project**, because the engineering was done at Phase 1. That's the whole point of building it in early.

---

## Part 7 — Assumptions and caveats

1. **Throughput numbers are from the sister project's benchmarks** (`sgx-vs-tdx-roi.md` §4) and match what the current code path does. Real-world numbers will differ: network latency to XRPL, sealing storage latency (NVMe vs SATA SSD), CPU thermal throttling under sustained load. Treat the 100 ops/sec figure as a **lower bound**, not a precise number. Before committing to Phase 2+ hardware budgets, run a sustained load test on the actual deployed box and re-measure.

2. **The "10 enclave ops per active user per day" assumption** is a guess, not a measurement. A high-frequency trader could hit 100+; a pure LP might hit 2–3. Once there's real traffic, replace this constant with a p95 measured from production logs.

3. **Hetzner SKU availability shifts.** Hetzner regularly rotates its dedicated-server line. The EX44 / EX101 / EX130 names used above reflect the 2026 catalogue; specific CPUs may have been replaced by newer equivalents by the time this budget is used. The principle is stable — Xeon E for cheap, Xeon Gold for scale — but confirm the exact SKU at order time.

4. **SGX BIOS enablement.** Not every dedicated-server provider ships SGX enabled in BIOS by default. Hetzner does on the E-class line the PM project has been using; on Xeon Scalable SKUs (EX101+), confirm with a support ticket before ordering a batch.

5. **Bare-metal commitment.** All prices above assume monthly rental. Hetzner offers lower rates for annual commitments (~10–15 % off). For Phase 1 and above, negotiate annual contracts.

6. **No TDX in this plan.** We're staying on SGX as long as Xeon E supports it (see `sgx-vs-tdx-roi.md` §9). If Intel removes SGX from the Xeon E roadmap, the plan changes — but that's not a 2026 event, and the ~1-week SGX→TDX porting effort is the escape valve if it ever becomes one.

7. **CLOB model is assumed.** These numbers are for the current in-enclave margin / CLOB-execution design. If the post-hackathon plan slides toward a vAMM-as-counterparty model (explicitly ruled out in `clob-vs-amm-alignment.md`, resolved 2026-04-13 in favour of Variant A — CLOB preserved), the throughput numbers shift and the plan needs rewriting.

---

## Part 8 — Summary for the business plan

- **Architecture is sharded from day one.** A shard = 1 cluster of 3 SGX nodes. Phase 1 runs 1 shard. Every subsequent phase adds more shards, not bigger boxes.
- **Per-shard hardware cost is flat: ~€2 500/year on EX44 Xeon E-2388G boxes.** This number does not change from Phase 1 through Phase 4.
- **Total hardware cost scales linearly with shard count.**
  - Phase 1 (1 shard, ~5 k users): **€2 500/year**
  - Phase 2 (1 shard multi-threaded, ~50 k users): **€2 500/year** (same hardware, code work only)
  - Phase 3 (2–4 shards, ~200 k users): **€5 000 – 10 000/year**
  - Phase 4 (5–20 shards, 1 M users): **€15 000 – 60 000/year**
- **Unit economics:** at 1 M users and €60 k/year infrastructure, hardware is **~€0.06 per user per year**. Well under 0.1 % of any credible revenue model for a perp DEX at that scale.
- **Engineering budget is the real cost, not hardware.** The two load-bearing investments:
  1. **Shard-id plumbing at Phase 1** (~1 week + router): pays for Phase 3 and beyond. Skipping this and retrofitting later is a rewrite under live traffic — the single biggest architectural debt the team can accumulate.
  2. **Enclave multi-threading before Phase 2** (~1–2 weeks): 5–8× throughput per box, mandatory for anything above 5 k users.

**Business-plan line items:**

| Line | Year 1 | Year 2 | Year 3 |
|---|---|---|---|
| SGX infrastructure (Hetzner) | €2 500 | €2 500 – 5 000 | €5 000 – 15 000 |
| Auxiliary infra (monitoring, DB, CDN) | €1 500 | €2 500 | €5 000 |
| Enclave engineering buffer | ~1 engineer-month (shard plumbing + multi-threading) | — | — |
| Cross-shard engineering | — | — | ~1–2 engineer-months |

**Budget the engineering time, not the hardware. The hardware is cheap, and because sharding is built in at Phase 1, it stays cheap all the way up to the million-user ceiling.**
