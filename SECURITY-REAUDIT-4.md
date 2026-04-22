# Security Re-Audit #4: XRPL Perpetual DEX — Orchestrator & Architecture

**Date**: 2026-04-22
**Scope**: 111 commits on the public repo since re-audit #3 (2026-04-07). Focus areas: authentication, P2P mesh, validator / state-hash replication, vault market-making, orderbook persistence, multisig withdrawal, mainnet switch, escrow setup tooling, singleton runner, DCAP integration, deposit/withdraw handling of DestinationTag / native XRP.

> **Criticals are not in this document.** Per re-audit-4 scope agreement, Critical findings that would constitute a 0-day on the live escrow multisig are reported only in the private enclave repository (`77ph/xrpl-perp-dex-enclave/SECURITY-REAUDIT-4.md`). This file is safe to publish.

---

## Summary (public findings only)

| Severity | Count | IDs |
|:--------:|:-----:|-----|
| **High**     | 5 | O-H1, O-H2, O-H3, O-H4, O-H5 |
| **Medium**   | 5 | O-M1, O-M2, O-M3, O-M4, O-M5 |
| **Low**      | 5 | O-L1, O-L2, O-L3, O-L4, O-L5 |
| **Info / operational** | 3 | O-I1, O-I2, O-I3 |

One or more Critical-severity findings were also identified; see the private enclave repository for those. Deployment on mainnet is **not** recommended until the private-repo Criticals are resolved.

---

## O-H1 — HIGH — Auth "alt hash" path accepts a timestamp-only signature for any empty-body POST

**File**: `orchestrator/src/auth.rs:185-235`
**Commits**: `4051ee3 fix(auth): try both URI and empty-body hashes`, `969911e feat(auth): session token login`

For a POST with an empty body (e.g. `/v1/auth/login`), the verifier tries two candidate hashes:

```rust
let hash = /* SHA-256(uri_path || timestamp) */;
let alt_hash = if body_bytes.is_empty() {
    let mut hasher = Sha256::new();
    hasher.update(timestamp_str.as_bytes()); // ← only the timestamp
    Some(hasher.finalize())
} else { None };
```

Both are accepted. The `alt_hash` branch means **any valid ECDSA signature over `SHA-256(ascii_timestamp)` by the victim's XRPL key** can be replayed to any empty-body POST endpoint to obtain a 30-minute Bearer session for the victim.

Combined with the two-mode verify loop directly below (Mode 1 direct, Mode 2 SHA-512Half for Crossmark/GemWallet compatibility), the set of signatures that satisfy login grows to `{hash, alt_hash} × {direct_sha256, sha512_half}` — four independent credentials per timestamp. Any oracle that produces one of them (including benign tools that sign "the current time" for proof-of-life or telemetry purposes) becomes a session-login credential.

Because `/v1/auth/login` is in the unauthenticated-endpoint allowlist at `auth.rs:275-283`, this is reachable without any prior authentication.

**Fix**

- Remove the `alt_hash` branch. Always include at least a domain-separating prefix: e.g. `SHA-256("perp-dex/login|v1/auth/login|" || timestamp)`.
- Prefer treating `/v1/auth/login` as a special case that signs `"login:" || timestamp || optional_nonce`, mandatory non-empty.

---

## O-H2 — HIGH — Validator's state-hash verification logs mismatches but still replays the batch

**File**: `orchestrator/src/main.rs` inside the validator replay loop, added by `c9fe0ed feat: validator verifies state hash after batch replay`

The commit message says: "Logs ERROR on mismatch (potential sequencer compromise)." The code does exactly that — and then falls through to the replay block. A compromised or misconfigured sequencer that publishes a batch whose `state_hash` does not match the fills is still replayed into the local enclave via `open_position` / `close_position`. The original TODO it replaced said "refuse to co-sign withdrawals"; that refusal is not implemented.

```rust
if local_hash != batch.state_hash {
    error!("STATE HASH MISMATCH — sequencer may be compromised");
} else {
    info!("state hash verified");
}
// falls through and replays anyway
```

