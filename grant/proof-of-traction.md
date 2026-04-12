# Proof of Traction — `xrpl-perp-dex`

**Compiled:** 2026-04-09
**Purpose:** independent verification pack for XRPL Grants reviewers.
Every claim below resolves to a URL, a tx hash, a commit, or a file in
the public repository.

---

## 1. Live endpoints (try them now)

| Endpoint | Expected response |
|---|---|
| `GET https://api-perp.ph18.io/v1/openapi.json` | 200, JSON OpenAPI 3.0 spec |
| `GET https://api-perp.ph18.io/v1/markets` | 200, `XRP-RLUSD-PERP` listing |
| `GET https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/funding` | 200, current funding rate |
| `GET https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/orderbook` | 200, bids/asks |
| `wss://api-perp.ph18.io/ws` | 101 Upgrade, then live ticker events |

```bash
# One-liner validation
curl -s https://api-perp.ph18.io/v1/openapi.json | jq '.info'
curl -s https://api-perp.ph18.io/v1/markets | jq '.markets[0]'
```

---

## 2. XRPL testnet on-chain evidence

**Escrow account:** [`rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx`](https://testnet.xrpl.org/accounts/rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx)

**SignerListSet state on chain:** 3-of-4 (after scenario 3.8 expansion)
— originally 2-of-3, rotated through scenarios 3.5–3.7, expanded in 3.8.

**Ten transaction hashes** from the failure-mode test suite, all on
XRPL testnet, all click-through to the live explorer:

### Payment transactions (multisigned Payment from escrow)

| Scenario | Hash |
|---|---|
| 3.1 alive signers success | [`FC9C32DBCC422368888518FE143AA0715BA1D1FD6E26B5EC45DA52B471D8A1D0`](https://testnet.xrpl.org/transactions/FC9C32DBCC422368888518FE143AA0715BA1D1FD6E26B5EC45DA52B471D8A1D0) |
| 3.4 honest retry after garbage sig | [`90819EC6CBA5ACC549DEF5496B665BBFA959C8FA7887C4500D779B2B917ED25F`](https://testnet.xrpl.org/transactions/90819EC6CBA5ACC549DEF5496B665BBFA959C8FA7887C4500D779B2B917ED25F) |
| 3.5 post-rotation withdraw | [`8274091DFB2A0FA83B33473E31A60E90E24C832DA9B12788259D4C5DD5887F14`](https://testnet.xrpl.org/transactions/8274091DFB2A0FA83B33473E31A60E90E24C832DA9B12788259D4C5DD5887F14) |
| 3.6 post-replacement withdraw | [`F455EBCCBC7B858854E1A1DCCEC86430C53897319BFE6C1D11083B280C68F7BE`](https://testnet.xrpl.org/transactions/F455EBCCBC7B858854E1A1DCCEC86430C53897319BFE6C1D11083B280C68F7BE) |
| 3.7 post-migration withdraw | [`8931EFAA52AED2351D7BAE0313B84B6157386CD7B73389C4190FF5E4B975CBBB`](https://testnet.xrpl.org/transactions/8931EFAA52AED2351D7BAE0313B84B6157386CD7B73389C4190FF5E4B975CBBB) |
| 3.8 post-expand 3-of-4 withdraw | [`49629B8F72E8E1624B6C781DEBED3E5D734BA27530492A7A5B2D2C2AFCC7E38F`](https://testnet.xrpl.org/transactions/49629B8F72E8E1624B6C781DEBED3E5D734BA27530492A7A5B2D2C2AFCC7E38F) |

### SignerListSet transactions (multisigned SignerListSet update)

| Scenario | Hash |
|---|---|
| 3.5 rotation | [`23E1C0EE68EB000A5C922E8D6D8A68AC042A024D1CF2FB8A437D65D47F11E809`](https://testnet.xrpl.org/transactions/23E1C0EE68EB000A5C922E8D6D8A68AC042A024D1CF2FB8A437D65D47F11E809) |
| 3.6 hardware replacement | [`B056CA00778054F041335F05054B106E6CBA22713A33611CCCFC924B19E1A686`](https://testnet.xrpl.org/transactions/B056CA00778054F041335F05054B106E6CBA22713A33611CCCFC924B19E1A686) |
| 3.7 cloud migration | [`8899292EE67D800A136741C9A3950054248658625156AD7C23E4CD24E7CA3E23`](https://testnet.xrpl.org/transactions/8899292EE67D800A136741C9A3950054248658625156AD7C23E4CD24E7CA3E23) |
| 3.8 expand to 3-of-4 | [`B992884B8A19204014AD59D899B0C66D5F3AFF69625C7795E1A5D7E3D80DAA8B`](https://testnet.xrpl.org/transactions/B992884B8A19204014AD59D899B0C66D5F3AFF69625C7795E1A5D7E3D80DAA8B) |

**How to verify independently:** each hash above is a clickable link to
the XRPL testnet explorer. Every transaction shows the multisig
`Signers[]` array with the 3 (or 4) enclave account addresses and their
DER-encoded signatures. No authorization is needed to view.

---

## 3. DCAP remote attestation (hardware proof)

All 3 Azure DCsv3 nodes return a valid 4,734-byte Intel-signed SGX
Quote v3 via `POST /v1/pool/attestation-quote`:

```bash
# From any Azure DCsv3 node (loopback, requires SSH to operator host)
curl -sk -X POST https://localhost:9088/v1/pool/attestation-quote \
  -H "Content-Type: application/json" \
  -d '{"user_data":"0xdeadbeef"}'
# → {"quote_hex":"0x03000200...","quote_size":4734,"status":"success"}
```

**Structure of a returned quote:**

- 2-byte header version (0x0300 — Quote v3)
- 2-byte attestation key type (0x0002 — ECDSA P-256)
- Reserved fields
- QE vendor ID
- User data (the 64-byte blob we passed in: `0xdeadbeef` padded to 64)
- Report body: MRENCLAVE, MRSIGNER, ISVPRODID, ISVSVN, CPUSVN, etc.
- Signature data: PCK certificate chain + QE3 report + quote signature

**Verification path (what a third party would do):**

1. Parse the 4,734-byte blob per [Intel SGX DCAP ECDSA Quote v3 format](https://download.01.org/intel-sgx/latest/dcap-latest/linux/docs/Intel_SGX_ECDSA_QuoteLibReference_DCAP_API.pdf)
2. Extract the PCK certificate chain
3. Verify the chain against Intel's SGX Root CA
4. Verify the QE3 report signature against the PCK cert
5. Verify the user quote signature against the QE3-issued attestation key
6. Extract the MRENCLAVE and compare against the hash of the published
   enclave.signed.so in our GitHub release

Our deployment docs walk through this:
`xrpl-perp-dex-enclave/EthSignerEnclave/docs/azure_dcap_deployment.md`

**Known issues we documented while making DCAP work on new Azure VMs:**

- `AZDCAP_DEBUG_LOG_LEVEL` must be set or the Azure DCAP client pollutes
  stdout of any child process
- Freshly-created VMs may need `az vm deallocate` + `az vm start` to
  land on a host with provisioned PCK certificates in Azure THIM

These hours-of-debugging-turned-into-one-paragraph moments are
documented so future grant teams don't rediscover them.

---

## 4. Multi-operator sequencer election (live verified)

`research/election-split-brain-test-report.md` documents end-to-end
verification on the live 3-node Azure cluster, including:

| Test | Result |
|---|---|
| Stable cluster, 5 min | ✅ zero false failovers |
| Kill sequencer → failover | ✅ 16.5 s (timeout = 15 s) |
| Restart sequencer → reclaim | ✅ 8 s |
| Network partition via `iptables DROP` | ✅ minority keeps old leader, majority elects new |
| Reconverge after reconnect | ✅ 3 s to single-leader state |
| Persistent libp2p peer_id | ✅ stable across restarts |
| Heartbeat debug observability | ✅ `RUST_LOG=...election=debug` |

Code: `orchestrator/src/election.rs` (509 lines, 10 unit tests),
`orchestrator/src/p2p.rs` (395 lines, 2 unit tests), wired in
`orchestrator/src/main.rs`.

---

## 4b. Passive cross-operator PostgreSQL replication (verified live)

Historical data (trades, liquidations, deposits) is stored in PostgreSQL
on each operator. To make sure all three operators have consistent
history regardless of which one served the original request, the
validator batch replay loop writes the same rows that the sequencer
wrote — relying on `ON CONFLICT (trade_id, market) DO NOTHING` for
idempotency.

Verified end-to-end on the live 3-node Azure cluster on 2026-04-09:

```
alice: limit SELL 10 @ 1.00 submitted at sgx-node-1 (sequencer)
bob:   market BUY  10 @ market submitted at sgx-node-1
       → matched, trade_id = 1 produced by the sequencer

5 seconds later, querying the local PostgreSQL on each node:

  sgx-node-1 | 1 | alice | bob | 1.0 | 10.0   ← wrote via submit_order
  sgx-node-2 | 1 | alice | bob | 1.0 | 10.0   ← wrote via validator replay
  sgx-node-3 | 1 | alice | bob | 1.0 | 10.0   ← wrote via validator replay
```

Propagation path: REST → CLOB match → `api.rs::submit_order` writes to
local PG → sequencer publishes OrderBatch via libp2p gossipsub →
validators deserialize and hit `validator_perp.open_position()` on
their local enclaves AND `db.insert_trade()` on their local PG. Total
latency from order submission to row present on all 3 PGs: <5 seconds.

Reproducer: `tests/test_b31_replication.py` in the repo. Schema
migration for existing databases: `orchestrator/migrations/001_passive_replication_idempotency.sql`.

---

## 5. Code quality and test coverage

### Repositories

| Repo | Language | Purpose |
|---|---|---|
| [`LemonTreeTechnologies/xrpl-perp-dex`](https://github.com/LemonTreeTechnologies/xrpl-perp-dex) | Rust (orchestrator), Python (tests), Markdown (research) | Off-enclave business logic, CLOB, P2P, REST, WebSocket, failure-mode tests |
| [`77ph/xrpl-perp-dex-enclave`](https://github.com/77ph/xrpl-perp-dex-enclave) | C/C++ (SGX enclave), C++ (server), CMake | Margin engine, position tracking, ECDSA signing, DCAP attestation |

### Test counts

| Layer | Count | Directory |
|---|---|---|
| Rust unit tests | **86** | `orchestrator/src/**/tests/` |
| Python integration / e2e | **22** | `tests/` and `orchestrator/tests/` |
| Enclave invariant (FP8 arithmetic) | **19** | `EthSignerEnclave/perpdextest/` |
| Failure mode scenarios | **9/9 pass** | `tests/scenarios_runner.py` |

```bash
# Reproduce Rust tests
cd orchestrator && cargo test
# Reproduce failure mode scenarios
cd tests && python3 scenarios_runner.py all --out report.json
```

### Security audit history

| Audit | Findings | Status |
|---|---|---|
| Initial audit | 23 | 19 fixed, 2 partial, 7 new minor issues (Apr 3) |
| Re-audit #1 | +2 NEW | both fixed |
| Re-audit #2 | +2 NEW | both fixed |
| Re-audit #3 | +2 NEW | both fixed |
| **Total** | **52 findings** | **50 fixed, 2 by-design** |

All audit reports are in the repository: `SECURITY-REAUDIT*.md`,
`docs/post-audit-status.md`. The 2 "by-design" findings are documented
with their rationale (single-operator MVP constraints).

---

## 6. Research and documentation

### Bilingual research series (all in `research/`)

| # | Topic | RU | EN |
|---|---|---|---|
| 01 | Feasibility analysis | `01-feasibility-analysis.md` | `01-feasibility-analysis.en.md` |
| 02 | TEE perp mechanics design | `02-tee-perp-mechanics-design.md` | `02-tee-perp-mechanics-design.en.md` |
| 03 | Production architecture | `03-production-architecture.md` | `03-production-architecture.en.md` |
| 04 | Multi-operator architecture | `04-multi-operator-architecture.md` | `04-multi-operator-architecture.en.md` |
| 05 | TEE rationale and API design | `05-tee-rationale-and-api-design.md` | `05-tee-rationale-and-api-design.en.md` |
| 06 | Latency analysis | `06-latency-analysis.md` | `06-latency-analysis.en.md` |
| 07 | Failure modes and recovery | `07-failure-modes-and-recovery.md` | `07-failure-modes-and-recovery.en.md` |
| 08 | TEE vs smart contract security | `08-tee-vs-smart-contract-security.md` | `08-tee-vs-smart-contract-security.en.md` |
| 09 | Grant narrative | `09-grant-narrative.md` | `09-grant-narrative.en.md` |
| 10 | Comparison with Hyperliquid | `10-comparison-with-hyperliquid.md` | `10-comparison-with-hyperliquid.en.md` |

### Test reports

| Report | Purpose |
|---|---|
| `research/failure-modes-test-report.md` | 9/9 scenario pass/fail matrix + 10 tx hashes |
| `research/election-split-brain-test-report.md` | Live election + split-brain verification on Azure |

### API and integration guides

| Doc | Purpose |
|---|---|
| `docs/frontend-api-guide.md` | Full REST + WebSocket guide including new Fill/OrderUpdate/subscription protocol |
| `docs/perp-dex-faq-ru.md` | Russian FAQ for non-trader developers (for XRPL Community pitch) |
| `DEPLOYMENT.md` | Production deployment guide |
| `EthSignerEnclave/docs/azure_dcap_deployment.md` | Azure DCsv3 DCAP deployment cookbook with known gotchas |

### Presentation materials

| Asset | Purpose |
|---|---|
| `presentation/perp-dex.md` + `.html` | Marp deck, full architecture talk |
| `presentation/perp-dex-pitch.md` + `.html` | 5-minute pitch for Hack the Block Paris |
| `presentation/demo-script.md` | Demo flow with timing |
| `presentation/demo.cast` | Asciinema recording of live demo including DCAP step |
| `presentation/demo-flow.sh` | Self-running demo script |

---

## 7. Git history as proof of work

Recent (last 14 days) commit milestones:

```bash
git log --oneline --since=2026-03-26 | wc -l
# → ~80 commits
```

Selected highlights (all on `master`, all signed):

| Commit | Description |
|---|---|
| `93f3bba` | Election + split-brain live verification report |
| `d2ee9ed` | Persistent libp2p identity + heartbeat debug logs |
| `7dce374` | 9/9 failure-mode scenarios pass with on-chain proofs |
| `0af93c8` | Scenarios runner for 3.1-3.4, 3.9 |
| `aae7bd0` | Multisig coordinator + setup for failure mode tests |
| `1e086ec` | WebSocket Fill/OrderUpdate/PositionChanged + subscriptions |
| `0f0642f` | WebSocket docs update |
| `e8d50fa` | Withdrawal endpoint — enclave margin check + XRPL submission |
| `03b314d` | Deterministic state replay on validators + rate limiting |
| `ec45f20` | Enclave invariant tests — 19/19 |

On the enclave repo:

| Commit | Description |
|---|---|
| `0d3d5b4` | Two new DCAP failure modes documented after Azure deployment |
| `9f68a15` | Azure DCAP deployment guide |
| `2a05c7f` | DCAP single-subprocess invocation with stdin/file IPC |
| `9c6810f` | DCAP direct dlsym calls |
| `53e9b02` | `Dockerfile.azure` for reproducible DCsv3 builds |

---

## 8. Deployment footprint

### Production

| Host | Role | Notes |
|---|---|---|
| Hetzner `94.130.18.162` | Dev + staging + current live sequencer | Ubuntu 22.04, PostgreSQL, nginx, orchestrator |
| Azure DCsv3 `20.71.184.176` (sgx-node-1) | Operator A (priority 0) | Intel SGX, DCAP verified |
| Azure DCsv3 `20.224.243.60` (sgx-node-2) | Operator B (priority 1) | Intel SGX, DCAP verified |
| Azure DCsv3 `52.236.130.102` (sgx-node-3) | Operator C (priority 2) | Intel SGX, DCAP verified |

### External infra

| Service | Purpose |
|---|---|
| `api-perp.ph18.io` | Public API domain (DNS → Hetzner) |
| Ethereum Sepolia CommitmentRegistryV4 | State commitment root for trustless verification |
| XRPL testnet faucet | Escrow funding for test cycles |
| Binance XRP/USDT | Mark price feed |
| GitHub | Source of truth, public releases |

---

## 9. What a reviewer can verify in under 30 minutes

1. Open `https://api-perp.ph18.io/v1/openapi.json` — API is live.
2. Click any of the 10 XRPL testnet tx hashes in §2 — multisig tx on
   chain, signed by enclave-derived addresses.
3. Open `research/failure-modes-test-report.md` — 9/9 scenarios
   detailed with pass criteria.
4. Open `research/election-split-brain-test-report.md` — end-to-end
   election verification including partition.
5. Open `orchestrator/src/election.rs` (tests at bottom) —
   10 unit tests of the election state machine.
6. Open `orchestrator/src/ws.rs` (tests at bottom) — 11 unit tests of
   the WebSocket gateway including channel-filter semantics.
7. `git log --oneline` — commit cadence consistent with claimed
   timeline.
8. Connect `wscat -c wss://api-perp.ph18.io/ws` — see live ticker
   events within 5 seconds.

None of the above requires authentication or contacting the team.

---

*This document is the single-file entry point for grant reviewers who
want to verify project claims independently. If any URL, hash, or file
reference in this document fails, please contact the team via the GitHub
profile.*
