# Perp DEX на Bitcoin — архитектурная feasibility

**Аудитория:** техническая команда и инвестор.
**Статус:** feasibility study, а не commitment. Bilingual: английская версия в `btc-perp-dex-feasibility.md`.
**Контекст:** этот документ существует потому, что вопрос естественно вытекает из истории самого проекта. Enclave signing stack в `xrpl-perp-dex-enclave` (форк более раннего `SGX_project`) уже прошёл путь от простого ECDSA до полного Bitcoin-grade signing surface — BIP340 Schnorr, MuSig2, FROST threshold signing — *до* того, как мы остановились на XRPL как первой цели. Поэтому естественный следующий вопрос: раз signing primitives, которые мы уже shipped'им, — это ровно те, которые использует Bitcoin, можно ли ту же архитектуру направить на Bitcoin и получить настоящий perp DEX?

Короткий ответ — **да, архитектура переносится чисто, и это bounded инженерный проект, а не новая платформа**. Длинный ответ ниже, написан честно про обе стороны: и возможность, и работу.

---

## 1. Executive summary (для инвестора)

- **Signing stack, который мы построили, уже Bitcoin-native.** Энклейв реализует ECDSA, BIP340 Schnorr, MuSig2 и FROST threshold signing на `libsecp256k1`. Это ровно те типы подписей, которые Bitcoin реально использует, а не их приближения. Taproot-расходы из FROST 2-of-3 vault'а работают на Bitcoin *сегодня* с тем кодом, который у нас уже есть.
- **Нечейн-специфичные части продукта переносятся напрямую.** Margin engine, position state machine, liquidation loop, funding rate, CLOB matching, модель threshold-кастоди 2-of-3 (сегодня XRPL SignerList ECDSA, на Bitcoin — FROST Schnorr), SGX enclave deployment, release pipeline — всё это chain-agnostic. Переносится *модель* кастоди, но не *механизм* подписи — подпись FROST будет новой работой, а не портированием. Около 80% инженерной работы, уже сделанной для XRPL-версии, переиспользуется как есть.
- **Chain-специфичная работа bounded и хорошо понятна.** Deposit/withdraw plumbing другой (Bitcoin транзакции вместо XRPL, с Taproot multisig vaults и confirmation-based crediting'ом), а дизайн залога и сеттлмента должен быть explicit про особенности BTC (10-минутные блоки, нет нативного стейблкоина, переменные fee, реорги). Ничто из этого не research; всё имеет известные решения в production где-то ещё.
- **Бизнес-возможность — крупнейший liquidity pool в крипте.** BTC примерно на порядок больше любого альткоина по market cap и дневному объёму деривативов. L1-нативная децентрализованная BTC perp-DEX категория по сути пуста — BitMEX и Deribit централизованы, GMX и Hyperliquid живут на L2 и используют wrapped BTC, а wrapped-BTC-на-Ethereum несёт bridge risk, который BTC-сообщество не любит. TEE-backed, FROST-custodied, L1-settled BTC perp DEX — это product category, которой по сути ещё не существует.
- **Тайминг.** Это *не* замена XRPL-трека. Рекомендация: закончить XRPL, доказать архитектуру в production, *потом* форкнуть BTC-версию — а пока держать codebase chain-agnostic, чтобы форк был 2-3 месяца работы, а не рерайт.
- **Опциональность за пределами BTC.** Любая цепь, которая подписывает транзакции через `secp256k1` (Bitcoin, Bitcoin Cash, Litecoin, Dogecoin, Zcash transparent addresses, Ethereum tx envelope), может быть хостом той же архитектуры. Bitcoin просто наиболее стратегически ценная первая цель после XRPL.

---

## 2. Почему вопрос вытекает из истории самого проекта

Текущий репо `xrpl-perp-dex-enclave` — это форк `SGX_project`, который начинал жизнь как general-purpose "signing-as-a-service в SGX-энклейве", нацеленный на Ethereum-style ECDSA. В ходе своей эволюции (как signer для разных чейнов, а затем по мере движения к BTC-семейству) энклейв аккумулировал следующий capability stack:

| Возможность | Статус в энклейве сегодня | Что нужно Bitcoin'у |
|---|---|---|
| secp256k1 арифметика (`libsecp256k1`) | Линкуется, используется везде | Та же библиотека, что использует Bitcoin Core |
| ECDSA (pre-Taproot P2PKH/P2WPKH) | Реализован | Используется в pre-Taproot Bitcoin адресах |
| **BIP340 Schnorr** | Реализован | Используется в Taproot P2TR расходах |
| **MuSig2** (2-party и n-party key aggregation) | Реализован, session management внутри энклейва | Позволяет multi-party Taproot расходы, выдающие одну Schnorr подпись |
| **FROST** threshold Schnorr (2-of-3) | Реализован, DKG поддерживается | Позволяет 2-of-3 Taproot custody, который on-chain выглядит как single-sig Schnorr расход |
| SGX sealing ключевого материала | Реализован | Одинаково для любой цепи |
| FROST ceremony между 3 независимыми операторами | Реализован | Одинаково для любой цепи |

То есть когда мы пивотнули от "generic BTC-family signer" к "perp DEX на XRPL", мы *не убрали* Bitcoin capability — мы построили perp-DEX слой поверх неё. XRPL-специфичные куски — это формат транзакции (STObject сериализация, SHA-512Half pre-hash, DER-encoded ECDSA) и on-chain settlement path (RLUSD escrow, XRPL testnet client, ledger monitoring). Они сидят поверх signing core, который строго является **надмножеством** того, что нужно Bitcoin'у.

Это не retrofit-аргумент. Это утверждение про то, что код *уже делает* сегодня.

---

## 3. Что портируется из XRPL-версии практически без изменений

Следующие слои chain-agnostic и были бы переиспользованы напрямую в BTC-версии:

**Энклейв, подпись, custody:**
- Весь `libsecp256k1` stack внутри энклейва (ECDSA, Schnorr, MuSig2, FROST).
- FROST 2-of-3 DKG и threshold signing ceremony.
- SGX sealing подписных шаров и enclave state (вся наша Часть 6 migration story из `sgx-enclave-capabilities-and-limits.md`).
- Remote attestation через DCAP, release ceremony из `deployment-procedure.md` с YubiKey-gated 2-of-3 signing, воспроизводимые сборки, per-node deploy agents.
- Side-channel posture, CPUSVN handling, threat model.

**Торговля и риск:**
- `PerpState` — position state machine.
- Margin engine, включая fixed-point арифметику и `fp_mul` / `fp_div` helpers.
- Open / close / liquidate flow.
- Применение funding rate.
- Liquidation scanning loop.
- Insurance fund accounting.
- CLOB matching семантика, включая `reduce_only` IOC для закрытий позиций (per `feedback_closes_must_route_clob.md`).

**Инфраструктура и operations:**
- Orchestrator архитектура (price feed pipeline, deposit monitor loop, liquidation loop, state-save cadence).
- REST / gRPC API surface для внешних клиентов.
- Мониторинг, алертинг, logging дисциплина.
- Multi-operator deployment модель (3 независимых персоны, нет cross-server доступа, FROST-подписанные релизы).
- План failure-mode тестирования.

Щедро оценивая, это 80% работы, которая ушла в XRPL-версию. Ничто из этого не надо переписывать или даже существенно модифицировать для BTC-цели — chain-специфичный код живёт тонким слоем сверху (конструирование транзакций и monitoring) и снизу (оркестровка к другому chain-клиенту).

---

## 4. Что действительно по-другому на Bitcoin

Эта секция существует, чтобы честно говорить про chain-специфичный инжиниринг, который требует BTC-версия. Ничто из этих пунктов не research; всё имеет production-grade reference implementations где-то ещё. Но это не ноль и это должно быть заложено в бюджет.

### 4.1 Задержка депозита

XRPL сеттлится за ~3 секунды и даёт детерминированную finality. Bitcoin блоки приходят каждые ~10 минут и несут вероятностную finality — стандартное правило 1 confirmation для low-value credit, 3-6 confirmations для сумм побольше. У этого прямые UX-последствия:

- Пользователь не может начать торговать на Bitcoin в момент подписания депозитной транзакции. Надо подождать как минимум до того, как tx попадёт в блок, а для значимых сумм — пока она не закопается под несколько дополнительных блоков.
- Оркестратор должен моделировать **pending balance** отдельно от **confirmed margin**, и margin engine энклейва должен быть уведомлён, когда pending-депозит становится confirmed.
- Реорги случаются (редко глубже 1-2 блоков, но случаются). Любой credit, выданный на низкой confirmation, должен быть обратимым, если содержащий блок orphan'ится. Это стандартная инженерная задача со стандартным решением (отслеживать каждый депозит до фиксированного числа confirmations, поддерживать reorg-depth инвариант, откатывать credits на deep reorg — ровно так же, как это делает любой custodial BTC service).

**Митигации, которые существуют и работают:**
- Credit на N=1 conf с консервативным haircut и без margin для открытых позиций; полный margin на N=6.
- Принимать Lightning Network депозиты в Lightning-ноду, управляемую оркестратором; Lightning-платежи мгновенны и финальны, и оркестратор кредитует margin в момент, когда HTLC resolving'нется. Это добавляет Lightning-ноду в operational surface, но это хорошо понятный компонент.
- Принимать RBF-disabled, confirmed-in-mempool депозиты с консервативными лимитами для очень маленьких аккаунтов.

### 4.2 Деноминация залога

XRPL даёт нам RLUSD — регулируемый USD стейблкоин on-chain. У Bitcoin нет нативного стейблкоина. Поэтому perp DEX должен сделать явный выбор, как он деноминирует позиции и залог:

- **Inverse контракты (BTC-деноминированные).** Контракты оценены в USD, но PnL выплачивается в BTC. Это оригинальная BitMEX XBTUSD модель, и она работает. Пользователи депонируют BTC, открывают long или short против USD-цены, их PnL сеттлится в BTC при закрытии. Залог никогда не приходится конвертировать. Maintenance margin выражен как процент от notional в BTC terms. Ликвидация использует mark price в USD и выплачивает в BTC. Это battle-tested в production в multi-billion-dollar масштабе на BitMEX, Deribit и OKX.
- **Linear (USD-деноминированные) контракты с BTC залогом в mark-to-market.** Пользователи депонируют BTC, энклейв непрерывно переоценивает BTC-залог по текущей mark price, margin выражен в USD, PnL в USD, вывод выплачивается в BTC по текущей цене. Это даёт пользователям более знакомый PnL experience, но связывает solvency залога с волатильностью BTC цены и усложняет ликвидации. Тоже production-proven (большинство современных ритейл perp платформ делают это).
- **Гибрид: позволить пользователю выбирать.** Стандартное предложение большинства современных perp venues.

Для первой BTC-версии inverse-contract путь — это выбор с наименьшим риском: математически проще, collateral-сторона системы становится стабильной в BTC terms, и избегает необходимости доверять внешнему USD-оракулу для solvency-решений.

### 4.3 On-chain custody примитив

На XRPL мы используем нативный escrow object. На Bitcoin эквивалент — это **Taproot P2TR адрес, чей key-path spend — это FROST 2-of-3 агрегированная Schnorr подпись**. Это элегантно и важно для инвестор-стори:

- Адрес vault'а on-chain неотличим от single-signature P2TR адреса. Он дешёв в расходовании (одна Schnorr подпись), privacy-preserving (никто не может из цепи сказать, что это multisig), и полностью нативный Bitcoin — нет smart contract, нет sidechain, нет wrapper-токена.
- 2-of-3 FROST signing ceremony происходит внутри наших трёх энклейвов, переиспользуя машинерию DKG и переноса долей, которую кластер уже гоняет и мигрирует через смены MRENCLAVE. **На XRPL-версии это работает иначе** — XRPL-сеттлмент подписывается кворумом SignerList независимыми ECDSA, так что сама церемония FROST — новая работа. On-chain вывод — это одна 64-байтная Schnorr подпись.
- Script-path spend может быть определён как fallback (например, timelocked single-operator recovery после длительной недоступности кворума) через Tapscript.
- Это *больше* privacy, чем что-либо доступное на XRPL-стороне, потому что на XRPL multisig-адрес визуально виден как multisig.

### 4.4 Обработка реоргов

Покрыто выше в §4.1. Небольшие реорги (1-2 блока) случаются на Bitcoin mainnet иногда. Глубокие реорги (глубже 6 блоков) чрезвычайно редки (измеряются годами). Оркестратор должен поддерживать confirmations-индекс для каждого депозита и вывода и откатывать state, если закредитованная tx убрана из best chain. Margin engine энклейва должен экспонировать явный `ecall_perp_revert_deposit(user_id, tx_hash)`, который откатывает ранее закредитованный баланс, защищённый той же duplicate-detection машинерией, что и forward-path. Это стандартный BTC-service инжиниринг и это не novel.

### 4.5 Вариативность fee и batching выводов

Bitcoin transaction fees варьируются от фактически бесплатных до десятков долларов в зависимости от условий mempool'а. Для пользовательских выводов:

- Оркестратор должен батчить выводы, когда возможно — одна Bitcoin транзакция может выплатить сотням получателей, амортизируя fee.
- Пользователи должны видеть текущий оценочный fee перед подтверждением вывода, и иметь опцию подождать более низких fee.
- Vault должен поддерживать fee-reserve UTXO, чтобы выводы никогда не блокировались на нехватке средств для fee.

Опять же, стандартно. Lightning-выводы (§4.1) также опция для маленьких сумм и избегают on-chain fee полностью.

### 4.6 Нет on-chain programmability

XRPL даёт нам нативный escrow, нативные multi-token балансы, скоро batch transactions и (возможно) WASM-контракты. Bitcoin даёт нам Taproot scripts, HTLCs и timelocks — и ничего больше. Это значит, что **энклейв ещё более центральный в BTC-версии, чем в XRPL-версии**. Bitcoin используется чисто как settlement rail; весь state, весь matching, весь risk management живёт в энклейве. Это, кстати, ровно так, как BitMEX работает внутри (их "Bitcoin account" — это vault, всё остальное — database state), и это чистая модель. BTC-сообщество уже комфортно с этим паттерном в форме Lightning и Fedimint.

---

## 5. Архитектурный набросок BTC-версии

Собрав всё вышесказанное, BTC perp DEX, использующий нашу архитектуру, выглядит так.

### 5.1 Flow депозита

1. Пользователь генерирует depozit-адрес, запрашивая его у оркестратора. Оркестратор выводит per-user depozit-адрес через BIP32 derivation под FROST-агрегированным xpub'ом (или, эквивалентно, использует один vault-адрес с уникальным OP_RETURN тегом на пользователя). Энклейв хранит маппинг `(user_id → deposit_tag)`.
2. Пользователь отправляет BTC на depozit-адрес. Оркестратор мониторит mempool и блокчейн через Bitcoin Core (или Electrum RPC / Esplora).
3. На 1 confirmation оркестратор зовёт `ecall_perp_deposit_credit_pending(user_id, amount, tx_hash, confirmations)` — энклейв кредитует *pending* баланс, который может поддерживать открытые позиции с консервативным haircut, но не может быть выведен.
4. На N confirmations (конфигурируемо, default 6) оркестратор зовёт `ecall_perp_deposit_confirm(tx_hash)`, и pending-баланс становится полностью confirmed margin.
5. Если в любой момент оркестратор обнаруживает, что содержащий блок был orphan'нут и tx больше не в best chain на каком-то более низком confirmation-счёте, он зовёт `ecall_perp_deposit_revert(tx_hash)`, и энклейв откатывает credit. Любые открытые позиции, профинансированные откаченным credit'ом, force-close'аются.

### 5.2 Торговля

Идентично XRPL-версии. Margin engine, CLOB, liquidation loop, funding loop и REST API не знают, в какую цепь система сеттлится. `PerpState` — та же структура. Это та часть порта, которая по-настоящему бесплатна.

### 5.3 Вывод

1. Пользователь submit'ит запрос на вывод, указывая сумму, destination-адрес и опциональный максимальный fee.
2. Оркестратор зовёт `ecall_perp_withdraw_check_and_sign_btc(user_id, amount, dest_addr, fee_rate, tx_template, sig_out)`. Энклейв:
   - выполняет margin check (solvency после вывода),
   - верифицирует, что структура `tx_template` — это валидный P2TR key-path spend из vault'а на `dest_addr`,
   - производит свой FROST share от Taproot Schnorr подписи.
3. Оркестратор агрегирует share'ы от 2-of-3 операторов, собирает финальную Schnorr подпись, броадкастит tx в Bitcoin сеть.
4. Энклейв атомарно дебетует user balance *до* release подписи (тот же TOCTOU-safe pattern, который мы используем на XRPL — check и sign в одном ecall'е).

### 5.4 Price feed

Внешний оракул (Pyth, Chainlink или self-run аггрегатор CEX-цен, подписываемый trusted feeder'ом). Feeder пушит цены в энклейв через `ecall_perp_update_price(mark_price, index_price, timestamp, feeder_sig)`. Энклейв валидирует подпись feeder'а и отказывается действовать на устаревших ценах. Идентичный паттерн XRPL-версии, тот же код.

### 5.5 Settlement market

Inverse контракты (XBTUSD-style) как первое предложение. Залог и PnL оба в BTC, price reference в USD. Linear USD-деноминированные markets добавляются позже, если есть спрос. Это тот же выбор, который делал каждый production BTC derivatives venue при запуске, и это никогда не было проблемой.

### 5.6 Chain client

Новый `btc_client` Python-модуль рядом с существующим `xrpl_client`, предоставляющий:
- `get_block(height)` и `get_tip_height()` для мониторинга блоков.
- `watch_address(addr)` для детекции депозитов.
- `estimate_fee_rate(target_blocks)` для sizing'а вывода.
- `broadcast_tx(raw_tx)` для settlement.
- `get_reorg_depth()` для безопасности.

Реализован против Bitcoin Core JSON-RPC для self-hosted, или против Electrum / Esplora для более лёгких setups. Это ~1000-1500 строк прямолинейного client-кода, значительно ниже инженерного усилия XRPL-клиента, который уже существует.

---

## 6. Конкурентный ландшафт

Кто сейчас предлагает перпетуальные фьючерсы на BTC, и где они архитектурно?

| Venue | Архитектура | BTC settlement | Custody | Децентрализован? |
|---|---|---|---|---|
| BitMEX | Централизованная биржа | L1 BTC депозиты/выводы | Один operator multisig | Нет |
| Deribit | Централизованная биржа | L1 BTC депозиты/выводы | Один operator custody | Нет |
| Binance / OKX / Bybit BTC perps | Централизованные биржи | L1 BTC депозиты/выводы | Один operator custody | Нет |
| dYdX v4 | Cosmos app-chain | Нет — только USDC | Validators | Частично (validator set) |
| GMX | Arbitrum smart contract | Wrapped BTC (WBTC) | Bridge custodians держат реальный BTC | Нет (bridge trust) |
| Hyperliquid | Custom L1 | Wrapped BTC через bridge | Validator set custody | Частично (validator set) |
| Synthetix perps | Optimism smart contract | Synthetic sBTC (нет реального BTC) | Нет — чисто синтетический | Да, но без реальной BTC экспозиции |
| Drift, Mango (Solana) | Solana smart contract | Wrapped BTC на Solana | Bridge custodians | Нет (bridge trust) |
| Lightning-based (experimental) | Разное | Lightning каналы | Разное | Варьируется, обычно малый масштаб |

Зазор, который это раскрывает, узкий и специфичный: **никто не запускает по-настоящему децентрализованный BTC perp DEX, который сеттлится напрямую в Bitcoin L1 с минимальным custody trust**. Каждый существующий продукт попадает в один из трёх failure modes с точки зрения BTC-пуриста:

1. Централизованная биржа с одним оператором, держащим user BTC (BitMEX, Deribit, Binance).
2. Децентрализованное matching, но wrapped BTC на другой цепи, то есть пользователи на самом деле держат WBTC, выпущенный кастодианом, который держит реальный BTC (GMX, Hyperliquid, Drift).
3. Синтетический BTC вообще без реальной BTC экспозиции (Synthetix).

Наша архитектура закрывает зазор: **matching происходит в TEE, custody — это 2-of-3 FROST с тремя независимыми операторами, settlement — это нативные Taproot-расходы напрямую на Bitcoin L1, и пользователи никогда не держат wrapped токен** (речь о предлагаемой здесь Bitcoin-ветке — живое XRPL-кастоди сегодня это кворум SignerList 2-of-3 ECDSA). Депозиты — реальный BTC. Выводы — реальный BTC. Энклейвы не могут двигать средства пользователей односторонне (FROST 2-of-3 требует кворума). Операторы не могут двигать средства пользователей без согласия энклейва (потому что они не держат полные подписные share'ы вне энклейва). Нет bridge, нет L2, нет синтетического актива.

Это **defensible position специально в BTC-сообществе**, которое заботится о self-custody, L1 settlement и избегании bridge-риска гораздо больше, чем Ethereum-сообщество. Это также — не случайно — тот же самый аргумент, который мы уже делаем для XRPL-версии: мы просто направляем его на цепь с самой большой и самой security-conscious user base.

---

## 7. Наш уникальный wedge

Positioning summary для инвестора:

- **L1-нативный settlement.** Реальный BTC внутрь, реальный BTC наружу, нет wrapping, нет bridge.
- **Минимальный custody trust.** 2-of-3 FROST через три энклейва, операемые тремя независимыми персонами (Bitcoin-ветка; на XRPL то же свойство достигается кворумом SignerList 2-of-3 ECDSA), каждый защищён hardware ключами и DCAP attestation. Ни один оператор не может двигать средства в одиночку; ни один энклейв не может двигать средства в одиночку; никакие две сговорившиеся стороны не могут двигать средства, если одна из них — это не энклейв, работающий аудированный код.
- **Аудируемое matching.** Matching и risk engine крутятся внутри SGX-энклейвов, чей бинарь воспроизводимо собран, а `MRENCLAVE` опубликован и attested. Пользователи могут криптографически верифицировать, что биржа запускает ровно тот код, который они думают, — гарантия, которую не даёт ни одна централизованная BTC-биржа.
- **Cutting-edge криптография shipped в production.** BIP340 Schnorr + FROST 2-of-3 Taproot vaults — это state-of-the-art Bitcoin custody, которое на сегодня почти ни один production venue реально не использует. Это технически привлекательно и press-worthy.
- **Знакомый продукт для BTC рынка.** Inverse XBTUSD-style perps — это нативная форма продукта, которую Bitcoin-пользователи уже знают по BitMEX. Это снижает market-education cost примерно до нуля.
- **Симметрия с нашим XRPL-треком.** Один codebase, одна команда, одна threat model, две цепи. Каждый rail усиливает доверие к другому.

---

## 8. Инженерный scope и реалистичный timeline

Предполагая, что XRPL-версия shipping'ится и стабильна, и что текущий codebase отрефакторен так, чтобы изолировать chain-специфичные concerns за тонкой границей (см. §11), BTC-порт имеет следующую примерную форму:

| Workstream | Усилие | Примечания |
|---|---|---|
| `btc_client` Python модуль (Bitcoin Core RPC, fee estimation, reorg tracking, address monitoring) | 3-4 недели | Прямолинейно, reference implementations существуют |
| FROST 2-of-3 Taproot key-path spend конструирование и signing flow | 2-3 недели | Signing primitives уже в энклейве; это склейка их с Bitcoin transaction serialization |
| Deposit / withdraw ecalls, специализированные под BTC (pending/confirmed states, reorg revert) | 2-3 недели | `ecall_perp_deposit_credit_pending`, `ecall_perp_deposit_confirm`, `ecall_perp_deposit_revert`, `ecall_perp_withdraw_check_and_sign_btc` |
| Orchestrator loops (deposit monitor, reorg handler, withdrawal batcher, fee estimator) | 3-4 недели | Зеркалят XRPL orchestrator |
| Inverse-contract математика в margin engine | 1-2 недели | Малое расширение `PerpState`, чтобы деноминировать позиции в BTC, а не предполагать stablecoin залог |
| Адаптация price feed signer'а (внешний oracle или self-run аггрегатор) | 1 неделя | Идентичный паттерн XRPL-версии |
| Lightning депозиты (опционально, может быть phase 2) | 3-4 недели | LND client, HTLC handling, instant-credit интеграция |
| End-to-end тестирование на Bitcoin signet и testnet | 2-3 недели | Стандартная pre-mainnet валидация |
| Внешний security audit | 4-6 недель | Параллельно разработке |
| Mainnet launch engineering, мониторинг, runbooks | 2-3 недели | |

**Итого, сериально, реалистично:** примерно **3 месяца** небольшой senior-команды для production-quality BTC запуска, плюс внешний аудит параллельно. Это настоящий проект, но это не research-проект — это инженерия против хорошо понятных Bitcoin primitives, использующая signing stack, который уже существует.

Этот бюджет **не включает**:
- Переписывание энклейва (его не надо переписывать).
- Переделывание SGX deployment pipeline (`deployment-procedure.md` применяется без изменений).
- Переделывание FROST ceremony или operator model.
- Построение matching или liquidation logic с нуля.

Оценка в 3 месяца кредибельна именно потому, что большая часть работы уже сделана.

---

## 9. Реальные риски и честные открытые вопросы

Эта секция существует, чтобы предотвратить типичный failure mode feasibility-документов — замалчивание вещей, которые могут реально укусить.

**Технические риски:**

1. **Deposit UX для маленьких, нетерпеливых пользователей.** Ждать 1-6 Bitcoin confirmations медленно по сравнению с XRPL или L2-опытом. Lightning митигирует это, но добавляет operational complexity. Без Lightning onboarding experience для "хочу попробовать прямо сейчас" пользователей на самом деле хуже, чем то, что они получают на централизованных биржах.

2. **Fee-шоки во время перегрузки.** Bitcoin fee spike (как случалось во время mempool перегрузок 2017, 2021 и 2024) может сделать выводы дорогими на день-два. Batching помогает, но не устраняет это. Пользователи жалуются.

3. **Reorg engineering должен быть правильным, а не приблизительно правильным.** Неправильно сделанный reorg handling в финансовой системе — это то, как исчезают деньги. Это зрелая инженерия, но всё равно её надо делать аккуратно, тестировать с синтетическими reorgs на signet'е и документировать failure modes. Это самый high-risk инженерный пункт во всём плане.

4. **Oracle trust assumption наследуется от XRPL-версии.** Price feed всё ещё oracle. Скомпрометированный или устаревший oracle может неправильно переоценить позиции и триггернуть неправильные ликвидации. Это не хуже на BTC, чем на XRPL, но и *не лучше* — мы не должны утверждать, что BTC решает нашу oracle проблему.

5. **BIP340 + FROST это state-of-the-art, но ещё не heavily production-tested.** Наша реализация FROST внутри энклейва — это тот же код, который мы запускаем для XRPL, поэтому уверенность в нём та же. Но на уровне Bitcoin scripting, Taproot key-path расходы из FROST-агрегированных публичных ключей — это cutting edge. Мы должны валидировать наш signing flow против нескольких независимых BIP340 верификаторов (Bitcoin Core, secp256k1 reference, libbtc) на signet, прежде чем доверять доллар пользовательских средств этому.

**Нетехнические риски:**

6. **Скептицизм BTC-сообщества к SGX.** Есть исторически громкое подмножество Bitcoin-сообщества, которое считает любую TEE-based систему "centralization theater". Аргумент FROST 2-of-3 через независимых операторов здесь помогает, как и тот факт, что мы публикуем MRENCLAVE и attestation, но этот разговор случится, и наш messaging должен быть готов. Честная формулировка: "SGX — это defense layer *поверх* FROST threshold custody, а не замена ему — даже полная SGX-компрометация одного энклейва не раскрывает пользовательские средства, потому что ни один энклейв не держит достаточно key material, чтобы подписать что-либо".

7. **Регуляторное внимание к деривативам на BTC.** BTC-деноминированные деривативы сталкиваются с большим регуляторным вниманием в US и EU, чем XRPL-деноминированные, потому что Bitcoin сам по себе — самый торгуемый крипто-актив. Legal posture, юрисдикция, KYC-требования и решения о листинге должны быть пересмотрены для BTC-продукта — XRPL legal framing не портируется автоматически.

8. **Конкурентный ответ централизованных venues.** BitMEX, Deribit и Binance не будут игнорировать credible L1-нативную децентрализованную альтернативу, если она наберёт meaningful TVL. Их ответ скорее всего будет fee compression и liquidity mining, а не технический паритет, и нам нужна sustainability story, которая не зависит от того, чтобы пережить subsidized competition.

9. **Фокус команды.** Запускать два chain-продукта одновременно маленькой командой — это как маленькие команды shippают ни одного хорошо. Это не гипотетически — это самый распространённый failure mode амбициозных feasibility-проектов. См. §10.

**Вещи, которые мы пока не знаем и должны решить:**

- Inverse контракты первыми и linear позже, или оба на запуске?
- Self-run oracle vs. внешний oracle (Pyth/Chainlink)?
- Bitcoin Core direct RPC vs. Esplora/Electrum indexer для address monitoring?
- Lightning на запуске или в phase 2?
- BIP32 per-user depozit derivation vs. single vault с OP_RETURN tagging?
- Внешняя audit-фирма (есть примерно 4-5 фирм, достаточно credible и для SGX enclave кода, *и* для Bitcoin transaction logic)?

Ни один из этих вопросов — не showstopper. Все они заслуживают явных решений до того, как дата запуска будет committed'на.

---

## 10. Рекомендация

**Не начинать BTC-форк сейчас.** XRPL-версия ещё не в production, план failure-mode тестирования ещё не завершён, и operational ceremony ещё не отрепетирована через трёх операторов. Split'ить внимание сейчас откладывает дату ship'а XRPL без пропорционального продвижения BTC-трека.

**Начиная сейчас, держать codebase chain-agnostic.** Конкретно:

- Ввести тонкий `ChainAdapter` интерфейс в оркестраторе, покрывающий deposit monitoring, address derivation, transaction construction и broadcast. Текущая XRPL реализация — один бэкенд; будущая BTC реализация — другой бэкенд. Это ровно та же дисциплина, которую мы рекомендуем для `TeeBackend` в Части 9 `sgx-enclave-capabilities-and-limits.md`, и её дёшево ввести сейчас, но дорого retrofit'ить позже.
- Держать `PerpState`, margin engine, liquidation loop, CLOB logic и ecall surface свободными от любых XRPL-специфичных предположений. Они должны принимать amounts как `int64_t` fixed-point units, деноминированные в том, что является settlement asset, без hardcoded ссылок на RLUSD или XRPL transaction formats.
- Держать chain-специфичные deposit/withdraw ecalls за suffix convention (`ecall_perp_withdraw_check_and_sign_xrpl` сегодня, `ecall_perp_withdraw_check_and_sign_btc` завтра), а не как overloaded generic ecall, чтобы tx-construction logic каждой цепи была explicit и аудируема в изоляции.

**Когда XRPL-версия в production и стабильна — форкнуть BTC-трек.** Scope — 3 месяца инженерии плюс внешний audit, исполняется той же командой, которая построила XRPL-версию, с той же operational моделью. Это момент, чтобы конвертировать латентную опциональность архитектуры во второй revenue stream.

**Для инвестора, специально спрашивающего про BTC сегодня:** правильная формулировка — "архитектура explicit спроектирована для порта на Bitcoin, signing и custody слои уже сегодня shippают ровно те криптографические примитивы Bitcoin, а BTC-версия — это bounded 3-месячный проект после XRPL-запуска, а не новый продукт, который надо строить с нуля". Это правдиво, это позитивно и это не overpromise.

---

## 11. Постскриптум — chain-agnostic архитектура как актив

Аргумент этого документа обобщается за пределы Bitcoin. Любая цепь, использующая `secp256k1` для подписи транзакций, может в принципе хостить ту же perp-DEX архитектуру с примерно тем же инженерным scope'ом: новый chain client, новый deposit/withdraw ecall surface, специализированный под формат транзакции этой цепи, и orchestration plumbing. Список цепей, которые это покрывает, не мал:

- Bitcoin, Bitcoin Cash, Litecoin, Dogecoin — прямые порты BTC-версии с незначительными корректировками.
- Zcash transparent addresses — те же signing primitives, другой address format.
- Ethereum и все EVM цепи — другой transaction envelope, но signing слой идентичен; порт сюда дополнительно выиграл бы от возможности сеттлиться против EVM DeFi экосистемы.
- Stacks, Rootstock и другие BTC sidechains — тривиально, как только BTC-версия работает.
- Любая будущая цепь, которая примет Schnorr / BIP340 подпись — автоматически.

Это форма product optionality, которая не появляется в feature list, но чрезвычайно ценна: **инвестиция, требуемая для поддержки цепи N+1, всегда строго меньше инвестиции, требуемой для поддержки цепи N**, потому что chain-agnostic core (энклейв, FROST, margin engine, risk management, deployment model) амортизируется через каждый rail. XRPL-версия — первая и самая дорогая цепь. BTC — вторая и дешевле. Каждая цепь после BTC ещё дешевле.

Формулировка для инвестора: XRPL-версия — это не только продукт, это *платформенная инвестиция*, чья стоимость front-loaded. BTC — это первая валидация этого platform claim. Ценность claim'а компаундится с каждым дополнительным rail, на который shippает архитектура, и стоимость на rail монотонно снижается.

---

## Приложение — Кросс-ссылки

- `sgx-enclave-capabilities-and-limits.md` — SGX гарантии, sealing, миграция и TDX portability аргумент. Threat model, обсуждаемая там, применяется без изменений к BTC-версии.
- `deployment-procedure.md` — operator model и release ceremony. Применяется без изменений.
- `xls-survey-for-perp-dex.md` — почему XRPL специально был выбран как первый rail. Аргумент "почему не BTC первым" появляется там: latency, fee variability и отсутствие нативной программируемости у BTC были корректно оценены как более высокая инженерная стоимость для PoC, а не как постоянные блокеры.
- `feedback_closes_must_route_clob.md` — дисциплина CLOB routing. Применяется без изменений.
- `project_post_hackathon_architecture.md` — план Тома для post-hackathon liquidity архитектуры. BTC-порт не зависит от того, разрешён ли этот план; он переиспользует любую liquidity архитектуру, на которой остановится XRPL-версия.
- `feedback_bilingual_docs.md` — bilingual policy. Этот документ имеет русскую версию.
