# Сценарии отказов и восстановление

**Дата:** 2026-03-29
**Статус:** Проектирование
**Контекст:** XRPL native multisig 2-of-3 (SignerListSet), 3 SGX оператора, полный инфраструктурный стек

---

## 1. Базовая модель: полный стек на оператора

Каждый оператор запускает полный стек из 4 компонентов + внешние зависимости:

```
                          Internet
                             │
                         DNS / LB
                      (CloudFlare, Route53)
                             │
                             ▼
┌─────────────────────────────────────────────────────────────┐
│                     Operator N                               │
│                                                              │
│   User ──► HAProxy :443 (public frontend)                   │
│                  │                                            │
│                  ▼                                            │
│            ┌───────────┐    ┌───────────┐    ┌───────────┐  │
│            │ Enclave 1 │    │ Enclave 2 │    │ Enclave 3 │  │
│            │ :9088     │    │ :9089     │    │ :9090     │  │
│            │ ECDSA Key │    │ ECDSA Key │    │ ECDSA Key │  │
│            │ TCSNum=1  │    │ TCSNum=1  │    │ TCSNum=1  │  │
│            └───────────┘    └───────────┘    └───────────┘  │
│                  ▲                                            │
│                  │                                            │
│            HAProxy :9443 (internal frontend, 127.0.0.1)     │
│                  ▲                                            │
│                  │                                            │
│            ┌─────────────────────────┐                       │
│            │    Orchestrator (Rust)  │                       │
│            │  ┌─ Order Book         │                       │
│            │  ├─ Price Feed         │──► Binance API        │
│            │  ├─ Deposit Monitor    │──► XRPL Mainnet       │
│            │  ├─ Liquidation Engine │                       │
│            │  ├─ Funding Rate       │                       │
│            │  └─ Sequencer/Validator│                       │
│            └────────────┬───────────┘                       │
│                         │                                    │
│                    P2P (libp2p gossipsub)                    │
│                         │                                    │
└─────────────────────────┼────────────────────────────────────┘
                          │
            ┌─────────────┼──────────────┐
            ▼             ▼              ▼
      Operator A    Operator B     Operator C
     (Sequencer)   (Validator)    (Validator)
            │             │              │
            └─────────────┼──────────────┘
                          ▼
                    XRPL Mainnet
                  (escrow account)
         SignerListSet: [rA, rB, rC], quorum=2
```

### Компоненты на каждом операторе

| # | Компонент | Порт | Описание |
|---|-----------|------|----------|
| 1 | **HAProxy** (public) | :443 | TLS termination, rate limiting, блокировка internal endpoints |
| 2 | **HAProxy** (internal) | :9443 (127.0.0.1) | Полный доступ для orchestrator, maxconn 1 per enclave |
| 3 | **SGX Enclave** x3 | :9088-9090 | perp-dex-server: margin engine, ECDSA ключи, sealed state |
| 4 | **Orchestrator** | нет (только исходящие) | Rust binary: order book, price feed, deposit monitor, sequencer/validator |

### Внешние зависимости

| Зависимость | Назначение | Критичность |
|-------------|-----------|-------------|
| **XRPL Mainnet** | Settlement, deposit monitor, escrow | Критичная для deposits/withdrawals |
| **Binance API** | Price feed (mark price) | Критичная для ликвидаций и funding |
| **DNS/LB** | Маршрутизация пользователей к операторам | Критичная для доступности |
| **P2P gossipsub** | State replication, price consensus, heartbeat | Критичная для мультиоператорной работы |

---

## 2. Матрица отказов компонентов

Что происходит при отказе каждого компонента. Предполагается: отказ на **одном** операторе, остальные два живы.

