# Обзор XLS-предложений — релевантность для нашего perp DEX

**Статус:** Исследовательская заметка. Не дизайн-документ. Фиксирует, что было изучено, что применимо, что нет, и как выглядит конкурентный ландшафт по состоянию на 2026-04-13.

**Английская версия:** `xls-survey-for-perp-dex.md` (по правилу bilingual docs).

**Источники:** репозиторий `XRPLF/XRPL-Standards` (raw markdown), листинги `opensource.ripple.com`. Где спецификацию не удалось получить (404 или JS-рендеренная оболочка) — это указано явно.

---

## 0. Краткая сводка

**Ни одно XLS-предложение на XRPL не конкурирует с тем, что мы строим.** Никто из перечисленных стандартов не предлагает перпов, off-ledger TEE-backed CLOB, пороговой кастодии, или чего-то напоминающего margin/funding/liquidation engine. Ближайшие соседи — спот-DEX/AMM примитивы (XLS-30 AMM, XLS-81 Permissioned DEX) и стагнировавшее предложение по опционам (XLS-62). Наше положение на XRPL — **first and only** для perp.

Из изученного: три предложения стоит интегрировать как реальные строительные блоки, два держать на watch list, остальные либо нерелевантны, либо неправильной формы, либо в другой категории.

**Полезно сейчас (или скоро):**
1. **XLS-47 Price Oracles** (Final) — как один из входов нашего mark-price oracle вместе с CEX-фидами.
2. **XLS-56 Batch** (Final) — для атомарной эмиссии многошаговых XRPL settlement-транзакций нашим оркестратором.
3. **XLS-70 Credentials + XLS-80 Permissioned Domains** (оба Final) — как опциональный KYC-gating слой, если/когда понадобится регулируемый tier.

**Watch list:**
4. **XLS-85 Token Escrow** (Final, активирован 2026-02-12) — применим для user-side депозитных эскроу с оговорками; не подходит как наш settlement engine.
5. **XLS-100 Smart Escrows** (Draft) — перспективный будущий примитив для oracle-settled per-position структур, но это не CLOB engine и пока только XRP.

**Не полезно в краткосроке, но архитектурно значимо:**
- **XLS-101 Smart Contracts** (Draft, ранняя версия). Добавляет *transaction emission* и *persistent contract state* — два недостающих примитива, которые ранее делали on-chain perps структурно невозможными на XRPL. Так что в теории пространство компромиссов меняется. На практике — нам не помогает: латентность консенсуса (3–5с) фундаментально слишком велика для CLOB perp venue вне зависимости от VM, gas-экономика не определена, и спека годы до активации. Детали в §2.11.
- **XLS-102 WASM VM** — execution substrate, на котором работают и XLS-100, и XLS-101. Foundational, без независимой ценности для нас, пока один из них не активирован.

**Не полезно для нас:**
- **XLS-66 Lending Protocol** — неправильная форма (off-chain underwriting, fixed-term, no liquidation).
- **XLS-65 Single Asset Vault** — теоретически мог бы быть LP-контейнером, но добавляет сложность поверх нашего orchestrator-managed MM-капитала без явного выигрыша.
- **XLS-62 Options** — стагнирует, неактивно.

---

## 1. Методология и охват

Изучены XLS-предложения, перечисленные на `opensource.ripple.com/docs`, и соответствующие директории под `XRPLF/XRPL-Standards`. Портал opensource рендерится на JS и не возвращает body через обычный HTTP fetch — всё содержательное извлечено из GitHub markdown.

Покрытые предложения (по номерам XLS): 30, 33, 34, 47, 51, 56, 62, 65, 66, 68, 70, 74, 75, 80, 81, 82, 85, 89, 94, 96, 100, 101, 102. Не каждое обсуждается подробно ниже — только те, что задевают наше проблемное пространство.

Что сознательно вне охвата: NFT-предложения (XLS-51), MPT metadata (XLS-89), confidential MPT (XLS-96), payment-channel-token escrow (XLS-34), sponsored fees (XLS-68), и пара account permissions / delegation (XLS-74/75). Они не пересекаются с архитектурой perp DEX очевидным образом, и их разбор разбавил бы документ.

---

## 2. Ключевые предложения, в порядке приоритета для нас

### 2.1 XLS-85 Token Escrow

**Статус:** Final. Активирован на mainnet XRPL 2026-02-12 при поддержке более 88% валидаторов.

