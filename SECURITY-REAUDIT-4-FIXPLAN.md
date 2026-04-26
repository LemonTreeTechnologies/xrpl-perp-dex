# Fix Plan — Security Re-Audit #4 (Public Orchestrator)

**Responds to:** `SECURITY-REAUDIT-4.md` (5H / 5M / 5L / 3I).
**Date:** 2026-04-22.
**Status:** Triage only — no fixes started. Each item is validated for fixability with rough effort and dependency ordering.

> Critical findings live in the private repo (`77ph/xrpl-perp-dex-enclave/SECURITY-REAUDIT-4-FIXPLAN.md`) per the 0-day policy. The two criticals most visible from this repo (open signing oracle, FROST primitive missing) are ship-blockers; this plan assumes they are being fixed in parallel.

Mainnet redeployment is blocked until:
1. Private-repo criticals (E-C1, X-C1) resolved.
2. O-H1..O-H5 resolved.
3. C-02, C-04 at least "compensating control documented."

Testnet continues under current code — the 10× vault-size reduction (`6b117cd`) bounds blast radius while the above are addressed.

---

## Dependency graph

```
O-H1 (alt_hash) ── independent
O-H2 (state-hash log-only) ──┐
O-H3 (sequencer TOFU) ───────┼── O-H5 is the compound; resolving any two breaks it
O-H4 (PG-trusted rebuild) ───┘
O-M1 (session store race) ── independent, trivial
O-M2 (timestamp drift 60s) ── pair with nonce work
O-M3 (close_position ownership implicit) ── independent
O-M4 (STP DoS) ── depends on existence of a cancel-rate limit; none today
O-M5 (vault MM max_inventory) ── independent
O-L1..O-L5 ── all independent, trivial
```

---

## Highs

### O-H1 — Auth `alt_hash` accepts timestamp-only signature

**Verified** at `orchestrator/src/auth.rs:196`. Branch still present.

**Fixable: YES, trivial.** Remove the `alt_hash` branch entirely. Replace with a domain-separated canonical hash for empty-body POSTs: `SHA-256("xperp/v1/login|" || uri_path || "|" || timestamp)`. Keep the two-mode verify (direct / SHA-512Half) for Crossmark/GemWallet compatibility — that's independent of this finding.

Effort: **0.5 day** including a unit test that rejects timestamp-only signatures.

Risk of breakage: Tom's current frontend uses the login flow — coordinate so the frontend change lands in the same release.

### O-H2 — Validator replays batches whose state_hash does not match

**Verified** at `orchestrator/src/main.rs` (validator replay loop, introduced by `c9fe0ed`). The `error!()` log is present but the code falls through.

**Fixable: YES, trivial.** On mismatch: `continue;` (skip enclave replay and PG write), and surface the mismatch on a counter metric `state_hash_mismatches_total{sequencer_id=...}` so observability catches a compromised sequencer.

Escalation path is deferrable: today we don't auto-demote a sequencer on mismatch; we just don't follow it. That's enough.

Effort: **0.5 day**.

### O-H3 — Validator locks on first-observed `sequencer_id` (TOFU)

**Verified** at `main.rs:687–709`. `known_leader` is set from the first batch and never reset.

**Fixable: YES.** `known_leader` must track the elected leader, not the first-observed one. The election module already emits completion events on the `election_inbound_tx`/`election_outbound_tx` channels — re-wire so that an election-complete event overwrites `known_leader` atomically with the winning peer_id; bootstrap path sets it from the last known election outcome (sealed or from PG).

Depends on: election module exposing current leader's peer_id in a way the batch-replay loop can read without deadlocking.

Effort: **1 day** plus tests that simulate race + reelection.

### O-H4 — Resting orders reloaded from PG without signature binding

**Verified** at `orchestrator/src/db.rs:252–286` and `main.rs:454`. Schema of `resting_orders` has no `signature_hex`.

**Fixable: YES.** Two approaches; we recommend (a) because it composes with existing auth:

- **(a) Store the user's original XRPL signature + timestamp + canonical hash alongside the row.** On reload, re-verify via `auth::verify_request` semantics before inserting into the in-memory CLOB. Reject rows that don't validate. Also rejects rows whose stored address doesn't match the signature's recovered pubkey.
- **(b) HMAC each row with an operator secret held only in the orchestrator process.** Shorter diff but introduces a new key-management surface we don't currently have.

Migration: `ALTER TABLE resting_orders ADD COLUMN signature_hex TEXT, timestamp_str TEXT, canonical_hash_hex TEXT;` + backfill is empty (rows are short-lived). Drop legacy rows on upgrade.

Effort: **1–2 days** including migration + the reload-side verifier plumbing.