| Компонент | Торговля | Deposits | Withdrawals | Цены | Ликвидации | Recovery | Время |
|-----------|----------|----------|-------------|------|------------|----------|-------|
| **HAProxy down** (один оператор) | ✅ через других | ✅ | ✅ (2-of-3) | ✅ | ✅ | Рестарт HAProxy, DNS failover | ~30 сек |
| **Orchestrator crash** (на sequencer) | ⚠️ пауза до failover | ⚠️ пауза | ⚠️ пауза | ⚠️ пауза | ⚠️ пауза | Heartbeat timeout 15s, validator становится sequencer | ~15-30 сек |
| **Orchestrator crash** (на validator) | ✅ | ✅ | ✅ (2-of-3) | ✅ | ✅ | Рестарт orchestrator, resync state | ~10 сек |
| **Enclave crash** (orchestrator жив) | ⚠️ деградировано | ✅ | ✅ (2-of-3 на уровне операторов) | ✅ | ✅ | ecall_perp_load_state, orchestrator reconnect | ~5-15 сек |
| **Полный оператор down** | ✅ на остальных | ✅ | ✅ (2-of-3) | ✅ (медиана 2) | ✅ | Sequencer failover + DNS redirect | ~15-30 сек |
| **P2P disconnect** (сетевая партиция) | ⚠️ **см. Split-Brain** | ✅ каждый мониторит | ❌ **заблокированы** до reconnect | ⚠️ нет консенсуса | ⚠️ расхождение state | Reconnect + state reconciliation | зависит от партиции |
| **Binance API недоступен** | ✅ (old price) | ✅ | ✅ | ❌ **заморожена** | ⚠️ stale price risk | Переключение на backup CEX / ожидание | ~1-60 мин |
| **XRPL node недоступен** | ✅ | ❌ **не детектятся** | ❌ **не отправляются** | ✅ | ✅ (внутренние) | Переключение на backup XRPL node | ~10-30 сек |
| **DNS/LB failure** | ❌ для пользователей | ❌ для пользователей | ❌ для пользователей | ✅ (внутренне) | ✅ (внутренне) | DNS failover / direct IP | ~1-5 мин |

### Детальное описание каждого отказа

---

### 2.1. HAProxy down (на одном операторе)

**Причина:** Процесс HAProxy упал, OOM, неудачный reload конфигурации.

**Последствия:**
- Пользователи этого оператора теряют доступ к API
- Orchestrator этого оператора не может обращаться к enclave (internal frontend тоже упал)
- Enclave instances продолжают работать, но недоступны

**Каскад:**
- Если этот оператор = sequencer → orchestrator не может обновлять state → heartbeat timeout → failover
- Если validator → теряет способность подписывать, но 2 других оператора достаточно

**Recovery:**
```
1. systemctl restart haproxy
2. HAProxy поднимает health check к enclave instances (/v1/pool/status)
3. Enclave instances уже работают — мгновенное восстановление
4. DNS health check помечает оператора как alive
```

**Митигация:** systemd watchdog для HAProxy, DNS health checks с TTL 30s.

---

### 2.2. Orchestrator crash (на sequencer)

**Причина:** Panic в Rust, OOM, неперехваченная ошибка при обработке XRPL/Binance ответа.

**Последствия:**
- Order book в RAM потерян (stateless, восстанавливается из open orders)
- Price feed остановлен
- Deposit monitor остановлен
- Heartbeat в P2P пропадает

**Каскад:**
- Через 15 секунд (3 пропущенных heartbeat) validators детектят отказ
- Validator B (следующий по priority) становится sequencer
- Пользователи перенаправляются на B через DNS

**Recovery sequencer:**
```
1. Validators детектят отсутствие heartbeat (3 x 5s = 15s)
2. Validator B принимает роль sequencer (priority-based election)
3. B начинает принимать ордера, мониторить XRPL, публиковать цены
4. Бывший sequencer A после рестарта:
   a. Orchestrator стартует
   b. Подключается к P2P mesh
   c. Запрашивает текущий state у нового sequencer
   d. Входит как validator
```

**Критично:** Order book в RAM не персистится. Открытые ордера (limit orders) должны быть пересозданы пользователями или восстановлены из state replication.

---

### 2.3. Orchestrator crash (на validator)

**Причина:** Те же, что для sequencer.

**Последствия:**
- Один validator временно недоступен для signing
- State replication к нему прекращается
- Не влияет на торговлю (sequencer жив)

**Recovery:**
```
1. Рестарт orchestrator
2. Подключение к P2P, запрос пропущенных state batches
3. Синхронизация state → готов к signing
```

**Время простоя для пользователей: 0** (sequencer продолжает работу, 2-of-3 signing обеспечен оставшимися).

---

### 2.4. Enclave crash (orchestrator жив)

**Причина:** Segfault в enclave коде, SGX exception, нехватка EPC memory.

**Последствия:**
- HAProxy health check (`/v1/pool/status`) помечает instance как down
- HAProxy перестаёт отправлять запросы на упавший instance
- Оставшиеся 2 instance на этом операторе продолжают обрабатывать запросы
- Если все 3 локальных instance упали — оператор деградирован до состояния "HAProxy down"