**Что делает:** расширяет существующий native escrow примитив (который ранее держал только XRP) для поддержки IOU-токенов (trustlines) и Multi-Purpose Tokens (MPTs). `EscrowCreate` теперь принимает поле `Amount`, которое может быть строкой (XRP drops) или объектом (issued currency / MPT). Эмитенты токенов должны явно включить функциональность: IOU-эмитенты ставят `lsfAllowTrustLineLocking` на свой AccountRoot (`AccountSet` с `SetFlag: 17`); MPT-эмитенты ставят `lsfMPTCanEscrow` на объекте `MPTokenIssuance`.

**Модель авторизации — критическая оговорка:** *только source или destination escrow могут finish'ить или cancel'ить его*. Нет third-party finish пути. Эмитенты не могут быть source escrow. Это единственный факт, определяющий, может ли XLS-85 играть какую-то роль в нашей системе.

**Обработка замороженных токенов:** deep/full freeze блокирует `EscrowFinish` (`tecFROZEN`). Оба типа freeze всё ещё разрешают `EscrowCancel`.

**Полезность для нас:**

- **Как user-side депозитный escrow:** *возможно*. Пользователь мог бы поместить RLUSD в escrow с pseudo-account протокола как destination, с временем `FinishAfter` и дедлайном `CancelAfter`. Протокол finish'ит во время кредитования margin. По сути это эквивалент прямого `Payment` плюс кредитной записи в энклейве, с дополнительной reserve cost и recovery-путём, если протокол исчезнет (пользователь может cancel'ить после `CancelAfter`).
- **Как in-flight settlement примитив (например, для блокировки залога позиции):** *нет*. Правило "только source или destination могут finish'ить" означает, что энклейв / протокол не может выступать как third-party арбитр, освобождающий средства на основе своего внутреннего состояния. Протоколу пришлось бы быть контрагентом каждого escrow — а это ровно та архитектура, которую наш memo `project_xrpl_amm_viability.md` фиксирует как фатальную: она делает протокол AMM'ом и ломает CLOB-инварианты.
- **Как withdrawal queue:** *возможно, но малоценно*. Escrow с `FinishAfter` мог бы реализовать отложенный вывод — но наша deploy + custody модель уже имеет multi-sig timing controls; добавление XLS-85 просто удваивает учёт.

**Вердикт:** держим на watch list. Реалистичное применение — "user депозит через XLS-85 escrow с `CancelAfter` как safety hatch", и только если решим, что свойство safety hatch стоит reserve cost. Не load-bearing.

---

### 2.2 XLS-100 Smart Escrows (с XLS-102 WASM VM)

**Статус:** Draft (XLS-100 последнее обновление 2025-11-20; XLS-102 обновлён 2026-02). Ожидает community review и валидаторного голосования. Не активирован.

**Что делает:** добавляет поле `FinishFunction` к объектам `Escrow`, содержащее скомпилированный WebAssembly. WASM экспортирует одну функцию `finish() -> i32`; если она возвращает `> 0`, escrow может быть released. Выполнение ограничено 100,000 газа (UNL-votable), код ограничен 100KB, со строго фиксированным Wasmi runtime для детерминированности консенсуса.

**Что WASM может читать:** ledger objects (read-only), oracle data (`PriceDataSeries`, выход XLS-47), credential objects (выход XLS-70), собственные поля escrow, time-производное состояние. Около 70 host functions: общий доступ к ledger, NFT lookup, криптография, float operations.

**Что не может:**
- **No transaction emission.** Может решить "release / don't release" и писать в собственное 4KB поле `Data`. Больше ничего.
- **No write-доступа к другим ledger objects.**
- **No итерации по произвольным directories** — весь доступ через bounded keylets.
- **Поддержки токенов XLS-85 пока нет.** Сноска в спеке говорит, что поддержка токенов "currently up for voting as part of TokenEscrow amendment", но на момент последней ревизии — только XRP.
- **Должен иметь `CancelAfter`** для митигации риска застрявших средств от багованного WASM.

**Список use cases явно включает derivatives:** "Oracle-Driven P2P Bets", "Options Contracts" и "Vesting Schedules". Это самое близкое в каталоге XLS к нативному programmable settlement.

**Полезность для нас:**

- **Как per-position settlement structure для *очень* простых продуктов** (например, бинарная oracle-settled ставка между двумя именованными аккаунтами): да, в принципе.
- **Как CLOB engine:** нет. Нет transaction emission, нет aggregate state machine между позициями, нет способа выразить "match этого taker против лучшего resting maker, обнови обе позиции, начисли fees, направь rebates". CLOB требует всего этого.
- **Как liquidation engine для существующих позиций:** тоже нет. Escrow может прочитать price oracle и решить release, но не может *принудительно* закрыть позицию — нет execution-on-trigger модели, только "проигравший спор не пойдёт claim'ить".
- **Когда token escrow появится внутри smart escrows** (следующий amendment cycle, предположительно), вывод не меняется — недостающее не токены, а emit и aggregate state.

