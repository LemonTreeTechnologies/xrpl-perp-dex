# Hack the Block Париж — план на 36 часов

**Команда:** Alex, Andrey, Tom
**Трек:** Challenge 2 — Impact Finance
**Проект:** Perpetual Futures DEX на XRPL с расчётами в RLUSD

---

## Позиционирование (что видят судьи)

**Публичный нарратив:** первая биржа бессрочных фьючерсов с нативным
расчётом в RLUSD на XRPL mainnet, на базе **Intel SGX Trusted Execution
Environments**. Средства пользователей хранятся в XRPL `SignerListSet`
2-of-3 multisig между независимыми SGX операторами. DCAP remote
attestation доказывает подлинность enclave. Без sidechain, без моста.

**Говорим свободно:**
- Intel SGX как технология (известный стандарт)
- DCAP attestation: "любой может проверить что enclave настоящий"
- Архитектура: User → Orchestrator → SGX Enclave → XRPL
- MRENCLAVE, quote size, Azure DCsv3
- 2-of-3 multisig, margin enforcement в hardware

**НЕ раскрываем:**
- Исходный код enclave (`xrpl-perp-dex-enclave` репо private)
- Внутренние детали margin engine C/C++ кода
- Формат sealed state, key derivation специфика

Если спросят исходный код enclave:
> "Orchestrator полностью open source (BSL 1.1). Бинарник enclave
> опубликован для DCAP верификации — можно захешировать и сравнить с
> MRENCLAVE. Исходный код будет опубликован после mainnet аудита."

---

## Что уже готово (НЕ делаем на хакатоне)

Всё ниже **live и проверено** на 10 апреля 2026:

- Live API: `api-perp.ph18.io` (nginx, TLS, CORS)
- Secure computation модуль с margin engine, трекинг позиций, ECDSA подпись
- Rust orchestrator: CLOB orderbook, P2P gossipsub, sequencer election
- 2-of-3 multisig withdrawal через XRPL нативный SignerListSet — работает, проверен
- WebSocket с Fill/OrderUpdate/PositionChanged + channel subscriptions
- PostgreSQL репликация трейдов между 3 операторами
- Persistence resting orders + failover recovery
- 16/16 E2E тестов, 9/9 failure mode scenarios с 10 on-chain tx
- Готовый пакет заявки на грант

**Стратегия: мы НЕ строим DEX на хакатоне. Он готов. Мы строим
ДЕМО-СЛОЙ который показывает судьям "это настоящее и работает на
XRPL прямо сейчас".**

---

## Таймлайн 36 часов

### Часы 0-2: Настройка (все 3)

- [ ] WiFi площадки, SSH до серверов, smoke test API + WebSocket
- [ ] Договориться о задачах и deliverables

### Часы 2-14: Спринт (параллельные треки)

**Трек A — Фронтенд UI (Tom, ~12ч)**

Собрать `perp.ph18.io`:
- [ ] Подключение кошелька (GemWallet / Crossmark)
- [ ] Live mark price + funding rate из REST
- [ ] Стакан (bids/asks) — REST + WebSocket обновления
- [ ] Отправка limit/market ордеров (подпись кошельком)
- [ ] Открытые ордера + позиции
- [ ] Real-time fills через WebSocket `user:rXXX`
- [ ] Кнопка "Verify Enclave" → `/v1/attestation/quote` → MRENCLAVE + "Intel SGX ✅"
- [ ] Секция "About": "Intel SGX enclave, XRPL settlement, 2-of-3 multisig, RLUSD"

**Минимум для демо:** цена + submit order + live fills. Кнопка "Verify Enclave" — wow-фактор.

**Трек B — Live trading демо (Andrey, ~4ч)**

- [ ] 2 тестовых кошелька + deposit в escrow
- [ ] Начальные ордера (spread вокруг Binance mid)
- [ ] Маркет-мейкер бот (50 строк Python, каждые 5 сек)
- [ ] Полный flow тест: deposit → trade → WS fill → multisig withdraw
- [ ] Backup asciinema запись
- [ ] Заранее пополненные кошельки (seeds сохранить)