**Recovery:**
```
1. HAProxy health check детектит отказ instance (~5s interval)
2. Перезапуск enclave process (systemd restart)
3. Enclave загружает sealed state: ecall_perp_load_state
4. HAProxy health check видит instance alive → возвращает в ротацию
```

**Нюанс:** Sealed state привязан к MRENCLAVE + CPU key. Если MRENCLAVE изменился (обновление кода) — sealed data не расшифруется. Нужен state export/import через orchestrator.

---

### 2.5. Полный оператор down

Подробно описан в разделе 3 (Operator-Level Scenarios).

---

### 2.6. P2P disconnect (сетевая партиция)

Подробно описан в разделе 4 (Split-Brain / Network Partition).

---

### 2.7. Binance API недоступен

**Причина:** Binance maintenance, rate limit, geo-block, DDoS на Binance.

**Последствия:**
- Price feed замирает на последней известной цене
- Ликвидации работают по stale price → **опасность**: цена могла сильно уйти
- Funding rate не обновляется
- Торговля формально продолжается, но с неактуальной ценой

**Защитные механизмы:**
- **Stale price timeout**: если цена не обновлялась > 60 секунд, orchestrator переводит систему в **price freeze mode**:
  - Новые позиции запрещены
  - Закрытие позиций разрешено
  - Ликвидации приостановлены (чтобы не ликвидировать по устаревшей цене)
  - Withdrawals разрешены
- **Backup price source**: переключение на другую CEX (Kraken, Bybit) или XRPL DEX oracle

**Recovery:**
```
1. Binance API восстанавливается
2. Orchestrator получает свежую цену
3. Stale price timeout сбрасывается
4. Система возвращается в нормальный режим
5. Ликвидации проверяются по новой цене (возможен всплеск ликвидаций)
```

**Риск:** Если Binance недоступен длительное время, а рынок резко двигается — позиции могут стать underwater. Митигация: insurance fund покрывает bad debt.

---

### 2.8. XRPL node недоступен

**Причина:** XRPL node maintenance, сетевая проблема, XRPL amendment freeze.

**Последствия:**
- **Deposits не детектятся**: orchestrator не видит новые Payment транзакции на escrow
- **Withdrawals не отправляются**: multisig транзакции не могут быть submitted
- Торговля продолжается (off-chain)
- Ликвидации работают (внутренний расчёт)

**Recovery:**
```
1. Переключение на backup XRPL node (список: s1.ripple.com, s2.ripple.com, собственная нода)
2. При восстановлении — сканирование пропущенных ledgers для deposits
3. Отложенные withdrawals отправляются
```

**Критично:** Deposit monitor должен запоминать последний обработанный ledger index и при reconnect сканировать с него, а не с текущего. Иначе пропущенные deposits будут потеряны.

---

### 2.9. DNS/LB failure

**Причина:** DNS registrar downtime, CloudFlare incident, неправильная DNS конфигурация.

**Последствия:**
- Пользователи не могут найти IP операторов
- Все пользовательские операции недоступны
- Внутренне система работает нормально (P2P, orchestrator, enclave)
- Ликвидации, funding, deposit monitor — всё продолжается

**Recovery:**
```
1. DNS провайдер восстанавливается
2. Альтернатива: пользователи обращаются по direct IP (публикуется в документации)
3. Резервный DNS (multi-provider: CloudFlare + Route53)
```

**Митигация:** Multi-provider DNS, низкий TTL (30-60 сек), публикация IP адресов операторов для экстренного доступа.

---

## 3. Сценарии на уровне операторов

### 3.1. Один оператор полностью офлайн

**Сценарий:** Operator C теряет связь (сервер упал, сеть пропала, обслуживание).

**Влияние:**
| Функция | Статус | Объяснение |
|---------|--------|------------|
| Торговля | ✅ Работает | Sequencer (A или B) жив, order book в его orchestrator |
| Deposits | ✅ Работает | Мониторинг XRPL любым живым оператором |
| Withdrawals | ✅ Работает | Multisig 2-of-3: A+B подписывают без C |
| Ликвидации | ✅ Работает | Любой живой оператор выполняет |
| Funding | ✅ Работает | Любой живой оператор применяет |
| Цены | ✅ Работает | Медиана от 2 операторов (менее устойчива к манипуляции) |
| State replication | ⚠️ Деградировано | Только между A и B, C отстаёт |

**Действия:**
- Система продолжает работу без вмешательства
- Алерт оператору C для восстановления

