# V1 vault E2E report — 2026-05-26

**Branch:** `LemonTreeTechnologies/xrpl-perp-dex` — `feat/v1-vault-vamm` @ `6815958`
**Cluster:** Azure 3-node testnet-cluster (`testnet-cluster` env)
**Tom-facing endpoint:** `https://api-dev.xperp.fi` (Hetzner nginx TLS-fronts Azure, passive failover via `proxy_next_upstream`)
**Audit context:** REQ-19 PASS + RESP-19-addendum PASS already on `private:main`; E2E ran post-closure for product-spec-faithfulness verification and Tom-handoff readiness.

## 0. Verdict

**PASS for V1 vault scope** (Tom-spec-faithfulness, external consumer accessibility, failover correctness, persistent state across respawn).

One real bug discovered and fixed during E2E (E-2E-1, nginx config). Three scenarios explicitly deferred with rationale.

## 1. Scenarios run

| ID | Scenario | Verdict | Notes |
|---|---|---|---|
| T1.1 | Hysteresis (UtilHot enter → close → resume below threshold) | PASS | util 0.152 → 0.028 across reduce cycle, no flapping |
| T1.3 | Multi-fill ladder consumption (single IOC across 4 levels) | PASS | 3 positions tracked, UtilHot picks largest-first |
| T2.2 | Sequencer kill mid-active-vault-state | PASS | nginx transparent retry, 5/5 signed POST 200 @ 67-153ms during failover |
| T2.3 | Restart with active positions | PASS (covered by T2.2) | Vault respawn re-initializes Curve + re-computes posture from live balance |
| T1.2 | Refresh threshold ≥5bps | SKIPPED | Unit-tested (`vamm::tests`); deterministic external repro is hard |
| T1.4 | Posture boundary util=0.0499 vs 0.0501 | SKIPPED | Same as T1.2 |
| T1.5 | DeltaHot posture | SKIPPED | Requires `delta_cap` config override; not exercised by smoke TOML |
| T2.1 | Sequencer kill mid-pending-close | COVERED by T2.2 | Same code path (singleton respawn re-fires UtilHot from new sequencer) |
| T2.4 | Mark feed glitch handling | DEFERRED | Already observed naturally (`XRPL account_tx error: Ledger indexes invalid` warns) — vault loop survives |

## 2. E2E findings

### E-2E-1 — nginx `proxy_next_upstream` missed `non_idempotent`

**Surfaced by:** external `api-dev.xperp.fi` probe pattern (loopback testing missed by design).

**Symptom:** signed POST `/v1/orders` returned 503 «this node is not the sequencer» in ~3/5 attempts. nginx round-robin selected one of three Azure nodes; validator returned 503; nginx **did not retry** even though `proxy_next_upstream error timeout http_503` was set.

**Root cause:** nginx by default treats POST as non-idempotent — `proxy_next_upstream` does NOT retry POST requests unless the `non_idempotent` keyword is added. The earlier failover test (kill node-1 → signed POST succeeded on node-2) worked only because the *connection failed before sending the request* (TCP refused = `error`, always retryable). Once a node accepts and *responds* 503 to a POST, nginx defaults to NO retry.

**Fix:** add `non_idempotent` to the upstream directive:
```nginx
proxy_next_upstream error timeout http_503 non_idempotent;
proxy_next_upstream_tries 3;
```

**Verified post-fix:** 5/5 signed POSTs returned 200 OK, then 5/5 returned 200 OK during deliberate node-1 kill / failover.

**Methodology fit:** exactly the class INV-OPS-2 / direction-1b (g) prompt was meant to catch — invisible from loopback testing, only surfaces from an external network position with the consumer's actual request pattern (POST with body).

**Operational impact:** Tom (and any external client) without this fix would have seen intermittent 503 errors in ~2/3 of write requests, with no observable pattern (round-robin is randomized). Could easily be misdiagnosed as «cluster instability» rather than «nginx config gap».

### E-2E-2 — vault closes stay open without external liquidity (expected)

vault's UtilHot reduce path uses `submit_close_order` (REQ-19-addendum X-19-1 fix) which sends a market IOC. With no opposing-side liquidity on the orderbook, the IOC cancels without filling. The vault's `UtilHot persistent — reducing` log message fires every tick until liquidity appears (or another fill drains position).

This is **not a bug** — it's correct semantic of «cancel-first / close-only-if-still-over». Document mentioned for Tom's awareness: vault under UtilHot needs external counterparty for position relief. In production, multi-LP / arb-bot environment ensures that always exists.

## 3. State after run

Vault on Azure node-1 (sequencer): 3 open short positions (residual from T1.3 multi-fill), util ≈ 0.20, UtilHot active (waiting for taker liquidity). Acceptable for E2E end state; will drain naturally over taker activity or get cleared in next reset cycle.

## 4. What was NOT tested (deferred / known gaps)

- **Long-running stability (Tier 3 passive 24-48h)** — would require a 24-hour observation window. State: vault left running with active positions; will check next session for memory growth, log volume drift, stuck states, election cycles.
- **DCAP attestation verification from Tom's side** — `/v1/attestation/*` endpoints exist and proxy through nginx, but Tom-side independent verification against Intel CA chain was not exercised. Tom's eventual quickstart should include this.
- **Multi-operator FROST withdrawal** — vault user can in principle initiate withdrawal; the multi-op signing relay code is exercised by separate test paths, not vault-specific.
- **Mainnet-like XRPL endpoint flap** — production XRPL endpoint resilience would need dedicated testing.

## 5. Recommendation for V1 vault closure

V1 vault `feat/v1-vault-vamm @ 6815958` is **product-spec-faithful, externally-accessible, failover-correct, multi-position-tracking-correct** for the scenarios in scope.

**Ready for Tom review** at `https://api-dev.xperp.fi` against the Tom-spec deliverables (`docs/post-hackathon-specs.md` + Tom's inline answers in `docs/post-hackathon-specs-response.md`). After Tom's product-spec acceptance, ready for merge of `feat/v1-vault-vamm` to `master`.

The nginx `non_idempotent` fix (E-2E-1) is a deployment-config change on Hetzner; it is NOT part of the orchestrator code branch and lives in `/etc/nginx/sites-enabled/api-dev.xperp.fi`. Should be codified in a runbook entry so future deployments preserve it.

## 6. Cross-references

- Branch: `feat/v1-vault-vamm @ 6815958` (PR #15 series; not yet merged to master pending this E2E + Tom review)
- Audit: `docs/audit/REQ-19.md` + `docs/audit/RESP-19.md` + `docs/audit/RESP-19-addendum.md` (private repo `main`)
- Workflow doc: `docs/test-env-workflow.{en,ru}.md` (public repo `master`)
- Methodology: AUDIT-PROTOCOL v1.3 (operability axis A-E + direction-1b prompt (g)); PROJECT-INVARIANTS v0.7 (INV-OPS-1 + INV-OPS-2)
- E-2E-1 anchor for INV-OPS-2: external-position smoke caught a config gap invisible from loopback. This is the second confirmed anchor for direction-1b (g) (first was V1 vault E2E's NSG-blocked-consumer discovery 2026-05-26 earlier the same day). Two-anchor count starting; per RESP-ACC-1 §3, if a 3rd surfaces within ~5 cycles, accessibility may promote from §1b footer to full Category F.
