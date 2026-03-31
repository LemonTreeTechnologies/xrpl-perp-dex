# Latency Analysis: Perp DEX on XRPL with SGX

**Configuration**: 3 SGX servers (1 Hetzner + 2 Azure DCsv3), FROST 2-of-3
**Transport**: HTTPS between operators, SGX enclave on each node

---

## Cryptographic Operations (inside SGX)

| Operation | Time | Note |
|---|---|---|
| secp256k1 ECDSA sign | ~1-5 ms | XRPL transaction signing |
| Margin check (FP8 arithmetic) | <1 ms | Position collateral verification |
| Position state update | <1 ms | Balance/position update |
| sgx_seal_data (~25 KB) | ~5-10 ms | State persistence |
| sgx_unseal_data (~25 KB) | ~5-10 ms | State loading |
| SHA-512Half | <1 ms | XRPL transaction hashing |

**Cryptography is not the bottleneck.**

## Network Latencies

| Route | Latency |
|---|---|
| Localhost (orchestrator → enclave) | <1 ms |
| Hetzner → Azure (West Europe) | ~50-100 ms |
| Azure → Azure (same region) | ~1-2 ms |
| Orchestrator → Binance API | ~50-200 ms |
| Orchestrator → XRPL testnet | ~200-500 ms |
| XRPL ledger finality | 3-5 sec |

---

## Latencies by Operation

### Trading Cycle (per order)

```
User → Orchestrator (HTTP)                ~1-50 ms (depends on distance)
Orchestrator: order book matching          <1 ms
Orchestrator → Enclave: open_position      ~5-10 ms (margin check + state update)
                                           ─────────
Total:                                     ~10-60 ms
```

**For the user:** order executes in ~10-60 ms.
Comparison: CEX ~1-10 ms, on-chain DEX ~3-12 sec (block time).

### Deposit (per new deposit)

```
User → XRPL Payment                       3-5 sec (XRPL finality)
XRPL → Orchestrator: deposit detected      ~1-5 sec (polling interval)
Orchestrator → Enclave: deposit_credit     ~5 ms
                                           ─────────
Total:                                     ~5-10 sec
```

Deposit becomes available for trading ~5-10 seconds after sending the XRPL Payment.

### Withdrawal (per withdrawal, single operator)

```
User → Orchestrator: withdraw request
Orchestrator → Enclave: margin check       ~1 ms
Enclave: ECDSA sign XRPL tx               ~5 ms
Orchestrator → XRPL: submit tx            ~200-500 ms
XRPL finality                              3-5 sec
                                           ─────────
Total:                                     ~4-6 sec
```

### Withdrawal (FROST 2-of-3, multi-operator)

```
Orchestrator → Enclave A: nonce_gen        ~100 ms  ┐
Orchestrator → Enclave B: nonce_gen        ~100 ms  ┤ PARALLEL
                                                    ┘
Orchestrator → Enclave A: partial_sign     ~100 ms  ┐
Orchestrator → Enclave B: partial_sign     ~100 ms  ┤ PARALLEL
                                                    ┘
Coordinator: sig_agg                       ~100 ms
Orchestrator → XRPL: submit tx            ~200-500 ms
XRPL finality                              3-5 sec
                                           ─────────
Total:                                     ~4-6 sec
```

FROST adds ~300 ms to withdrawal — negligible compared to XRPL finality.

### Liquidation (event-driven)

```
Orchestrator: price update                 every 5 sec
Orchestrator → Enclave: check_liquidations ~5 ms
Enclave: scan all positions                <1 ms (up to 200 positions)
Orchestrator → Enclave: liquidate          ~5 ms
If FROST withdrawal needed:                +300 ms
                                           ─────────
Total:                                     ~5-10 sec (from price change)
```

**Risk:** within 5-10 seconds a position can go deeper into loss. Acceptable for PoC. For production: reduce price_interval to 1 sec.

### Funding Rate (every 8 hours)

```
Orchestrator: compute funding rate         <1 ms
Orchestrator → Enclave: apply_funding      ~10-50 ms (iterating all positions)
                                           ─────────
Total:                                     ~50 ms
```

Negligible — runs 3 times per day.

### DKG — Distributed Key Generation (one-time during setup)

```
Round 1: 3 polynomial generations          PARALLEL  ~100 ms
Round 1: 6 share exports (sealed)          SEQUENTIAL ~600 ms
Round 2: 6 share imports + VSS verify      SEQUENTIAL ~600 ms
Finalize: 3 aggregations                   PARALLEL  ~100 ms
                                                     ─────────
Total:                                               ~1.4 sec
```

Runs once when creating the escrow account.

### State Save (every 5 minutes)

```
Enclave: sgx_seal_data (~25 KB)            ~10 ms
Enclave → disk: ocall_save_to_file         ~5 ms
                                           ─────────
Total:                                     ~15 ms
```

---

## Comparison with Alternatives

| Operation | Our TEE DEX | CEX (Binance) | On-chain DEX (EVM) | XRPL native DEX |
|---|---|---|---|---|
| Order execution | ~10-60 ms | ~1-10 ms | ~3-12 sec | ~3-5 sec |
| Deposit availability | ~5-10 sec | ~1-30 min (confirmations) | ~3-12 sec | ~3-5 sec |
| Withdrawal | ~4-6 sec | ~10-60 min | ~3-12 sec | ~3-5 sec |
| Liquidation latency | ~5-10 sec | ~100 ms | ~3-12 sec | N/A |
| Funding rate | ~50 ms | ~100 ms | ~3-12 sec | N/A |

**Conclusion:** TEE DEX is closer to CEX in speed than to on-chain DEX. Most time is spent on XRPL settlement (3-5 sec), not on computation.

---

## Bottlenecks and Optimizations

| Bottleneck | Current Value | Optimization | Gain |
|---|---|---|---|
| Price feed polling | 5 sec interval | WebSocket stream from Binance | Real-time (~100 ms) |
| Deposit polling | 1-5 sec (AccountTx) | XRPL WebSocket subscribe | Real-time (~1 sec) |
| XRPL settlement | 3-5 sec | Not optimizable (L1 finality) | — |
| Enclave TCSNum=1 | One request at a time | HAProxy maxconn 1 + 3 instances | 3x throughput |
| State save | 15 ms every 5 min | Partitioned sealing | Support for >1000 users |
| Network (multi-operator) | ~100 ms per hop | Persistent connections, same region | ~50 ms per hop |

---

## When Latencies Are Critical

| Scenario | Frequency | Latency | Impact |
|---|---|---|---|
| Trading (order fill) | High | ~10-60 ms | Acceptable for perp DEX |
| Deposit | Medium | ~5-10 sec | User waits, acceptable |
| Withdrawal | Medium | ~4-6 sec | Faster than CEX |
| Liquidation | Rare | ~5-10 sec | Risk: deep loss. Mitigation: insurance fund |
| Funding | 3 times/day | ~50 ms | Zero impact |
| DKG setup | One-time | ~1.4 sec | Zero impact |
| FROST signing | On withdrawal | ~300 ms | Negligible compared to XRPL finality |

**Conclusion: multi-machine FROST signing latency (~300 ms) and enclave computation (~5-10 ms) are negligible compared to XRPL settlement (3-5 sec). The system is production-ready in terms of latency.**
