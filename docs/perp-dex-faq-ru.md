# Perp DEX — FAQ для разработчика SDK

**Аудитория:** программист с опытом DeFi (Uniswap), но не трейдер
**Цель:** понять что такое perp futures и как наша система работает,
чтобы презентовать это XRPL Community

---

## Часть 1: Что такое Perpetual Futures (без формул)

### 1.1 В чём разница со спотом?

**Спот (Uniswap):** ты обменял USDC на ETH. Теперь у тебя есть ETH. Если цена ETH вырастет — ты заработал, если упадёт — потерял. Простая логика "купил-продал".

**Perp futures:** ты не покупаешь ETH. Ты заключаешь **контракт** который говорит: "я ставлю что цена ETH вырастет (или упадёт)". У тебя нет ETH на руках, есть только **позиция** — запись в системе типа "Andrey владеет long 100 ETH @ entry price 3000".

Когда цена меняется, твой PnL (profit and loss) меняется. Ты можешь "закрыть позицию" — превратить виртуальный PnL в реальные деньги.

### 1.2 Зачем это нужно?

1. **Плечо (leverage):** ты ставишь $100 как залог, а контролируешь позицию на $2000 (20x leverage). Если цена ETH вырастет на 5%, ты заработаешь $100 (100% от твоего залога). Это ускоряет и прибыль, и убытки.

2. **Шорт:** в spot ты не можешь "продать ETH который у тебя нет". В perp можешь — открываешь short позицию. Если ETH упадёт — ты заработал.

3. **Никогда не expires:** обычные futures имеют дату исполнения (settlement). Perp — "perpetual" — без даты, можно держать вечно. Это упрощает торговлю — не надо постоянно "перекатывать" позиции.

### 1.3 Long vs Short — простыми словами

**Long XRP @ 0.55:**
- Ты говоришь системе: "я думаю XRP вырастет с 0.55"
- Если XRP поднимется до 0.60 → ты заработал 0.05 за каждый XRP в позиции
- Если XRP упадёт до 0.50 → ты потерял 0.05 за каждый XRP

**Short XRP @ 0.55:**
- Ты говоришь: "я думаю XRP упадёт с 0.55"
- Если XRP упадёт до 0.50 → ты заработал 0.05 за каждый XRP
- Если XRP поднимется до 0.60 → ты потерял 0.05 за каждый XRP

В обоих случаях твой PnL = `size × (current_price - entry_price)` для long, или с обратным знаком для short.

### 1.4 Margin (маржа) — твой залог

**Маржа** — это деньги которые ты ставишь под позицию. Без маржи система не пустит торговать.

Пример с 5x leverage:
- Хочешь открыть позицию на 100 XRP по цене 1.0 → notional = $100
- Required margin = $100 / 5 = **$20**
- Тебе нужно иметь $20 RLUSD на счёте чтобы открыть эту позицию

Чем больше leverage — тем меньше нужно маржи, но тем выше риск ликвидации.

### 1.5 Liquidation — самое страшное слово

**Ликвидация** = система принудительно закрывает твою позицию когда у тебя слишком мало денег для её поддержания.

Простой пример:
- Open long 1000 XRP @ 1.0 with 10x leverage → margin locked = $100
- XRP падает до 0.91 → твой нереализованный убыток = -$90
- У тебя осталось $10 маржи против $910 позиции = маржинальное соотношение 1.1%
- Если упадёт ниже **maintenance margin** (у нас 0.5%) → **ликвидация**

При ликвидации ты теряешь всю маржу + платишь штраф (0.5% от notional). Поэтому low leverage = безопаснее.

---

## Часть 2: Order Book vs AMM (как Uniswap)

Это ключевое отличие нашей архитектуры от того что ты знаешь.

### 2.1 Uniswap (AMM)

В Uniswap нет ордеров. Есть **пул ликвидности** с двумя токенами и формула:

```
x × y = k  (Uniswap V2)
```

Когда ты делаешь swap, формула пересчитывает цену автоматически. Ты не "матчишься" с другим трейдером — ты торгуешь с пулом.

**Плюсы:** простота, всегда есть ликвидность
**Минусы:** slippage растёт с размером сделки, impermanent loss для LP, нет лимитных ордеров

### 2.2 Order Book (наш подход)

У нас классический **orderbook** как на Binance/Coinbase:

```
ASKS (sells)             BIDS (buys)
1.32 — 50 XRP            1.30 — 100 XRP
1.31 — 100 XRP           1.29 — 200 XRP
1.305 — 200 XRP          1.28 — 50 XRP
                ↑
        spread = 1.305 - 1.30 = 0.005
```

