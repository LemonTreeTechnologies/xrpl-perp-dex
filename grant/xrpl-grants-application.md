# Заявка XRPL Grants — Perpetual Futures DEX на XRPL через Trusted Execution Environments

**Название проекта:** Perp DEX на XRPL (рабочее название: `xrpl-perp-dex`)
**Заявитель:** ph18.io
**Дата заявки:** 2026-04-09
**Запрашиваемая сумма:** USD $150,000 (программа на 12 месяцев)
**Трек:** Software Developer Grants Program
**Live демо:** https://api-perp.ph18.io
**GitHub:**
- https://github.com/77ph/xrpl-perp-dex (Rust orchestrator, тесты, research)
- https://github.com/77ph/xrpl-perp-dex-enclave (C/C++ SGX enclave, perp engine)

---

## 1. Executive summary

XRPL не поддерживает смарт-контракты. Это блокирует весь DeFi-стек
деривативов — бессрочные фьючерсы, опционы, margin lending — на нативном
уровне ledger. Проекты либо уходят на EVM sidechain'ы (жертвуя гарантиями
безопасности и производительности XRPL), либо строят централизованные
кастодиальные сервисы (жертвуя trustless-свойством которое делает DeFi
интересным).

Мы заменяем смарт-контракты на **Intel SGX Trusted Execution Environments**.
Анклав исполняет ту же логику что и смарт-контракт — margin engine,
трекинг позиций, ликвидация, funding rate, подпись withdrawal — но с
**hardware-enforced целостностью**. XRPL используется для того, в чём он
лучший: расчёты.

Результат — **DEX бессрочных фьючерсов с нативным расчётом в RLUSD на
XRPL mainnet**, где withdrawal пользователя требует 2-of-3 multisig трёх
независимых SGX операторов через нативный `SignerListSet`. Нет sidechain,
нет моста, нет собственного L1.

PoC **работает сегодня** на `https://api-perp.ph18.io`. Есть рабочий
margin engine, CLOB, price feed, WebSocket gateway, DCAP remote
attestation на Azure DCsv3, и верифицированный end-to-end 2-of-3
multisig withdrawal flow на XRPL testnet с **10 on-chain transaction
hashes как доказательство** по всем девяти сценариям отказов из нашего
исследовательского документа 07.

Грант покроет 12-месячный путь от работающего testnet PoC до
**audited запуска на XRPL mainnet с RLUSD settlement**.

---

## 2. Проблема

### 2.1 У XRPL нет DeFi деривативов

XRPL имеет нативный spot DEX (CLOB) с момента запуска и AMM с момента
XLS-30, но он не может поддерживать синтетические активы, leverage,
perpetual futures, или любой инструмент требующий Turing-полного
состояния. Ограничения:

| XRPL mainnet | Статус |
|---|---|
| Встроенный CLOB DEX (spot) | Работает с запуска |
| AMM (XLS-30) | Работает с 2024 |
| RLUSD стейблкоин | Live, >$1.2B market cap |
| **Смарт-контракты** | **Не поддерживаются** |
| **Hooks** | **Не на mainnet** (только на Xahau sidechain) |
| **Деривативы** | **Невозможны без off-ledger логики** |

Последствия реальные: держатели RLUSD, которым нужен yield или
хеджирование, должны бриджить на Ethereum или Solana, теряя гарантии
settlement XRPL и регуляторное позиционирование, платя bridge fees и
задержки.

### 2.2 Существующие альтернативы имеют свои проблемы

Команды, которые не хотят смарт-контракты, попадают в один из трёх
лагерей, у каждого свои подводные камни:

1. **Централизованный order book** с custodial user deposits — требует
   доверия, single point of failure, регуляторные риски
2. **EVM sidechain** (XRPL EVM Sidechain, Xahau) — теряют гарантии
   settlement XRPL, наследуют всю attack surface Solidity (re-entrancy,
   flash loans, MEV) и операционную нагрузку второго chain
3. **Multi-operator multisig с человеческими подписантами** — мишень
   социнженерии, которая стоила Drift Protocol $280M в апреле 2026,
   когда атакующие полгода втирались в доверие членов multisig Security
   Council и затем использовали Solana durable nonces чтобы дренировать
   протокол во время рутинного теста insurance fund