### O-H5 — Composite (O-H2 × O-H3 × O-H4)

**Fixable:** automatically resolved when any two of O-H2/O-H3/O-H4 are resolved, per the audit. No separate work item.

---

## Mediums

### O-M1 — Session store `OnceLock` racy lazy initialization

**Verified** at `auth.rs:83–94`. Yes — `set()` silently fails if `get_or_init` ran first.

**Fixable: YES, trivial.** Call `init_session_store()` before the `axum::serve` call; make a subsequent `SESSION_STORE.set(...)` failure a hard error (panic) so regressions don't reappear. Simpler variant: drop `init_session_store` and rely entirely on `get_or_init` — that path has the racy-two-stores problem but only if two different initializers exist, which the drop removes.

Effort: **0.25 day**.

### O-M2 — Timestamp drift widened to ±60s

**Fixable: YES.** Add a per-session nonce store (short-TTL, ~120s) and require a `nonce` field in signed headers. Reject duplicates. Keeps the 60s drift tolerance but kills the replay window.

Effort: **1 day** including the TTL store and a frontend contract change (Tom needs to include `nonce` in the signed payload).

Coordinate with frontend — not gated on, but landing-together is cleaner than two deploys.

### O-M3 — `close_position` ownership check is implicit

**Fixable: YES, trivial.** After balance-response parse in the `close_position` handler, add explicit assertion:
```rust
balance.positions.iter().any(|p| p.position_id == path.position_id && p.user_id == caller)
```
Return 403 on mismatch.

Effort: **0.5 day**.

### O-M4 — Self-trade DoS amplifier via STP decrement-and-cancel

**Fixable: YES**, not urgent. Per-user cancel-rate limit on the CLOB before the STP path decides; cap `cross-of-own-orders/minute/address` at a low number (e.g. 30/min). The audit is correct that at hackathon scale this is benign; treat as pre-mainnet hygiene.

Effort: **1 day**. Schedule with the MM hardening pass.

### O-M5 — Vault MM fixed pyramid has no max-inventory guardrail

**Verified** at `vault_mm.rs:197+`. No check on aggregate inventory before re-quoting.

**Fixable: YES.** Before placing a level: check `abs(vault_inventory) + level_size < max_inventory`. Skip levels that would exceed the cap; if all levels would exceed, pause quoting until inventory drains. Make `max_inventory` a CLI flag with a conservative default (e.g. 50 XRP with the current 3.8/7.6/15.2 sizing).

Effort: **0.5 day** for the check + flag, plus a test that walks a one-sided sweep and asserts the cap holds.

Combined with Delta-Neutral: if `--delta-neutral` is on, the cap applies to net (hedged) inventory rather than gross.

---

## Lows

### O-L1 — `GET /v1/perp/liquidations/*` publicly accessible

**Fixable: YES, trivial.** Remove from the unauth allowlist; restrict to caller's own liquidations. XRPL ledger remains the canonical source for anyone who wants to scrape.

Effort: **0.25 day**.

### O-L2 — Zero-balance-for-unknown-user masks enumeration

**Accept.** Behaviour is intentional and privacy-preserving. Add rate-limit to the balance endpoint as a defence-in-depth.

Effort: **0.5 day** for rate-limit if not already present.

### O-L3 — `escrow-setup` takes seed via argv

**Verified** in commit `7312ee3`.

**Fixable: YES.** Support `--escrow-seed-file /path/to/seedfile` (mode 0600), deprecate `--escrow-seed`. Write derived artefacts to `/etc/perp-dex/` or `$XDG_CONFIG_HOME/perp-dex/` at 0600 instead of cwd. Document a post-ceremony `shred` step; remind to `asfDisableMaster` in the same session.

Effort: **0.5 day** + a short runbook update in `docs/ops/escrow-ceremony.md`.

### O-L4 — `danger_accept_invalid_certs(true)` scattered

**Fixable: YES, trivial.** Introduce `fn loopback_http_client() -> reqwest::Client` that accepts invalid certs and is only used against `127.0.0.1`; switch all non-loopback clients to default TLS verification. Dovetails with enclave-side **E-M2**.

Effort: **0.5 day**.

### O-L5 — Health endpoint exposes `peer_count` without auth

**Fixable: YES, trivial.** Split into `/v1/health` (public liveness: 200 OK + version + network) and `/v1/status` (auth-required, full details). Tom's frontend currently calls `/v1/health` — verify which fields it reads before splitting.

Effort: **0.25 day** + a Tom-facing note on the schema.

---

## Info / operational

### O-I1 — Singleton runner aborts on demote (observation)

Consider a cooperative-shutdown shim (`tokio::select!` on a shutdown signal) so the demoted operator sends explicit cancels before aborting. Not a fix — a hygiene item.

