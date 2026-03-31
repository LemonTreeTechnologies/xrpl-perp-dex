# Мультиоператорная архитектура

**Дата:** 2026-03-30
**Статус:** Проектирование
**Зависимость:** XRPL native multisig (SignerListSet) + ECDSA ключи в enclave

> **Note on signing:** XRPL uses ECDSA (secp256k1), not Schnorr. Threshold signing on XRPL is achieved via native SignerListSet (multi-signature), not FROST. Each SGX instance holds an independent ECDSA key. The enclave also supports FROST for Bitcoin Taproot operations.

---

## Проблема

Единый оператор = единая точка отказа:
- Оператор офлайн → торговля остановлена, ликвидации не работают
- Оператор злонамеренный → может задерживать withdrawals (хотя не может украсть средства — ключи в SGX)
- Один сервер → hardware failure = downtime

---

## Решение: 2-of-3 оператора

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│   Operator A     │     │   Operator B     │     │   Operator C     │
│   (Sequencer)    │     │   (Validator)    │     │   (Validator)    │
│                  │     │                  │     │                  │
│ ┌──────────────┐ │     │ ┌──────────────┐ │     │ ┌──────────────┐ │
│ │SGX Enclave   │ │     │ │SGX Enclave   │ │     │ │SGX Enclave   │ │
│ │ECDSA Key A   │ │     │ │ECDSA Key B   │ │     │ │ECDSA Key C   │ │
│ └──────────────┘ │     │ └──────────────┘ │     │ └──────────────┘ │
│ ┌──────────────┐ │     │ ┌──────────────┐ │     │ ┌──────────────┐ │
│ │Orchestrator  │ │     │ │Orchestrator  │ │     │ │Orchestrator  │ │
│ │(Sequencer)   │ │     │ │(Replica)     │ │     │ │(Replica)     │ │
│ └──────────────┘ │     │ └──────────────┘ │     │ └──────────────┘ │
│ ┌──────────────┐ │     │ ┌──────────────┐ │     │ ┌──────────────┐ │
│ │HAProxy       │ │     │ │HAProxy       │ │     │ │HAProxy       │ │
│ └──────────────┘ │     │ └──────────────┘ │     │ └──────────────┘ │
└────────┬─────────┘     └────────┬─────────┘     └────────┬─────────┘
         │                        │                        │
         │    P2P gossip protocol (state + signing)        │
         └────────────────────────┼────────────────────────┘
                                  │
                                  ▼
                            XRPL Mainnet
                          (escrow account)
                 SignerListSet: [rA, rB, rC], quorum=2
```

---

## Роли

### Sequencer (1 оператор)

- Принимает все пользовательские ордера
- Строит authoritative state (позиции, балансы)
- Определяет порядок транзакций (ordering)
- Транслирует state updates валидаторам
- Инициирует multisig signing для withdrawals (собирает 2 ECDSA подписи)

### Validators (2 оператора)

- Получают state updates от sequencer
- Верифицируют корректность (margin checks, PnL расчёты)
- Участвуют в multisig signing (2-of-3 ECDSA подписи для withdrawals)
- Могут отказать в подписи если state некорректен
- При отказе sequencer → один из validators становится sequencer (failover)

---

## Протоколы

### 1. State Replication

```
Sequencer                    Validator B              Validator C
    │                            │                        │
    │ ── state_update(batch) ──► │                        │
    │ ── state_update(batch) ──────────────────────────► │
    │                            │                        │
    │                     verify(batch)            verify(batch)
    │                            │                        │
    │ ◄── ack/nack ──────────── │                        │
    │ ◄── ack/nack ──────────────────────────────────── │
```

**State batch** содержит:
- Список операций (deposits, trades, liquidations, funding)
- Resulting state hash
- Sequencer signature

Validators реплеят операции детерминистически и проверяют state hash.

### 2. Multisig Withdrawal Signing (XRPL SignerListSet)

```
User: "withdraw 50 RLUSD to rXXX"
    │
    ▼