Все три подхода принимают компромиссы, противоречащие самому смыслу
построения на trust-minimized ledger.

---

## 3. Решение: SGX enclave как "смарт-контракт"

### 3.1 Архитектура

```
┌──────────────────────────────────────────────────────────────┐
│                       Браузер пользователя                   │
└────────────────────────────┬─────────────────────────────────┘
                             │ TLS (X-XRPL-Signature auth)
                             ▼
               ┌──────────────────────────────┐
               │  nginx (api-perp.ph18.io)    │
               │  rate-limit + block internal │
               └──────────────┬───────────────┘
                              │
                 ┌────────────▼──────────────┐
                 │  Orchestrator (Rust)      │
                 │  • CLOB order book        │
                 │  • Price feed (Binance)   │
                 │  • WebSocket push         │
                 │  • Sequencer election     │
                 │  • Validator replay       │
                 └─────┬───────────────┬─────┘
                       │ HTTPS         │ libp2p gossipsub
                       ▼               │
               ┌────────────────┐      │   ┌─────────────┐
               │  SGX Enclave   │      └──▶│  Пиры       │
               │  (C/C++)       │          │  (2 of 3)   │
               │  • margin      │          └─────────────┘
               │  • позиции     │
               │  • ликвидация  │
               │  • ECDSA подп. │
               │  • sealed state│
               └───────┬────────┘
                       │ secp256k1 + DER
                       ▼
                ┌─────────────────┐
                │   XRPL Mainnet  │
                │  SignerListSet  │
                │  2-of-3 escrow  │
                │  RLUSD токены   │
                └─────────────────┘
```

### 3.2 Почему SGX вместо смарт-контрактов

| Смарт-контракты (Solidity, Move) | TEE (Intel SGX) |
|---|---|
| Требуют поддержки на уровне chain | Работает с любым chain включая XRPL |
| Публичный код в runtime (MEV, front-running) | Код исполняется в зашифрованной памяти |
| Gas за каждую операцию | Нет gas — compute оплачивается на хосте |
| Обновление требует chain governance | Обновление требует новой DCAP attestation |
| Re-entrancy, flash-loan attack surface | Attack surface — hardware side-channel, не application логика |

### 3.3 Почему XRPL — правильный settlement layer

- **RLUSD** — регулируемый стейблкоин с market cap >$1.2B и реальными
  институциональными контрагентами
- **3-4 секунды** финальность — достаточно быстро для perp ликвидаций
- **Нативный multisig** через `SignerListSet` — ledger primitive
  спроектированный именно для N-of-M operator custody, без смарт-контракта
- **Комиссии < $0.001** за транзакцию — влияние комиссии на PnL
  пренебрежимо мало
- **Нет mempool, нет MEV** — консенсус XRPL исключает surface для
  фронтраннинга, характерную для Ethereum-based DEX'ов

### 3.4 Почему SGX — правильный TEE

- **Intel SGX с DCAP remote attestation** — единственный широко
  развёрнутый hardware enclave с доступностью в облаке (Azure DCsv3) и
  зрелым attestation стеком, верифицируемым без обращения к Intel в
  runtime
- **Azure THIM** (Trusted Hardware Identity Management) предоставляет
  provisioned PCK certificates так что любая третья сторона может
  верифицировать 4,734-байтный SGX Quote v3 против root of trust Intel
- **Sealed data** привязывает persistent state к конкретному MRENCLAVE —
  злонамеренный оператор не может вмешаться в sealed margin state без
  падения enclave при загрузке

---

## 4. Уникальность и защитимость

### 4.1 Мы не Drift

Drift Protocol на Solana потерял $280M 1 апреля 2026 из-за шестимесячной
атаки социнженерии против его человеческого 5-of-N Security Council
multisig. Весь наш дизайн — это прямой ответ на этот класс отказа:

- **Подписанты — процессоры, а не люди** — multisig quorum в нашем
  дизайне это 3 SGX enclave на географически разделённых Azure DCsv3
  хостах. Enclave'ы нельзя "убедить" подписать невалидный withdrawal,
  потому что они исполняют детерминированный код, который повторно
  доказывает margin при каждом запросе на подпись.
