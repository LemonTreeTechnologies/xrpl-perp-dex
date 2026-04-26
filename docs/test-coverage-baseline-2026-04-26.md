# Test coverage baseline — 2026-04-26

**Purpose:** the Day 0 measurement called for in `SECURITY-REAUDIT-4-FIXPLAN.md` Appendix C, Phase 1.0. Records the state of automated testing in both the orchestrator (Rust) and the enclave (C/C++) before any audit-cycle test work begins, plus a triage of the existing Python test suite. Subsequent commits ratchet the numbers; this file is the reference point.

**How produced:**
- Rust: `cargo-llvm-cov llvm-cov --summary-only --no-fail-fast` on Hetzner against `~/llm-perp-xrpl/orchestrator` at commit `e729a95` (master).
- Enclave: grep for `test_*.cpp`, `*_test.cpp`, ad-hoc `static int test_*` patterns under `~/xrpl-perp-dex-enclave/EthSignerEnclave/` at origin/main `8934f63`.
- Python: read each file's docstring + first 25 lines, classified per Appendix C §1.3.

---

## 1. Orchestrator (Rust)

### Aggregate

| Metric | Value |
|---|---|
| Tests passing | **117** (111 unit + 6 integration) |
| Tests failing | 0 |
| Line coverage | **30.40%** (2440 covered / 8026 total executable lines) |
| Region coverage | 33.89% |
| Function coverage | 34.61% (235 covered / 679 total) |

### Per-file (lines covered / total, sorted)

**Strong (>80% line coverage):**
| File | Lines cov | Notes |
|---|---|---|
| `types.rs` | 95.09% | core FP8 / Side / serde — well-tested |
| `xrpl_signer.rs` | 90.76% | XRPL signing / address derivation primitives |
| `singleton.rs` | 91.40% | small file, well-covered |
| `orderbook.rs` | 88.68% | resting-orders + matching |
| `election.rs` | 85.29% | sequencer election / heartbeat |
| `http_helpers.rs` | 83.02% | loopback-only client factory |

**Medium (40–80%):**
| File | Lines cov | Notes |
|---|---|---|
| `auth.rs` | 70.54% | XRPL auth + alt-hash; O-H1 fix tested |
| `ws.rs` | 64.19% | WebSocket control frames + event fan-out |

**Low (10–40%):**
| File | Lines cov | Notes |
|---|---|---|
| `p2p.rs` | 27.59% | gossipsub + signing relay; X-C1 tests bump this |
| `vault_mm.rs` | 17.00% | market-making vault; some inventory tests |
| `commitment.rs` | 10.53% | state commitment helpers |

**Critical gaps (<10%):**
| File | Lines cov | Notes |
|---|---|---|
| `cli_tools.rs` | **7.45%** | **Where APP-AUTH-2 (`derive_keypair_from_seed`) lives. The bug was never caught because there is essentially no test of any CLI path including the seed derivation that fails the XRPL Family Generator spec.** |
| `withdrawal.rs` | 0% | full multisig withdrawal orchestration — completely untested |
| `api.rs` | 0% | 907 lines — REST API routes; hit only via mock-enclave integration test (which only covers happy paths) |
| `path_a_redkg.rs` | 0% | Path A admin-share-export driver |
| `pool_path_a_client.rs` | 0% | Path A enclave client — **APP-WIRE-1 lived here uncaught** |
| `perp_client.rs` | 0% | enclave RPC wrapper |
| `xrpl_monitor.rs` | 0% | XRPL deposit scanner |
| `trading.rs` | 0% | order/position routing |
| `shard_router.rs` | 0% | shards.toml + path_a_groups |
| `db.rs` | 0% | PostgreSQL persistence |
| `main.rs` | 0% | orchestration glue (acceptable for `main`-style code) |
| `price_feed.rs` | 0% | price oracle |

### Reading

The high-coverage files are the ones that have shipped real bug-fix tests over the audit cycle (X-C1 in `p2p.rs`, O-H1 in `auth.rs`, XRPL primitives in `xrpl_signer.rs`). The zero-coverage files are everything that touches the network boundary plus all Path A code. **The two appendix-A findings caught manually (APP-AUTH-2, APP-WIRE-1) sit precisely in the zero/low-coverage areas.**

The integration tests in `tests/integration_test.rs` use an inline Rust axum mock — same-language already. The pre-existing `tests/mock_enclave_server.py` (Flask) was a separate Docker-only mock with no active consumer; both it and `tests/Dockerfile.mock-enclave` were deleted on 2026-04-27 (Phase 1.2 completion) along with adding a Rust axum mock inside `pool_path_a_client.rs::tests` that exercises the Path A wire shape end-to-end, including an APP-WIRE-1 regression-lock test.