Трейдеры размещают **лимитные ордера** ("я готов купить 100 XRP по 1.30"). Когда приходит **рыночный ордер** ("купи мне 50 XRP по любой цене"), он матчится с лучшими лимитными.

**Плюсы:** точное управление ценой, можно поставить лимит и ждать, нет slippage если ликвидность есть
**Минусы:** нужен matching engine, нужны market makers

### 2.3 Почему мы выбрали Orderbook, а не AMM?

1. **Profitability для market makers** — на orderbook MM зарабатывают spread, на AMM — fees минус impermanent loss
2. **Точное hedge'ирование** — institutional traders предпочитают orderbook
3. **Hyperliquid, dYdX, Drift** — все perp DEX используют orderbook, не AMM
4. **Простота для трейдера** — "поставь limit buy на 0.55" понятнее чем формулы

---

## Часть 3: Как работает наш код (модули)

### 3.1 Орхестратор vs Энклав — кто что делает

```
Пользователь
    │
    ▼  HTTPS (XRPL signature auth)
nginx :443
    │
    ▼
Orchestrator (Rust :3000)
    │
    ├── ① Order Book (CLOB)        — матчинг ордеров
    │
    ├── ② XRPL Auth                — проверка secp256k1 подписи
    │
    ├── ③ XRPL Monitor             — следит за депозитами
    │
    ├── ④ Price Feed               — Binance XRP/USDT каждые 5 сек
    │
    ├── ⑤ WebSocket Broadcast      — real-time события
    │
    ├── ⑥ PostgreSQL               — история торговли
    │
    └── ⑦ Calls Enclave            — для margin checks и signing
              │
              ▼ HTTPS (localhost:9088)
        SGX Enclave
              │
              ├── Margin engine    — проверяет хватит ли залога
              ├── Position tracker — список открытых позиций
              ├── Balance store    — RLUSD + XRP collateral
              └── ECDSA signer     — подписывает withdrawal txs
```

**Принцип разделения:**
- **Orchestrator** = большой "горячий" кэш + matching engine. Стейт в RAM. Падает — restart без потери данных (их нет, всё в pg/enclave).
- **Enclave** = единственный источник истины для **денег и позиций**. Маленький, sealed на диск. Невозможно подделать — hardware-attested.

### 3.2 Пример: трейдер делает market buy

Покажу пошагово что происходит когда Alice делает `POST /v1/orders` с body `{"side":"buy","type":"market","size":"100","leverage":5}`.

```
[1] Orchestrator получает HTTP request
    ↓
[2] auth.rs: проверяет X-XRPL-Signature, X-XRPL-Timestamp
    Если timestamp > 30s → reject (replay protection)
    Если signature invalid → reject
    Если user_id в body != address из подписи → reject
    ↓
[3] api.rs: parse order params, validate leverage 1..20
    ↓
[4] trading.rs: PRE-CHECK margin
    perp.get_balance(alice_id) — спрашивает enclave
    Если available < required_margin → reject ДО матчинга
    (это важно! без pre-check мы могли бы consumed maker liquidity
    для ордера, который enclave потом отклонит)
    ↓
[5] orderbook.rs: match_order
    Идём по asks от лучшей цены вниз:
      ask 1.30 — 50 XRP   → fill 50 @ 1.30
      ask 1.31 — 50 XRP   → fill 50 @ 1.31
    Order filled полностью.
    Returns: 2 trades
    ↓
[6] trading.rs: для каждого trade →
    perp.open_position(alice, "long", 50, "1.30000000", 5)
    perp.open_position(maker_alice_matched, "short", 50, "1.30000000", 5)
    (потому что когда Alice buy, maker по другой стороне продаёт = short)
    ↓
[7] enclave: margin check + create position + deduct fee
    Если ok → возвращает success + position_id
    Если margin вдруг кончилась (race condition) → returns -4
    ↓
[8] db.rs: insert trades в PostgreSQL (fire-and-forget)
    ↓
[9] ws.rs: broadcast WebSocket events
    {"type":"trade", "price":"1.30", "size":"50", ...}
    {"type":"orderbook", "bids":[...], "asks":[...]}
    ↓
[10] HTTP response → Alice
```

### 3.3 Что хранится где

| Данные | Где | Почему |
|--------|-----|--------|
| Orderbook (текущие активные ордера) | Orchestrator RAM | Matching должен быть быстрым (~5ms) |
| User balances (RLUSD, XRP) | **Enclave sealed** | Источник истины, невозможно подделать |
| Open positions | **Enclave sealed** | Margin зависит от позиций |
| Mark price, funding rate | Enclave RAM + Orchestrator atomic | Меняется часто |
| Trade history | **PostgreSQL** | Долгосрочное хранение, аналитика |
| Deposit log (XRPL tx hashes) | Enclave (для dedup) + pg | Чтобы не кредитовать дважды |
| Funding payments per user | **PostgreSQL** | Раньше был в enclave, перенесли в pg для масштабируемости |

