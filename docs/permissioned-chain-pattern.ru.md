# Архитектурный паттерн: permissioned-chain с разделением кода и оператора

**Статус:** внутренняя архитектурно-словарная заметка, 2026-06-06. **Чисто концептуальная — не юридическая, не регуляторная и не маркетинговая позиция.** Фиксирует ментальную модель для будущих технических обсуждений.

## Предпосылка

На архитектурном уровне система ближе к **permissioned-chain паттерну** (Hyperledger Fabric / J.P. Morgan Quorum / R3 Corda), чем к «public-chain DEX» (Uniswap-class smart-contract на permissionless L1) или «централизованной DEX» (один оператор управляет всем доверительно). Словарь permissioned-цепей переносится напрямую и снижает трение в технических разговорах.

## Паттерн

| Слой | Public chain (Bitcoin, Ethereum) | Permissioned chain (Fabric, Quorum, Corda) | Наша система |
|---|---|---|---|
| Софт ноды | Open-source клиенты (Geth, Erigon, Lighthouse…) | Fabric peer бинари, форки Quorum | Reproducibly-built MRENCLAVE (написанный командой dev) |
| Validator/operator set | Permissionless (любой) | Закрытый consortium, member-curated | XRPL M-of-N SignerList, сейчас 2-of-3, membership через quorum |
| State agreement | PoW / PoS consensus | PBFT / Raft / IBFT | Sealed-state continuity через Path A migration под подписью operator quorum |
| Code-update governance | Hard fork (валидаторы решают принимать или нет) | Consortium approval нового бинаря | Path A migration ceremony — старый enclave экспортит state ТОЛЬКО на новый MRENCLAVE, подписанный quorum'ом |
| Settlement layer | Сам chain | Сам chain | XRPL (мы НЕ свой chain) |

## Различающая ось: авторство кода ≠ операторская деятельность

Главное архитектурное свойство — **разделение entity-автора кода от entity-оператора**, причём это разделение **криптографически enforce'нуто**, а не только организационно.

Два инварианта делают разделение реальным:

1. **Reproducible-build foundation invariant.** Исходник публикуется. Любой оператор может независимо пересобрать enclave-бинарь в pinned Docker окружении и получить bit-identical MRENCLAVE. Операторы проверяют что они собираются запустить; entity-автор не может подсунуть бинарь, который не воспроизводится.

2. **Path A migration ceremony.** Обновления кода gate'ятся operator quorum'ом. Текущий (старый) enclave экспортит свой sealed state ТОЛЬКО на новый MRENCLAVE, подписанный M-of-N quorum'ом; новый MRENCLAVE импортит state только если он attested как пришедший с previously-recognized старого MRENCLAVE. У операторов есть явное право вето на то, какой код запускается.

В Bitcoin / Ethereum аналогии: операторы играют роль валидаторов, решающих принимать ли client release. Разделение «кто написал Geth» и «кто запускает Geth» имеет ту же форму что «кто написал наш enclave» и «кто запускает наш кластер».

## Где мы расширяем паттерн: TEE execution privacy

Permissioned chains (Fabric, Quorum, Corda) обычно открывают transaction content всем участвующим peer'ам. Наша система использует TEE attestation чтобы делать выполнение **приватным от самих операторов** — оператор запускает бинарь, но не может читать order content, position state или pending withdrawals во время исполнения. По духу ближе к MPC-based confidential compute чем к private blockchain.

Сдвиг trust model: операторы доверяют бинарю (verified через reproducible-build + DCAP attestation), а не друг другу хранить transaction content конфиденциальным.

## Где мы НЕ расширяем: не свой chain

Мы не производим блоки; мы не запускаем consensus по transaction history. Settlement layer — это XRPL: deposits, withdrawals, signer-list changes, operator membership — всё settle'ится на XRPL как обычные транзакции под M-of-N SignerList. Enclave кластер — это off-chain execution environment с on-chain settlement — ближе к rollup'овскому «execution layer + L1 settlement» split, чем к самодостаточному chain'у.

