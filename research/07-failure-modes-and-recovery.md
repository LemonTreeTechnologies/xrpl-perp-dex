# Сценарии отказов и восстановление

**Дата:** 2026-03-31
**Статус:** Проектирование
**Контекст:** FROST 2-of-3 DKG, 3 SGX оператора, XRPL mainnet settlement

---

## Базовая модель

```
Operator A (Hetzner)     Operator B (Azure)      Operator C (OVH)
┌──────────────┐         ┌──────────────┐         ┌──────────────┐
│ SGX Enclave  │         │ SGX Enclave  │         │ SGX Enclave  │
│ FROST Share 1│         │ FROST Share 2│         │ FROST Share 3│
│ Sealed State │         │ Sealed State │         │ Sealed State │
└──────────────┘         └──────────────┘         └──────────────┘
```

- **Escrow account** на XRPL: group public key = FROST 2-of-3
- **Signing threshold**: 2 из 3 операторов достаточно для подписи
- **State**: sealed внутри каждого enclave (MRENCLAVE-bound)
- **Средства**: RLUSD на XRPL mainnet, контролируемые group key

---

## 1. Один оператор офлайн

### Сценарий
Operator C теряет связь (сервер упал, сеть пропала, обслуживание).

### Влияние
| Функция | Статус | Объяснение |
|---------|--------|------------|
| Торговля | ✅ Работает | Order book в orchestrator, не в enclave |
| Deposits | ✅ Работает | Мониторинг XRPL любым живым оператором |
| Withdrawals | ✅ Работает | FROST 2-of-3: A+B подписывают без C |
| Liquidations | ✅ Работает | Любой живой оператор выполняет |
| Funding | ✅ Работает | Любой живой оператор применяет |
| State persistence | ✅ Работает | Каждый instance сохраняет свой state |

### Действия
- Нет. Система продолжает работу.
- Алерт оператору C для восстановления.

### Восстановление C
1. C перезапускает сервер
2. Enclave загружает sealed state с диска (`ecall_perp_load_state`)
3. C реплицирует пропущенные state updates от A или B
4. C возвращается в ротацию

**Время простоя для пользователей: 0**

---

## 2. Два оператора офлайн

### Сценарий
Только Operator A жив. B и C недоступны.

### Влияние
| Функция | Статус | Объяснение |
|---------|--------|------------|
| Торговля | ✅ Работает | Order book в orchestrator A |
| Deposits | ✅ Работает | A мониторит XRPL |
| **Withdrawals** | ❌ **Заблокированы** | FROST нужен 2-of-3, A один не может подписать |
| Liquidations | ⚠️ Частично | Внутренние ликвидации работают, но вывод margin — нет |
| Funding | ✅ Работает | |
| State persistence | ✅ Работает | |

### Действия
- Торговля продолжается, но withdrawals приостановлены
- Средства в безопасности на XRPL escrow (A не может вывести в одиночку)
- Ожидание восстановления хотя бы одного из B/C

### Критичность
- **Средства не потеряны** — escrow на XRPL, ключ внутри SGX
- **Withdrawal queue** — запросы на вывод копятся, исполняются после recovery
- **Max downtime risk**: если ситуация затягивается, пользователи не могут вывести средства

**Время без withdrawals: до восстановления одного из B/C**

---

## 3. Все три оператора офлайн

### Сценарий
Все серверы одновременно недоступны (катастрофа, координированная атака, ошибка).

### Влияние
| Функция | Статус |
|---------|--------|
| Всё | ❌ Остановлено |

### Безопасность средств
- **RLUSD на XRPL escrow** — средства на chain, не на серверах
- **Никто не может вывести** — ни операторы, ни атакующий (нет 2-of-3 подписи)
- **XRPL ledger** — immutable, средства видны публично

### Восстановление
1. **Scenario A: серверы вернулись** — каждый enclave загружает sealed state, система рестартует
2. **Scenario B: hardware уничтожен** — Shamir backup recovery (см. раздел 9)

---

## 4. Один оператор злонамеренный

### Сценарий
Operator B пытается украсть средства или манипулировать торговлей.

