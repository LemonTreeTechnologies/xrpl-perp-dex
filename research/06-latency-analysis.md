# Анализ задержек: Perp DEX на XRPL с SGX

**Конфигурация**: 3 SGX сервера (1 Hetzner + 2 Azure DCsv3), XRPL native multisig 2-of-3 (SignerListSet)
**Транспорт**: HTTPS между операторами, SGX enclave на каждом узле

---

## Криптографические операции (внутри SGX)

| Операция | Время | Примечание |
|---|---|---|
| secp256k1 ECDSA sign | ~1-5 ms | Подпись XRPL транзакции |
| Margin check (FP8 arithmetic) | <1 ms | Проверка обеспечения позиции |
| Position state update | <1 ms | Обновление баланса/позиции |
| sgx_seal_data (~25 KB) | ~5-10 ms | Сохранение состояния |
| sgx_unseal_data (~25 KB) | ~5-10 ms | Загрузка состояния |
| SHA-512Half | <1 ms | XRPL transaction hashing |

**Криптография — не узкое место.**

## Сетевые задержки

| Маршрут | Задержка |
|---|---|
| Localhost (orchestrator → enclave) | <1 ms |
| Hetzner → Azure (West Europe) | ~50-100 ms |
| Azure → Azure (один регион) | ~1-2 ms |
| Orchestrator → Binance API | ~50-200 ms |
| Orchestrator → XRPL testnet | ~200-500 ms |
| XRPL ledger finality | 3-5 sec |

---

## Задержки по операциям

### Торговый цикл (каждый ордер)

```
Пользователь → Orchestrator (HTTP)        ~1-50 ms (зависит от расстояния)
Orchestrator: order book matching          <1 ms
Orchestrator → Enclave: open_position      ~5-10 ms (margin check + state update)
                                           ─────────
Итого:                                     ~10-60 ms
```

**Для пользователя:** ордер исполняется за ~10-60 ms.
Сравнение: CEX ~1-10 ms, on-chain DEX ~3-12 sec (block time).

### Deposit (каждый новый депозит)

```
Пользователь → XRPL Payment               3-5 sec (XRPL finality)
XRPL → Orchestrator: deposit detected      ~1-5 sec (polling interval)
Orchestrator → Enclave: deposit_credit     ~5 ms
                                           ─────────
Итого:                                     ~5-10 sec
```

Депозит доступен для торговли через ~5-10 секунд после отправки XRPL Payment.

### Withdrawal (каждый вывод, single operator)

```
Пользователь → Orchestrator: withdraw request
Orchestrator → Enclave: margin check       ~1 ms
Enclave: ECDSA sign XRPL tx               ~5 ms
Orchestrator → XRPL: submit tx            ~200-500 ms
XRPL finality                              3-5 sec
                                           ─────────
Итого:                                     ~4-6 sec
```

### Withdrawal (XRPL multisig 2-of-3, multi-operator)

```
Orchestrator → Enclave A: ECDSA sign       ~100 ms  ┐
Orchestrator → Enclave B: ECDSA sign       ~100 ms  ┤ PARALLEL
                                                    ┘
Orchestrator: assemble Signers array       <1 ms
Orchestrator → XRPL: submit multisig tx   ~200-500 ms
XRPL finality                              3-5 sec
                                           ─────────
Итого:                                     ~4-6 sec
```

Multisig signing добавляет ~100 ms к withdrawal — незаметно на фоне XRPL finality.

### Liquidation (event-driven)

```
Orchestrator: price update                 каждые 5 sec
Orchestrator → Enclave: check_liquidations ~5 ms
Enclave: scan all positions                <1 ms (до 200 позиций)
Orchestrator → Enclave: liquidate          ~5 ms
Если multisig withdrawal нужен:            +100 ms
                                           ─────────
Итого:                                     ~5-10 sec (от изменения цены)
```