Это сознательный выбор: «chain-is-settlement» системы (Bitcoin, Ethereum, Fabric, Corda) владеют своей consensus проблемой. Мы делегируем её XRPL, что даёт нам battle-tested settlement layer и лишает нас optionality писать свои consensus правила. Для perpetuals DEX use case'а размен корректный.

## TRUST как TEE co-processor framing

Stack описанный выше иногда internal'но называется **TRUST** — **Trusted Unchained Settlement**. Acronym — shorthand для той же архитектуры что описывает этот документ; включён здесь чтобы будущие references в технических материалах resolve'ились чисто.

### Где это сидит в L1 / L2 / L3 vocabulary — нигде

L2 (Arbitrum, Optimism, …) означает специфические вещи — execution outsourced from L1, L1-verifiable state через fraud или validity proofs, state batched back to L1. Мы ничего этого не делаем: chain (XRPL) не verify'ит наш execution state; anchor'им только multisig payments и Domain-field updates. Calling this L2 sets the wrong expectation. L3 (app-rollup на top of L2) тоже не подходит.

Ближайший industry vocabulary — **co-processor** — используется zk командами (Axiom, RISC Zero) для off-chain compute с on-chain anchoring. Мы не zk co-processor; мы **TEE co-processor**. Структурно co-processor делает **computation**, не block production — что и есть TRUST. Naming it as such избегает L-tier misframing.

One-line gloss для section title: **TRUST — это TEE co-processor pattern, добавляющий smart-contract-equivalent programmability к chains без native VM, settle'ящий final state changes через native multisig primitives того же chain'а.**

### Что TRUST добавляет, архитектурно

Программность для chains, settlement layer которых НЕ несёт Turing-complete VM (XRPL сегодня; структурно applicable к Bitcoin / Litecoin / другим UTXO или amendment-driven chains). Composition:

- **Native chain primitives** — что бы underlying chain native'но не exposed: M-of-N multisig, atomic transactions, on-chain data field для commit'а payloads, public verifiable ledger. XRPL даёт `SignerListSet`, escrow accounts, `Payment` с `DestinationTag` / `Memos`, `AccountSet.Domain`. Другие на Bitcoin (Script multisig + `OP_RETURN`), другие на Cardano, другие на Stellar — но **форма** «нам нужны эти четыре вещи» переносится.
- **TEE-attested business logic** — perpetuals matching engine, vault, settlement math, FROST signing. Runs in SGX сегодня; портабельно на TDX / SEV-SNP / Nitro в principle — см. `EthSignerEnclave/docs/contingency/` в private repo для substrate-migration анализов.
- **Code/operator separation cryptographically enforced** — reproducible-build invariant + Path A migration ceremony, как описано выше. Это что делает паттерн composable, а не «просто TEE running code».

Результат — smart-contract-**equivalent** business logic без smart contracts: execution делает работу которую contract сделал бы, settlement anchor'ит state changes через native primitives chain'а, и операторы control'ируют какой код запускается без control'я каждой individual operation. «Equivalent» qualifier важен: это не «behaves like smart contracts». См. § «Что это стоит» ниже.

### Что TRUST добавляет vs permissioned-chain pattern

Два расширения над Fabric / Quorum / Corda:

1. **Execution privacy** — transaction content приватен от самих операторов, не только от outsiders. Fabric/Quorum/Corda peers видят transaction payloads; TRUST операторы видят encrypted ciphertext. Это часть design'а которая делает perpetuals exchange usable: positions, pending orders, live margin не readable оператором running binary.
2. **Substrate portability в principle** — Fabric это свой chain; Quorum это Geth fork; Corda имеет свою network. TRUST'овский settlement substrate — какой chain deployment выберет. Сегодня XRPL; архитектура не требует XRPL. См. weaknesses ниже для gap'а между «in principle» и «сегодня».

