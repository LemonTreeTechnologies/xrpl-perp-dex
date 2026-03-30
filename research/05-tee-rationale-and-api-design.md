# TEE Rationale & API Design

**Дата:** 2026-03-30
**Статус:** Проектирование

---

## 1. Зачем TEE? Проблема которую мы решаем

XRPL mainnet не имеет смарт-контрактов. Это осознанный дизайн-выбор — XRPL оптимизирован для быстрых платежей, не для программируемой логики. Но perpetual futures требуют:

- Margin engine (проверка обеспечения перед каждой сделкой)
- Liquidation engine (мониторинг и принудительное закрытие позиций)
- Funding rate (периодические перебалансировки между longs и shorts)
- Order matching (сопоставление ордеров)

**Проблема:** Как запустить всё это на XRPL, если нет смарт-контрактов?

**Стандартный подход:** Централизованный сервер. Пользователи отправляют средства оператору и доверяют ему полностью. Это CEX — то от чего DeFi пытается уйти.

**Наш подход:** TEE (Trusted Execution Environment) как verifiable computation layer.

---

## 2. Что конкретно даёт TEE

### 2.1 Custody без доверия оператору

```
Без TEE:                          С TEE:
User → отправляет средства        User → отправляет на escrow
       оператору                         контролируемый SGX
       (оператор может украсть)          (приватный ключ ВНУТРИ enclave,
                                          оператор не имеет доступа)
```

Приватный ключ escrow аккаунта на XRPL **генерируется внутри SGX enclave** и никогда не покидает его. Оператор запускает enclave, но физически не может извлечь ключ — это гарантия на уровне hardware (Intel SGX).

### 2.2 Verifiable execution

Пользователь может проверить что **именно тот код** который опубликован (open source) работает внутри enclave:

1. Enclave при запуске создаёт **MRENCLAVE** — криптографический хеш бинарного кода
2. Пользователь проверяет MRENCLAVE через **remote attestation** (подписано Intel)
3. Если MRENCLAVE совпадает с хешем published code → enclave запускает ровно тот код

**Что это значит для perp DEX:**
- Margin check — нельзя обойти (код в enclave, оператор не может изменить)
- Liquidation rules — прозрачны и неизменяемы (часть attested кода)
- Withdrawals — подписываются только после margin check (атомарно внутри enclave)

### 2.3 Anti-MEV

```
Traditional DEX:              TEE DEX:
Order → mempool (видим всем)  Order → encrypted → TEE decrypts inside
      → frontrun возможен            → matching inside enclave
      → sandwich возможен            → operator sees only ciphertext
```

Оператор видит только зашифрованный трафик. Ордера расшифровываются и сопоставляются **только внутри enclave**. Front-running, sandwich attacks, MEV extraction — невозможны.

### 2.4 Сравнение trust models

| Свойство | CEX | Smart Contract DEX | TEE DEX (наш) |
|----------|-----|-------------------|---------------|
| Custody | Оператор | Контракт на chain | SGX enclave |
| Verifiable logic | Нет | Да (on-chain) | Да (attestation) |
| Anti-MEV | Нет | Частично | Да (encrypted orders) |
| Скорость | ~1ms | 3-12 сек (block time) | ~1ms computation + 3-5 сек settlement |
| XRPL compatible | Да (offchain) | **Невозможно** (нет контрактов) | **Да** |
| Ключевой risk | Доверие оператору | Smart contract bugs | Intel SGX side-channels |

**Ключевое:** Smart contract DEX на XRPL невозможен. TEE — это единственный путь к verifiable computation на XRPL без sidechain.

### 2.5 XRPL + TEE = Native L1 Settlement