**Риск:** в течение 5-10 секунд позиция может уйти глубже в убыток. Для PoC приемлемо. Для production: уменьшить price_interval до 1 sec.

### Funding Rate (каждые 8 часов)

```
Orchestrator: compute funding rate         <1 ms
Orchestrator → Enclave: apply_funding      ~10-50 ms (обход всех позиций)
                                           ─────────
Итого:                                     ~50 ms
```

Незначительно — выполняется 3 раза в сутки.

### ECDSA Key Generation + SignerListSet (однократно при setup)

```
3 инстанса генерируют ECDSA keypair       PARALLEL  ~10 ms
Orchestrator: SignerListSet tx на escrow   ~200-500 ms
Orchestrator: DisableMasterKey tx          ~200-500 ms
XRPL finality (2 tx)                       ~6-10 sec
                                                     ─────────
Итого:                                               ~7-11 sec
```

Каждый инстанс генерирует независимый ECDSA ключ (secp256k1). Orchestrator настраивает SignerListSet с quorum=2 на escrow account.

### State Save (каждые 5 минут)

```
Enclave: sgx_seal_data (~25 KB)            ~10 ms
Enclave → disk: ocall_save_to_file         ~5 ms
                                           ─────────
Итого:                                     ~15 ms
```

---

## Сравнение с альтернативами

| Операция | Наш TEE DEX | CEX (Binance) | On-chain DEX (EVM) | XRPL native DEX |
|---|---|---|---|---|
| Order execution | ~10-60 ms | ~1-10 ms | ~3-12 sec | ~3-5 sec |
| Deposit availability | ~5-10 sec | ~1-30 min (confirmations) | ~3-12 sec | ~3-5 sec |
| Withdrawal | ~4-6 sec | ~10-60 min | ~3-12 sec | ~3-5 sec |
| Liquidation latency | ~5-10 sec | ~100 ms | ~3-12 sec | N/A |
| Funding rate | ~50 ms | ~100 ms | ~3-12 sec | N/A |

**Вывод:** TEE DEX по скорости ближе к CEX чем к on-chain DEX. Основное время тратится на XRPL settlement (3-5 sec), не на computation.

---

## Узкие места и оптимизации

| Узкое место | Текущее значение | Оптимизация | Выигрыш |
|---|---|---|---|
| Price feed polling | 5 sec интервал | WebSocket stream от Binance | Реалтайм (~100 ms) |
| Deposit polling | 1-5 sec (AccountTx) | XRPL WebSocket subscribe | Реалтайм (~1 sec) |
| XRPL settlement | 3-5 sec | Не оптимизируемо (L1 finality) | — |
| Enclave TCSNum=1 | Один запрос за раз | HAProxy maxconn 1 + 3 instances | 3× throughput |
| State save | 15 ms каждые 5 мин | Partitioned sealing | Поддержка >1000 пользователей |
| Network (multi-operator) | ~100 ms per hop | Persistent connections, same region | ~50 ms per hop |

---

## Когда задержки критичны

| Сценарий | Частота | Задержка | Влияние |
|---|---|---|---|
| Торговля (order fill) | Высокая | ~10-60 ms | Приемлемо для perp DEX |
| Deposit | Средняя | ~5-10 sec | Пользователь ждёт, приемлемо |
| Withdrawal | Средняя | ~4-6 sec | Быстрее чем CEX |
| Liquidation | Редко | ~5-10 sec | Риск: глубокий убыток. Митигация: insurance fund |
| Funding | 3 раза/сутки | ~50 ms | Нулевое влияние |
| Key gen + SignerListSet | Однократно | ~7-11 sec | Нулевое влияние |
| Multisig signing (2 ECDSA) | При withdrawal | ~100 ms | Незаметно на фоне XRPL finality |

**Заключение: задержки multi-machine multisig signing (~100 ms) и enclave computation (~5-10 ms) пренебрежимо малы по сравнению с XRPL settlement (3-5 sec). Система production-ready по latency.**