### Что это стоит (weaknesses, declared honestly)

Pattern не strict improvement над smart-contract DEX или centralized DEX — это другая точка в trade-off space. Costs реальны и должны быть named.

1. **Substrate portability — aspirational, не factual сегодня.** Текущая implementation использует XRPL-specific primitives. Porting на Bitcoin требует re-do on-chain vocabulary (Script multisig семантика отличается от XRPL); porting на другие chains требует similar rework. «Any chain» overclaim для current state; архитектура supports it but код пока нет.
2. **On-chain composability потеряна.** Smart contracts композируются друг с другом — одна transaction может touch Uniswap, Aave, Compound атомарно. TRUST — нет. TEE-attested execution layer не может быть called другими on-chain contracts на settlement chain'е, и TRUST не может их call'ить. Bridges потребовались бы для любой cross-protocol composition.
3. **Public state auditability потеряна.** Smart-contract DEX state readable любым кто читает chain. TRUST state sealed внутри TEE. Users trust attestation + reproducible build вместо direct chain inspection. Другая trust model — не «strictly better», не «strictly worse»; другая.
4. **Liveness compound.** Smart contracts на public chain работают пока chain работает. TRUST работает только когда **оба** chain жив И operator quorum reachable. Две dependencies in series вместо одной.
5. **MEV не absent, просто relocated.** Public chains face on-chain MEV через transaction ordering observable in mempool. TRUST faces TEE-internal sequencing fairness — controllable inside enclave, но другая проблема, не solved. Sequencer fairness becomes internal architectural concern.
6. **Composability между TRUST instances не работает out-of-the-box.** Два TRUST deployments на двух разных chains не композируются. Bridges или sequential commitment требовались бы; это не existing primitive паттерна.

### Зачем declare эти costs явно

Потому что pattern's positioning otherwise reads как «strict improvement over both smart-contract DEX and centralized DEX», что не true и не survive'ит technical due diligence. Declaring costs up front lets conversation move to «is this trade-off appropriate for the use case» — what conversation we want to be having.

## Текущее фактическое состояние: capability vs deployment

Multi-entity capability реализована в коде; сегодняшние три Azure DCsv3 ноды run'ятся под одной operator entity для удобства разработки. Путь к onboarding'у дополнительной operator entity — существующая operator-onboarding procedure под существующий quorum, плюс новый оператор независимо воспроизводит MRENCLAVE до подписания. Изменения кода для перехода от одной к нескольким distinct operator entities не требуется; только процедурная последовательность под quorum'ом и независимая rebuild verification каждым новым оператором.

Это различение — capability vs current deployment — это честный ответ когда технический разговор доходит до «сколько distinct entities оперирует сегодня.»

## Чем этот документ НЕ является

- Не юридическая или регуляторная позиция. Operator-quorum gating распределяет architectural control; как это отображается в legal liability — отдельный анализ.
- Не маркетинговый или investor narrative. Этим use case'ам нужны свои формулировки сверху.
- Не полное decentralization claim. Decentralization — multi-axis (operator distribution, code authorship, settlement layer, user permissionlessness); этот документ называет оси, не выносит вердикт.
- Не сравнение с конкретными blockchain проектами кроме как архитектурно-паттерн аналогии. Мы не «Hyperledger для perps» — мы «permissioned-execution + L1-settlement паттерн, который использует TEE для execution privacy.»

## Cross-reference

- `docs/multi-operator-architecture.{en,ru}.md` — конкретный operator-quorum протокол
- `docs/test-env-workflow.{en,ru}.md` — preflight → staging → acceptance workflow под этой моделью
- Audit-side foundation invariant'ы (referenced через private audit repo): reproducible-build invariant; upgrade-path invariant (Path A migration ceremony); TEE-thesis invariant (приватный user state живёт sealed в enclave, не на chain'е)