**Трек C — Питч и материалы (Alex, ~6ч)**

- [ ] Attestation verifier `perp.ph18.io/verify`:
  - Кнопка "Fetch quote" → `/v1/attestation/quote` → MRENCLAVE + 4,734 bytes + "Intel SGX ✅"
  - Сравнение с хешем опубликованного бинарника
  - Не показывает исходный код — только верификацию бинарника
- [ ] Landing page `perp.ph18.io/about`:
  - Диаграмма (User → API → Orchestrator → SGX Enclave → XRPL)
  - "2-of-3 SGX multisig защищает ваши средства"
  - "DCAP attestation — любой может проверить enclave"
  - Ссылка на XRPL testnet explorer с escrow
- [ ] 5-минутный питч:
  - Проблема → Решение → Почему XRPL → Live демо → "проверьте на XRPL" → Призыв
  - Прогнать 2 раза
- [ ] Q&A шпаргалка (10 вопросов + ответы)
- [ ] 1-страничное summary для нетворкинга

### Часы 14-18: Интеграция (все 3)

- [ ] Фронт ↔ API end-to-end
- [ ] Полный демо-flow вместе:
  1. UI → живые цены
  2. Кошелёк → limit order → стакан
  3. Crossing order → fill на WS
  4. "Verify Enclave" → DCAP quote → MRENCLAVE → "Intel SGX ✅"
  5. Explorer → "funds тут, на XRPL"
  6. Withdraw multisig → tx hash на explorer
- [ ] Backup видео если время

### Часы 18-24: Сон (не пропускать)

### Часы 24-30: Полировка + practice

### Часы 30-34: Подготовка демо

- [ ] Сабмит проекта
- [ ] Alex: проблема + решение (2 мин)
- [ ] Tom: live демо (2 мин)
- [ ] Andrey: архитектура + Q&A (1 мин)

### Часы 34-36: Презентация

---

## Что говорить (и что нет)

**Говорим свободно:** Intel SGX, DCAP attestation, MRENCLAVE, Azure DCsv3, архитектура, 2-of-3 multisig.

**Не раскрываем:** исходный код enclave (бинарник опубликован, исходники private до mainnet аудита).

| Вопрос | Ответ |
|---|---|
| "Можно посмотреть код enclave?" | "Orchestrator полностью open source. Enclave бинарник опубликован для DCAP верификации. Исходный код — после mainnet аудита." |
| "Это MPC?" | "Нет. Каждый оператор — свой SGX enclave. Multisig — нативный XRPL SignerListSet." |
| "Оператор может украсть?" | "Нет. Enclave проверяет margin в hardware-isolated memory. Скомпрометированный оператор не может заставить enclave подписать невалидный withdrawal." |
| "Как проверить?" | "Два пути: DCAP attestation доказывает подлинность enclave, и XRPL escrow видно в любом explorer." |
| "Аудит?" | "52 findings, 50 fixed, 2 by-design. Отчёт в репо." |
| "Open source?" | "Orchestrator: BSL 1.1 → Apache 2.0 через 4 года, на GitHub. Enclave бинарник: опубликован. Исходный код enclave: после mainnet." |

---

## Чего НЕ делать

1. **Не трогать backend** — работает, 16/16 тестов
2. **Не пересобирать SGX enclave** — долгий rebuild + signing
3. **Не добавлять фичи** — scope creep
4. **Не раскрывать исходный код enclave** — бинарник публичен, исходники после mainnet аудита
5. **Не пытаться mainnet** — testnet безопаснее

---

## Ключевые цифры

- **$280M** — потери Drift Protocol (social engineering на human multisig, апрель 2026)
- **4,734 байт** — Intel-signed DCAP attestation quote наших SGX enclave
- **2-of-3** — XRPL нативный SignerListSet multisig между SGX операторами
- **16.5 сек** — failover sequencer (live test на 3-нодовом Azure кластере)
- **3 сек** — reconvergence после network partition
- **12** — верифицированных multisig tx на XRPL testnet
- **16/16** — E2E test pass rate
- **52** — findings в security audit (50 fixed, 2 by-design)
- **$150K** — заявка на грант готова
