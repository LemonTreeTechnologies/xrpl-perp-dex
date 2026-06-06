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