### Что B может сделать
| Действие | Возможно? | Почему |
|----------|-----------|--------|
| Украсть средства | ❌ Нет | Нужно 2-of-3 FROST подписи, B имеет только 1 share |
| Подписать фейковый withdrawal | ❌ Нет | A и C не подпишут невалидную транзакцию |
| Остановить withdrawals | ⚠️ Частично | Если B = один из двух живых, может отказаться подписывать. Но A+C всё равно = 2-of-3 |
| Манипулировать ценой | ⚠️ Ограничено | Если B = sequencer, может задержать ордера. Митигация: sequencer rotation |
| Видеть ордера | ❌ Нет | Ордера зашифрованы для TEE (anti-MEV) |
| Извлечь ключ из SGX | ❌ Нет* | SGX hardware protection. *Теоретические side-channel атаки |

### Действия
- A и C обнаруживают аномалию (например, B отказывается подписывать валидные withdrawals)
- A+C вместе = 2-of-3 → продолжают работу без B
- B исключается из ротации

---

## 5. SGX compromise (side-channel атака)

### Сценарий
Атакующий извлекает FROST share из одного enclave через side-channel vulnerability (Spectre, Foreshadow и т.п.).

### Влияние
- Утечка 1 share из 3 — **недостаточно для подписи**
- Атакующему нужны 2 shares для FROST 2-of-3
- Компрометация одного SGX не даёт доступ к средствам

### Действия
1. Intel выпускает microcode update для уязвимости
2. Обновить SGX microcode на скомпрометированном сервере
3. Пересобрать enclave (новый MRENCLAVE)
4. **Key rotation**: запустить новый DKG → перевести средства на новый escrow
5. Старые shares бесполезны после key rotation

### Key Rotation Protocol
```
1. Все 3 оператора запускают новый DKG → новый group_pubkey → новый XRPL address
2. Подписывают XRPL Payment: старый escrow → новый escrow (всё RLUSD)
3. Обновляют конфигурацию
4. Старые shares можно безопасно удалить
```

---

## 6. Hardware failure (SGX CPU)

### Сценарий
CPU с SGX на сервере B физически вышел из строя. Sealed data на диске не расшифровывается (привязана к MRENCLAVE + CPU key).

### Влияние
- B share утерян
- A + C = 2-of-3 → **система продолжает работу**
- Но теперь нет запаса — потеря ещё одного оператора = потеря подписи

### Действия
1. **Немедленно**: A+C продолжают работу (withdrawals, trading — всё ОК)
2. **Срочно**: key rotation на новый 2-of-3 DKG (A+C+D, где D = новый сервер)
3. Перевести средства на новый escrow
4. Старый escrow пуст, можно забыть

### Время на recovery
- Если D уже подготовлен (standby): ~5 минут (DKG + transfer)
- Если D нужно развернуть: ~1-2 часа (provision Azure VM + install SGX + DKG)

---

## 7. Миграция: смена облачного провайдера

### Вопрос от 8Baller: "Can operators change cloud provider?"

### Ответ: Да, без потери средств.

### Процедура
```
Текущее: A (Hetzner), B (Azure), C (OVH)
Цель: A (Hetzner), B (AWS), C (OVH)   ← B мигрирует Azure → AWS

1. Развернуть новый SGX instance D на AWS
2. Запустить DKG 2-of-3 между A, D, C → новый group_pubkey
3. FROST signing (A+C): перевести RLUSD со старого escrow на новый
4. Обновить конфигурацию: D заменяет B
5. Выключить B (Azure)

Время миграции: ~30 минут
Время без withdrawals: ~5 минут (только момент перевода)
```

### Ключевое
- **Не нужно** выгружать ключи из SGX
- **Не нужно** доверять новому провайдеру — ключ генерируется ВНУТРИ нового enclave
- Средства всегда на XRPL — не на серверах
- Remote attestation на D подтверждает что код тот же (MRENCLAVE)

---

## 8. Масштабирование: "books get too big"

### Вопрос от 8Baller: "Can they move to a more performant box?"

### Ответ: Да, пошагово.