Effort: **1 day** when scheduled.

### O-I2 — `ON CONFLICT DO NOTHING` on passive trade replication

Behavior is correct. Once O-H2 lands (state-hash mismatch aborts replay), the silent-conflict case becomes impossible by construction.

No work item.

### O-I3 — `set_shard_id` backward-compat logs a warning

Acceptable. Pair with a metric `enclave_supports_shard_id{value=0|1}` and an alert on `value=0` in multi-shard deployments.

Effort: **0.25 day** for metric + alert config.

---

## Carry-overs from prior audits

### C-02 — Admin session key on `update_price` / `apply_funding` / `check_liquidations`

Still open. Fix is orchestrator-side easy (pass a session-key header on those calls) **and** enclave-side (actually check it). See enclave fix plan.

Effort (orchestrator side): **0.5 day**.

### C-04 — Deposit trust model (no SPV)

Still open. Deposit path extended by `16a678e` (DestinationTag) and `beafac4` (native XRP) but the trust model did not change. See enclave fix plan, Option 1 (M-of-N operator attestation) is the recommended near-term mitigation.

Orchestrator-side work for M-of-N: each orchestrator publishes its observed deposit events onto a new gossipsub topic `deposit-events-v1`, signed with its peer identity; enclave receives ≥M matching events. Effort: **2 days** orchestrator + 2 days enclave.

---

## Sequencing recommendation

**Week 1 — unblock:**
- O-H1, O-H2, O-H3, O-M1, O-M3, O-M5 (all trivial-to-medium; parallel to enclave E-C1 Path B).

**Week 2 — mainnet path:**
- O-H4 (migration + verifier), O-M2 (nonce + frontend contract), O-L3 (seed-file mode + runbook), C-02 orchestrator side, C-04 orchestrator side.

**Week 3 — cleanup:**
- O-M4, O-L1, O-L2 rate-limit, O-L4, O-L5, O-I3. Non-blocking.

Total: ~3 calendar weeks on the orchestrator side, assuming one engineer; enclave fix plan runs in parallel.

---

## Items flagged as "unresolvable" or "needs-research"

None in this repo. Every public finding is straightforwardly fixable; the only research-flavoured items (C-04 full SPV, E-H5 long-term DCAP linkage) live in the enclave plan.

---

# Appendix A — Findings from Re-Audit #4 implementation cycle

