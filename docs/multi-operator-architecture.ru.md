# Архитектура мульти-операторного кластера

**Статус:** обязателен для любого изменения в bootstrap кластера, deploy, DKG или составе участников.
**Аудитория:** все, кто пишет или ревьюит код, пересекающий границы нод.
**Спутник:** `testnet-enclave-bump-procedure.{en,ru}.md` (конкретный operator runbook для testnet'а сегодня).

## 0. Зачем нужен этот документ

Эта система — perpetuals DEX, чья гарантия безопасности — "ни один оператор не может перевести средства пользователей в одиночку". Эта гарантия выживает только если кластер выполняет **те же кодовые пути** на testnet'е и mainnet'е. Двухрежимные codebase'ы (один путь для testnet'а, другой для production'а) утекают допущениями: testnet валидирует один набор поведений, аудит затем возражает на другой набор в production'е, мы переписываем, переписанное снова частично временное, и мы зацикливаемся. Каждая петля наблюдаема как комментарий "потом перепишем для production'а", флаг `--testnet-only`, SSH-цепочка которая оркестрирует код, который должен оркестрировать сам себя.

Этот документ фиксирует правила. Правила сформулированы как обязательства, а не пожелания.

**Обязательство.** Каждый subcommand и модуль, пересекающий границу ноды, проходит **single-mode check** перед мержем:

1. Работает ли он одинаково на testnet'е и mainnet'е?
2. Встраивает ли он какое-либо межоператорское допущение — SSH к VM peer'а, общая файловая система, центральный bastion — которое production не может выполнить?

Если ответ на (1) — нет, или на (2) — да, изменение не мержится. Переписываем.

**Operator workflow это НЕ system code.** Человек-оператор (или AI-ассистент действующий от его имени) может SSH'иться в свои собственные VM, запускать Ansible playbook'и, fan-out скрипты через parallel-ssh, водить deployment'ы с центрального bastion'а который он контролирует — это его sysadmin-тулинг. Ничего из этого не живёт в нашем репозитории. Когда этот документ говорит "система делает X", имеется в виду наш закоммиченный код делает X. Когда говорит "оператор делает X", имеется в виду что человек (возможно с AI-ассистентом) делает X используя свой персональный тулинг, вне нашего репо.

Критическое следствие: testnet сегодня, где один человек запускает три ноды и SSH'ится между ними свободно, не другой режим. Системный код, работающий на testnet'е — тот же код, что работает на mainnet'е. Единственная разница — топология операторов: один человек владеет тремя нодами (testnet) против трёх независимых людей, каждый владеющий одной нодой (mainnet).

## 1. Trust model

Доверие, которое мы оказываем каждому компоненту:

| Компонент | Доверенный? | Что может | Что не может |
|---|---|---|---|
| Intel SGX enclave (после DCAP attestation'а) | Да | Хранить sealed FROST shares, подписывать ими, выполнять AEAD над ECDH-derived ключами | Персистить что-либо переживающее MRENCLAVE bump; общаться без прохождения через host |
| Host OS на VM каждого оператора | Нет | Читать/писать любой файл вне enclave'а; манипулировать сетью; наблюдать тайминг enclave'а | Читать память enclave'а; подделать DCAP quote; произвести shares соответствующие другому MRENCLAVE |
| Оператор (человек, запускающий свою ноду) | Нет относительно других операторов | Управлять своей VM; наблюдать свою сеть; подменять или останавливать свой host бинарник | Получить доступ к VM другого оператора; подписать multisig транзакцию в одиночку (quorum 2-of-N для N≥3); произвести enclave-bound key material другого оператора |
| XRPL ledger | Да (в рамках Byzantine-resilience consensus'а) | Отражать SignerListSet, AccountSet, Payment транзакции; обеспечивать durable публичное хранилище для `AccountSet.Domain` operator entries | Быть односторонне переписанным любой стороной; разрешать operator-internal disputes |
| libp2p mesh между orchestrator'ами | Authenticated, не confidential | Нести gossipsub topic'и которые ноды публикуют и слушают; обеспечивать peer-discovery | Заменять end-to-end attested encryption (мы накладываем Path A v2 поверх для share transport'а) |

Cluster invariants, выпадающие из этого:

1. **Ни один оператор не может подписать withdrawal в одиночку.** XRPL multisig с quorum ≥ 2 предотвращает это; SignerList — on-chain authority и обновляется только quorum-met multisig'ом.
2. **Ни один оператор не может произвести валидную FROST подпись.** FROST 2-of-N (или выше) предотвращает это; `frost_group` state sealed внутри каждого enclave'а.
3. **Ни один оператор не может ротировать состав операторов.** On-chain SignerList — источник истины membership'а; обновления SignerListSet требуют existing-quorum multisig.
4. **Ни один оператор не может в одиночку заблокировать прогресс.** N-1 из N операторов могут подписать и выполнить (если quorum достигнут).

## 2. Operator scope

Каждый оператор владеет:

- Своим железом: типично одна SGX-capable VM (Azure DCsv3 в текущем testnet'е). Контролирует её OS, сетевую конфигурацию, root access, физический-или-виртуальный decommissioning.
- Своими enclave-internal ключами (FROST share, ECDH identity). Существуют только внутри его enclave'а; даже сам оператор не может их прочесть.
- Своими бэкапами: enclave sealing означает что бэкапы вне enclave'а бесполезны против MRENCLAVE bump. "Бэкап" оператора фундаментально социальный (заново войти в кластер после fresh enclave'а).
- Своим XRPL operator account'ом. Его master key сгенерирован его enclave'ом на первом запуске, остаётся внутри enclave'а, подписывает multisig участия.

Ни один оператор не имеет:

- Доступа — SSH, файлового, сетевого, никакого — к VM другого оператора.
- Копии FROST share другого оператора. По конструкции DKG и инвариантам Path A v2 transport'а только держатель-enclave когда-либо видит свой собственный share.
- Authority менять SignerList, escrow account state, или cluster membership без quorum-met multisig'а.
- Возможности знать, честен ли host другого оператора, malicious или offline, кроме как через наблюдаемое on-chain или libp2p поведение.

Кластер — emergent property из N независимых операторов, которые согласовали (off-chain) tag релиза софта, развернули его независимо, зарегистрировали свой публичный материал on-chain, и достигли quorum'а на полученном SignerList'е.

## 3. Cross-node coordination — что использует система

Три канала, в порядке приоритета:

### 3.1 libp2p mesh + gossipsub

Orchestrator daemon на каждой ноде поддерживает libp2p mesh (порт 4001 в текущей testnet топологии). Authenticated peer соединения через libp2p noise protocol. Gossipsub topic'и несут cluster-internal сообщения.

Topic'и в использовании:

| Topic | Направление | Payload | Существует |
|---|---|---|---|
| `perp-dex/path-a/peer-quote` | каждая нода периодически публикует | DCAP quote bound to (shard, group_id, ECDH pubkey) | да |
| `perp-dex/path-a/share-v2` | sender → recipient (filtered) | AEAD-wrapped FROST или DKG share envelope | да |
| `perp-dex/cluster/dkg-step` | leader → all | DKG ceremony coordination signal (round-1-go, round-2-go, finalize) | новый (Phase 2.1c-replacement) |

libp2p — единственный канал cross-node coordination для live cluster operation. Всё что должно происходить между нодами во время DKG, share rotation, или membership change идёт через него. В production code нет пути "out-of-band SSH из coordinator'а".

### 3.2 DCAP cross-attestation (Path A v2)

Операторы не доверяют host'ам друг друга, но доверяют enclave'ам друг друга при условии что DCAP их аттестует. Path A v2 protocol (`docs/path_a_ecdh_over_dcap.md` в enclave-репо) даёт каждому enclave'у verified peer ECDH identity, заполняет per-peer attestation cache (5-минутный TTL), и предоставляет AEAD primitive используемый share transport'ом. Path A v2 — субстрат под libp2p share-v2 topic'ом — libp2p доставляет envelope, Path A v2 делает безопасной саму доставку.

### 3.3 On-chain XRPL state

XRPL ledger — durable, публичная, tamper-evident память кластера. Используем для трёх вещей:

| Что живёт on-chain | Поле | Обновляется |
|---|---|---|
| Cluster membership | `SignerList` на escrow account'е | SignerListSet через existing-quorum multisig |
| ECDH pubkey каждого оператора | `AccountSet.Domain` на собственном XRPL account'е каждого оператора | Каждый оператор независимо |
| Cluster operations (withdrawals и т.д.) | Payment / Escrow транзакции на escrow account'е | 2-of-N multisig подписанный orchestrator'ами |

`Domain` — поле 256 байт на каждом XRPL account'е. Используем его для публикации ECDH identity public key каждого оператора. Формат структурированный: `xperp-ecdh-v1:<33-byte hex>`. Discovery для любой ноды: query `AccountObjects` escrow account'а → получить SignerList → для каждого entry query `AccountInfo` → распарсить `Domain` → извлечь ECDH pubkey. Никакого отдельного registry, никакой off-chain coordination для pubkey discovery.

### 3.4 Out-of-band — только governance

Некоторые решения inherently социальные: кто в initial set операторов, когда bump'ить MRENCLAVE для security release, как реагировать на инцидент. Они происходят на любом канале, который выберет группа операторов (Discord, mailing list, отдельный governance contract). Этот документ не специфицирует off-chain канал, потому что system code никогда не читает с него. Единственный output off-chain governance, имеющий значение для системы — **on-chain XRPL транзакции**. Происходит governance, `SignerListSet` приземляется on-chain, система это наблюдает.

## 4. Различие operator workflow vs system code

System code в этом репозитории отвечает за всё, что происходит между нодами во время cluster operation. Это исчерпывающе покрыто §3 выше: libp2p, Path A v2, on-chain.

Operator workflow — что делает человек (или его ассистент) для оперирования его ноды:

- SSH в собственную VM
- Запуск `systemctl` для управления сервисами
- Использование Ansible / parallel-ssh / one-off bash скрипта для fan-out той же node-local команды на несколько собственных VM (актуально на testnet'е где один человек владеет всеми тремя)
- Прохождение sequence-of-steps процедуры через чтение runbook'а и выполнение каждого шага по очереди

Operator workflow — его собственный, ему его и проектировать и поддерживать. System code не предполагает существование какого-либо конкретного workflow тулинга. Когда этот документ ссылается на operator activity (например, "оператор разворачивает свою ноду"), он описывает runbook step который оператор выполняет используя свой собственный тулинг, не Rust subcommand который мы поставляем.

Single-mode check из §0 предотвращает протекание: Rust subcommand требующий `ssh user@another-operator's-host` для функционирования — это system code встраивающий cross-operator workflow assumption. Это fail'ит check.

## 5. Cluster lifecycle

Шесть событий в жизни кластера. Каждое описано конкретно в §6–§9.

| Событие | Частота | Drivers |
|---|---|---|
| **Genesis** | Один раз | Все N founding операторов независимо + designated founder для trusted-dealer escrow setup'а |
| **Steady state** | Непрерывно | Orchestrator daemon на каждой ноде работает; libp2p mesh persist'ит; on-chain operations выполняются на multisig'е |
| **MRENCLAVE bump** | Per security release (редко) | Каждый оператор независимо; coordinated start signal off-chain; новый DKG следует |
| **Operator addition** | Редко (governance) | Existing quorum + новый оператор; SignerListSet добавляет члена; новый DKG |
| **Operator removal** | Редко (governance / emergency) | Existing quorum (excluding removed); SignerListSet удаляет члена; новый DKG |
| **Disaster recovery** | Per-incident | Affected оператор + remaining quorum; сводится к membership change + новый DKG |

Все пять событий после genesis механически одинаковы: governance производит SignerListSet транзакцию подписанную текущим quorum'ом, кластер наблюдает новый membership, затем новый DKG бежит через libp2p. Genesis особенный только потому что нет текущего quorum'а для подписи.

## 6. Bootstrap protocol — genesis

N founding операторов off-chain согласовали tag релиза софта. Согласовали XRPL operator address'а друг друга. Согласовали quorum (типично 2-of-3 или 3-of-5). Согласовали кто играет роль founder'а для trusted-dealer escrow setup'а.

### 6.1 Каждый оператор независимо разворачивает свою ноду

Каждый оператор, на своей собственной VM, выполняет:

1. `git checkout <release-tag>` обоих репозиториев.
2. `docker build --no-cache -f Dockerfile.azure ...` для enclave образа. Верифицирует что полученный SHA256 `enclave.signed.so` и MRENCLAVE совпадают с опубликованными в release tag'е (см. §7.1).
3. `cargo build --release` для orchestrator binary.
4. Кладёт артефакты в их canonical paths на VM (`/home/<user>/perp/`).
5. Конфигурирует startup аргументы orchestrator'а: enclave URL (только loopback), libp2p listen address, libp2p peer addresses (другие N-1 публичные адреса операторов, согласованные off-chain), database URL, без escrow address пока.
6. Стартует enclave service. Запускает single `node-bootstrap` команду на этой VM (см. §10) для генерации XRPL keypair оператора внутри enclave'а; enclave seal'ит private key и эмитит публичный xrpl_address + compressed_pubkey.

После этого шага каждый оператор обладает (локально) свежесгенерированным `node-<i>.json` содержащим его xrpl_address, compressed_pubkey, и authentication session_key.

### 6.2 Каждый оператор публикует свой ECDH pubkey on-chain

Каждый оператор query'ит свой enclave для его ECDH identity public key (`/v1/pool/ecdh/pubkey`) и сабмитит `AccountSet` транзакцию на свой XRPL operator account, выставляя `Domain` в structured hex value: `xperp-ecdh-v1:<33-byte ECDH pubkey hex>`. Сабмит со своей собственной машины через XRPL JSON-RPC; SSH не задействован.

После этого все N operator account'ов публично несут свой ECDH pubkey. Любая сторона (включая peer'ов, observer'ов, аудиторов) может это запросить standard XRPL `AccountInfo` request'ом.

### 6.3 Off-chain согласование initial SignerList'а

Операторы обмениваются своими `node-<i>.json` файлами через off-chain канал (Discord, email). Каждый верифицирует что content совпадает с тем что он сам сабмитил. Финальная согласованная SignerList composition — set из N xrpl_address'ов с quorum'ом K.

Это единственный шаг, который НЕ enforce'ится system code'ом. Это governance. Система наблюдает результирующий on-chain SignerListSet и доверяет только ему.

### 6.4 Founder запускает trusted-dealer escrow setup

Designated founder запускает `escrow-init` (см. §10) с любой машины с internet access'ом. Этот subcommand:

1. Генерирует свежий secp256k1 XRPL wallet (escrow account).
2. Faucet-funds его (testnet) или founder funds его из своих собственных XRP holdings (mainnet — точный механизм governance, здесь не специфицирован).
3. Сабмитит `SignerListSet` с N согласованными operator address'ами и quorum'ом K.
4. Сабмитит `AccountSet asfDisableMaster`. С этого момента у founder'а нет особой authority над escrow.
5. Пишет seed в canonical path (`~/.secrets/perp-dex-xrpl/escrow-<env>.json`, mode 0600).
6. Печатает escrow address.

Seed file output шага 5 уникален для этой команды — он документирует что founder когда-то им владел, но post-disable у seed'а нет операционной мощи. Храним для proof of provenance и forensics.

После этого шага escrow address публичный, SignerList on-chain, master отключён.

### 6.5 Операторы публикуют escrow address своим orchestrator'ам

Каждый оператор обновляет startup configuration своего orchestrator'а чтобы включить escrow address (значение броадкаст'ится операторам founder'ом; верифицируемо on-chain любым). Они рестартуют свой orchestrator. Orchestrator теперь boots, присоединяется к libp2p mesh'у, наблюдает on-chain SignerList, и начинает discovering peer ECDH pubkeys через `AccountInfo` queries против SignerList members.

### 6.6 libp2p mesh формируется

Каждый orchestrator dial'ит другие N-1 known peer addresses. Gossipsub mesh формируется автоматически. Никаких manual bootstrap шагов. Existing topic'и subscribed.

### 6.7 Cross-node DCAP attestation rounds

Existing periodic peer-quote announcer (одна задача на FROST group, на orchestrator, период 240s) публикует DCAP quotes bound to `(shard_id, group_id, ECDH_pubkey)`. На genesis нет FROST `group_id` пока, поэтому announcer использует bootstrap sentinel `group_id = 32 нулевых байта`. Другие orchestrator'ы получают announcement, вызывают `verify-peer-quote` против своего локального enclave'а для заполнения `peer_attest_cache` для sentinel'а.

В пределах ~10 минут после того как все операторы online, attestation cache каждого enclave'а заполнен для каждого peer ECDH pubkey'я под sentinel'ом.

### 6.8 DKG ceremony через libp2p

Согласованный ceremony leader (per off-chain agreement, часто `pid=0`) инициирует DKG публикуя на новом `perp-dex/cluster/dkg-step` topic'е: typed message stream покрывающий round-1-start, round-1.5-export, round-2-import-status, finalize. Каждый orchestrator обрабатывает каждый шаг локально (вызывая `/pool/dkg/round1-generate`, `/pool/dkg/round1-export-share-v2` и т.д. enclave'а на своём собственном loopback enclave'е) и публикует step-completion ack'и на том же topic'е. Round-1.5 envelopes публикуются на existing `perp-dex/path-a/share-v2` topic'е с discriminant'ом обозначающим DKG-bootstrap context.

Когда все N orchestrator'ов acknowledge finalize и leader cross-проверил их `group_pubkey` outputs (broadcast'нутые через topic), leader публикует final-ack message содержащий canonical group_pubkey. Каждый orchestrator записывает его локально и обновляет свою `frost_group_id` configuration.

Полная ceremony завершается в том же time bound'е что и testnet manual procedure (~35 секунд wall-clock для N=3) но без вовлечения operator SSH.

### 6.9 Steady state

Orchestrator'ы работают с:
- libp2p mesh stable
- `frost_group_id` известен и сконфигурирован
- Periodic peer-quote announcer работает с реальным `frost_group_id`
- `frost_group` инициализирован в каждом enclave'е; может подписывать

Кластер теперь готов для withdrawals (multisig submitted через XRPL `submit_multisigned`), order processing, deposit detection и т.д.

## 7. MRENCLAVE bump (coordinated upgrade)

### 7.1 Reproducible build — фундамент

Каждый release несёт Git tag. Release process производит deterministic MRENCLAVE который любой оператор может воспроизвести запуская:

```
git checkout <tag>
docker build --no-cache -f Dockerfile.azure -t perp-dex:<tag> .
docker run --rm perp-dex:<tag> sha256sum /build/out/enclave.signed.so
docker run --rm perp-dex:<tag> sgx_sign dump -enclave /build/out/enclave.signed.so -out /tmp/sigstruct -dumpfile /tmp/sigstruct.txt
# Прочитать MRENCLAVE field из /tmp/sigstruct.txt
```

Release tag включает expected MRENCLAVE в его release notes / signed announcement'е. Оператор чей local build производит другой MRENCLAVE имеет non-reproducible build — он расследует и фиксит до deploy.

Reproducible build — что делает "мы все запускаем тот же MRENCLAVE" верифицируемым без того чтобы кто-то из нас доверял другим. Каждый оператор независимо подтверждает.

### 7.2 Каждый оператор независимо разворачивает

Каждый оператор, на своей собственной VM:

1. Верифицирует что его built MRENCLAVE совпадает с published MRENCLAVE release tag'а.
2. Запускает `node-deploy` (см. §10) локально на своей VM. Этот subcommand останавливает локальные сервисы, swap'ит binaries с backup'ом, рестартует enclave only.

Sealed state на каждом enclave'е не переживает MRENCLAVE bump. `frost_group` data каждого оператора утрачена после swap'а (сохранена только как forensic backup directory). Это by design: enclave не имеет понятия decryption keys для старых shares peer enclave'а.

### 7.3 Coordinated rollout

Bump координирован через off-chain governance: операторы согласуют deploy window. Каждый оператор разворачивается в пределах этого window. libp2p mesh детектирует "N orchestrator'ов online с новым MRENCLAVE" через periodic peer-quote announcer (peer-quote messages теперь аттестуют новый MRENCLAVE).

### 7.4 Свежий DKG следует

Как только N операторов online с новым MRENCLAVE и их attestation caches заполнены для sentinel `group_id`, кластер запускает свежий DKG через §6.8 выше. Новый `group_pubkey`. Старый escrow остаётся on-chain — тот же address, тот же SignerList — просто новая FROST group под ним. Existing SignerList escrow'а не затронут (XRPL multisig использует per-operator XRPL keys, которые были сгенерированы в §6.1 и seal'ятся через MRENCLAVE bumps только если оператор выбрал их сохранить; в текущем testnet'е нет, поэтому отдельный operator-rekey бежит вместе; в mainnet'е операторы могут выбрать иначе, но это per-operator решение).

## 8. Membership changes

### 8.1 Добавить оператора

Off-chain, existing операторы согласуют добавить оператора M+1. Новый оператор независимо билдит и разворачивается (§6.1). Публикует свой ECDH pubkey on-chain (§6.2). Existing операторы верифицируют что новый MRENCLAVE совпадает (§7.1). Один existing оператор draft'ит `SignerListSet` с N+1 signers и распространяет для подписи; existing-quorum multisig подписывает и сабмитит.

После того как SignerListSet приземлился, кластер наблюдает новый membership. Свежий DKG следует (§6.8) через N+1 операторов с новым threshold'ом (типично `ceil((N+1)*2/3)` или per governance).

### 8.2 Удалить оператора

Та же форма. Existing-quorum (excluding removed оператора) draft'ит и подписывает SignerListSet с N-1 signers. Приземляется on-chain. Свежий DKG через N-1 операторов.

### 8.3 Почему это тот же primitive что MRENCLAVE bump с code perspective

Оба события заканчиваются: "effective membership кластера изменился; требуется свежий DKG". System code имеет один DKG ceremony driver (§6.8). Он бежит после любого membership-affecting события. Нет special-case "мы были 3, теперь 4" пути.

## 9. Disaster recovery

Три сценария, все сводящиеся к membership change.

### 9.1 Оператор X теряет свой share (host crash, MRENCLAVE bump и т.д.)

Если только X затронут и N-1 операторов всё ещё держат valid shares: кластер всё ещё может подписывать на quorum'е K (предполагая K ≤ N-1). Чтобы X re-join'нулся, запустить §8.1 — обращаться с X как с новым добавлением (с новой ECDH identity post-fresh-enclave). Эквивалентно свежий DKG с тем же membership set, просто X стартует с нуля.

Если несколько операторов затронуты и мы падаем ниже quorum'а: on-chain escrow несигнируем. Кластер должен ребилдиться с genesis (§6) с тем же escrow account address — операторы сабмитят новый SignerListSet через... они не могут, потому что не могут достичь quorum'а. Этот случай — continuing on-chain backstop founder'а: master key escrow'а был отключён в §6.4, поэтому founder не может помочь. Recovery — XRPL-level operation которая out of scope здесь; consult XRPL recovery mechanisms (none unilateral). Этот сценарий — почему кластер ДОЛЖЕН поддерживать как минимум quorum live операторов в любой момент.

### 9.2 Ключ оператора X скомпрометирован (host malicious, у атакующего enclave X'а)

Existing операторы детектируют compromise через on-chain misbehavior или off-chain signal. Они запускают §8.2: удалить X через SignerListSet подписанный N-1 honest операторами (предполагая N-1 ≥ K). Свежий DKG с N-1.

Если quorum не может быть достигнут без X (K = N), кластер в риске и должен оперировать осторожно пока membership не вырастет (§8.1) или X не recover'ится (§9.1).

### 9.3 Оператор X offline indefinitely

Неотличимо от §9.2 с perspective кластера. Та же recovery procedure: запустить §8.2 для удаления X. Если оператор возвращается позже, обращаться как с новым добавлением (§8.1).

## 10. Subcommand model

Система exposes четыре класса subcommand'ов. Каждый committed subcommand укладывается в один класс.

### 10.1 Node-local — бежит на одной ноде

Node-local subcommand affects только ноду на которой бежит. Может читать локальный enclave (loopback HTTPS), писать локальную файловую систему, query'ить XRPL JSON-RPC (read-only или signing с keys этой ноды). НЕ принимает node addresses или SSH targets как input.

Примеры:
- `operator-setup` (existing, переименуется в `node-bootstrap`): сгенерировать operator keypair этой ноды в локальном enclave'е; эмитировать `node-<i>.json`.
- `node-config-apply` (новый): query on-chain SignerList; для каждого члена query их `Domain`; собрать локальный `signers_config.json` с `local_signer` выставленным в entry этой ноды; рестартануть локальный orchestrator service.
- `node-deploy` (новый): swap локальные binaries, управлять локальными systemd сервисами. Заменяет SSH-driven `cluster-deploy`.

### 10.2 XRPL-only — бежит откуда угодно с интернетом

XRPL-only subcommand не общается с никаким enclave'ом. Использует XRPL JSON-RPC для query state или submit транзакций.

Примеры:
- `escrow-init` (новый): faucet-fund escrow, submit SignerListSet, submit AccountSet asfDisableMaster, write seed file. Заменяет `setup_testnet_escrow.py`.
- `domain-set` (новый, sub-step `node-bootstrap`): submit AccountSet на operator account'е этой ноды с `Domain = xperp-ecdh-v1:<hex>`. Опционально включён в `node-bootstrap`.

### 10.3 Cluster-coordinated — бежит на одной ноде, drives через libp2p

Cluster-coordinated subcommand бежит на одной ноде и drives cluster-wide operation через gossipsub. НЕ использует SSH. Другие ноды участвуют благодаря тому что их orchestrator daemon слушает relevant gossipsub topic.

Примеры:
- `dkg-coordinate` (новый): на одной ноде (leader), публикует `perp-dex/cluster/dkg-step` сообщения; followers отвечают вызовом локальных enclave endpoints; leader ждёт finalize-ack'и; эмитит group_pubkey по успеху. Заменяет SSH-driven `dkg-bootstrap`.

### 10.4 Чего НЕ существует

Нет subcommand'а который принимает список "remote nodes" и SSH'ится в них. Нет "fan-out" режима. Нет testnet-only пути. Операторы желающие запустить node-local subcommand на нескольких своих собственных VM делают это своим собственным sysadmin тулингом вне этого репозитория.

## 11. Текущий код mapped to model

### 11.1 Production-grade primitives

Эти существуют и корректны под multi-operator моделью:

- `xrpl-perp-dex-enclave`: ECDH identity, DCAP cross-attestation, Path A v2 share transport, DKG v2 wire format, FROST signing primitives.
- `orchestrator/src/p2p.rs`: libp2p mesh + gossipsub topic'и для `peer-quote` и `share-v2`.
- `orchestrator/src/path_a_redkg.rs`: existing share-export driver (для share rotation, не bootstrap DKG). Loopback admin route. Соблюдает no-cross-operator-SSH правило.
- `orchestrator/src/withdrawal.rs`: multisig withdrawal flow через XRPL `submit_multisigned`. Enclave каждого оператора подписывает своим собственным ключом; aggregation on-chain.
- `orchestrator/src/cli_tools.rs::operator_setup`: node-local. Генерирует keypair оператора через локальный enclave, эмитит `node-<i>.json`. Будет promoted до `node-bootstrap` и расширен чтобы также публиковать `Domain`.

### 11.2 Помечено как debt — должно быть заменено

Следующее присутствует в репозитории но нарушает модель. Они помечены `[DEPRECATED — нарушает multi-operator architecture; replace per Phase 2.1c]` doc-комментариями в верху модулей. Они хранятся временно потому что нам нужны testnet operations во время transition'а; они НЕ предназначены для use на любом non-throwaway state'е.

- `orchestrator/src/dkg_bootstrap.rs` (commit `5fe5aa1`+`78710a9`): SSH-driven DKG ceremony. Замена: `dkg-coordinate` (cluster-coordinated, libp2p-driven).
- `orchestrator/src/cluster_deploy.rs` (commit `fefd3c9`): SSH-driven multi-node binary swap + service lifecycle. Замена: `node-deploy` (node-local). Coordinated MRENCLAVE bumps — governance, не одна команда.

### 11.3 Replacement plan (Phase 2.1c continuation)

Replacement работа происходит в этом порядке:

| Phase | Subcommand | Класс | Заменяет | Время |
|---|---|---|---|---|
| 2.1c-A | `node-bootstrap` | node-local | `operator-setup` (rename + extend with Domain publish) | ~1 день |
| 2.1c-B | `escrow-init` | XRPL-only | `setup_testnet_escrow.py` | ~½ дня |
| 2.1c-C | `node-config-apply` | node-local | новый (нет current equivalent'а) | ~½ дня |
| 2.1c-D | `dkg-coordinate` | cluster-coordinated | `dkg-bootstrap` (SSH) | ~2 дня |
| 2.1c-E | `node-deploy` | node-local | `cluster-deploy` (SSH) | ~½ дня |
| 2.1c-F | retire `dkg_bootstrap.rs` + `cluster_deploy.rs` | (delete) | (retiring debt) | ~½ дня |

Total: ~5 дней. В течение этого времени testnet оперирует на deprecated SSH-driven path'е; mainnet'а ещё нет. Deprecated-path operations во время transition'а explicitly понимаются как testnet-developer-convenience, не system-code, и никогда не информируют mainnet design.

## 12. Глоссарий

| Термин | Определение |
|---|---|
| Operator | Сторона, запускающая одну ноду в кластере. Владеет своей VM, своим enclave'ом, своим XRPL operator account'ом. Может быть человеком или организацией. |
| Node | VM одного оператора, запускающая enclave + orchestrator. |
| Enclave | Intel SGX TEE запускающий наш `enclave.signed.so` со специфичным MRENCLAVE. Sealed state bound to MRENCLAVE. |
| MRENCLAVE | SHA256-based identity конкретного enclave binary. Тот же source + тот же toolchain производит тот же MRENCLAVE (reproducible build). |
| Founder | Сторона выполняющая trusted-dealer escrow setup на genesis. После AccountSet asfDisableMaster у founder'а нет on-going authority. |
| Quorum | Required-weight threshold on-chain SignerList'а для multisig'а. K-of-N, с K ≥ 2 в любой реалистичной конфигурации. |
| FROST group | Set операторов участвовавших в одной DKG ceremony. Каждый участник держит sealed share; вместе они могут произвести одну BIP340-style подпись. |
| Path A v2 | ECDH-over-DCAP протокол для cross-machine share transport'а. Wraps shares в AES-128-GCM keyed на enclave-pair ECDH после того как обе стороны DCAP-аттестировали друг друга. |
| ECDH identity | Per-enclave secp256k1 keypair используемый только для Path A transport'а. Отличный от FROST signing keys. Public key публикуется on-chain через `AccountSet.Domain`. |