**Восстановление C:**
```
1. C перезапускает сервер
2. HAProxy стартует, health check к enclave instances
3. Enclave загружает sealed state: ecall_perp_load_state
4. Orchestrator подключается к P2P mesh
5. Запрашивает пропущенные state batches от A или B
6. После синхронизации — C возвращается в ротацию (validator)
```

**Время простоя для пользователей: 0**

---

### 3.2. Два оператора офлайн

**Сценарий:** Только Operator A жив. B и C недоступны.

**Влияние:**
| Функция | Статус | Объяснение |
|---------|--------|------------|
| Торговля | ✅ Работает | Order book в orchestrator A |
| Deposits | ✅ Работает | A мониторит XRPL |
| **Withdrawals** | ❌ **Заблокированы** | Multisig нужен 2-of-3, A один не может подписать |
| Ликвидации | ⚠️ Частично | Внутренние ликвидации работают, но вывод margin нет |
| Funding | ✅ Работает | |
| Цены | ⚠️ Одна точка | Только цена A, нет медианы — уязвимость к манипуляции |

**Действия:**
- Торговля продолжается, withdrawals приостановлены
- Withdrawal queue: запросы копятся, исполняются после recovery
- Средства в безопасности на XRPL escrow (A не может вывести в одиночку)

**Критичность:**
- **Средства не потеряны** — escrow на XRPL, ключ внутри SGX
- **Max downtime risk**: пользователи не могут вывести средства до восстановления хотя бы одного из B/C
- **Price risk**: единственный источник цены, при манипуляции — возможны некорректные ликвидации

**Время без withdrawals: до восстановления одного из B/C**

---

### 3.3. Все три оператора офлайн

**Сценарий:** Все серверы одновременно недоступны (катастрофа, координированная атака, ошибка).

**Влияние:**
| Функция | Статус |
|---------|--------|
| Всё | ❌ Остановлено |

**Безопасность средств:**
- **RLUSD на XRPL escrow** — средства on-chain, не на серверах
- **Никто не может вывести** — ни операторы, ни атакующий (нет 2-of-3 multisig подписи)
- **XRPL ledger** — immutable, средства видны публично

**Восстановление:**
1. **Серверы вернулись** — каждый enclave загружает sealed state, система рестартует
2. **Hardware уничтожен** — Shamir backup recovery (см. раздел 3.9)

---

### 3.4. Один оператор злонамеренный

**Сценарий:** Operator B пытается украсть средства или манипулировать торговлей.

| Действие | Возможно? | Почему |
|----------|-----------|--------|
| Украсть средства | ❌ Нет | Нужно 2-of-3 ECDSA подписи (multisig), B имеет только 1 ключ |
| Подписать фейковый withdrawal | ❌ Нет | A и C не подпишут невалидную транзакцию (enclave проверяет margin) |
| Остановить withdrawals | ⚠️ Частично | Если B = один из двух живых, может отказаться подписывать. Но A+C = 2-of-3 |
| Манипулировать ценой | ⚠️ Ограничено | Медиана от 3 операторов защищает. Если B = sequencer, может задержать ордера |
| Видеть ордера | ❌ Нет | Ордера зашифрованы для TEE (anti-MEV) |
| Извлечь ключ из SGX | ❌ Нет* | SGX hardware protection. *Теоретические side-channel атаки |
| Подменить enclave код | ❌ Нет | Remote attestation: пользователи и другие операторы проверяют MRENCLAVE |
| Отправить фейковый state batch | ❌ Нет | Validators детерминистически реплеят операции и сверяют state hash |

**Действия:**
- A и C обнаруживают аномалию (B отказывается подписывать, B отправляет невалидные state batches)
- A+C = 2-of-3 → продолжают работу без B
- B исключается из ротации
- При необходимости: key rotation, замена B на нового оператора D

---

### 3.5. SGX compromise (side-channel атака)

**Сценарий:** Атакующий извлекает ECDSA ключ из одного enclave через side-channel vulnerability (Spectre, Foreshadow, SGAxe и т.п.).

**Влияние:**
- Утечка 1 ключа из 3 — **недостаточно для подписи** (нужно 2-of-3 multisig)
- Атакующему нужны 2 ключа для multisig 2-of-3
- Компрометация одного SGX не даёт доступ к средствам