### 3.4 Ключевые модули orchestrator (Rust)

| Файл | Что делает |
|------|-----------|
| `api.rs` | HTTP routes (axum), `/v1/orders`, `/v1/markets`, etc. |
| `auth.rs` | XRPL signature verification (secp256k1, low-S, timestamp) |
| `orderbook.rs` | CLOB — matching, price-time priority, FOK/IOC/GTC |
| `trading.rs` | Wires orderbook fills to enclave position opens |
| `perp_client.rs` | HTTP client → enclave (deposit, balance, open_position) |
| `xrpl_monitor.rs` | Polls XRPL ledger для escrow deposits |
| `price_feed.rs` | Binance API → mark price |
| `withdrawal.rs` | Builds XRPL Payment, asks enclave to sign, submits |
| `db.rs` | PostgreSQL writes (trades, deposits, withdrawals) |
| `ws.rs` | WebSocket broadcast (tokio::sync::broadcast) |
| `election.rs` | Multi-operator sequencer election (heartbeat) |
| `p2p.rs` | libp2p gossipsub для replication между операторами |

### 3.5 Ключевые ecalls (C++) в enclave

| ecall | Назначение |
|-------|-----------|
| `ecall_perp_deposit_credit` | Кредитует RLUSD после XRPL deposit |
| `ecall_perp_open_position` | Margin check + создаёт позицию |
| `ecall_perp_close_position` | Закрывает позицию, реализует PnL |
| `ecall_perp_get_balance` | Возвращает balance + positions JSON |
| `ecall_perp_update_price` | Обновляет mark/index price |
| `ecall_perp_check_liquidations` | Сканирует все позиции на margin ratio |
| `ecall_perp_liquidate` | Принудительно закрывает позицию |
| `ecall_perp_apply_funding` | Применяет funding rate ко всем позициям |
| `ecall_perp_withdraw_check_and_sign` | Margin check + ECDSA подпись XRPL tx |
| `ecall_perp_save_state` / `load_state` | Sealed persistence |

---

## Часть 4: Числа и формулы (для presentation)

### 4.1 Margin calculation

```
notional       = size × price
required_margin = notional / leverage
fee            = notional × 0.0005      (0.05% taker fee)
```

**Пример:** Open long 100 XRP @ 1.31 with 5x leverage
```
notional       = 100 × 1.31 = 131.00 RLUSD
required_margin = 131.00 / 5 = 26.20 RLUSD  (заблокировано)
fee            = 131.00 × 0.0005 = 0.0655 RLUSD  (списано с баланса)
```

### 4.2 PnL (Profit and Loss)

**Long:**
```
unrealized_pnl = size × (mark_price - entry_price)
```

**Short:**
```
unrealized_pnl = size × (entry_price - mark_price)
```

### 4.3 Liquidation threshold

```
margin_ratio = (margin + unrealized_pnl) / notional

Если margin_ratio ≤ 0.005 (0.5%) → ликвидация
```

**Пример:**
```
Position: long 1000 XRP @ 1.0, margin = 100 RLUSD (10x)
XRP падает до 0.91:
  upnl = 1000 × (0.91 - 1.00) = -90 RLUSD
  notional = 1000 × 0.91 = 910 RLUSD
  margin_ratio = (100 - 90) / 910 = 0.011 (1.1%)
  → ещё не ликвидируется (>0.5%)

XRP падает до 0.905:
  upnl = -95
  margin_ratio = (100 - 95) / 905 = 0.0055 (0.55%)
  → почти граница

XRP падает до 0.904:
  upnl = -96
  margin_ratio = (100 - 96) / 904 = 0.0044 (0.44%)
  → ЛИКВИДАЦИЯ
```

### 4.4 Funding rate

Funding — механизм который держит цену perp близко к spot.

```
funding_rate = clamp((mark_price - index_price) / index_price, -0.0005, 0.0005)
```

Каждые 8 часов:
- Если mark > index (perp дороже spot) → longs **платят** shorts
- Если mark < index → shorts платят longs

```
payment_per_position = size × mark_price × funding_rate
```

Это incentive для арбитражёров — если perp дороже spot, выгодно открыть short (получаешь funding) и купить spot. Это сжимает разницу.