**Date:** 2026-04-26.
**How found:** during Phase 7 Path A wire-test deployment on the 3-Azure testnet cluster (task #80). Each item below was discovered live, in-the-loop, when the deploy hit it. None of these were caught by automated tests — all were caught only by manual E2E exercise of the full bump procedure. See "Methodology gap" at the end.

The appendix is **additive** to the audit findings above (O-H/M/L/I). New items here use prefix **APP-** to signal "post-audit, found during fix cycle".

## APP-AUTH-2 — Orchestrator's secp256k1 seed derivation does not implement XRPL Family Generator

**Severity:** Medium (functional blocker for any secp256k1 user; not a security exploit).
**File:** `orchestrator/src/cli_tools.rs:566-625` (`derive_keypair_from_seed`).
**Used by:** `escrow_setup` (escrow seed, would be wrong if operator gives a secp256k1 seed instead of ed25519), `cli_balance`, `cli_withdraw`, `sign_request`, `operator_address` — i.e. all CLI auth paths.

**What the spec says** (https://xrpl.org/cryptographic-keys.html#key-derivation):
For secp256k1, the user-visible `classic_address` is derived from the **master** keypair, where master is computed from a *family-generator* chain:
```
1. ROOT priv  = first valid SHA-512(entropy ‖ counter₁_be32)[:32]
2. INTERMEDIATE priv = first valid SHA-512(root_pub_compressed ‖ 0u32_be ‖ counter₂_be32)[:32]
3. MASTER priv = (root_priv + intermediate_priv) mod n      ← n = secp256k1 order
4. classic_address = derive_address(master_pub)
```

**What we do:** only step 1, then return the **root** keypair. Steps 2–3 are not implemented; step 4 derives the address from the *root* pubkey.

**Symptom:** any user who generates a secp256k1 wallet via `xrpl-py` / XUMM / GemWallet / rippled / any standard tool ends up with a master-derived `classic_address`. They send XRP to escrow from that address. They sign an orchestrator API call with the same seed — orchestrator computes a *root*-derived address, which is different, and authentication's `verify_request` matches *that* address. The orchestrator's view of "who is the user" diverges from the on-chain depositor. `state.perp.get_balance(orch_addr)` returns 500 (no such user in enclave); orchestrator falls back to hardcoded zeros → `margin_balance: "0.00000000"`. Withdraw fails the balance check.

**Reproducer (live, this session):**
```
xrpl-py:                rMWkQJYY31ujnmiGCDPxgDYzmJUjGUNAhr   ← receives 5 XRP on testnet escrow
orchestrator from seed: rpY3wEh813BHmzKMisx1yLUNjQREt1ULzJ   ← what auth signs as
```

**Fix:** implement steps 2–3 in `derive_keypair_from_seed` for the secp256k1 branch. ED25519 path is unaffected (no family generator there). Estimated effort: ~30 LoC + unit tests against the published XRPL test vector (`snoPBrXtMeMyMHUVTgbuqAfg1SUTb` → `rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh`).

**Why audit missed it:** the audit focused on `auth.rs` flows (alt-hash, drift, replay) but treated `derive_keypair_from_seed` as an opaque correct dependency. There is no test validating it against a known XRPL keypair. Adding such a vector to `cli_tools.rs::tests` would have caught this in CI.

## APP-PATHA-1 — `dcap_verify.cpp` strict QV-result policy is not configurable

**Severity:** Medium.
**Status (split):**
- **Policy content** (which verdicts to accept on Azure DCsv3): **Accepted-risk** — see [`docs/accepted-platform-risks.md`](docs/accepted-platform-risks.md) entry P-1. Decision: `{OK, SW_HARDENING_NEEDED}` is the correct policy for any Azure DCsv3 deployment given INTEL-SA-00615's nature (MMIO Stale Data, mitigation requires OS-level VERW which Azure applies but cannot prove via DCAP). This is no longer "open until tightened" — it is the documented intentional choice.
- **Implementation form** (hardcoded vs config-driven): **Open** — must be fixed before any deploy beyond the current Azure DCsv3 testnet. Hardcoded allowlists obscure the policy from operators and break audit-traceability.
**File:** `EthSignerEnclave/Enclave/dcap_verify.cpp` (commit `8934f63` — landed 2026-04-26).

**Background.** This session relaxed the strict `SGX_QL_QV_RESULT_OK`-only check to also accept `SGX_QL_QV_RESULT_SW_HARDENING_NEEDED`, because Azure DCsv3 currently reports the latter. The relaxation is hardcoded in the enclave. The *content* of the relaxation is correct (operator @77ph reviewed the underlying advisory INTEL-SA-00615 and accepted P-1 in `accepted-platform-risks.md` on 2026-04-26). The *form* (hardcode rather than config) still needs to ship.

**What needs to ship.** Turn the verdict allowlist into an `ecall_verify_peer_dcap_quote` parameter (or a sealed config value set at first run). Host reads `PERP_DCAP_ACCEPTED_QV_RESULTS` env var, defaults to `OK,SW_HARDENING_NEEDED` (matching P-1), passes the bitmask to the ecall. The deploy procedure documents which environments use which value (right now: Azure DCsv3 testnet AND any future Azure DCsv3 mainnet → `OK,SW_HARDENING_NEEDED`; non-Azure SGX hardware that legitimately attests OK → `OK` only).

**Effort:** ~1 day (enclave EDL change → libapp wrapper → host plumbing → docs); requires another enclave bump cycle.

**Until the config-driven version ships:** the running binary IS already on the policy P-1 documents. There is no behavioural change required pre-mainnet; the gap is documentation/auditability of the policy, not the policy itself. Treat as "ship a refactor, not a security fix."

**Architectural context.** This finding sits inside a broader question — the orchestrator+enclave use DCAP-based peer cross-attestation as the cluster-trust model, while the sibling project Phoenix PM (`77ph/SGX_project`) uses an operator-signed roster instead. The divergence is intentional in each project (each followed its own audit's prescription), but raises a question the next audit should address explicitly. Full reasoning, comparison, and the question for the auditor are in [`docs/cluster-trust-model-decision.md`](docs/cluster-trust-model-decision.md). Anyone touching `dcap_verify.cpp`, the verdict policy, or Path A peer-attest should read that ADR first, then `docs/accepted-platform-risks.md` P-1 for the verdict-policy reasoning.

## APP-WIRE-1 — Path A `import-v2` wire mismatch (export returns nested envelope, import expects top-level fields)

**Severity:** Medium (Path A v2 wire flow non-functional until fixed).
**Status:** Fixed 2026-04-26 in commit `d72f08c`.

**Issue:** `EthSignerEnclave/server/api/v1/pool_handler.cpp::handleFrostShareImportV2` reads `threshold`, `n_participants`, `sender_pubkey` from the **top level** of the request body. `EthSignerEnclave/server/api/v1/pool_handler.cpp::handleFrostShareExportV2` returns those fields **inside** the `envelope` object. Orchestrator's `pool_path_a_client.rs::frost_share_import_v2` was forwarding the envelope as-is — the import handler returned 400 "Missing required field: threshold".

**Fix shipped:** orchestrator side now lifts the three fields out of envelope into top-level body. Enclave side unchanged.

**Better long-term fix:** unify the wire — either make import accept the same nested shape that export returns, or make export return flat fields. Tracking as future cleanup; current state is functional.

**Why audit missed it:** Path A landed AFTER the audit (Phases 5c.3 and 6a–6b are post-2026-04-22). No e2e wire test existed; the import path was never exercised in CI.

## APP-OPS-1 — Azure global SGX certification cache TCB endpoint is non-responsive for our FMSPC

**Severity:** Operational (blocks Path A peer-attest until worked around).
**Workaround applied:** point `/etc/sgx_default_qcnl.conf` at Intel PCS direct (`https://api.trustedservices.intel.com/sgx/certification/v4/`), drop the `LOCAL_PCK_URL=...169.254.169.254/...THIM/...` line.

**Issue:** `https://global.acccache.azure.net/sgx/certification/v4/tcb?fmspc=00606a000000` does not respond within 10 s on any of our 3 Azure DCsv3 nodes. Same endpoint at the root (`/rootcacrl`) responds 200 OK in <100 ms — the TCB-info-by-FMSPC path specifically times out.

**Effect on enclave path:** `sgx_qv_verify_quote(collateral=NULL)` returns `0xE03A SGX_QL_TCBINFO_CHAIN_ERROR` because QPL can't fetch TCB info. Path A peer-attest cache never populates → `share-export-v2` refuses (peer not attested).

**Why we missed it:** pre-Path A code did not call `sgx_qv_verify_quote` over the network; it generated quotes only. The dependency on Azure cache being healthy is new.

**Documentation update:** add to `docs/testnet-enclave-bump-procedure.{en,ru}.md` and to `feedback_azure_dcap_findings.md` (memory). Mainnet: same change required, plus a **monitoring task** to watch Azure cache status (Microsoft side).

## APP-OPS-2 — `libsgx-dcap-default-qpl` was in `iU` (installed-but-unconfigured) state on sgx-node-3

**Severity:** Operational (single-node breakage of DCAP verify).
**Status:** Fixed 2026-04-26 by `dpkg --configure libsgx-dcap-default-qpl` with `--force-confold`.

**Issue:** node-3's apt history showed `libsgx-dcap-default-qpl 1.25.100.1-jammy1` was uninstalled then reinstalled (apt log dates 2026-04-08 18:19 and 18:21). The reinstall left the package in `iU` because dpkg's conffile prompt for `/etc/sgx_default_qcnl.conf` deadlocked over non-interactive ssh (no stdin to answer Y/I/N/O). With Intel default QPL not configured, the loader fell back to az-dcap-client at `/usr/local/lib/libdcap_quoteprov.so` (from the `az-dcap-client` package, no `.so.1` SONAME, picked up via `/usr/local/lib` precedence in ld.so.cache). az-dcap-client uses Microsoft THIM, which does not validate the chain our verifier expects → `0xE03A` again, but for a different reason than APP-OPS-1.

**Why we missed it:** no health check that the loaded QPL on each node is the *intended* one (Intel default). Path A onboarding did not verify per-node DCAP-toolchain integrity.

**Fix forward:** add a startup check to the orchestrator (or to the enclave server) that, before serving traffic, calls `sgx_qv_verify_quote` on a self-generated test quote and asserts the verdict is in the allowlist. Surfaces APP-OPS-1, APP-OPS-2, and any future DCAP infra issue at boot, not at first peer-quote attempt.

---

# Appendix B — Methodology gap

Every item in Appendix A was found by **manually running the deploy procedure end-to-end and watching it fail**. None were caught by automated unit, integration, or smoke tests. This is a structural problem, independent of any individual bug.

## Concrete missing tests (sized by what would have caught each)

| Finding | Missing test | Effort to add |
|---|---|---|
| APP-AUTH-2 | Unit test: `derive_keypair_from_seed("snoPBrXtMeMyMHUVTgbuqAfg1SUTb")` → `rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh` (XRPL spec test vector) | 30 min |
| APP-WIRE-1 | Integration test: orchestrator's `frost_share_import_v2` call → mocked enclave-server route asserts top-level fields present | 1–2 h |
| APP-OPS-1, 2 | Startup self-check: `sgx_qv_verify_quote` against own quote returns OK or SW_HARDENING_NEEDED | 2 h enclave + 1 h docs |
| APP-PATHA-1 | E2E test on a full 3-node cluster that exercises share-export-v2 → share-import-v2 → frost_sign roundtrip | 4–6 h harness |

## Broader picture

The pattern is structural, not incidental. Before any of Tom's roadmap functionality starts landing, two foundations need to be in place: **test coverage that guarantees finding bugs in production code**, and **automated, repeatable deployment** (with manual SSH+scp+systemctl forbidden as a primary path). Appendix C below specifies them.

---

# Appendix C — Testing methodology and deploy automation

## C.1 Red lines (project-wide invariants)

These are **not** suggestions. They are project-level invariants for any code that lands.

1. **Unit tests live inside the product source.** Rust unit tests in `#[cfg(test)] mod tests` blocks beside the code they test. C/C++ unit tests in the enclave repo, beside the source files they test, built via the same toolchain. *No* Rust unit tests in Python; *no* enclave unit tests in Rust. The auditor reads production code and the tests next to it, in one language.

2. **Why** unit tests must be in the product language: an external auditor who reviews `auth.rs` does not — and should not — also have to review a Python wrapper to know the auth code is correct. The Rust code must stand on its own under Rust-language tests. We just lived through APP-AUTH-2 — a Rust derivation bug never caught because the existing Python tests built against a *different* (xrpl-py-correct) derivation and produced consistent results without ever exercising our path.

3. **Python is allowed when it plays the role of a remote API client** — i.e. it drives the system over the same public HTTP / JSON-RPC surface a real frontend, operator, or external integrator would use. From the audit's perspective the Python code is *outside* the product trust boundary; the audit subject is what the server returns when an arbitrary client calls it. Examples that fit:
   - `tests/test_full_e2e.py` — POSTs to `http://orchestrator:3000/v1/perp/withdraw`, verifies response. Same role as `curl`.
   - `tests/test_xrpl_withdrawal.py` — observes XRPL ledger, asserts state. Black-box.
   - `scripts/setup_testnet_escrow.py` — operator harness, drives faucet + signs txs externally.

4. **Python is NOT allowed when it plays the role of an internal component.** A test where Rust production code calls *into* a Python process — the production-side caller and the Python-side callee can drift in their type contracts, and CI cannot detect drift across the language boundary. The historical `tests/mock_enclave_server.py` (Flask) and its `Dockerfile.mock-enclave` were a Docker-only stand-in for the real enclave with no active consumer; deleted 2026-04-27 (Phase 1.2). Same-language Rust mocks now live inline in `tests/integration_test.rs` (full API exercised) and inside `src/pool_path_a_client.rs::tests` (Path A wire-shape locked in unit tests). Any future addition of a cross-language internal mock is forbidden by this rule.

5. **The deploy parallel.** Manual `ssh + scp + systemctl restart` chains are the operational analogue of cross-language mocks: they "work" until the deploy goes wrong, and then nobody can tell whether the procedure or the tooling failed. Same rule: deployment logic that touches our binaries lives as a Rust subcommand of the orchestrator binary, or as bash with explicit per-step assertions checked in to the repo. Not as a notebook of one-off SSH calls.

## C.2 Current state (measured 2026-04-26)

| Layer | Tests today | Notes |
|---|---|---|
| Orchestrator unit (Rust) | 109 `#[test]` fns across 12 modules | covers auth, p2p, election, orderbook, vault_mm, ws, xrpl_signer, etc. |
| Orchestrator integration (Rust) | 6 tests in `tests/integration_test.rs` (~287 LoC) | uses inline Rust axum mock — same-language, no drift risk |
| Orchestrator e2e | none in Rust | 11 Python files in `tests/` (last touched 2026-04-02 to 2026-04-17) — pre-X-C1, pre-Path A; classification pending in Phase 1.3 |
| Enclave unit (C/C++) | not measured yet | needs grep in `EthSignerEnclave/` repo as Phase 1.0 |
| Coverage measurement | none | no `cargo llvm-cov` / `grcov` in CI |
| CI | `.github/workflows/check.yml` runs `cargo fmt + check + clippy + test` on push/PR | no Python e2e gate, no enclave docker-build gate |

## C.3 Phase 1 — testing parity (~1 week)

### Phase 1.0 — measurement (½ day)

- Run `cargo llvm-cov --html` locally on orchestrator. Record current line coverage as the baseline.
- Grep the enclave repo (`EthSignerEnclave/Enclave/tests/`, `libapp/tests/`) for existing C/C++ tests. Record count.
- Output: a one-page `docs/test-coverage-baseline-2026-04-26.md` file checked in.

### Phase 1.1 — TDD-style fix of audit + appendix items

For each open finding, **the test is written first** (and fails). Then the fix lands and the test passes. No "fix without test."

| Finding | Test to write first | Then fix |
|---|---|---|
| APP-AUTH-2 | Rust unit test in `cli_tools::tests` asserting `derive_keypair_from_seed("snoPBrXtMeMyMHUVTgbuqAfg1SUTb")?.address == "rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh"` (XRPL spec vector). Plus a second vector for ed25519. | Implement family-generator (root + intermediate + master) in `derive_keypair_from_seed`. |
| APP-WIRE-1 (already shipped d72f08c) | Add a Rust integration test: orchestrator's `frost_share_import_v2` body is asserted to contain `threshold` / `n_participants` / `sender_pubkey` at top level. Locks the wire contract. | Already shipped. |
| O-M2 | Rust unit test: timestamp older than 30 s OR newer than +30 s rejected. | Narrow drift to ±30 s. |
| O-M4 | Rust unit test: STP cancel rate-limit returns rate-limited after N cancels in window. | Add cancel rate limiter. |
| O-L2 | Rust unit test: zero-balance fallback no longer fires for unknown user (returns 404 instead). | Drop fallback. |
| O-I2 | Rust unit test on passive-replication path. | Replace `ON CONFLICT DO NOTHING` with stricter handling. |

### Phase 1.2 — Python internal mock removal (✅ completed 2026-04-27)

The original plan was "port `mock_enclave_server.py` to Rust". On inspection it turned out the existing `tests/integration_test.rs` already used an inline Rust axum mock; the Python file was a separate Docker-only stand-in with no active consumer. Actual deliverable shipped:

- Deleted `tests/mock_enclave_server.py` and `tests/Dockerfile.mock-enclave`.
- Added six wire-shape unit tests inside `src/pool_path_a_client.rs::tests` using a same-language inline axum mock. The mock asserts on body shape inline (`for f in [...] { assert!(body.get(f).is_some()) }`), so any drift between client and the enclave-server's actual route contract fails the test at CI time. One test is explicitly an APP-WIRE-1 regression lock.
- `pool_path_a_client.rs` ratchets from 0% to substantial coverage. `perp_client.rs` and `withdrawal.rs` remain at 0% — next ratchet step.

### Phase 1.3 — triage Python e2e files (½ day)

For each of `tests/test_full_e2e.py`, `test_invariants.py`, `test_b31_replication.py`, `test_operator_failure.py`, `test_xrpl_withdrawal.py`, `test_trading_api.py`, `test_e2e.py`, `multisig_coordinator.py`, `scenarios_runner.py`, `setup_multisig_escrow.py`, `reset_multisig_setup.py`:

- Read.
- Classify each by **how it interacts with the system**:
  - **(K-Client) Keep — Python as remote API client** (per C.1 §3). The file uses only the orchestrator's public HTTP/JSON-RPC surface (or XRPL's), inspects no internal state. Bring it back into rotation: pin to current API contract, run against a freshly bumped testnet, fix anything stale, add to the e2e workflow.
  - **(K-Op) Keep — operator harness** (faucet, XRPL submission, etc., not "tests"). Move to `scripts/operator/` to stop misnaming as tests; no CI gate.
  - **(R) Replace** — file stubs out an internal component or asserts on internal state via non-public hooks (per C.1 §4). Add a Rust integration-test ticket for the same scenario; delete the Python file once the Rust replacement lands.
  - **(D) Delete** — tests for code paths removed during the audit cycle.

Most files in `tests/` are expected to fall into **(K-Client)** — they POST to `:3000/v1/...` and check response shape. Those stay; we just need them green again.

Output: a table in the test-coverage doc with a row per file and its classification.

### Phase 1.4 — coverage gate in CI (½ day)

- Add `.github/workflows/coverage.yml` running `cargo llvm-cov --lcov --output-path lcov.info` after the existing `check` job.
- Upload to Codecov OR keep local artifact + a script that fails if line coverage drops below `last_committed_coverage - 1%`.
- Once baseline is measured, ratchet upward in 5-point steps over time. No hard absolute target until we know what's achievable.

### Phase 1.5 — enclave-side parity (½ day, if needed; spillover into Phase 2 OK)

- If grep in 1.0 shows existing C/C++ tests, audit them for the same wire contract: do they assert what the orchestrator expects? Add the missing ones.
- If none exist, write the first three: `dcap_verify` against mock collateral, `frost_keygen` round-trip with `share_export_v2`/`share_import_v2`, `peer_attest_cache_lookup` TTL.
- Run via `docker build -f Dockerfile.azure --target=test` (add a `test` stage in the Dockerfile if absent). Hook into a new `enclave-check.yml` GHA workflow once we have a Hetzner self-hosted runner.

## C.4 Phase 2 — deploy automation (~1 week)

### Phase 2.1 — testnet bump as Rust subcommand (~3 days)

Goal: replace the manual 12-step SSH chain we executed today (§3–§12 of `docs/testnet-enclave-bump-procedure.md`) with `perp-dex-orchestrator testnet-bump --to all --dry-run` / `--apply`.

The subcommand:
1. Runs the dirty-tree check (we wrote it into the doc — encode it in code).
2. Builds artefacts (cargo + docker), captures sha256 + git_sha into a build manifest.
3. Stops services in dependency order.
4. Distributes binaries (3 nodes parallel SSH-via-Hetzner-bastion).
5. Restarts services with health-check assertions between steps (`/v1/pool/ecdh/pubkey` returns 200, `is-active`, etc.).
6. Triggers operator-setup, collects entries, runs `setup_testnet_escrow.py` (`scripts/operator/`), distributes `signers_config.json`, updates `start_orchestrator.sh`, restarts orchestrators.
7. Runs `frost/keygen` on chosen dealer, propagates `frost_group_id` into `shards.toml`, restarts orchestrators.
8. Triggers Path A wire test via `/admin/path-a/share-export`, asserts `published == len(targets)` for each call.

Same script must support `--rollback` to invoke the per-step `prev-<TS>` restoration.

### Phase 2.2 — startup self-check in enclave-server (~1 day)

Before serving traffic, the enclave-server:
- Generates a self quote via `sgx_get_target_info` + `attestation-quote`.
- Calls `sgx_qv_verify_quote` against it.
- Asserts the verdict is in the configured allowlist (env-driven, see APP-PATHA-1).
- On failure: logs the verdict, exits with non-zero.

This catches APP-OPS-1 and APP-OPS-2 at boot, not at first peer-quote attempt 4 minutes later.

### Phase 2.3 — mainnet rotation script (~2 days)

Wrap `docs/deployment-procedure.md §11.5 — Path B` as a bash script `scripts/mainnet-rotation.sh` with **explicit per-step `read -p "confirm step N: "` gates**. It does not auto-execute — it executes on operator confirmation per step. Output of each step is logged to a session file for audit.

The deliberate constraint: production deploys **always** require a human-in-the-loop. Automation reduces the chance of a fat-finger error mid-procedure; it does not remove the operator's signature.

### Phase 2.4 — orchestrator-only deploy gating (~½ day)

Existing `orchestrator/scripts/deploy.sh` is solid (208 lines, supports rollback). Two changes:
- Refuse to deploy if `cargo test --release` doesn't pass (current state of `target/release/` is already proof that the build worked, but we don't run tests there yet).
- After deploy, hit `/v1/health` on each node and assert the response includes `version_sha == git rev-parse --short HEAD`.

## C.5 Sequencing — slotting Phase 1 / 2 into the day-plan

The day-plan from the body of this fix-plan stays, but each Day's first action becomes a *test*, not a fix:

- **Day 0 (~½ day) — measurement:** Phase 1.0. Read what we have before changing anything.
- **Day 1 (4 hours) — TDD bug closures:**
  - Phase 1.1 for AUTH-2 (test → fix), O-M2, O-L2, O-I2, O-I3 — all small, fits in a half-day.
  - Day-1 ends with all those green; coverage measurement re-run; baseline updated.
- **Day 2 (4 hours) — middle-effort + automation foundations:**
  - O-M4 STP rate limiter.
  - Phase 1.2 (Rust mock enclave) — unblocks future integration tests.
  - APP-PATHA-1 enclave config plumb.
- **Day 3 (½ day) — coverage gate + first automated deploy:**
  - Phase 1.4 coverage CI.
  - Phase 2.1 first cut of `testnet-bump` subcommand.
  - Use the subcommand to do the deploy of all the Day-1/2 fixes — validates the automation by use.
- **Day 4 — handoff:**
  - Phase 2.2 startup self-check.
  - Phase 2.3 mainnet rotation script (manual gates, machine-executable).
  - Phase 1.3 Python triage outcome documented.
  - Tom's roadmap implementation can start Day 5+: every change he or anyone else makes to the orchestrator routes through `cargo test` (with coverage gate) and `testnet-bump` (validated deploy). Mainnet rotation is the only manual ceremony, and it's now scripted.

## C.6 Out of scope (intentionally)

- Frontend (Tom's domain). Frontend deploy and frontend testing are a separate workstream; this appendix does not constrain what tooling Tom chooses *for the frontend*.
- Adding new functional features. Phase 1 + 2 explicitly only mature what exists today; new features wait for Day 5+ on top of the two pillars.
- Performance / load testing. Out of audit-closure scope; tracked separately if needed.