Sequencer (orchestrator):
    1. Margin check → OK
    2. Build XRPL Payment tx
    3. Send tx to Enclave A → ECDSA sign (key A)
    4. Send tx to Enclave B → ECDSA sign (key B)
    │
    5. Assemble Signers array: [sig_A, sig_B]
    6. Submit multisig tx to XRPL
```

Минимум 2 из 3 операторов должны подписать (quorum=2 в SignerListSet). Если один офлайн — оставшиеся 2 всё равно могут подписать.

### 3. Price Consensus

```
Operator A: fetch_price() → $1.34
Operator B: fetch_price() → $1.34
Operator C: fetch_price() → $1.35
                    │
                    ▼
            median($1.34, $1.34, $1.35) = $1.34
```

Каждый оператор получает цену независимо. Sequencer использует медиану от всех 3. Если один оператор манипулирует ценой — медиана защищает.

### 4. Sequencer Failover

```
Normal:     A = Sequencer,  B,C = Validators
A offline:  B = Sequencer,  C = Validator    (A rejoins as Validator)
B offline:  A = Sequencer,  C = Validator
A+B offline: C = Sequencer (degraded mode, no multisig possible
                             until at least one more operator rejoins)
```

Failover через heartbeat timeout:
- Sequencer отправляет heartbeat каждые 5 секунд
- Если heartbeat пропущен 3 раза (15 сек) → validators выбирают нового sequencer
- Выбор: по заранее определённому priority (A > B > C)

---

## Что уже реализовано

| Компонент | Статус | Где |
|-----------|--------|-----|
| ECDSA keypair generation | ✅ Готов | Enclave: каждый инстанс генерирует независимый ECDSA ключ |
| XRPL SignerListSet setup | ✅ Готов | Orchestrator: настройка multisig на escrow account |
| ECDSA signing (secp256k1) | ✅ Готов | Enclave: ecall_sign (XRPL транзакции) |
| FROST (для Bitcoin Taproot) | ✅ Готов | Enclave: ecall_frost_* / ecall_dkg_* (не для XRPL) |
| Margin engine | ✅ Готов | Enclave: ecall_perp_* |
| Single-operator orchestrator | ✅ Готов | Rust binary |

## Что нужно добавить

| Компонент | Сложность | Описание |
|-----------|-----------|----------|
| P2P gossip | Средняя | libp2p или простой TCP mesh для state replication |
| State batch protocol | Средняя | Сериализация + подпись batch'ей |
| Sequencer election | Низкая | Priority-based с heartbeat |
| Multisig signing coordinator | Средняя | Orchestrator собирает ECDSA подписи от 2 инстансов, собирает Signers array |
| Price consensus | Низкая | Медиана от 3 операторов |
| Deterministic state replay | Средняя | Validators реплеят операции и сверяют hash |

---

## Модель доверия

| Сценарий | Результат |
|----------|-----------|
| 1 оператор злонамеренный | Не может украсть средства (нужно 2-of-3). Может задержать signing если он один из двух. |
| 1 оператор офлайн | Система работает (2-of-3 signing, failover). |
| 2 оператора офлайн | Торговля продолжается на оставшемся (он sequencer), но withdrawals заблокированы (нужно 2 ECDSA подписи для multisig). |
| 2 оператора сговорились | Могут подписать любой withdrawal. Риск: сговор. Митигация: операторы юридически/географически разделены. |
| Все 3 офлайн | Торговля остановлена. Средства безопасны на XRPL escrow. Recovery через Shamir backup keys. |

---

## Хостинг операторов

Для максимальной децентрализации — разные провайдеры с SGX:

| Оператор | Провайдер | SGX Hardware |
|----------|-----------|--------------|
| A | Hetzner (текущий dev сервер) | Intel Xeon E-2388G |
| B | Azure Confidential Computing | DCsv3 (SGX-enabled VM) |
| C | OVH / Equinix Metal | Bare metal с SGX |

Каждый оператор:
- Запускает свой enclave с идентичным MRENCLAVE
- Держит свой независимый ECDSA ключ (sealed, не покидает enclave)
- Верифицируется через remote attestation (пользователи проверяют MRENCLAVE)