Downstream effect: validator PG (`a688d00 feat(db): passive trade replication`) also writes the fills as trades, so every operator's history ends up tainted by the bogus batch.

**Fix**

- On mismatch, `continue;` past the enclave replay and the PG write.
- Bubble the error up to the election / heartbeat path so the local operator can flag the sequencer via the election protocol.

---

## O-H3 — HIGH — Validator locks on the first sequencer_id it sees (trust-on-first-use)

**File**: `orchestrator/src/main.rs`
**Commit**: `95582f8 feat: validator rejects batches from unexpected sequencer`

```rust
let mut known_leader: Option<String> = None;
while let Some(batch) = batch_rx.recv().await {
    if !batch.sequencer_id.is_empty() {
        if let Some(ref known) = known_leader {
            if *known != batch.sequencer_id { continue; /* ignored */ }
        } else {
            known_leader = Some(batch.sequencer_id.clone());
        }
    }
    // ...
}
```

The first `sequencer_id` seen becomes authoritative for the lifetime of the validator process. A peer that races a legitimate sequencer at startup (publishes a crafted `OrderBatch` first) pins the validator to an impostor; legitimate batches are then silently dropped. Because libp2p identities are now persistent (`d2ee9ed feat(election,p2p): persistent libp2p identity`), an attacker who wins the race once retains the capability across restarts.

The commit itself acknowledges this limitation: `// TODO: compare batch.sequencer_id with current elected leader peer_id`. The TODO is still in place.

**Fix**

- Cross-check `batch.sequencer_id` against the elected leader from the `election` module; `known_leader` should track the elected leader, not the first-observed one.
- Reset `known_leader` on every successful election-complete event.

---

## O-H4 — HIGH — Resting orders are restored from PostgreSQL on failover without any signature or integrity binding

**File**: `orchestrator/src/db.rs:252-286`, `orchestrator/src/main.rs:454`
**Commit**: `f5289da feat(orderbook): persist resting orders to PG for failover recovery`

Every resting limit order is written to `resting_orders` when it enters the book and deleted when it is filled or cancelled. On failover / restart, `load_resting_orders` reads the rows back and puts them directly into the in-memory CLOB. The row schema does **not** carry the user's XRPL signature, timestamp hash, or a MAC from the operator — so the orderbook trusts whatever is in the PG table.