**Действия:**
1. Intel выпускает microcode update для уязвимости
2. Обновить SGX microcode на скомпрометированном сервере
3. Пересобрать enclave (новый MRENCLAVE)
4. **Key rotation**: каждый инстанс генерирует новый ECDSA keypair → обновить SignerListSet → перевести средства на новый escrow
5. Старые ключи бесполезны после key rotation

**Key Rotation Protocol:**
```
1. Все 3 инстанса генерируют новые ECDSA keypair → новые XRPL адреса (rA', rB', rC')
2. Создают новый escrow account с SignerListSet: [rA', rB', rC'], quorum=2
3. Multisig подпись (2-of-3 старыми ключами): перевести RLUSD со старого escrow на новый
4. Обновляют конфигурацию
5. Старые ключи можно безопасно удалить
```

---

### 3.6. Hardware failure (SGX CPU)

**Сценарий:** CPU с SGX на сервере B физически вышел из строя. Sealed data на диске не расшифровывается (привязана к MRENCLAVE + CPU key).

**Влияние:**
- ECDSA ключ B утерян
- A + C = 2-of-3 multisig → **система продолжает работу**
- Нет запаса: потеря ещё одного оператора = потеря подписи

**Действия:**
1. **Немедленно**: A+C продолжают работу (withdrawals, trading — всё ОК)
2. **Срочно**: развернуть оператора D, генерация нового ECDSA ключа → обновить SignerListSet на [rA, rD, rC]
3. Перевести средства на новый escrow (или обновить SignerListSet на старом)

**Время на recovery:**
- Standby оператор D подготовлен: ~5 минут (keygen + SignerListSet update)
- D нужно развернуть с нуля: ~1-2 часа (provision VM + install SGX + keygen + SignerListSet)

---

### 3.7. Миграция: смена облачного провайдера

**Процедура:**
```
Текущее: A (Hetzner), B (Azure), C (OVH)
Цель: A (Hetzner), B (AWS), C (OVH)   ← B мигрирует Azure → AWS

1. Развернуть новый SGX instance D на AWS
2. D генерирует ECDSA keypair внутри enclave → адрес rD
3. Обновить SignerListSet: [rA, rD, rC], quorum=2 (multisig подпись A+C)
4. D подключается к P2P mesh, синхронизирует state
5. Обновить DNS: B → D
6. Выключить B (Azure)

Время миграции: ~30 минут
Время без withdrawals: ~5 минут (момент обновления SignerListSet)
```

**Ключевое:**
- Не нужно выгружать ключи из SGX
- Не нужно доверять новому провайдеру — ключ генерируется ВНУТРИ нового enclave
- Remote attestation на D подтверждает идентичный MRENCLAVE

---

### 3.8. Масштабирование: добавление операторов

**Order book:** живёт в orchestrator (Rust), не в enclave. Нет ограничений SGX:
- Горизонтальное масштабирование orchestrator
- In-memory order book → можно переходить на более мощный сервер
- Stateless restart (order book восстанавливается из open orders)

**Enclave state:** только balances + positions + margin (~25 KB для PoC, ~5 MB для production)

**Увеличение числа операторов:**
```
Текущее: 2-of-3 [A, B, C]
Цель: 3-of-5 [A, B, C, D, E]

1. D и E генерируют ECDSA keypair в своих enclave
2. Обновить SignerListSet: [rA, rB, rC, rD, rE], quorum=3
3. D и E подключаются к P2P mesh
4. Синхронизация state
5. XRPL SignerListSet поддерживает до 32 signers — без ограничений
```

---

### 3.9. Catastrophic recovery: все 3 сервера уничтожены

**Сценарий:** Все три оператора одновременно потеряли доступ к sealed data.

**Backup: Shamir's Secret Sharing для master key**

При initial setup:
1. Каждый enclave генерирует encrypted state export, зашифрованный master key
2. Master key разделяется через Shamir 3-of-5 между доверенными custodians
3. Encrypted backups хранятся вне enclave (USB, сейф, банк)

**Восстановление:**
```
1. 3 из 5 custodians предоставляют Shamir shares
2. Реконструируют master key ВНУТРИ нового attested enclave
3. Расшифровывают backup → восстанавливают state + ECDSA ключи
4. Новые enclaves начинают работу
5. Key rotation рекомендуется после recovery
```

**Альтернатива: XRPL как source of truth**

Даже без Shamir backup:
- Все deposits видны на XRPL ledger
- Можно восстановить кто сколько депонировал
- Открытые позиции потеряны (off-chain state), но collateral безопасен
- **Worst case**: pro-rata распределение escrow balance на основе XRPL deposit history

