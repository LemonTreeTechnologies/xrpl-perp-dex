# Regression Bibliography

**Status:** MVP, 2026-06-08. Living document — every audit cycle closure SHOULD append.

## Purpose

For each finding that was reviewed → fixed → closed via audit cycle, this document records the **regression artifact** that pins the fix so future code changes cannot silently reintroduce the bug.

Without this document the audit cycle reviews CHANGES (happy-path verification) but does not protect against history-path regressions — a refactor that reintroduces a previously-fixed bug would pass review by default.

## Discipline (proposed; pending auditor confirmation)

When a REQ-N / RESP-N cycle closes with a PASS-class verdict on a fix, **before merge** the dev SHOULD ensure one of these regression artifacts exists and append the row to §Bibliography below:

| Artifact type | Where | Example |
|---|---|---|
| **Unit test** with name encoding the finding ID | source file's `mod tests` | `pubkey_to_account_id_matches_xrpl_spec_vector` (auth.rs, REQ-20 R2 commit 4) |
| **`static_assert`** in C++ encoding the architectural invariant | enclave source | `static_assert(... <= ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD, ...)` (Enclave.cpp, X-IMPL-1) |
| **Check script** in `scripts/` | `scripts/check-*.sh` invoked by CI | `scripts/check-seal-policy.sh` (REQ-16, MRSIGNER→MRENCLAVE) |
| **Coverage-tracked file** | `coverage-baseline.json` + `scripts/check-coverage.py:TRACKED_PATHS` | Per-file ratchet on `vault_mm.rs` (V1 vault) |

The auditor's RESP-N PASS verdict should require existence of one such artifact (or explicit «no artifact applicable — invariant lives in design layer» with reasoning).

## Bibliography

### REQ-16 — MRSIGNER → MRENCLAVE seal policy fix

- **Bug class:** 24-day security-theater bug; `sgx_seal_data` defaults to MRSIGNER policy, allowing any enclave sharing the signing key to unseal customer state. Required MRENCLAVE-binding for sealed state.
- **Artifact:** `EthSignerEnclave/scripts/check-seal-policy.sh` invoked by CI; scans for `sgx_seal_data(` calls and asserts each is paired with `seal_mrenclave_*` wrapper or an explicit comment.
- **Tagged in code:** `EthSignerEnclave/Enclave/seal_mrenclave.h` (the wrapper that enforces correct policy).
- **Audit cycle:** REQ-16 (private repo).

### REQ-7 / REQ-7.5 / REQ-8 — Path A migration cap arithmetic

- **Bug class:** sealed-state cumulative chunk math could exceed Path A migration framework's per-shard file cap; silently breaks `ecall_perp_save_state`.
- **Artifact:** `static_assert` at `EthSignerEnclave/Enclave/Enclave.cpp:5172-5180` against `ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD`. Compile fails if any future struct-size growth exceeds the cap.
- **Audit cycle:** REQ-7.5 (private repo).

### REQ-18 — A-PA-1 LA-verify must reject debug-attribute enclaves