### 4.5 Параметры рынка XRP-RLUSD-PERP

| Параметр | Значение |
|----------|----------|
| Settlement | RLUSD |
| Collateral | RLUSD (100% LTV) + XRP (90% LTV) |
| Max leverage | 20x |
| Taker fee | 0.05% |
| Maker fee | 0% |
| Maintenance margin | 0.5% |
| Liquidation penalty | 0.5% |
| Funding interval | 8 hours |
| Funding rate cap | ±0.05% per period |

---

## Часть 5: Что особенного в нашей реализации

### 5.1 Откуда берётся доверие?

**Uniswap:** код в smart contract, публичный, верифицируется любым через blockchain explorer.

**Наш Perp DEX:**
- **Код в SGX enclave** — не публичный bytecode, а C/C++ скомпилированный
- **MRENCLAVE** — SHA-256 хеш скомпилированного бинарника
- **DCAP attestation** — Intel-подписанный quote доказывает что enclave с конкретным MRENCLAVE действительно запущен
- Любой может проверить MRENCLAVE против опубликованного исходного кода → доказательство что в enclave работает именно опубликованный код

Это **другая модель доверия**, но эквивалентная по силе:
- Uniswap: "я доверяю байткоду который вижу"
- Мы: "я доверяю Intel SGX и хешу кода"

### 5.2 Почему не AMM?

XRPL уже имеет нативный AMM (Uniswap-like). Если бы мы хотели сделать spot DEX — использовали бы его. Но perp futures **невозможны на AMM** в том виде как у Hyperliquid/dYdX:

- Perp требует matching между long и short
- Margin engine с liquidations
- Funding rate чтобы держать цену близко к spot
- Variable position sizes с разными leverage

Всё это надо implement внутри smart contract. У XRPL нет smart contracts → нельзя сделать на mainnet → нужен другой подход. Наш подход: TEE = "smart contract в hardware".

### 5.3 Сравнение с Hyperliquid

| | Hyperliquid | Наш Perp DEX |
|---|---|---|
| Где работает | Свой L1 (HyperBFT) | XRPL mainnet |
| Custody | Bridge на Arbitrum | XRPL escrow (SignerListSet 2-of-3) |
| Trust | BFT валидаторы (4+ нод) | TEE attestation + multi-operator |
| Code | Closed source | Open source (BSL 1.1) + DCAP verifiable |
| TPS | 100k+ (свой L1) | ~200 (один enclave bottleneck) |
| Settlement | Sub-second | 3-4 sec (XRPL) |

Hyperliquid быстрее но centralized (4 валидатора). Мы медленнее но **никакого моста** — funds на XRPL L1. И код можно proof'ить через DCAP.

---

## Часть 6: Что показывать XRPL Community

### 6.1 Главные тезисы

1. **"DeFi на XRPL без smart contracts — впервые на mainnet"**
2. **"RLUSD получает первый perp DEX"** (RLUSD до этого только для payments)
3. **"Native primitives only"** — escrow, multisig, RLUSD, без bridges
4. **"Hardware-verifiable code"** — DCAP attestation вместо public bytecode
5. **"Live now"** — `https://api-perp.ph18.io`, не вапор

### 6.2 Live demo flow (что показывать в терминале)

```bash
# 1. List markets
curl https://api-perp.ph18.io/v1/markets

# 2. Mark price (обновляется каждые 5 сек)
curl https://api-perp.ph18.io/v1/markets/XRP-RLUSD-PERP/funding

# 3. Place order (with auth)
python3 tools/xrpl_auth.py --secret <seed> \
  --request POST https://api-perp.ph18.io/v1/orders \
  '{"side":"buy","type":"limit","price":"1.30","size":"100","leverage":5}'

# 4. WebSocket — real-time trades
wscat -c wss://api-perp.ph18.io/ws

# 5. DCAP attestation от Azure DCsv3
ssh azureuser@sgx-node-2 \
  'curl -sk -X POST https://localhost:9088/v1/pool/attestation-quote \
    -d "{\"user_data\":\"0xdeadbeef\"}"'
# Возвращает 4734-byte Intel-signed quote
```

### 6.3 Технические достижения для презентации

- **130 автоматизированных тестов** (unit + integration + e2e + invariants)
- **52 audit findings** найдено и исправлено (2 by-design)
- **DCAP attestation** работает на Azure DCsv3
- **Production deploy** на 2 серверах (Hetzner + Azure)
- **Multi-operator architecture** (sequencer election, P2P replication)

### 6.4 Вопросы которые могут задать на presentation