**Вердикт:** watch list. Действительно интересный примитив, и мы должны его отслеживать, особенно если XLS-101 Smart Contracts (более широкое programmability предложение) когда-нибудь получит transaction emission семантику. Но не меняет нашу краткосрочную архитектуру, и "перечитываем XLS-100 раз в полгода" — правильная каденция.

---

### 2.3 XLS-47 Price Oracles

**Статус:** Final (создан август 2023).

**Что делает:** добавляет ledger object `PriceOracle`, держащий до десяти price pairs на инстанс. Обновление через `OracleSet`, удаление через `OracleDelete`. Только владелец может писать. Поле `LastUpdateTime` отслеживает freshness. Native API `get_aggregate_price` вычисляет mean, median и trimmed-mean между несколькими oracle-инстансами с опциональной фильтрацией по freshness.

**Полезность для нас:**

- **Как один из входов в нашу mark price:** хорошее соответствие. Наша mark price нуждается в multi-source устойчивости (CEX-фиды, наша собственная EWMA по последним fills, sanity-check сигнал). XLS-47 + `get_aggregate_price` — это именно тот "trimmed-mean между N oracle writers", который хочется иметь как sanity-check tier.
- **Не заменяет CEX-фиды.** Каждый oracle принадлежит одному аккаунту, так что доверие сводится к тому, кто запускает writers. Aggregator помогает, если есть несколько независимых writers; на XRPL сегодня множество writers маленькое.
- **Полезно как public commitment.** Если наш энклейв подписывает и пишет свою mark price в XLS-47 oracle на каждом settlement boundary — это даёт нам публичный, on-ledger, timestamped trail того, какую mark price протокол использовал в любой момент. Дёшево добавить, ценно для аудита и споров.

**Вердикт:** интегрировать. Это самая простая "хорошая штука, которой мы должны пользоваться" из списка. Action item: добавить однострочную задачу "publish mark price в XLS-47 oracle на каждом funding interval" в post-hackathon бэклог. Стоимость маленькая, ценность нетривиальная.

---

### 2.4 XLS-56 Batch

**Статус:** Final (последнее обновление 2026-02-10).

**Что делает:** оборачивает 2–8 inner-транзакций внутри одной outer `Batch` транзакции. Четыре режима атомарности:

- `ALLORNOTHING` — каждая inner tx должна успешно выполниться, иначе не выполняется ни одна
- `ONLYONE` — первый успех коммитится, остальные блокируются
- `UNTILFAILURE` — последовательно, останавливается на первом failure
- `INDEPENDENT` — выполняются все независимо от индивидуальных результатов

Single-account batches подписывают только outer tx. Multi-account batches используют массив `BatchSigners` — каждый участвующий аккаунт подписывает outer tx.

**Полезность для нас:**

- **Атомарные settlement events.** Когда наш orchestrator эмитит XRPL-side settlement (кредитуем победителей + дебетуем проигравших + платим funding + переводим fees), `ALLORNOTHING` — правильный примитив. Сегодня нам пришлось бы либо выпускать их последовательно и обрабатывать partial failure вручную, либо упаковывать в одну business-logic транзакцию внутри энклейва и полагаться на получателя. С XLS-56 атомарность даётся даром.
- **Multi-leg депозиты/выводы.** Открытие или закрытие позиции пользователем может быть выражено как batched (margin-update + position-credit + fee-payment) tx.
- **Лимит 8 inner tx в порядке** для нашей settlement granularity — funding intervals или per-batch settlement events комфортно помещаются.

**Вердикт:** интегрировать. Это вторая "хорошая штука, которой мы должны пользоваться". Action item: когда подключим периодический settlement из энклейва в XRPL (сейчас TODO вне ликвидации), строить это на `Batch ALLORNOTHING` с первого дня, а не ретрофитить позже.

---

### 2.5 XLS-70 Credentials + XLS-80 Permissioned Domains

**Статус:** оба Final. XLS-70 создан июнь 2024, XLS-80 финализирован сентябрь 2024.

**Что делают:**
- **XLS-70:** вводит ledger object `Credential` (subject, issuer, type, expiration, опциональный URI). Три транзакции: `CredentialCreate`, `CredentialAccept`, `CredentialDelete`. KYC-документы остаются off-chain; on-chain живёт только аттестация.
- **XLS-80:** вводит "permissioned domain" — коллекцию принимаемых credential types. Membership *неявный*: аккаунт является членом тогда и только тогда, когда у него есть хотя бы один принимаемый credential. Нет явного join/leave, нет behavioural restrictions, только membership predicate.