- **Приватные ключи никогда не существуют вне CPU** — ключевой материал
  secp256k1 генерируется внутри enclave, sealed к MRENCLAVE, и
  используется только через in-enclave ECDSA routine. У host процесса
  нет к ним доступа.
- **MRENCLAVE можно публиковать и верифицировать** — любой пользователь
  может запросить attestation quote до доверия системе, захешировать
  наш open-source build, и сравнить.

### 4.2 Мы не Hyperliquid

Hyperliquid запускает собственный L1 со своим validator set,
консенсусом, и допущениями экономической безопасности. Это ~$50M
инвестиций в engineering. Мы переиспользуем ~$30B экономической
безопасности XRPL, используя XRPL как settlement layer и трактуя enclave
строго как computation layer. Наш "validator set" это off-the-shelf
DCsv3 instances и "консенсус" это сам XRPL — нам нужно только доказать,
что подпись каждого оператора пришла из верифицируемого enclave, что
DCAP даёт бесплатно.

### 4.3 Мы не custodial CEX

Средства пользователей лежат в XRPL-нативном escrow аккаунте,
защищённом `SignerListSet` 2-of-3 multisig между 3 независимыми SGX
операторами. Ни один оператор — и ни одна пара в сговоре против
третьего — не может переместить средства пользователей без одобрения
enclave'ов. Полный audit trail каждого deposit и withdrawal живёт на
XRPL, не на наших серверах. Катастрофическая потеря всех трёх operator
машин восстанавливается только из истории XRPL ledger.

---

## 5. Traction и proof points (на 2026-04-09)

**Всё ниже работает и независимо верифицируется.** URL'ы и хеши
транзакций включены где применимо.

### 5.1 Live инфраструктура

| Компонент | Доказательство |
|---|---|
| Публичный API | https://api-perp.ph18.io (TLS через nginx, CORS, rate limit) |
| OpenAPI spec | `GET /v1/openapi.json` |
| Live WebSocket | `wss://api-perp.ph18.io/ws` прямо сейчас шлёт ticker events |
| 3 SGX enclave | Azure DCsv3 (`20.71.184.176`, `20.224.243.60`, `52.236.130.102`) |
| DCAP attestation | Все 3 ноды возвращают 4,734-байтный SGX Quote v3, Intel-signed, верифицировано 2026-04-08 |

### 5.2 On-chain testnet transactions (XRPL testnet)