---

## 4. Split-Brain / Сетевая партиция

### 4.1. Проблема

P2P gossipsub между операторами может быть разорван: firewall, провайдер, BGP incident. Результат — два (или более) изолированных кластера, каждый считает себя главным.

### 4.2. Сценарии партиции

```
Scenario 1: [A] | [B, C]     ← A изолирован
Scenario 2: [A, B] | [C]     ← C изолирован
Scenario 3: [A] | [B] | [C]  ← полная фрагментация
```

### 4.3. Два sequencer'а одновременно (split-brain)

**Как возникает:**
1. A = sequencer, B и C = validators
2. Сеть разделяется: [A] | [B, C]
3. B и C не получают heartbeat от A (15 секунд)
4. B становится sequencer по priority
5. Теперь: A считает себя sequencer, B тоже считает себя sequencer

**Проблема:** Два sequencer'а строят разный state (разный порядок ордеров, разные ликвидации).

### 4.4. Разрешение split-brain

**Принцип: Majority wins.**

| Партиция | Кто продолжает | Кто останавливается | Почему |
|----------|---------------|---------------------|--------|
| [A] vs [B,C] | B,C (2 оператора) | A (1 оператор) | Majority у [B,C] |
| [A,B] vs [C] | A,B (2 оператора) | C (1 оператор) | Majority у [A,B] |
| [A] vs [B] vs [C] | Никто | Все | Нет majority |

**Механизм:**

1. **Quorum check при sequencer election:** Validator становится sequencer только если видит majority операторов (>= 2 из 3). Если B и C видят друг друга, но не A → B становится sequencer (видит majority).
2. **Isolated operator self-demotion:** Если A перестаёт видеть хотя бы 1 другого оператора и не является частью majority → A переводит себя в **read-only mode**:
   - Принимает запросы на чтение (балансы, позиции)
   - Отклоняет запросы на запись (открытие/закрытие позиций, withdrawals)
   - Логирует: "isolated, waiting for reconnect"
3. **Reconnect reconciliation:** При восстановлении связи:
   ```
   1. Isolated operator (A) запрашивает текущий state hash у majority
   2. Если state расходится — A discards свой state, принимает majority state
   3. A возвращается как validator
   ```

### 4.5. Withdrawals при партиции

- **[A] vs [B,C]:** B+C = 2-of-3 → withdrawals работают. A не может подписать (нет quorum для signing).
- **[A,B] vs [C]:** A+B = 2-of-3 → withdrawals работают. C не может подписать.
- **[A] vs [B] vs [C]:** Никто не может подписать (нужно 2-of-3). Withdrawals заблокированы.

### 4.6. Защита от double-spending при партиции

**Риск:** Если split-brain не обнаружен мгновенно, оба sequencer'а могли одобрить conflicting withdrawals.

**Защита:** XRPL Sequence number на escrow account. Каждая транзакция увеличивает Sequence. Если оба кластера пытаются отправить транзакцию:
- Первая попадает в ledger
- Вторая отклоняется с `tefPAST_SEQ` или `tefMAX_LEDGER`

**Дополнительная защита:** Orchestrator проверяет текущий Sequence перед отправкой withdrawal. При конфликте — один из двух withdrawal'ов задерживается до reconciliation.

---

## 5. Каскадные отказы

### 5.1. Сценарий: Каскад через перегрузку

```
Timeline:
T+0:    Orchestrator A (sequencer) crash
T+5s:   HAProxy A health check fails на /v1/pool/status
        (orchestrator не перезапускает enclave, но enclave ещё жив)
T+15s:  Validators B,C не получают heartbeat → B = new sequencer
T+16s:  DNS health check видит A down → все пользователи перенаправлены на B
T+17s:  B получает 3x нормального трафика (свой + бывший A)
T+20s:  HAProxy B: queue overflow (maxconn 1 x 3 instances = 3 concurrent)
        Latency растёт: 5s → 15s → timeout
T+30s:  Пользователи видят таймауты
T+60s:  Часть пользователей идёт на C → C тоже нагружается
```

### 5.2. Митигации каскадных отказов

**Rate limiting на HAProxy:**
```haproxy
frontend perp-public
    # Не более 50 req/s per IP
    stick-table type ip size 100k expire 30s store http_req_rate(10s)
    http-request deny deny_status 429 if { sc_http_req_rate(0) gt 500 }
```