**Важная оговорка:** XLS-80 сам по себе *не* gating'ует фичи. Это membership примитив. Feature gating происходит в предложениях, которые потребляют domains (например, XLS-81 Permissioned DEX добавляет поле `DomainID` к offer'ам native DEX).

**Полезность для нас:**

- **Как KYC tier flag.** Если когда-нибудь захотим предложить "KYC-only market" (institutional pool) рядом с открытым рынком — XLS-70 + XLS-80 правильный способ идентифицировать, у кого какие credentials. Наш orchestrator читает credentials пользователя при подключении, gating'ует вход, тегирует трейды по domain.
- **Как access control для REST API оркестратора:** также чисто. Энклейв может требовать credential signature на `POST /v1/orders` для регулируемого tier.
- **Privacy:** on-chain `Credential` объект коммитится только к аттестации, не к самим KYC-данным. Приемлемо.

**Вердикт:** интегрировать когда понадобится. Не на критическом пути для permissionless launch, но правильный примитив для будущего регулируемого tier. Action item: держать в уме при дизайне auth-модели API, чтобы не пришлось ретрофитить.

---

### 2.6 XLS-30 Automated Market Maker

**Статус:** Final, активирован.

**Что делает:** native AMM, weighted geometric mean (сейчас ограничен 50/50). LP-токены — настоящие XRPL-токены. Fee управляется голосованием LP (до 8 активных vote slots), 0–1000 bps. Отличительная фича: 24-часовой continuous auction slot, в котором арбитражёры могут торговаться за discount-доступ (1/10 стандартного fee), bid идёт LP-холдерам.

**Полезность для нас:** уже покрыто в `project_xrpl_amm_viability.md` — пять блокеров делают его непригодным как наш liquidity engine. Кратко: spot а не derivative, неправильная форма кривой для perp (constant-product даёт бесконечный slippage при низком TVL), settlement collision с нашей off-ledger моделью, низкий TVL на релевантных парах, нет native USD на XRPL. Возможное niche применение: читать его mid-price как один сигнал в нашем oracle, не более.

**Вердикт:** не конкуренция, не инфраструктура. Уже проанализировано в другом месте.

---

### 2.7 XLS-66 Lending Protocol

**Статус:** Draft (2026-01-14). Зависит от XLS-65 (Vault) и XLS-64.

**Что делает:** off-chain-underwritten, fixed-term loans. Новые ledger objects `LoanBroker` (тип 0x0088) и `Loan` (тип 0x0089). Процентные ставки в 1/10 bps с четырьмя тирами (base / late / close / overpayment). First-Loss Capital pool абсорбирует начальные default'ы. Нет автоматической ликвидации, нет leverage, нет интеграции с oracles.

**Полезность для нас:** *ноль*. Форма неправильная по каждой оси, которая нам важна:
- Loans fixed-term, не perpetual.
- Нет автоматической ликвидации.
- Нет leverage / margin.
- Нет oracle / mark-to-market.
- Risk assessment явно off-chain.

Нет пути от XLS-66 к perp margin engine. Автор спеки сознательно выбрал другое design space (institutional credit origination, не DeFi leverage).

**Вердикт:** игнорировать. Полезный кусок XRPL DeFi мебели, но не для нас.

---

### 2.8 XLS-65 Single Asset Vault

**Статус:** Draft (2025-11-17). Требует XLS-33 (MPT).