Escrow account: [`rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx`](https://testnet.xrpl.org/accounts/rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx)

Десять верифицированных транзакций по девяти сценариям отказов из
`research/07-failure-modes-and-recovery.md`:

| Сценарий | Событие | XRPL testnet tx hash |
|---|---|---|
| 3.1 Один оператор offline | 2-of-3 withdrawal успех | `FC9C32DBCC422368888518FE143AA0715BA1D1FD6E26B5EC45DA52B471D8A1D0` |
| 3.4 Malicious operator | Honest retry после отвергнутой garbage sig | `90819EC6CBA5ACC549DEF5496B665BBFA959C8FA7887C4500D779B2B917ED25F` |
| 3.5 SGX compromise | Key rotation через SignerListSet | `23E1C0EE68EB000A5C922E8D6D8A68AC042A024D1CF2FB8A437D65D47F11E809` |
| 3.5 | Withdrawal с rotated key | `8274091DFB2A0FA83B33473E31A60E90E24C832DA9B12788259D4C5DD5887F14` |
| 3.6 Hardware failure | SignerListSet после wipe | `B056CA00778054F041335F05054B106E6CBA22713A33611CCCFC924B19E1A686` |
| 3.6 | Withdrawal с replacement key | `F455EBCCBC7B858854E1A1DCCEC86430C53897319BFE6C1D11083B280C68F7BE` |
| 3.7 Cloud migration | SignerListSet rotation | `8899292EE67D800A136741C9A3950054248658625156AD7C23E4CD24E7CA3E23` |
| 3.7 | Post-migration withdrawal | `8931EFAA52AED2351D7BAE0313B84B6157386CD7B73389C4190FF5E4B975CBBB` |
| 3.8 Scaling 2-of-3 → 3-of-4 | SignerList expansion | `B992884B8A19204014AD59D899B0C66D5F3AFF69625C7795E1A5D7E3D80DAA8B` |
| 3.8 | Post-expand 3-of-4 withdrawal | `49629B8F72E8E1624B6C781DEBED3E5D734BA27530492A7A5B2D2C2AFCC7E38F` |

Полный отчёт: `research/failure-modes-test-report.md`

### 5.3 Multi-operator sequencer election верифицирован live

09 апреля 2026 state machine sequencer election (`orchestrator/src/election.rs`)
был верифицирован end-to-end на живом 3-нодовом Azure кластере,
включая network partition (split-brain) через `iptables DROP` на порту 4001.
Отчёт: `research/election-split-brain-test-report.md`. Сводка:

- Стабильный кластер: 5 минут, ноль ложных failover
- Kill sequencer → failover за 16.5 сек (timeout = 15 сек)
- Restart sequencer → reclaim за 8 сек
- Network partition → majority выбирает нового leader, minority
  сохраняет старого (обе стороны корректны)
- Reconnect → единый sequencer восстановлен за 3 сек
- Persistent libp2p identity (peer_id стабилен между рестартами)
- Heartbeat-level debug observability для операционной форензики

### 5.4 Test coverage

- **Rust unit tests**: 86 (auth, orderbook, election, p2p, trading, types, WebSocket)
- **Python integration / e2e**: 22 (auth + trading + WebSocket + multisig)
- **Enclave invariant tests**: 19 (FP8 arithmetic correctness)
- **Failure mode scenarios**: 9/9 pass с on-chain proofs
- **Security audit**: 52 findings, 50 fixed, 2 by-design (документировано в `SECURITY-REAUDIT*.md`)

### 5.5 Public demo и документация

- Asciinema запись live trading flow + DCAP attestation шаг
- Marp slide deck (RU + EN) для Hack the Block Paris (11-12 апреля 2026)
- Bilingual research документы: 10 тем в `research/` (RU + EN)
- Frontend API guide в `docs/frontend-api-guide.md`
- Русский FAQ для non-trader разработчиков в `docs/perp-dex-faq-ru.md`

---

## 6. Команда

**ph18.io** — команда из двух человек с предыдущим production SGX опытом
из отдельного проекта signing-инфраструктуры (`EthSignerEnclave`). SGX
enclave, использованный как основа для perp DEX, это очищенный fork
этой production системы — мы не начинали SGX работу с нуля для этого
гранта.

- **Lead developer / архитектор** — 10+ лет опыта systems engineering,
  основной автор SGX enclave инфраструктуры, XRPL интеграции, и Rust
  orchestrator.
- **Operations / security** — предыдущий production SGX deployment
  опыт (Azure DCsv3, DCAP attestation), XRPL testnet multisig operations,
  реакция на security audit.

Оба участника команды активно коммитят в codebase (см. `git log` в
публичных репозиториях). Подробные team bios и контактная информация
доступны по запросу.

---

## 7. Майлстоуны

Согласно структуре XRPL Grants описанной в program FAQ, майлстоуны
разбиваются примерно **30% product/integration + 70% growth**. Мы
предлагаем пять майлстоунов за 12 месяцев на общую сумму USD $150,000.

### M1 — Production orchestrator multisig integration (Product, 15% / $22,500)

**Цель:** 2026-Q2 конец (месяц 2)

- Портировать Python multisig coordinator (`tests/multisig_coordinator.py`)
  в Rust orchestrator как first-class withdrawal flow. Текущий
  `orchestrator/src/withdrawal.rs` — явный single-operator MVP stub;
  этот майлстоун заменяет его на peer-to-peer multisig подпись через
  libp2p gossipsub.
- End-to-end integration тест от orchestrator REST
  (`POST /v1/withdraw`) до on-chain 2-of-3 multisigned Payment.
- Все 9 сценариев отказов из research doc 07 перепроверяются против
  нового integrated flow.

**Deliverables:** коммит на `master`, passing тесты, обновлённый API guide.

### M2 — Audited XRPL mainnet launch с RLUSD (Product, 15% / $22,500)

**Цель:** 2026-Q3 середина (месяц 5)

- Независимый security audit обоих репозиториев (`xrpl-perp-dex` и
  `xrpl-perp-dex-enclave`). Scope аудита: margin engine, ECDSA signing
  path, XRPL transaction construction, multisig quorum logic, DCAP
  attestation flow.
- Все critical и high findings исправлены и повторно аудированы.
- XRPL mainnet escrow account provisioned с SignerListSet 2-of-3.
- RLUSD trustlines установлены для escrow.
- Mainnet запуск perp DEX в "restricted beta": сначала invite-only,
  публичное открытие через 2 недели live monitoring.
- Public DCAP attestation endpoint документирован для пользовательской
  верификации.

**Deliverables:** audit report (опубликован), mainnet escrow address,
live public API на mainnet.

### M3 — Первый $50,000 TVL на mainnet (Growth, 20% / $30,000)

**Цель:** 2026-Q3 конец (месяц 7)

- Достичь $50,000 total value locked в XRPL mainnet escrow.
- Не менее 30 уникальных user XRPL адресов с депозитами.
- Не менее 1,000 orders выполнено (open + close) на mainnet.
- Trading history queryable из XRPL ledger через `account_tx`.

**Измерение:** независимо верифицируется на XRPL mainnet через любой
XRPL explorer. Мы опубликуем live dashboard на `dashboard.ph18.io`
включая все метрики выше.

### M4 — 500 уникальных wallet'ов (Growth, 25% / $37,500)

**Цель:** 2026-Q4 конец (месяц 9)

- Достичь 500 уникальных XRPL адресов, открывших хотя бы одну позицию.
- Funding-rate система live в течение 30 дней подряд без ручного
  вмешательства.
- WebSocket gateway держит >50 concurrent clients без backpressure.

**Измерение:** тот же dashboard что M3 + public WebSocket connection
statistics endpoint.

### M5 — $1,000,000 cumulative mainnet volume (Growth, 25% / $37,500)

**Цель:** 2027-Q1 конец (месяц 12)

- Достичь $1M total trading volume на XRPL mainnet с момента запуска.
- 3-of-4 multisig expansion отработан хотя бы один раз (добавление
  четвёртого оператора).
- Public quarterly post-mortem покрывающий операционные инциденты.
- Monthly state commitment опубликован на EVM mainnet (Ethereum или
  Sepolia) через существующий CommitmentRegistry contract.

**Измерение:** on-chain volume sum из XRPL tx history escrow, проверено
против operator trade database.

---

## 8. Разбивка бюджета

Всего запрашиваемый грант: **USD $150,000 на 12 месяцев**

| Категория | Сумма | Заметки |
|---|---|---|
| Security audit (внешний) | $45,000 | Совместный аудит SGX enclave + Rust orchestrator известной blockchain security firm |
| Development (lead dev, 6 месяцев, 50%) | $36,000 | Rust multisig integration, frontend работа, operational tooling |
| Development (ops/security, 6 месяцев, 50%) | $24,000 | Azure / XRPL operations, incident response, monitoring |
| Azure DCsv3 hosting (3 × DC2s_v3) | $6,000 | $0.20/час × 3 ноды × 12 месяцев ≈ $5,256 + запас на короткоживущие экспериментальные ноды |
| XRPL mainnet операционные расходы | $2,000 | Escrow funding, reserve, активация аккаунта, fee buffer на ~1M tx |
| Frontend (trading UI) | $15,000 | Contractor для production UI на `perp.ph18.io` |
| Legal / compliance review | $10,000 | Custody структура, XRPL multisig как operator model, terms of service для restricted beta и публичного открытия |
| Monitoring & ops tooling | $5,000 | Grafana / Loki / Prometheus стек, alerting, on-call setup для 3 операторов |
| Community & documentation | $5,000 | Developer docs, integration tutorials, присутствие в XRPL developer Discord |
| Contingency / buffer (~1.5%) | $2,000 | Unallocated резерв на непредвиденное |
| **Итого** | **$150,000** | |

Средства выплачиваются по завершении майлстоунов согласно графику XRPL
Grants: 30% по M1–M2 (product/integration) и 70% по M3–M5 (growth).

---

## 9. 12-месячный roadmap

```
Месяц 1  ──▶ M1: Rust multisig интеграция, internal тесты pass
Месяц 2  ──▶ M1 ЗАВЕРШЁН. Kickoff аудита.
Месяц 3  ──▶ Раунды аудита 1-2, frontend contractor нанят.
Месяц 4  ──▶ Раунд аудита 3, critical fixes, testnet regression.
Месяц 5  ──▶ M2: mainnet restricted beta launch с RLUSD.
Месяц 6  ──▶ Публичное открытие mainnet (конец restricted beta).
Месяц 7  ──▶ M3: первый $50K TVL.
Месяц 8  ──▶ Funding rate loop стабилизирован, WebSocket масштабирован.
Месяц 9  ──▶ M4: 500 уникальных wallet'ов.
Месяц 10 ──▶ Расширение до 3-of-4 multisig (добавление 4-го оператора).
Месяц 11 ──▶ Incident retrospective, hardening ops tooling.
Месяц 12 ──▶ M5: $1M cumulative volume, quarterly post-mortem опубликован.
```

---

## 10. Финансовая устойчивость после гранта

Наша post-grant модель дохода — fee take от самого DEX. Orchestrator
уже собирает **5 bps taker fee** и **2 bps maker fee** (см.
`orchestrator/src/trading.rs`), которые платятся в fee account внутри
enclave и учитываются по операторам.

На M5 target в $1M месячного volume, 5 bps gross take генерирует
**$500 в месяц**. Этого недостаточно чтобы быть устойчивым самостоятельно,
поэтому 12-месячный post-grant план следующий:

1. **Volume-based fee take** растёт с mainnet adoption. Breakeven для
   двухчеловеческой команды (примерно $120K/год engineering cost)
   — ~$2M месячного volume при 5 bps, что реалистично в Q3–Q4 2027 если
   M4–M5 траектории держатся.
2. **Дополнительные рынки**: ETH-RLUSD-PERP и BTC-RLUSD-PERP могут быть
   добавлены с минимальной дополнительной engineering работой, поскольку
   margin engine и orchestrator market-agnostic. Каждый рынок добавляет
   независимый fee stream.
3. **Protocol staking** (`PerpStake` модуль уже реализован):
   пользователи могут стейкать XRP для fee discount, stake fees текут к
   операторам.
4. **Vault LP income** (спроектирован в research doc 02 section 6):
   liquidity providers зарабатывают funding rate и maker rebates, а
   протокол берёт skim.
5. **Institutional RFQ** как off-book продукт, как только mainnet
   liquidity позволит.

Мы будем публиковать monthly financials на `ph18.io/transparency` во
время и после grant period.

---

## 11. XRPL integration стратегия

Этот грант **не** просит XRPL Grants финансировать DEX, который
случайно settle'ится на XRPL — он просит XRPL Grants финансировать DEX,
который возможен **только благодаря конкретным XRPL primitive'ам**:

1. **SignerListSet native multisig** — наша security story опирается на
   нативный 2-of-3 quorum XRPL. На chain без native multisig нам бы
   понадобился on-chain смарт-контракт, чего мы как раз избегаем. На
   XRPL quorum — ledger-enforced primitive.
2. **RLUSD** — наша settlement currency. Наши целевые пользователи это
   именно держатели RLUSD, у которых сегодня нет способа использовать
   RLUSD для perps без bridge'инга с XRPL.
3. **Sub-4-second финальность** — достаточно быстро чтобы включать
   liquidation settlements on-chain в реальном времени, что невозможно
   на chain с минутной финальностью.
4. **Нет mempool / нет MEV** — наши пользователи явно защищены от
   sandwich и frontrunning атак, потому что консенсус XRPL не
   экспонирует ordered pending-tx pool для bidder'ов.
5. **Ledger-as-audit-trail** — сценарий catastrophic-recovery (3.9)
   опирается на 50-летние архивные гарантии XRPL. Каждый deposit, каждое
   SignerListSet update, каждый withdrawal персистится на XRPL и
   восстановим без каких-либо наших серверов.
6. **Низкие комиссии** — <$0.001 за tx значит что fee-to-PnL ratio
   пренебрежимо мал для perp DEX, что не так на Ethereum mainnet.

Если бы у XRPL не было SignerListSet и RLUSD, этот проект бы строился
на другом chain.

---

## 12. Open-source лицензирование

- **Код**: Business Source License 1.1 (BSL 1.1), конвертируется в
  **Apache 2.0** через 4 года. BSL разрешает non-commercial
  использование и review любой третьей стороной в течение restricted
  периода; commercial использование требует отдельной лицензии. Apache
  2.0 автоматически срабатывает в license change date.
- **Research документы** (`research/*.md`): Creative Commons
  Attribution-NonCommercial-NoDerivatives 4.0 International.
- **Design диаграммы**: то же что research документы.

Оба репозитория публичны с первого дня. Все коммиты подписаны
верифицированными GitHub identity команды разработчиков.

Модель BSL-с-автоматической-конверсией-в-Apache-2.0 — распространённый
паттерн для DeFi протоколов (используется Uniswap v3, Mysten, и
другими) и даёт нам защитное окно против fork-and-dump конкурентов,
одновременно гарантируя что сообщество унаследует permissively
licensed codebase через 4 года.

---

## 13. Риски и митигации

| Риск | Вероятность | Impact | Митигация |
|---|---|---|---|
| SGX side-channel исследование сломает нашу isolation assumption | Низкая | Высокий | Активный трекинг Intel TCB updates; миграция на AMD SEV-SNP или AWS Nitro как fallback; публикация monthly patch status |
| Azure deprecates DCsv3 family | Низкая | Средний | Доказана портируемость: тот же enclave работает на non-Azure bare-metal (Hetzner) для development. Документировать production migration path как часть M4 deliverables |
| RLUSD регуляторный статус меняется | Средняя | Средний | Дизайн контракта не зависит от RLUSD-specific features; можно переключиться на USDC/USDT trustlines на XRPL без изменений протокола |
| Низкая mainnet adoption после запуска | Средняя | Высокий | Growth майлстоуны M3–M5 явно измеримы; если промахнёмся, не получим эти выплаты. Грант структурирован чтобы защитить XRPL Grants от adoption risk |
| Operator collusion (все 3 оператора скомпрометированы одновременно) | Очень низкая | Катастрофический | Сценарий catastrophic-recovery 3.9 уже протестирован: funds восстанавливаются только из XRPL ledger history. Митигация — рост до 5+ операторов с географическим и юрисдикционным разнообразием к концу grant period |
| Независимый аудит находит critical bugs | Средняя | Средний | M2 явно включает audit + fix итерации перед mainnet launch. Launch gated на clean audit re-review |

---

## 14. Что мы НЕ просим

- **Equity или token allocation** — XRPL Grants программа явно
  non-dilutive, и мы не выпускаем governance token.
- **Marketing budget** сверх documentation и community development.
- **Office или hardware** — вся разработка и operations удалённые.
- **Salary top-up** сверх development line item.

---

## 15. Контакт и следующие шаги

**Основной контакт:** ph18.io team (email в GitHub profile)
**Demo:** https://api-perp.ph18.io (live), https://github.com/77ph/xrpl-perp-dex
**Demo видео:** `presentation/demo.cast` (asciinema) в репозитории
**Appearing at:** Hack the Block, Paris Blockchain Week, 11-12 апреля 2026
(Challenge 2: Impact Finance)

Мы понимаем, что wave 2025 закрыт и что Spring 2026 programming ещё не
анонсирован. Этот application package предназначен как early-awareness
submission для следующего wave и как pitch material для Hack the Block
Paris. Мы были бы рады conversation с командой XRPL Grants по
`info@xrplgrants.org` до следующего formal application window.

---

*Заявка скомпилирована 2026-04-09. Весь код, тесты, и on-chain evidence
упомянутые выше публичны и независимо верифицируются по URL'ам
перечисленным выше.*