**Connection queue management:**
```haproxy
backend enclave_instances
    timeout queue 5s          # Не ждать дольше 5 секунд в очереди
    option redispatch         # Если instance down — перенаправить на другой
    retries 1                 # Максимум 1 retry
```

**Graceful degradation:**
- При перегрузке — HAProxy возвращает 503 с Retry-After header
- Frontend показывает "система перегружена, попробуйте через 30 секунд"
- Критичные операции (withdrawals, liquidations) имеют приоритет в queue

**Auto-scaling enclave instances:**
- При нагрузке — запуск дополнительных enclave instances (9091, 9092...)
- HAProxy динамически добавляет новые backends
- Ограничение: EPC memory на CPU (обычно 128-256 MB)

### 5.3. Сценарий: Каскад через Binance API

```
Timeline:
T+0:    Binance API rate limit (429) для оператора A
T+5s:   A переключается на backup source (Kraken)
T+10s:  Kraken тоже rate limited (все операторы переключились)
T+15s:  Все 3 оператора имеют stale price
T+60s:  Price freeze mode на всех операторах
T+??:   Рынок двигается, позиции становятся underwater
```

**Митигация:**
- Каждый оператор использует свой API key для Binance
- Staggered requests (A запрашивает в :00, B в :02, C в :04 секунды)
- Multiple backup sources: Kraken, Bybit, XRPL DEX, CoinGecko
- Circuit breaker: если > 2 sources недоступны, автоматический price freeze

### 5.4. Сценарий: Каскад через XRPL

```
Timeline:
T+0:    XRPL node оператора A потеряла связь
T+1s:   Deposits не детектятся на A
T+5s:   A переключается на backup XRPL node
T+6s:   Backup node тоже недоступен (XRPL amendment freeze, все ноды на update)
T+10s:  A не может отправить withdrawal tx
T+15s:  B,C тоже теряют XRPL connectivity
T+??:   Все withdrawals и deposits заблокированы глобально
```

**Митигация:**
- Несколько XRPL nodes (s1.ripple.com, s2.ripple.com, собственная нода, xrplcluster.com)
- Deposit monitor буферизирует: при reconnect сканирует пропущенные ledgers
- Withdrawal queue: запросы копятся, отправляются при восстановлении
- XRPL amendment freeze — редкость, обычно < 15 минут

---

## 6. Инфраструктурные гарантии

### Что защищено hardware (Intel SGX)
- Приватные ECDSA ключи — никогда не покидают enclave
- State в памяти — изолирован от ОС и оператора
- Sealed data — зашифрована CPU key + MRENCLAVE
- Remote attestation — пользователи верифицируют что код не изменён

### Что защищено HAProxy
- Пользователи не имеют доступа к internal endpoints (deposit, price, liquidate, state)
- Сериализация запросов к однопоточным enclave instances (maxconn 1)
- Health check — автоматический вывод/возврат enclave instances
- Rate limiting — защита от DDoS и перегрузки

### Что защищено Orchestrator
- Deposit monitor — детектирует все входящие платежи на escrow
- Price feed — обновление mark price каждые 5 секунд
- Liquidation engine — проверка margin каждые 10 секунд
- State save — периодическое сохранение (каждые 5 минут)
- Sequencer/validator logic — ordering, state replication, heartbeat

### Что защищено протоколом (XRPL SignerListSet 2-of-3)
- Ни один оператор не может подписать в одиночку (quorum=2)
- Для кражи средств нужно скомпрометировать 2 из 3 SGX
- Key rotation через обновление SignerListSet без прерывания сервиса

### Что защищено P2P (gossipsub)
- State replication — все операторы имеют согласованное состояние
- Price consensus — медиана от нескольких источников
- Sequencer election — автоматический failover
- Heartbeat — детектирование отказов за 15 секунд

### Что защищено XRPL
- Средства всегда on-chain (RLUSD на escrow)
- Deposit history — permanent, auditable
- Settlement — atomic, финальный через 3-5 секунд
- Sequence number — защита от double-spending при split-brain

### Что защищено DNS/LB
- Маршрутизация пользователей к ближайшему/живому оператору
- Health check — автоматическое переключение при отказе оператора
- DDoS protection (CloudFlare)

### Что НЕ защищено (требует внешних митигаций)
| Элемент | Риск | Митигация |
|---------|------|-----------|
| Off-chain state (позиции, PnL) | Потеря всех 3 серверов = потеря state | Periodic sealed backups + Shamir |
| Order book | Живёт в orchestrator RAM | Stateless restart, пересоздание из open orders |
| Funding rate history | Вычисляется на лету | Логирование, восстановление из логов |
| Price feed | Зависимость от Binance API | Multiple backup sources, price freeze mode |
| P2P connectivity | Партиция = split-brain | Quorum check, majority wins, self-demotion |