**Q: Почему не использовать XRPL AMM для perp?**
A: AMM не поддерживает perp futures — нужен margin engine, liquidations, funding rate. Это требует Turing-complete computation, чего нет на XRPL. TEE заменяет smart contract.

**Q: Что если SGX enclave hacked?**
A: Атакующий с физическим доступом к CPU может попытаться side-channel attack. Это требует лабораторного оборудования, недели работы. Сравнить с Drift Protocol (Solana, апрель 2026) — драйн $280M НЕ через баг в коде, а через social engineering Security Council multisig (5 подписантов). Атакующий полгода втирался в доверие команды, использовал Solana durable nonces для отложенных транзакций, и в момент легитимного теста insurance fund активировал заранее подписанные admin-операции. В нашей архитектуре подписанты — это процессоры (SGX enclaves), их нельзя social-engineer'ить и нельзя "убедить" подписать невалидную транзакцию.

**Q: Как пользователи доверяют что код тот который вы показываете?**
A: DCAP attestation. Любой может запросить quote с challenge nonce, получить Intel-signed proof что enclave с конкретным MRENCLAVE запущен. Сравнивая MRENCLAVE с хешем нашего открытого кода → доказательство.

**Q: Почему RLUSD?**
A: Это нативный stablecoin XRPL (от Ripple). Регулируемый, институциональный. Делать DeFi DEX на mainnet без stablecoin бессмысленно — все цены к чему-то нужно привязывать.

**Q: Multi-operator — это сейчас работает?**
A: Архитектура спроектирована (sequencer election, libp2p gossipsub, XRPL multisig). Сейчас один оператор для PoC. Запуск 3 операторов после хакатона.

**Q: Сравнение с Hyperliquid? Drift?**
A: Hyperliquid — свой L1, мы используем существующий XRPL. Drift — Solana perp DEX, апрель 2026 потерял $280M из-за компрометации multisig через social engineering (не баг в смарт-контракте, а человеческий фактор). У нас подписанты — SGX enclaves, ключи никогда не существуют вне процессора, multisig между процессорами а не людьми → этот класс атак не применим.

---

## Часть 7: Шпаргалка терминов

| Термин | Простыми словами |
|--------|-----------------|
| **Perp / Perpetual** | Фьючерсный контракт без даты экспирации |
| **Long** | Ставка на рост цены |
| **Short** | Ставка на падение цены |
| **Leverage** | Множитель — 5x значит позиция в 5 раз больше залога |
| **Margin** | Залог под позицию |
| **Notional** | Полный размер позиции в долларах (size × price) |
| **PnL** | Profit and Loss — прибыль/убыток |
| **Mark price** | Цена для расчёта PnL и ликвидаций (обычно "честная" цена) |
| **Index price** | Цена с внешнего источника (Binance) |
| **Funding rate** | Платёж между longs и shorts каждые 8h, держит mark близко к index |
| **Liquidation** | Принудительное закрытие позиции при недостатке маржи |
| **Maintenance margin** | Минимальное margin ratio до liquidation (у нас 0.5%) |
| **Maker** | Тот кто поставил limit order и ждёт |
| **Taker** | Тот кто исполнил против существующего ордера (market order) |
| **Spread** | Разница между best bid и best ask |
| **Order book / CLOB** | Список лимитных ордеров (Central Limit Order Book) |
| **MM / Market Maker** | Профессиональный поставщик ликвидности |
| **Slippage** | Разница между ожидаемой и фактической ценой исполнения |
| **DCAP** | Data Center Attestation Primitives — Intel'овский protocol для верификации SGX |
| **MRENCLAVE** | Хеш скомпилированного enclave кода |
| **Sealed storage** | Шифрованное хранилище SGX, доступное только этому enclave на этом CPU |

---

## Часть 8: Полезные ссылки

- Live API: https://api-perp.ph18.io
- OpenAPI spec: https://api-perp.ph18.io/v1/openapi.json
- GitHub: https://github.com/LemonTreeTechnologies/xrpl-perp-dex
- Enclave: https://github.com/77ph/xrpl-perp-dex-enclave
- Presentation: `presentation/perp-dex-pitch.html`
- Demo recording: `presentation/demo.cast` (asciinema, can convert to gif)
- Frontend (TBD): perp.ph18.io

**Research docs (research/):**
- 01 — Feasibility Analysis
- 02 — TEE Perp Mechanics
- 03 — Production Architecture
- 04 — Multi-Operator Architecture
- 05 — TEE Rationale
- 06 — Latency Analysis
- 07 — Failure Modes
- 08 — TEE vs Smart Contract Security (Drift hack analysis)
- 09 — Grant Narrative
- 10 — Comparison with Hyperliquid