В отличие от sidechain подходов (EVM sidechain, Xahau):
- Settlement происходит **напрямую на XRPL mainnet** в RLUSD
- Нет bridge risk (нет моста между chain'ами)
- Нет отдельного токена или chain
- Deposit/withdrawal — обычные XRPL Payment транзакции

TEE добавляет computation layer **поверх** XRPL, не рядом с ним.

---

## 3. API Design: Orders, Positions, Trades

### 3.1 Текущее состояние (PoC)

PoC API — минималистичный, объединяет orders и positions в одном endpoint. Для production нужно разделение.

### 3.2 Production API (целевая архитектура)

Референс: [Thalex API](https://thalex.com/docs/thalex_api.yaml)

**Принцип:** Orders — это намерения. Positions — это результат исполненных orders. Trades — это записи о сделках.

#### Orders API

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/orders` | Создать новый ордер (limit, market, stop) |
| DELETE | `/v1/orders/{order_id}` | Отменить ордер |
| GET | `/v1/orders` | Список активных ордеров пользователя |
| GET | `/v1/orders/{order_id}` | Статус конкретного ордера |
| DELETE | `/v1/orders` | Отменить все ордера (cancel all) |

**Order types:**
- `limit` — ордер по указанной цене, ждёт на книге
- `market` — исполнить немедленно по лучшей доступной цене
- `stop_market` — активируется при достижении trigger price
- `take_profit` — закрывает позицию при достижении target price

**Order request:**
```json
{
    "market": "XRP-RLUSD-PERP",
    "side": "buy",
    "type": "limit",
    "size": "1000.00000000",
    "price": "0.55000000",
    "leverage": 10,
    "reduce_only": false,
    "time_in_force": "GTC",
    "client_order_id": "user-defined-id-123"
}
```

#### Positions API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/positions` | Все открытые позиции пользователя |
| GET | `/v1/positions/{market}` | Позиция по конкретному рынку |

Позиция — **нетто** всех исполненных ордеров по рынку. Если пользователь купил 100 XRP и продал 30 XRP → позиция = 70 XRP long.

**Position response:**
```json
{
    "market": "XRP-RLUSD-PERP",
    "side": "long",
    "size": "70.00000000",
    "entry_price": "0.54285714",
    "mark_price": "0.55000000",
    "liquidation_price": "0.51500000",
    "unrealized_pnl": "5.00000000",
    "margin": "38.00000000",
    "leverage": "10.00"
}
```

#### Trades API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/trades` | История сделок пользователя |
| GET | `/v1/trades/recent` | Последние сделки по рынку (public) |

**Trade record:**
```json
{
    "trade_id": "T-00001234",
    "order_id": "O-00005678",
    "market": "XRP-RLUSD-PERP",
    "side": "buy",
    "size": "100.00000000",
    "price": "0.55000000",
    "fee": "0.02750000",
    "timestamp": "2026-03-30T12:00:00Z",
    "role": "taker"
}
```

#### Account API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/account/balance` | Баланс (margin, available, equity) |
| GET | `/v1/account/deposits` | История депозитов |
| GET | `/v1/account/withdrawals` | История выводов |
| POST | `/v1/account/withdraw` | Запрос на вывод |

#### Market Data API (public, без аутентификации)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v1/markets` | Список доступных рынков |
| GET | `/v1/markets/{market}/orderbook` | Книга ордеров |
| GET | `/v1/markets/{market}/ticker` | Текущая цена, volume, funding |
| GET | `/v1/markets/{market}/trades` | Последние сделки |
| GET | `/v1/markets/{market}/funding` | История funding rate |

### 3.3 Масштабирование API

Для поддержки большого числа пользователей:

**Read-heavy endpoints** (balance, positions, orderbook, trades):
- Кешируются на HAProxy/nginx уровне
- Не требуют ecall для каждого запроса
- Orchestrator публикует snapshot'ы → read replicas

**Write endpoints** (orders, withdraw):
- Проходят через HAProxy → enclave instance
- maxconn 1 per instance, queue для ожидания
- Горизонтальное масштабирование: больше enclave instances

**WebSocket** (real-time updates):
- Отдельный сервис вне enclave
- Subscribes к event stream от orchestrator
- Streams: orderbook updates, trades, positions, funding

```
┌─────────┐     ┌──────────┐     ┌──────────────┐
│ Users   │────►│ HAProxy  │────►│ Enclave (write)│
│ (HTTP)  │     │          │     └──────────────┘
│         │     │          │     ┌──────────────┐
│ (WS)    │────►│          │────►│ WS Gateway   │◄── Orchestrator events
└─────────┘     └──────────┘     └──────────────┘
                      │          ┌──────────────┐
                      └─────────►│ Read Replica │◄── State snapshots
                                 └──────────────┘
```

### 3.4 Миграция от PoC к Production API

| PoC Endpoint | Production Equivalent |
|---|---|
| `POST /v1/perp/position/open` | `POST /v1/orders` (order → fill → position) |
| `POST /v1/perp/position/close` | `POST /v1/orders` (reduce_only=true) |
| `GET /v1/perp/balance` | `GET /v1/account/balance` + `GET /v1/positions` |
| `POST /v1/perp/withdraw` | `POST /v1/account/withdraw` |
| `GET /v1/perp/liquidations/check` | Internal (orchestrator only) |
| `POST /v1/perp/price` | Internal (orchestrator only) |