---

## 7. Сводная таблица рисков

| # | Сценарий | Торговля | Deposits | Withdrawals | Средства | Recovery | Время |
|---|----------|----------|----------|-------------|----------|----------|-------|
| 1 | HAProxy down (1 оператор) | ✅ | ✅ | ✅ | ✅ | Рестарт, DNS failover | ~30 сек |
| 2 | Orchestrator crash (sequencer) | ⚠️ пауза | ⚠️ пауза | ⚠️ пауза | ✅ | Heartbeat failover | ~15-30 сек |
| 3 | Orchestrator crash (validator) | ✅ | ✅ | ✅ | ✅ | Рестарт, resync | ~10 сек |
| 4 | Enclave crash (orch. жив) | ⚠️ деград. | ✅ | ✅ | ✅ | Load sealed state | ~5-15 сек |
| 5 | 1 оператор полностью down | ✅ | ✅ | ✅ (2-of-3) | ✅ | Автоматический failover | ~15-30 сек |
| 6 | 2 оператора down | ✅ | ✅ | ❌ ожидание | ✅ | Ждём recovery 1 | variable |
| 7 | Все 3 down | ❌ | ❌ | ❌ | ✅ (XRPL) | Shamir / restart | часы |
| 8 | P2P партиция [1] vs [2] | ✅ (majority) | ✅ | ✅ (majority) | ✅ | Reconnect + reconcile | ~15-60 сек |
| 9 | P2P полная фрагментация | ❌ read-only | ✅ (каждый) | ❌ | ✅ | Reconnect | variable |
| 10 | Binance API down | ⚠️ freeze | ✅ | ✅ | ✅ | Backup source / wait | ~1-60 мин |
| 11 | XRPL node down | ✅ | ❌ не детект. | ❌ не отправл. | ✅ | Backup XRPL node | ~10-30 сек |
| 12 | DNS/LB failure | ❌ для users | ❌ для users | ❌ для users | ✅ | DNS failover / direct IP | ~1-5 мин |
| 13 | 1 злонамеренный оператор | ✅ | ✅ | ✅ (2 honest) | ✅ | Исключить из ротации | минуты |
| 14 | SGX side-channel | ✅ | ✅ | ✅ | ✅ (1 ключ мало) | Key rotation | часы |
| 15 | Hardware failure | ✅ | ✅ | ✅ (2-of-3) | ✅ | Key rotation + SignerListSet | 5 мин - 2 часа |
| 16 | Миграция провайдера | ✅ | ✅ | ⚠️ 5 мин пауза | ✅ | Keygen + SignerListSet | ~30 мин |
| 17 | Масштабирование | ✅ | ✅ | ⚠️ 5 мин пауза | ✅ | Key rotation + SignerListSet | ~30 мин |
| 18 | Каскад: перегрузка | ⚠️ latency | ⚠️ latency | ⚠️ latency | ✅ | Rate limiting, queue mgmt | ~1-5 мин |
| 19 | Catastrophic (все 3 уничт.) | ❌ | ❌ | ❌ | ✅ (XRPL) | Shamir 3-of-5 | часы-дни |

---

## 8. Гибкость threshold: не только 2-of-3

XRPL SignerListSet поддерживает до 32 signers. Каждый signer имеет вес, quorum задаётся произвольно.

| Схема | Операторов | Для подписи | Допустимые отказы | Для сговора | Применение |
|-------|-----------|-------------|-------------------|-------------|------------|
| 2-of-3 | 3 | 2 | 1 | 2 (67%) | PoC, малая команда |
| 3-of-5 | 5 | 3 | 2 | 3 (60%) | Production |
| 5-of-9 | 9 | 5 | 4 | 5 (56%) | Высокая децентрализация |
| 7-of-11 | 11 | 7 | 4 | 7 (64%) | Максимальная децентрализация |
| 16-of-32 | 32 | 16 | 16 | 16 (50%) | Максимум XRPL SignerList |

**Рекомендация:** t = ceil(n/2) + 1 (простое большинство + 1).

> **Примечание:** FROST/DKG остаётся доступным в enclave для Bitcoin Taproot use cases, но не используется для XRPL операций.