**Что делает:** vault, держащий один asset (XRP / IOU / MPT) и эмитирующий share-токены (MPT от pseudo-account vault'а). Двухшаговый deposit/withdraw с rounding'ом, спроектированным для предотвращения арбитража нереализованных убытков. Public или domain-permissioned. **Нет timelocks, нет withdrawal queues** — first-come, first-served. Owner не может lock out shareholders. Yield идёт от внешних протоколов, не от самого vault; tracking долга — ответственность потребляющего протокола.

**Полезность для нас:**

- **Как LP-контейнер для MM-стороны** (кто бы это ни делал, если post-hackathon план Тома пойдёт по LP-маршруту): возможно. Vault держит XRP или RLUSD, эмитит shares, наш orchestrator занимает у него как протокольная "MM treasury". External debt tracking совпадает с нашей enclave-managed моделью.
- **Caveat — first-come-first-served withdrawals плохи для трейдингового vault.** Когда MM-сторона теряет деньги, withdrawals будут гонкой, и поздние shareholders съедят убытки. Perp MM vault реально требует withdrawal delays / cooldowns / share-price-at-T+N семантику. XLS-65 этого не предоставляет.
- **Vs самописное:** для hackathon-grade продукта существующий direct-orchestrator-managed MM капитал проще. XLS-65 начинает окупаться только когда захотим third-party LP.

**Вердикт:** держать в уме для фазы LP-onboarding (опция (c) из AMM viability memo). Не сейчас. Если/когда пойдём этим путём — придётся либо принять FCFS withdrawal limitation, либо обернуть vault в свою queue-логику.

---

### 2.9 XLS-81 Permissioned DEX

**Статус:** Final.

**Что делает:** добавляет поле `DomainID` к Offer'ам и Payment'ам native DEX, сегрегируя orderbooks по domain. Permissioned offer может пересекаться только с другим permissioned offer в том же domain; не может пересекаться с открытыми offers и наоборот. Построен на XLS-80 + XLS-70.

**Полезность для нас:** прямой пользы нет. Наш matching off-ledger внутри энклейва, не на native DEX. Мы вообще не постим offers в native книгу (кроме cross-pair hedge legs, которые в любом случае были бы открытыми offers). Упоминаю для контекста: если когда-нибудь захотим "KYC-only orderbook", можем либо построить его сами поверх XLS-70/80, либо постить offers в permissioned native domain — второй подход теряет наше atomic margin enforcement, так что первый правильный.

**Вердикт:** игнорировать для matching. Прочитать только для контекста.

---

### 2.10 XLS-62 Options

**Статус:** Stagnant (последнее изменение апрель 2025).

**Что делает:** physically-settled call/put опционы. Sellers лочат collateral. Strike и expiration задаются участниками. Нет oracle dependency.

**Полезность для нас:** никакой. Stagnant означает отсутствие momentum. Если когда-нибудь зашипится — будет complementary, не конкурирующим: опционы и перпы обслуживают разные стратегии.

**Вердикт:** игнорировать.

---

### 2.11 XLS-101 Smart Contracts

**Статус:** Draft / Proposal. Создан 2025-07-28 Mayukha Vadari. Спека сама описывает себя как "достаточно ранний draft" с TODO-шами по тексту.

**Что добавляет — три новых ledger object типа:**
- **`ContractSource`** — хранит WASM-байткод с reference counting, чтобы одинаковый код в разных деплоях не множил storage cost.
- **`Contract`** — развёрнутый инстанс, живёт на собственном pseudo-account, держит owner, hash кода и instance parameters.
- **`ContractData`** — *постоянное хранилище* для contract state. Поддерживает и contract-level, и per-user данные. Это полноценная state model, а не 4KB scratch field, как у XLS-100 escrows.

**Шесть новых tx-типов:** `ContractCreate`, `ContractCall`, `ContractModify`, `ContractDelete`, `ContractUserDelete`, `ContractClawback`. Плюс два RPC: `contract_info`, `event_history`.

**Два изменения, которые меняют наше пространство компромиссов:**

1. **Transaction emission — ДА.** Контракт может submit'ить свои собственные XRPL-транзакции через свой pseudo-account. Это единственная capability, которую XLS-100 явно запрещает, а XLS-101 явно разрешает.

2. **Persistent contract state — ДА.** За пределами крошечного data-field. `ContractData` спроектирован как настоящее state-хранилище контракта, не как scratch buffer.

Вместе эти два изменения означают, что в эпоху XLS-101 perp DEX, выраженный как on-chain contract logic, перестаёт быть структурно невозможным. Контракт мог бы держать позиции в per-user `ContractData`, принимать депозиты через стандартные payments, обрабатывать ордера через `ContractCall` и эмитить settlement-транзакции через свой pseudo-account. Полностью-on-XRPL дизайн, который мы исключили в `project_xrpl_amm_viability.md`, перестал бы быть исключённым *из-за отсутствующих примитивов*.

**Почему практическое пространство почти не сдвигается:**

- **Латентность.** Консенсус XRPL — 3–5 секунд на ledger close. Каждый ордер, каждый match, каждый cancel становится ledger-транзакцией. CLOB-перпы для любого серьёзного трейдера требуют миллисекундных fills, sub-second cancel-and-replace, и узких maker-спредов, которые зависят от возможности обновлять котировки быстрее следующего рыночного движения. Это свойство латентности консенсуса, не VM, и никакое XLS-предложение этот гэп не закрывает. Тот on-chain perp DEX, который можно построить на XLS-101, структурно — 3-секундный tick-batch auction, не CLOB.

- **No directory iteration.** XLS-101 наследует от XLS-102 дизайн, что доступ к ledger должен идти через bounded keylets — никакого walk'инга произвольных directories. Построение orderbook (который фундаментально — итерация по отсортированной directory) становится эргономической проблемой. Решаемо, но примитив не для этого.

- **No background scheduling.** Контракты выполняются только при вызове через `ContractCall`. Ликвидации и funding-rate тики требуют внешнего poker'а. Та же ограниченность, что мешает XLS-100, мешает XLS-101 — нужен permissionless keeper bot, и нужно дизайнить fees, чтобы его компенсировать.

- **Read-only ledger access из собственного code path контракта.** Модификации состояния происходят через эмитированные транзакции, которые являются обычными XRPL-транзакциями и должны идти через consensus pipeline. Так что даже внутри одного `ContractCall` нельзя атомарно прочитать-изменить-записать несколько ledger entries — вы в той же транзакционной модели, что и обычный transaction submitter.

- **Gas-экономика неизвестна.** Спека говорит, что лимиты и fees будут UNL-votable. Perp engine, трогающий много state slots на fill (mark price update, position update, margin update, fee accrual, funding accrual), будет дорогим. Пока нет опубликованной gas-таблицы — вопрос "конкурентоспособно ли это по цене с off-ledger" даже не может быть оценён.

- **Статус: ранний Draft.** XLS-100 в Draft и предположительно впереди XLS-101 в очереди. Реалистичный временной горизонт до активации mainnet — годы, не кварталы.

**Что это означает для существующих рекомендаций:**

Преимущество "TEE-backed off-ledger CLOB" из раздела 4 остаётся реальным, но формулировка должна быть честной: дело не в том, что другие подходы *невозможны*, а в том, что другие подходы *latency-bound к ~3с тикам и cost-bound по per-fill ledger fees*. Для трейдеров деривативами, ожидающих миллисекундное execution и узкие спреды, эти ограничения не "минорные неудобства", а disqualifying. Так что наше преимущество — "единственный дизайн, дающий trader-grade execution semantics на XRPL", а не "единственный возможный дизайн".

Преимущество "First-mover для перпов" не меняется — XLS-101 в годах, и даже когда зашипится, наиболее вероятные первые продукты — медленные batch-auction примитивы, не CLOB-перпы.

**Вердикт:** следить пристально, перечитывать раз в полгода. Если опубликованная gas-таблица сделает per-fill стоимость конкурентоспособной (почти наверняка не сделает), или если будущее XLS-предложение добавит что-то напоминающее sub-ledger-close execution scheduling (почти наверняка не добавит) — пересмотреть. Иначе вердикт не меняется: TEE off-ledger CLOB — правильный дизайн на ближайшие несколько лет.

### 2.12 XLS-102 WASM VM

**Статус:** Draft. Execution substrate, на котором работают и XLS-100 Smart Escrows, и XLS-101 Smart Contracts.

**Что устанавливает:** детерминированный Wasmi-based execution layer с ~70 host functions, gas metering по instructions / memory / host calls, лимиты размера кода и computation, voted UNL'ом. Read-only ledger access, нет traversal произвольных directories, нет temporal scheduling (выполняется только при обработке транзакций), модификации идут через ту calling proposal (XLS-100 или XLS-101), которая их определяет.

**Вердикт:** foundational, без независимого action item — его влияние полностью покрывается тем, что XLS-100 и XLS-101 с ним делают.

---

## 3. Конкурентный ландшафт на XRPL

Честное резюме: **на XRPL сегодня нет ни одного perp DEX, и ничто в XLS pipeline на него не указывает.**

| Предложение | Категория | Конкурирует с нами? | Почему / почему нет |
|---|---|---|---|
| XLS-30 AMM | Спот AMM | Нет | Только спот. Нет leverage, нет funding, нет ликвидаций. |
| XLS-62 Options | Деривативы | Нет | Стагнирует. Другой продукт. |
| XLS-66 Lending | Кредит | Нет | Fixed-term, нет автоматизации, нет leverage. |
| XLS-81 Permissioned DEX | Спот CLOB | Нет | Только спот. Наш — off-ledger и perpetual. |
| XLS-100 Smart Escrows | Programmable settlement | Adjacent | Может в принципе выразить одну oracle-settled ставку, но не CLOB perp venue. |

**Off-XRPL конкуренция** (здесь не каталогизирована подробно): стандартные perp DEX'ы на других чейнах — dYdX (Cosmos app-chain), Hyperliquid (свой L1), GMX (Arbitrum), Drift (Solana), Vertex (Arbitrum + свой sequencer). Никого из них на XRPL нет. Ближайший архитектурный родственник — Hyperliquid (off-chain matching, on-chain settlement, кастомная инфраструктура для low-latency matching) — но они работают на собственном L1 с консенсусом, заточенным под трейдинг, а мы получаем XRPL settlement и asset issuance бесплатно, не платя за bootstrap чейна.

---

## 4. Наши преимущества, прямо

Это то, что у нас есть и чего не предоставляет ни одно XLS-предложение и ни один on-XRPL конкурент сегодня:

1. **Реальная perpetual механика** — funding rate, mark price, isolated/cross margin, автоматическая ликвидация. Ни одно из этого не нативно на XRPL и ни одно из активированных предложений этого не добавляет. Ближайшее — XLS-100 + XLS-47 + XLS-85, сшитые вместе, и даже это даёт single-position oracle-settled структуры, не perp venue. XLS-101 Smart Contracts (всё ещё Draft, годы впереди) сделал бы fully-on-chain perp дизайн в принципе *выразимым*, но не *практичным* — см. пункт 2.

2. **TEE-backed off-ledger CLOB с trader-grade execution semantics.** Sub-millisecond matching, реальная maker/taker динамика, atomic margin enforcement внутри энклейва. Честная формулировка: дело не в том, что другие подходы невозможны на XRPL, а в том, что они latency-bound консенсусом ledger (3–5 секунд на close) и cost-bound per-fill ledger fees. Для трейдеров деривативами, ожидающих миллисекундное execution и узкие maker-спреды, эти ограничения disqualifying. Наш дизайн — единственный на XRPL, дающий trader-grade execution semantics — и это свойство structural, не временное лидерство.

3. **Распределённая кастодия 2-of-3 (XRPL SignerList — независимые ECDSA-подписи операторов, не агрегированная пороговая подпись).** Лучшая модель доверия, чем issuer-controlled escrow / vault модель, которую предполагает каталог XLS. Ни один оператор не может подписать вредоносный settlement, ни один оператор не может rug'нуть протокол. Документ deployment-procedure, который мы только что написали, расширяет это свойство на deploy path.

4. **XRPL settlement без on-ledger-per-fill стоимости.** Batched settlement (XLS-56, как только подключим) даёт нам лучшее из двух миров: XRPL finality и asset ecosystem, off-ledger trading скорость и экономику. Ни один on-XRPL конкурент не разделяет эти слои так чисто.

5. **Asset agnosticism через MPT/IOU.** После активации XLS-85 token escrow (она уже состоялась 2026-02-12), и после того, как поддержка токенов появится в smart escrows, мы сможем collateralise позиции в любом RLUSD-class IOU или MPT, не только в XRP. Наш энклейв уже говорит на словаре IOU/MPT.

6. **First-mover для перпов в сети.** Не техническое преимущество, но стоит сказать: нет инкумбента, которого надо вытеснять. Что бы мы ни зашипили — это reference implementation по умолчанию.

Чего у нас *нет* (честная версия):
- Аудированного продакшен-деплоя мультиподпись + enclave стека — это работа deploy procedure в `deployment-procedure.md`, сейчас draft.
- Ликвидности. Day-one ликвидность — нерешённая проблема вне зависимости от выбранной архитектуры. План Тома (post-hackathon vAMM + arb bot) — рабочий ответ; этот XLS обзор не меняет этих расчётов.
- Network effects. Вся XRPL DeFi экосистема маленькая; это проблема market-making и BD, не техническая.

---

## 5. Конкретные рекомендации по интеграции

В грубом порядке impact-per-effort:

1. **XLS-56 Batch — подключить для settlement emission.** Когда post-hackathon работа по периодическому settlement из энклейва в XRPL начнётся — строить на `Batch ALLORNOTHING` с первого дня. Не ретрофитить. Усилие: малое (это новый XRPL tx-тип, который наш signer должен поддержать). Ценность: большая (атомарный multi-leg settlement — это разница между "иногда рваные settlement'ы, требующие сверки" и "транзакционно чистые").

2. **XLS-47 Oracles — публиковать нашу mark price.** На каждом funding interval энклейв подписывает и submit'ит `OracleSet`, записывая текущую mark price в protocol-owned `PriceOracle`. Public, timestamped audit trail. Стоимость: тривиальная. Ценность: дешёвая страховка на случай будущих споров "какую mark price вы использовали в момент T?".

3. **XLS-47 Oracles — читать внешние источники.** Когда другие участники (Pyth on XRPL, RippleX feeds, Band) публикуют XLS-47 oracles по нашим торгуемым активам — подключить их в наш mark-price aggregator вместе с CEX-фидом. Использовать `get_aggregate_price` с trimmed-mean. Усилие: умеренное (oracle adapter внутри энклейва). Ценность: умеренная (устойчивость к single-source отказам).

4. **XLS-70 + XLS-80 — спроектировать API auth так, чтобы credential gating вставлялся чисто.** Не реализовывать KYC сейчас. Но не загонять себя в угол: при дизайне `/v1/auth` и user identity model оставить hook для "этот аккаунт в domain X", чтобы добавление регулируемого tier позже было одним новым модулем, а не переписыванием.

5. **XLS-85 Token Escrow — держать на watch list, строить только если use case заставит.** User-side "deposit с safety hatch" паттерн возможен, но не load-bearing; по умолчанию — прямые payments + enclave bookkeeping, если только кто-то (Том, инвестор, регулятор) не назовёт конкретную причину добавить escrow indirection.

6. **XLS-65 Vault — пересмотреть, когда LP onboarding станет реальной продуктовой линией** (т.е. post-vAMM, когда вопрос "откуда MM капитал" будет требовать публичного ответа). Не сейчас.

7. **XLS-100 Smart Escrows — перечитывать раз в полгода.** Особенно когда token escrow появится внутри smart escrows, и особенно когда XLS-101 прояснит, на столе ли transaction emission или persistent contract state. Любое из этих изменений меняет архитектурное trade space.

---

## 6. Открытые вопросы для будущих проходов

1. **XLS-101 published gas table.** Сейчас спека говорит, что лимиты и fees UNL-votable, но не коммитится к числам. Когда появится draft gas table — пересчитать стоимость per-fill state update под XLS-101 и проверить заявление "структурно слишком дорого для CLOB perps" реальными числами, а не рассуждениями. ~~Не удалось получить XLS-101 в прошлом проходе~~ — сделано в этой ревизии, см. §2.11.

2. **XLS-94 Dynamic MPT и XLS-96 Confidential MPT** — оба касаются модели asset, в которой мы collateralise. Пропущены в этом проходе; стоит one-paragraph review каждый.

3. **XLS-74 Account Permissions и XLS-75 Permission Delegation** — могут быть релевантны для operator/treasury модели, особенно delegation. Стоит follow-up.

4. **Есть ли XLS-предложение для funding-rate-style периодических переводов.** Не нашёл. Если существует — оно прямо повлияет на то, как мы моделируем funding payment settlement on-ledger.

5. **Что делают другие команды с XLS-100 или с прототипами XLS-101.** Если кто-то опубликует smart-escrow-based или smart-contract-based options/futures продукт — это ближайшее к peer в пространстве, и нам стоит прочитать их код.

6. **XLS-101 sub-ledger-close execution scheduling.** Если будущая ревизия XLS-101 (или отдельное предложение) когда-нибудь введёт что-то напоминающее intra-ledger execution, scheduled callbacks или sub-second contract triggering — trade space значительно меняется. Сейчас такого предложения нет. Стоит перепроверять ежегодно.

---

## Приложение A — Snapshot статусов всех изученных XLS

| XLS | Название | Статус | Релевантность для нас |
|---|---|---|---|
| 30 | AMM | Final, активирован | Уже проанализирован (`project_xrpl_amm_viability.md`) — не подходит как наш liquidity engine |
| 33 | Multi-Purpose Tokens | Final | Asset model для collateral; интегрируется по необходимости |
| 47 | Price Oracles | Final | **Интегрировать** как один из входов + публиковать mark price |
| 56 | Batch | Final, 2026-02 | **Интегрировать** для атомарной эмиссии settlement |
| 62 | Options | Stagnant | Игнорировать |
| 65 | Single Asset Vault | Draft | Watch — возможный LP-контейнер позже |
| 66 | Lending Protocol | Draft | Игнорировать — wrong shape |
| 70 | Credentials | Final | Интегрировать, когда понадобится KYC tier |
| 80 | Permissioned Domains | Final | Интегрировать, когда понадобится KYC tier |
| 81 | Permissioned DEX | Final | Игнорировать — только спот |
| 85 | Token Escrow | Final, активирован 2026-02-12 | Watch — не load-bearing для нас |
| 100 | Smart Escrows | Draft | Watch — перечитывать раз в 6мес |
| 101 | Smart Contracts | Draft (early Proposal, 2025-07) | Watch — добавляет tx emission + persistent state, но латентность/экономика всё равно исключают CLOB-перпы |
| 102 | WASM VM | Draft | Substrate только для 100/101 |

## Приложение B — Кросс-ссылки на project memory

- `project_xrpl_amm_viability.md` — покрывает XLS-30 AMM и vAMM design space; этот документ комплементарен, не заменяет.
- `project_post_hackathon_architecture.md` — план Тома; рекомендации здесь (XLS-56, XLS-47) предназначены вписаться в этот план, не заменить его.
- `feedback_closes_must_route_clob.md` — ограничение close-routing; именно поэтому XLS-85 не может быть нашим settlement primitive (third-party finish был бы единственной приемлемой моделью, а XLS-85 этого не разрешает).
- `project_perp_dex_xrpl.md` — родительский feasibility memo.