---

## 2. Enclave (C/C++)

### Test artefacts found

| Location | Tests | Description |
|---|---|---|
| `Enclave/Enclave.cpp` (in-tree, `static int test_*`) | **9** | `test_find_account_in_pool`, `test_load_account_to_pool`, `test_unload_account_from_pool`, `test_generate_account_in_pool`, `test_sign_with_pool_account`, `test_get_pool_status`, `test_keccak_address_generation`, `test_pool_capacity_and_hash_table`, `test_signature_generation`. Driven via the `test_suite_t` ad-hoc runner; no gtest / catch2. |
| `libapp/test/test_libapp.cpp` (399 lines) | **10** | `test_load_account`, `test_unload_account`, `test_pool_status`, `test_generate_account`, `test_generate_account_with_recovery`, `test_sign_with_session`, `test_schnorr_sign_with_session`, `test_schnorr_taproot_sign_with_session`, `test_get_report`, `test_embedded_tests` (the last reaches into the Enclave.cpp suite via an ECALL). |
| `Makefile` `test:` target | builds-only | `test: all` — builds the enclave+libapp+server but does not actually run a test command in the recipe (we'd need to invoke the built `test_libapp` binary manually after `make test`; not wired). |

**Total enclave-side tests:** 19 (9 in-enclave + 10 host-via-libapp), no harness automation, no CI hook.

### Path A coverage gap

| File | Tests |
|---|---|
| `Enclave/dcap_verify.cpp` (DCAP peer-quote verify) | **0** |
| `Enclave/ecdh_identity.cpp` (per-instance ECDH identity, sealed) | **0** |
| `Enclave/peer_attest_cache.cpp` (32-slot LRU + TTL) | **0** |
| `Enclave/ecdh_aes.cpp` (HKDF + AES-128-GCM with structured AAD) | **0** |
| `ecall_verify_peer_dcap_quote` ecall | 0 |
| `ecall_frost_share_export_v2` / `_import_v2` ecalls | 0 |

**Reading:** the entire Path A trust+transport infrastructure has zero in-tree tests. Phase 5 of Path A (REST endpoints, libapp wrappers) shipped without parallel test files. This explains why APP-WIRE-1 (top-level field mismatch in `import-v2`) and APP-PATHA-1 (verdict-policy hardcoding) were not caught at build time.

The pre-Path-A code has reasonable test coverage (account pool / sign / keccak / signature_gen), so the precedent for adding tests in the enclave exists; Path A was the regression.

---

## 3. Python e2e triage

Per `SECURITY-REAUDIT-4-FIXPLAN.md` Appendix C §1.3 classification.

| File | LoC | Class | Notes |
|---|---|---|---|
| `tests/test_full_e2e.py` | 12 858 | **K-Client** | Drives orchestrator REST + WebSocket + (legacy) SSH-tunnel access to enclaves. Pin to current API after Phase 1.2 lands the Rust mock; remove SSH-tunnel paths in favour of bastion-routed ssh as our deploy already does. |
| `tests/test_e2e.py` | 16 025 | **K-Client** | Pure REST client of `:3000` — auth → deposit → trade → position → withdraw lifecycle. Cleanest of the bunch. |
| `tests/test_invariants.py` | 20 529 | **R** (Replace) | Tests **internal** FP8 arithmetic of the enclave (fp_mul, fp_div, balance/margin/PnL math) by reaching into enclave HTTP. By Appendix C §1.4 this is exactly the cross-language pattern we're removing. Replace with C++ unit tests for FP8 + Rust unit tests for any orchestrator-side mirroring (there's a `types.rs` FP8 already at 95% coverage — extend it or pull the C++ logic into a shared test vector file). Once Rust+C++ replacements exist, delete the Python file. |
| `tests/test_b31_replication.py` | 9 041 | **K-Client** | Drives orchestrators on each Azure node, observes PG replication. Cross-process E2E. Modernize SSH-tunnel arch (currently `:3091/:9188` — should use direct bastion-via-:3000). |
| `tests/test_operator_failure.py` | 13 146 | **K-Client** | Multisig-failure scenarios via API. SSH-tunnel arch needs same modernization. |
| `tests/test_xrpl_withdrawal.py` | 6 510 | **K-Client** | Withdraw flow against live testnet escrow. Will need rebasing on the new `rGkUXr1S…` escrow created during this session's deploy. |
| `tests/test_trading_api.py` | 3 584 | **K-Client** | Smallest, simplest REST-of-`:3000` smoke. Easy to keep green. |
| `tests/multisig_coordinator.py` | 11 624 | **K-Op** | "Coordinator that submits multisig'd XRPL tx for failure-mode testing" — orchestration tooling, not assertion-driven. Move to `scripts/operator/multisig_coordinator.py`, add the C.1 §3 disclaimer header. |
| `tests/scenarios_runner.py` | 35 243 | **K-Op** | Failure-mode scenario framework. Operator-controlled batch runner. Move to `scripts/operator/`. |
| `tests/setup_multisig_escrow.py` | 3 900 | **K-Op** | Faucet escrow setup. Already partially superseded by `orchestrator/scripts/setup_testnet_escrow.py` (which has the canonical-seed-file logic) — likely **D** once that script subsumes the operator-flow pieces. Re-check during Phase 1.3 implementation; for now classify K-Op pending merge. |
| `tests/reset_multisig_setup.py` | 4 028 | **K-Op** | Wipes & resets test infrastructure on the 3-Azure cluster. Move to `scripts/operator/`. |