- **Bug class:** SGX DEBUG attribute is NOT measured into MRENCLAVE; a debug-launched allowlisted enclave has the SAME MRENCLAVE but is fully host-controllable.
- **Artifact:** `static_assert` / runtime check + regression test (queued — task #102 «First-non-debug-build gate set (R18-L1 + A-PA-1 regression test)»; **NOT YET LANDED**).
- **Audit cycle:** REQ-18 (private repo).
- **Gap:** regression artifact not yet in place. Tracked under task #102.

### REQ-20 D-7 — spec-vs-code cap reconciliation

- **Bug class:** Spec claimed `MAX_DEPOSIT_BINDINGS = 100k` aligned with `MAX_PERP_USERS` pattern; actual `MAX_PERP_USERS = 5k`. 20× spec/code mismatch persisted across 3 audit rounds (RESP-20, RESP-20.1, RESP-20.2) before catch.
- **Artifact:** Direction (b) reconciliation locked numeric constants in `PerpState.h:23-27` (`MAX_PERP_USERS = 10000`, `MAX_TX_HASHES = 30000`, `MAX_DEPOSIT_BINDINGS = 30000`); `static_assert` in `Enclave.cpp` exercises the cumulative cap math.
- **Methodology rule:** RESP-20.3 §Q5'' proposed **VCE (Verify-Claimed-Equivalences-against-actual-code)** — when spec text claims «matches X», «aligned with Y», «per existing pattern at file:line», auditor MUST grep/read the actual code, not trust the spec restatement. **Pending formalization in AUDIT-PROTOCOL.**
- **Audit cycle:** REQ-20 → REQ-20.3 (private repo).

### REQ-20 X-IMPL-1 — `ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD` cap bump

- **Bug class:** RESP-20.3 §Q4'' verified per-term `uint32` arithmetic safe but did NOT cumulate against the hard cap `= 32u`. At direction (b) sized state, cumulative usage = 176 chunks.
- **Artifact:** `static_assert` at `EthSignerEnclave/Enclave/Enclave.cpp:5172-5180` already in place (caught the issue at compile time); cap bumped to 200u at `EnclaveLimits.h:70` with explicit comment naming the 176-chunk derivation + headroom.
- **Audit cycle:** REQ-20-impl R1 commit 1 (private repo).

### REQ-20 — DestinationTag attribution fix

- **Bug class:** Deposit scanner correctly parsed XRPL `DestinationTag` but credit path used `tx.Account` directly, ignoring tag. For exchange users (sender = hot wallet), all funds attributed to the exchange address as `user_id`.
- **Artifact (orchestrator):** 4 unit tests in `orchestrator/src/auth.rs:1153-1247` pinning `pubkey_to_account_id` derivation against XRPL spec vector + self-consistency invariants. This pins the load-bearing identity gate that the enclave constant-time compares (X-IMPL-2 design (b)).
- **Artifact (enclave, audit-merged via RESP):** D-AM-1..16 acceptance matrix items verified in `docs/audit/RESP-20-impl-R1.md` per-commit table.
- **Artifact gap (acknowledged):** L-IMPL-1 (XRP-asset bindings unsupported) is documented as known v1 limitation; expected `DB_INVARIANT_VIOLATION` is the regression signal but not yet a regression test. Follow-up commit adds asset_class field.
- **Audit cycle:** REQ-20 → REQ-20-impl R1 (private repo, merged 2026-06-07).

### REQ-19 — V1 vault operability fixes

- **Bug class:** Three operability bugs caught during cluster smoke (X-19-1 close-order path, X-19-2 cold-start sentinel, Q-19-4 live denominator for margin balance).
- **Artifact:** Fixed in commit `6815958`. Per-finding tests NOT YET landed — vault_mm.rs at 17.0% coverage with most of the fix paths uncovered.
- **Gap:** vault_mm.rs needs regression tests for the 3 X/Q findings. Added to coverage TRACKED_PATHS 2026-06-08 (this PR) so ratchet now applies; per-finding unit tests pending.
- **Audit cycle:** REQ-19 (private repo, closed).

### RESP-ACC-1 — Accessibility methodology cycle

- **Bug class:** V1 vault E2E discovered API blocked by NSG from external; passed internal-correctness + audit + CI but blocked by network-layer config. Same arch-bug class as «rebuild to tune util_cap», but at network layer.
- **Artifact:** `docs/test-env-workflow.{en,ru}.md` (workflow discipline preflight → staging → Tom acceptance); E-2E-1 nginx `non_idempotent` fix recorded in memory `reference_nginx_non_idempotent_fix`.
- **Methodology rule:** INV-OPS-2 added to PROJECT-INVARIANTS v0.7 — every API/service MUST be reachable from intended-consumer-environment, codified per env, verified per REQ.
- **Gap:** nginx config lives only on Hetzner host, not in any repo. Production-grade resolution = move to managed (Ansible/Terraform/repo-tracked) form. Backlog.
- **Audit cycle:** REQ-ACC-1 (private repo, closed).

## Pending entries (referenced by existing tasks)

| Task | Finding | Artifact needed |
|---|---|---|
| #93 | Margin accounting double-count in close/liquidate | Regression test in trading.rs or vault_mm.rs reproducing the double-count math |
| #98 | R17-I1: `check-seal-policy.sh` comment-line regex | Update the existing script (regression artifact is the script itself) |
| #99 | Auditor-note 2026-05-22 legacy signing residuals (O-1, O-2) | TBD per finding scope |
| #102 | First-non-debug-build gate set (R18-L1 + A-PA-1 regression test) | The «regression test» component of this task IS the artifact for REQ-18 |
| #108 | O-1 api.rs req.label privilege gate (before V2 rebate) | Unit test + handler-side privilege check |
| #112 (post-R2-merge) | REQ-20-impl R2 D-AM matrix items 2/3/5/7/8/9 | Cluster smoke + per-D-AM test artifact |

## Methodology amendment (proposed for auditor)

This document codifies a discipline that does NOT yet exist as a formal audit rule. The proposed amendment to AUDIT-PROTOCOL:

> **Rule N+1 — Regression Artifact Requirement.** RESP-N PASS verdict requires the dev to point to a regression artifact (unit test, `static_assert`, check script, or coverage-tracked file) that would fail if the bug were silently reintroduced by a future change. The artifact MUST be referenced in `docs/REGRESSION-BIBLIOGRAPHY.md` before RESP-N PASS is recorded. Exception: «no artifact applicable — invariant lives in design layer» with reasoning; auditor judges.

Parallel to the VCE rule (RESP-20.3 §Q5'', also pending formalization). Both rules belong in the same AUDIT-PROTOCOL amendment cycle.

## Cross-reference

- `docs/audit/AUDIT-PROTOCOL.md` (private repo) — the canonical audit cycle protocol; this discipline should land there once auditor confirms
- `scripts/check-coverage.py` + `coverage-baseline.json` — coverage ratchet, one of the four artifact types
- `EthSignerEnclave/scripts/check-seal-policy.sh` (private repo) — example check-script artifact
- `EthSignerEnclave/Enclave/Enclave.cpp:5172-5180` (private repo) — example `static_assert` artifact
- `orchestrator/src/auth.rs:1153-1247` — example unit-test artifact (REQ-20 R2 pubkey_to_account_id pinning)