If PostgreSQL is compromised (credential leak, container escape, SQL injection elsewhere), an attacker inserts arbitrary rows: any `user_id`, any `side`, any `size`, any `price`. On the next sequencer boot (or any operator's failover), the book loads the forged orders. These orders match against legitimate incoming orders, and when a fill occurs the taker side is replayed into the enclave as `open_position(fake_user_id, ...)` — succeeding as long as the victim still has margin.

Note that the same PG is also written by the passive-replication path from `a688d00`, so a compromised PG on *any* validator can seed forged orders that will be loaded by whoever becomes the next sequencer.

**Fix**

- Add a `signature_hex` column to `resting_orders` and re-verify via `auth::verify_request` semantics on reload. Reject rows whose signature does not validate against the stored user's pubkey.
- Alternatively, HMAC each row with an operator secret that never leaves the orchestrator process memory; only accept rows whose HMAC validates.

---

## O-H5 — HIGH — Sequencer TOFU (O-H3) + state-hash log-only (O-H2) + PG-trusted rebuild (O-H4) compound into silent state poisoning

Not a new bug per se — but worth calling out as a composite: an attacker who wins the `known_leader` race (O-H3) can publish fills whose state_hash mismatch is logged but still replayed (O-H2), which write trade rows into PG (`a688d00`), which then participate in the resting-order table (O-H4). An attacker who controls one of these points controls the historical record of all operators.

**Fix**: resolving O-H2, O-H3, O-H4 individually breaks the chain.

---

## O-M1 — MEDIUM — Session store is a process singleton with racy lazy initialisation

**File**: `orchestrator/src/auth.rs:83-94`

```rust
pub static SESSION_STORE: std::sync::OnceLock<Arc<SessionStore>> = std::sync::OnceLock::new();

pub fn init_session_store() -> Arc<SessionStore> { ... }

pub fn session_store() -> &'static Arc<SessionStore> {
    SESSION_STORE.get_or_init(|| Arc::new(SessionStore::new()))
}
```

If the axum router starts serving requests before `init_session_store()` is called, the first `session_store()` call creates a *different* `SessionStore` than `init_session_store()` will later try to register (`let _ = SESSION_STORE.set(store.clone());` at line 88 silently ignores the failure). Tokens created via the auth endpoint then go into store A, while middleware lookups read from store B (whichever was created first). All Bearer tokens would then appear invalid.

**Fix**: call `init_session_store()` before binding the TCP listener; make `set()` failure a hard error; or drop `init_session_store` entirely and rely solely on `get_or_init`.

---

## O-M2 — MEDIUM — Timestamp drift widened to ±60 s doubles the replay window for signed requests

**Commit**: `18b3f83 fix(auth): increase timestamp drift tolerance from 30s to 60s`
**File**: `orchestrator/src/auth.rs:133`

The commit rationale is "Browser wallets can have slight clock skew." Combined with no per-request nonce, a single intercepted `(hash, timestamp, signature)` triple is replayable for up to 60 s — twice the old window. Practical impact is limited because the body includes a `user_id` / `order_id` / destination and the matching middleware enforces `user_id == authenticated_address`, but idempotent-looking endpoints (e.g. status queries against the caller's own address) can still be replayed for reconnaissance.

**Fix**

- Keep the 60-s window but also require a client-generated nonce; server stores the (nonce, timestamp) pair in a short TTL map and rejects duplicates.
- Or: require the request body to include the timestamp as a field and hash it as part of the canonical payload so replays across different times are already cryptographically distinct (today this is provided by the external `x-xrpl-timestamp` header, but it is not bound to the body itself for non-empty bodies).

---

## O-M3 — MEDIUM — `close_position` routes through the CLOB without confirming the caller's ownership of `position_id`

**Commit**: `978324c fix: close_position routes through CLOB, not directly at mark price`
**File**: `orchestrator/src/api.rs` `close_position` handler

The handler authenticates the caller, then extracts the position's side/size via `get_balance(caller)` — the comment in the diff says "The enclave only returns currently-open positions ... so an ownership check is sufficient." This assumes the balance endpoint only returns the caller's own positions. That's true today, but the check is implicit (derived from the enclave's `get_balance` scope rather than explicit on the request's `position_id`).

If a future change has the balance RPC return positions across shards or across users (e.g. for admin diagnostics), the close handler inherits the wider scope and the "ownership check is sufficient" comment goes stale silently.

**Fix**: inside the handler, after the balance response is parsed, explicitly assert `balance.positions.any(|p| p.position_id == path.position_id && p.user_id == caller)` before submitting the reduce-only IOC.

---

## O-M4 — MEDIUM — Self-trade prevention is decrement-and-cancel, with no rate limit or quota — amplifies cancel-book pressure

**Commit**: `6eb21fb fix(orderbook): self-trade prevention + add close-position endpoint`

STP is now Decrement-and-Cancel: a same-user cross consumes both sides for the cross amount and generates no trade. This is the right default for spot/perp DEXes, but combined with the new "any user can submit market IOC orders freely" flow (close_position), it enables a free way for one user to cheaply consume their own maker liquidity — effectively pulling a resting quote without paying the cancel-rate-limit (if one exists; none is visible in `orderbook.rs`).

Operational impact: a grief attacker with two XRPL addresses can bounce STPs through the vault-MM quotes (`vault_mm.rs`) to force continuous cancel-and-replace cycles, amplifying PG writes (`resting_orders` delete + insert) and gossipsub traffic. At Paris-hackathon scale this is benign; at mainnet scale with `--vault-mm` enabled, it is a DoS amplifier against the operator.

**Fix**: per-user cancel rate limit on the CLOB before the STP path; cap on cross-of-own-orders per minute per address.

---

## O-M5 — MEDIUM — Vault MM fixed pyramid sizing (`3.8/7.6/15.2` XRP post-mainnet fix) lacks max-inventory guardrail

**Commit**: `6b117cd fix(vault): reduce order sizes 10x for mainnet safety`, preceded by `0c75840 feat(vault): pyramid order sizing`

The 10× reduction is good defence-in-depth, but `run_vault_mm` at `vault_mm.rs:197+` has no check on the vault's aggregate open position before placing the next level. Under one-sided order flow (a whale sweeping all three bid levels), the vault accumulates 11.4 XRP of long exposure per level triplet and immediately re-quotes — effectively removing all inventory discipline.

Combined with the Delta-Neutral strategy from `ce28719 feat(vault): add Delta Neutral strategy`, if DN hedge latency exceeds mark-price move latency, the "delta-neutral" framing does not hold.

**Fix**: before quoting a level, check `abs(vault_inventory) + level_size < max_inventory`; skip levels that would exceed the cap, and if all levels would, pause quoting. Make `max_inventory` a CLI flag.

---

## O-L1 — LOW — `GET /v1/perp/liquidations/*` is in the public endpoint allowlist

**File**: `orchestrator/src/auth.rs:275-283`

```rust
|| uri.starts_with("/v1/perp/liquidations/")
```

Any caller can enumerate liquidation history across all users. Low severity because the data is arguably public anyway (XRPL transactions are on-chain), but the leakage of `(user_id, price, position_id)` in compact form is strictly more convenient for a scraper than the XRPL ledger. Consider requiring auth and restricting to the caller's own liquidations.

---

## O-L2 — LOW — `return zero balance for unknown users` silently masks user enumeration

**Commit**: `f07241c fix(api): return zero balance for unknown users instead of 500`

The new behaviour trades a 500 for a 200-with-zeros. An attacker probing for registered users can no longer differentiate "user exists with zero balance" from "user never registered." This is actually a *small* privacy win if "user exists" itself is sensitive. Note it for completeness — the behaviour is intentional and acceptable, but make sure rate limiting is present so a brute-force probe can't harvest the valid-user set from timing differences in the enclave lookup path.

---

## O-L3 — LOW — Escrow setup CLI accepts the XRPL seed via argv and persists derived keys to disk in the working directory

**Commit**: `7312ee3 feat(escrow-setup): local XRPL tx signing with Ed25519 support`

The `escrow-setup` / `operator-setup` subcommands take `--escrow-seed` as a CLI argument; `ps aux` reveals it on the operator's box. Derived keypairs are written to the working directory rather than a protected path. Because this is a one-time ceremony (not a recurring flow), severity is Low, but document the procedure:

- Pipe the seed in from a file with 0600 permissions rather than argv.
- Write derived artefacts to `/etc/perp-dex/` or `$XDG_CONFIG_HOME/perp-dex/` with 0600, not `cwd`.
- After the ceremony, shred the seed file; master-key disabling (`asfDisableMaster`) should follow the same session so the seed's theft window is minutes, not days.

---

## O-L4 — LOW — `danger_accept_invalid_certs(true)` on every `reqwest::Client`

**Files**: `orchestrator/src/p2p.rs:421`, `orchestrator/src/withdrawal.rs:262`, others

Enclave RPC is localhost-only so TLS verification being off is mostly moot; however the same pattern is used for `xrpl_url` (public XRPL node) in some places. Audit the list:

```
$ grep -rn "danger_accept_invalid_certs" orchestrator/src/
```

Any non-loopback usage is a stronger finding than this; the loopback-only ones are Info.

**Fix**: introduce a helper `fn loopback_http_client()` that sets `danger_accept_invalid_certs(true)` and binds to `127.0.0.1`; use it only for enclave RPC. For XRPL RPC, verify certs.

---

## O-L5 — LOW — Health endpoint exposes `peer_count` without auth

**Commits**: `b6dd7ae feat(ops): health endpoint, systemd unit, deploy script`, `fda4ded fix(health): use /pool/status for enclave probe`, `5c6a98d fix(deploy): add curl timeout to health checks`

`/v1/health` returns operational details (peer count, sequencer state, etc.) that let a scanner fingerprint the mesh shape. The endpoint is in the unauth allowlist. Low because the same info can be pulled via libp2p identify once a peer dials in (see cross-layer finding in the private repo about the open mesh), but keep the attack surface narrow.

---

## O-I1 — INFO — Singleton runner gates MM/DN on `is_sequencer` correctly

**Commit**: `01cf681 feat: complete infrastructure — singleton runner, ...`

The singleton runner (starts vault MM / DN on promote, aborts on demote) is implemented as documented. Observationally sound; no specific finding. Only flag: the abort on demote is a `tokio::task::abort()` — if the MM task holds any in-flight state that needs graceful shutdown (e.g. pending limit-order cancellations), abort will leave those orders in the book. Downstream C5.1 (resting-order PG persistence) means they are cleaned up on the next sequencer's boot, but the transient window can produce double-listed quotes across operators.

**Observation**: consider a cooperative-shutdown shim (`tokio::select!` on a shutdown signal) so the demoted operator explicitly sends cancels before aborting.

---

## O-I2 — INFO — Passive trade replication relies on `ON CONFLICT DO NOTHING`

**Commit**: `a688d00 feat(db): passive trade replication across operators via validator replay`

The composite (`trade_id`, `market`) and `position_id` unique keys + `ON CONFLICT DO NOTHING` pattern is correct for idempotent writes, and the migration `001_passive_replication_idempotency.sql` correctly wraps dedup + constraint add in a single transaction. One subtle note: `DO NOTHING` silently drops conflicting rows whose non-key columns differ. If the sequencer and a validator disagree on `price` / `size` for the same `trade_id`, whichever row is inserted first becomes canonical and the discrepancy is invisible. Pair with O-H2's state-hash enforcement — once state-hash mismatches stop the replay, this path becomes safe.

---

## O-I3 — INFO — `set_shard_id` backward-compat logs a warning instead of erroring

**Commit**: `84c77e9 fix: make set_shard_id non-fatal for backward compat with old enclaves`
**File**: `orchestrator/src/shard_router.rs:43-55`

Accepting an old enclave silently defaults it to `shard_id=0`. In a multi-shard deployment, an operator that accidentally runs an old enclave binary joins shard 0 regardless of config. Acceptable for rolling-upgrade hygiene, but pair it with a startup-time emitted metric (e.g. `enclave_supports_shard_id = 0`) so observability catches it.

The underlying enclave-side concern about `set_shard_id` being unauthenticated is a separate, higher-severity finding in the private repo.

---

## Re-audit #3 open items — verification (orchestrator side)

- **C-02** (admin session key on price/funding/liquidation ecalls): **OPEN**. No new code in this audit window adds the admin check. Orchestrator-side mitigation (restricting which callers can reach those ecalls) exists through the `localhost-only` property of the enclave HTTP server plus the P2P mesh as the only remote driver — but the mesh itself is not an adequate access boundary (see the cross-layer Critical in the private repo).

- **C-04** (deposit trust model): **OPEN**. The deposit path was *extended* by `16a678e` (DestinationTag) and `beafac4` (native XRP) but still trusts the orchestrator's XRPL monitor to present `xrpl_tx_hash` correctly. No SPV / multi-operator verification is introduced.

- **NEW-06 / NEW-07**: fixed on the enclave side (see private repo).

---

## Final status (public findings)

| Severity | Open |
|----------|:----:|
| Critical | (see private repo) |
| High     | 5 |
| Medium   | 5 |
| Low      | 5 |
| Info     | 3 |

**Deployment recommendation**: do not proceed with the mainnet switch (`b44b0ec`) until:

1. The private-repo Criticals are resolved (FROST share transport, signing oracle).
2. O-H1..O-H5 are resolved.
3. C-02 and C-04 (carried from the original audit) reach at least "acknowledged and compensating controls documented" status.

Testnet is acceptable under current code; the vault size reduction (`6b117cd`) bounds the blast radius while higher-severity items are addressed.