**Triage totals:**
- **K-Client (6 files):** keep, modernize against current API + bastion arch, include in CI e2e workflow when Phase 2 lands.
- **K-Op (4 files, possibly 5 after merge with `setup_testnet_escrow.py`):** move to `scripts/operator/`, add disclaimer headers; not in CI.
- **R (1 file):** `test_invariants.py` — schedule replacement with same-language unit tests during Phase 1.5 (enclave-side parity).
- **D (0 files):** none flagged for outright deletion.

---

## 4. Summary

| Layer | What we have | What's missing |
|---|---|---|
| Orchestrator unit (Rust) | 111 tests, 30.40% line coverage | XRPL Family Generator vector test for `derive_keypair_from_seed`; coverage of `cli_tools`, `withdrawal`, `pool_path_a_client`, `perp_client`, `path_a_redkg` |
| Orchestrator integration (Rust) | 6 tests via inline Rust axum mock; +6 unit tests on Path A wire shape via in-module Rust mock (added 2026-04-27, Phase 1.2) | coverage of full withdraw flow remains 0%; integration test for that is a separate ratchet step |
| Enclave unit (C/C++) | 19 tests for pre-Path-A code | Tests for ALL Path A files: `dcap_verify`, `ecdh_identity`, `peer_attest_cache`, `ecdh_aes`, v2 ecalls |
| Python e2e | 6 K-Client + 4 K-Op + 1 R | CI workflow that runs them; modernization to current bastion arch + new escrow address |
| Coverage gating | none | `cargo llvm-cov` ratchet in CI per Appendix C §1.4 |

## 5. Ratchet plan (Phase 1 deliverables, in priority order)

Each line below is a separate PR / atomic change:

1. **Phase 1.1 — TDD bug closures.** AUTH-2 first (test → fix → ratchet `cli_tools.rs` upward). O-M2, O-L2, O-I2, O-I3 to follow.
2. **Phase 1.2 — Rust mock enclave.** ✅ Completed 2026-04-27 (commits `7819aeb` series). `tests/mock_enclave_server.py` and `tests/Dockerfile.mock-enclave` deleted (no active consumer). Six new wire-shape unit tests added to `pool_path_a_client::tests` (in-module Rust mock — same-language so any client-side drift breaks at compile time or fails the assert). One of the tests is an APP-WIRE-1 regression lock — pins that `frost_share_import_v2` body lifts `threshold/n_participants/sender_pubkey` to top level. `pool_path_a_client.rs` ratchets from 0% to substantial coverage; `perp_client.rs` still 0% (next ratchet step).
3. **Phase 1.3 — Python triage execution.** Move K-Op files to `scripts/operator/`. Disclaimer headers. K-Client files stay; CI workflow added.
4. **Phase 1.4 — Coverage gate in CI.** `cargo-llvm-cov` baseline = 30.40%. PR fails if line coverage drops below `current - 1pp`. Ratchet upward as Phase 1.1/1.2 work lands.
5. **Phase 1.5 — Enclave parity.** First three test files: `test_dcap_verify.cpp`, `test_ecdh_aes.cpp`, `test_peer_attest_cache.cpp`. Wire `Makefile test:` to actually run `test_libapp` binary + the Enclave.cpp suite.
6. **Phase 1 (R replacement).** Port `test_invariants.py`'s FP8 arithmetic vectors into a shared `tests/fp8_test_vectors.json`, consume from both Rust unit tests and C++ unit tests. Delete the Python file once both consumers green.

The same-language testing red lines from Appendix C §1 govern all of the above.