### Order book
Order book живёт в **orchestrator (Rust)**, не в enclave. Нет ограничений SGX:
- Горизонтальное масштабирование orchestrator
- В-memory order book → можно переходить на более мощный сервер в любой момент
- Нет sealed state для order book — stateless restart

### Enclave state
Enclave хранит только balances + positions + margin (~25 KB для PoC, ~5 MB для production):
- При росте: partitioned sealing (seal по частям)
- При ребалансировке: key rotation на более мощный сервер

### Процедура upgrade
```
1. Развернуть новый мощный SGX сервер D
2. Key rotation: DKG(A,B,C) → DKG(A,D,C)  (B заменяется на D)
3. Перевести средства
4. Orchestrator на D обрабатывает больший order book
```

---

## 9. Catastrophic recovery: все 3 сервера уничтожены

### Сценарий
Все три оператора одновременно потеряли доступ к sealed data (пожар дата-центра, координированная конфискация серверов).

### Backup: Shamir's Secret Sharing для master key

При initial setup (DKG):
1. Каждый enclave генерирует **encrypted state export** зашифрованный master key
2. Master key разделяется через Shamir 3-of-5 между доверенными custodians
3. Encrypted backups хранятся вне enclave (на USB, в сейфе, в банке)

### Восстановление
```
1. 3 из 5 custodians собираются, предоставляют Shamir shares
2. Реконструируют master key ВНУТРИ нового attested enclave
3. Расшифровывают backup → восстанавливают state + FROST shares
4. Новые enclaves начинают работу
5. Key rotation рекомендуется после recovery
```

### Альтернатива: XRPL как source of truth
Даже без Shamir backup:
- Все deposits видны на XRPL ledger
- Можно восстановить кто сколько депонировал
- Открытые позиции потеряны (off-chain state), но collateral безопасен
- **Worst case**: pro-rata распределение escrow balance на основе XRPL deposit history

---

## 10. Сводная таблица рисков

| # | Сценарий | Торговля | Withdrawals | Средства | Recovery |
|---|----------|----------|-------------|----------|----------|
| 1 | 1 оператор офлайн | ✅ | ✅ (2-of-3) | ✅ | Автоматический |
| 2 | 2 оператора офлайн | ✅ | ❌ Ожидание | ✅ | Ждём recovery 1 |
| 3 | Все 3 офлайн | ❌ | ❌ | ✅ (XRPL) | Shamir / restart |
| 4 | 1 злонамеренный | ✅ | ✅ (2 честных) | ✅ | Исключить из ротации |
| 5 | SGX side-channel | ✅ | ✅ | ✅ (1 share мало) | Key rotation |
| 6 | Hardware failure | ✅ | ✅ (2-of-3) | ✅ | Key rotation на новый DKG |
| 7 | Миграция провайдера | ✅ | ⚠️ 5 мин | ✅ | DKG + transfer |
| 8 | Масштабирование | ✅ | ⚠️ 5 мин | ✅ | Key rotation |
| 9 | Catastrophic (все 3) | ❌ | ❌ | ✅ (XRPL) | Shamir 3-of-5 |

---

## 11. Инфраструктурные гарантии

### Что защищено hardware (Intel SGX)
- Приватные ключи (FROST shares) — никогда не покидают enclave
- State в памяти — изолирован от ОС и оператора
- Sealed data — зашифрована CPU key + MRENCLAVE

### Что защищено протоколом (FROST 2-of-3)
- Ни один оператор не может подписать в одиночку
- Для кражи средств нужно скомпрометировать 2 из 3 SGX
- Key rotation без прерывания сервиса

### Что защищено XRPL
- Средства всегда on-chain (RLUSD на escrow)
- Deposit history — permanent, auditable
- Settlement — atomic, финальный через 3-5 секунд

### Что НЕ защищено
- Off-chain state (позиции, PnL) — потеря всех 3 серверов = потеря state
- Order book — живёт в orchestrator RAM, не персистентный
- Funding rate history — вычисляется на лету

### Митигации для незащищённого
- Periodic state sealed backups (каждые 5 минут)
- Encrypted state exports (Shamir backup)
- XRPL deposit history как last-resort source of truth
