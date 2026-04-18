# Perp DEX кластер — Dual-Mode + статус инфраструктуры

**Дата**: 2026-04-18
**Статус**: Текущее состояние + план для обзора.
**Автор**: AL + Claude.

Этот документ повторяет формат Phoenix PM `cluster_dual_mode_status`
чтобы дать коллегам единый источник правды:
- что построено и чего не хватает в инфраструктуре perp-DEX
- топология Hetzner/Azure и бюджет
- режимы работы dev/prod
- порядок оставшихся инфраструктурных задач

## 1. Режимы работы — Dev vs Prod

### Dev mode (Azure VM деаллоцированы, mainnet эскроу пуст)

Это **целевое состояние после вывода средств** (ожидаем подтверждение
Tom + kupermind).

- Hetzner запускает два изолированных инстанса параллельно:

```
Hetzner EX44 (94.130.18.162) — Xeon E3-1275 v6, 64GB RAM, SGX1 HW
├── MAINNET инстанс
│   ├── Enclave (perp-dex-server): порт 9088, SGX HW mode
│   ├── Оркестратор:               порт 3000, XRPL mainnet
│   ├── Sealed data:               /tmp/perp-9088/
│   └── БД:                        perp_dex (PostgreSQL)
│
└── DEV/STAGING инстанс
    ├── Enclave (perp-dex-server): порт 9089, SGX HW mode
    ├── Оркестратор:               порт 3001, XRPL testnet
    ├── Sealed data:               /tmp/perp-9089/
    └── БД:                        perp_dex_dev (PostgreSQL)
```

- libp2p mesh N=1 (только Hetzner). Hetzner — секвенсер тривиально.
- Однопоставщная криптография: XRPL single-account ECDSA (один ключ
  SGX на Hetzner). Без мультисига — Azure enclave'ы не нужны.
- Enclave и оркестратор обновляются свободно на dev-инстансе.
  Mainnet обновляется только после успешного тестирования на dev.
- **Стоимость Azure: €0.**

### Prod mode (Azure VM запущены)

- Кластер из 4 нод: Hetzner (priority=0, предпочтительный секвенсер) +
  3 Azure DCsv3 VM (priority 1/2/3).
- Все 4 ноды участвуют в: gossipsub mesh, выборе секвенсера,
  P2P signing relay.
- Распределённая подпись доступна:
  - **XRPL SignerListSet 2-of-3** через 3 Azure SGX enclave'а.
  - Подпись withdrawal через gossipsub (<10мс для кворума).
- DCAP remote attestation доступна (только Azure DCsv3 — 4734-байтные
  Intel-подписанные quote'ы).
- **Стоимость Azure: 3× DCsv3 VM ≈ €300/мес.**

### Переключение режимов

- `az vm deallocate -g SGX-RG -n sgx-node-{1,2,3}` → Dev mode.
- `az vm start -g SGX-RG -n sgx-node-{1,2,3}` → Prod mode.
- Без рестарта оркестратора на Hetzner. libp2p mesh адаптируется.
- Режим подписи деградирует: 2-of-3 мультисиг недоступен в dev mode,
  fallback на одну подпись.
- ~30 секунд при загрузке Azure VM до подключения enclave'ов.

## 2. Текущая ситуация — что построено, чего не хватает

### Инфраструктура (chain-agnostic)

| Компонент | Статус | Примечания |
|-----------|--------|------------|
| libp2p mesh + Noise mTLS | ✅ Готово | gossipsub, per-peer whitelist, persistent peer_id |
| Выбор секвенсера | ✅ Готово | priority + heartbeat, тест со split-brain |
| P2P signing relay | ✅ Готово | <10мс кворум на 3-нодном testnet кластере |
| Скрипт деплоя | ✅ Готово | `deploy.sh` — rolling deploy на Azure через Hetzner bastion |
| systemd (Azure) | ✅ Готово | `perp-dex-orchestrator.service` на всех 3 Azure нодах |
| systemd (Hetzner) | ❌ Нет | Hetzner работает через nohup, без auto-restart |
| Health endpoint | ✅ Готово | `/v1/health` — статус enclave, peers, роль, uptime, версия |
| Механизм отката | ❌ Нет | Только ручная замена бинарника |
| Трекинг версий | ❌ Нет | Нет `version.json` на нодах, нет трекинга MRENCLAVE |
| Мониторинг signer count | ❌ Нет | Нет алерта при SignerListSet count ≠ ожидаемому |

### Perp DEX (прикладной уровень)

| Компонент | Статус | Примечания |
|-----------|--------|------------|
| Perp engine (enclave) | ✅ Готово | 11 ecall'ов: депозит, вывод, открытие/закрытие позиции, ликвидация, фандинг, персистенция |
| CLOB order book | ✅ Готово | Мэтчинг внутри enclave, reduce_only IOC для закрытий |
| MM vault | ✅ Готово | `vault:mm` как пассивный маркет-мейкер на CLOB |
| Price feed | ✅ Готово | Binance WebSocket → enclave `update_price` |
| Liquidation loop | ✅ Готово | Периодический `check_liquidations` + авто-ликвидация |
| Withdrawal flow | ✅ Готово | Атомарная проверка маржи + ECDSA подпись в enclave, 2-of-3 мультисиг |
| Мониторинг депозитов | ✅ Готово | XRPL ledger polling для входящих платежей |
| DCAP аттестация | ✅ Готово | Только Azure — генерация + верификация quote |
| Shard-first архитектура | ✅ Готово | `shard_id` first-class в состоянии enclave, N=1 |
| Partitioned sealing | ✅ Готово | 5 000 пользователей на sealed-партицию |
| Локальная XRPL подпись | ✅ Готово | Ed25519 + secp256k1 полиморфная подпись, без Python |
| Frontend API | ✅ Готово | REST endpoints, HMAC auth, защита от replay |

### Безопасность / операции

| Элемент | Статус | Примечания |
|---------|--------|------------|
| Master key mainnet | ⚠️ НЕ отключён | Seed в plaintext JSON на диске Hetzner. План: вывести средства → отключить или создать новый. |
| Escrow SignerListSet | ✅ Настроен | 2-of-3 на ключах Azure enclave (testnet эскроу `rLTFG...`) |
| Mainnet SignerListSet | ⚠️ Косметический | Master key включён → мультисиг обходим |
| Тулинг обновления enclave | ❌ Не построен | Стратегия 4 (rolling key rotation) спроектирована, не реализована |
| Процедура обновления enclave | ✅ Документирована | `deployment-dilemma.md` — 8 векторов атак проанализированы |
| Тестирование отказов | ✅ Завершено | 11/11 сценариев пройдено на живом кластере |
| Аудит безопасности | ✅ Завершён | 52 находки, 50 исправлены, 2 документированы как by-design |

## 3. Текущее состояние кластера (live)

### Hetzner (94.130.18.162)

| Процесс | Порт | Статус | С |
|---------|------|--------|---|
| `ethsigner-server` (Phoenix PM enclave) | 8085 (HTTPS) | Работает | Apr 03 |
| `perp-dex-server` (perp enclave) | 9088 (HTTPS) | Работает | Apr 07 |
| `perp-dex-orchestrator` (mainnet) | 3000 | Работает | Apr 12 |
| nginx | 80/443 | Работает | — |

- SGX: HW mode (Kaby Lake, SGX1). Без DCAP аттестации.
- Enclave accounts: 48 sealed аккаунтов в `/tmp/perp-9088/accounts/`
- Perp state: sealed на диск (users, positions, vaults, tx hashes)
- Эскроу: `r4rwwSM9PUu7VcvPRWdu9pmZpmhCZS9mmc`, 108.36 XRP
- **Без systemd** — ручной nohup. Ребут = ручной перезапуск.
- `p2p_identity.key` в `/tmp/perp-9088/` — переживает ребут только
  случайно. Бэкап в `~/.secrets/`.

### Azure node-1 (20.71.184.176)

| Процесс | Порт | Статус | Роль |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Работает | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Работает | **Секвенсер** |

- Uptime: ~14ч, версия 0.1.0, 2 peer'а подключены
- systemd: `perp-dex-orchestrator.service` включён

### Azure node-2 (20.224.243.60)

| Процесс | Порт | Статус | Роль |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Работает | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Работает | **Валидатор** |

- Uptime: ~14ч, версия 0.1.0, 2 peer'а подключены

### Azure node-3 (52.236.130.102)

| Процесс | Порт | Статус | Роль |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Работает | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Работает | **Валидатор** |

- Uptime: ~14ч, версия 0.1.0, 2 peer'а подключены

### Почему Hetzner не в P2P mesh

Оркестратор на Hetzner работает с `--priority 0`, но подключен к XRPL
**mainnet**, тогда как Azure ноды подключены к **testnet**. Это
отдельные кластеры на одной физической машине.

После опустошения mainnet эскроу и переключения Hetzner на testnet,
Hetzner может присоединиться к Azure mesh как 4-я нода (priority=0,
предпочтительный секвенсер) — идентично архитектуре Phoenix PM.

## 4. Сравнение с Phoenix PM кластером

Оба проекта используют один и тот же enclave codebase и сходятся к
одинаковым инфраструктурным паттернам. Ключевые различия:

| Аспект | Perp DEX | Phoenix PM |
|--------|----------|------------|
| **Распределённая подпись** | XRPL SignerListSet 2-of-3 (3 ECDSA) через P2P relay | BTC FROST 2-of-3 (Schnorr) через libp2p + XRPL мультисиг через SSH тоннели |
| **SSH тоннели в подписи** | ✅ Убраны (P2P relay) | ⚠️ Ещё используются для XRPL мультисига |
| **Репликация состояния** | Не реализована (single-sequencer) | ✅ PG state-log + snapshot catch-up |
| **Внешний RPC corroboration** | Не реализовано | ✅ Race-corroboration агрегатор |
| **Singleton runner** | Не реализовано | ✅ «Запускать только на секвенсере» абстракция |
| **DCAP аттестация** | ✅ Работает на Azure | ✅ Работает на Azure |
| **FROST enclave fix** | Не cherry-picked (не используем FROST) | ✅ `9bd4f0d` — ECDH+AES-GCM cross-machine |
| **Production трафик** | ~0 TPS, 108 XRP в эскроу | 38 031 маркетов на api.ph18.io |
| **Управление процессами (Hetzner)** | ❌ nohup | ✅ systemd |

### Что перенять у Phoenix PM

1. **systemd на Hetzner** — у нас есть на Azure, но не на Hetzner.
   PM имеет `phoenix-pm-enclave.service`, `phoenix-rs.service`. Нужно
   добавить `perp-dex-server.service` + `perp-dex-orchestrator.service`.

2. **Singleton runner** (`singleton.rs`) — «запускать только на
   секвенсере». Полезно для vault MM, price feed, liquidation loop.
   Сейчас они работают на каждой ноде, что избыточно.

3. **Репликация state-log** — не срочно (наш state живёт в enclave,
   не в PG), но полезно для репликации order book и истории сделок
   на ноды-валидаторы.

4. **FROST enclave fix** — cherry-pick когда понадобится cross-machine
   FROST (не сейчас, а когда реализуем ротацию ключей Стратегии 4).

## 5. Ближайший план инфраструктурных работ

### Приоритет 0: Опустошить mainnet эскроу (блокирован Tom + kupermind)

- Tom и kupermind выводят свои XRP (~108.36 всего)
- Метод: простой XRPL Payment, подписанный master key (seed на диске)
- После вывода: Hetzner свободен для обновлений, нет риска для средств
- **Статус: ожидаем адреса получателей**

### Фаза I: Hetzner dual-instance (после опустошения эскроу, ~3 часа)

| Шаг | Результат | Риск |
|-----|-----------|------|
| I.1 | Сборка нового `perp-dex-server` + `perp-dex-orchestrator` из последнего кода | Нет — только сборка |
| I.2 | Создать каталог dev enclave `/tmp/perp-9089/` с testnet конфигом | Нет |
| I.3 | Запустить dev enclave на порту 9089, dev оркестратор на порту 3001 | Низкий — отдельный процесс |
| I.4 | Проверить health dev инстанса, создать тест-аккаунт, прогнать цикл deposit/trade на testnet | Нет |
| I.5 | Перенести `p2p_identity.key` в `~/.config/perp-dex/` (постоянное место) | Низкий — нужен рестарт |
| I.6 | Добавить systemd units для mainnet и dev инстансов на Hetzner | Низкий |

### Фаза II: Lifecycle деплоя (~1 день)

| Шаг | Результат |
|-----|-----------|
| II.1 | `deploy.sh rollback <node>` — восстановление предыдущего бинарника из `.prev` |
| II.2 | `version.json` на каждой ноде — хеш бинарника, git commit, MRENCLAVE, timestamp деплоя |
| II.3 | `deploy.sh status` — опрос всех нод: версия, uptime, health, signer count |
| II.4 | Health check на signer count SignerListSet — алерт при count ≠ ожидаемому |

### Фаза III: CLI онбординга операторов (~1-2 дня)

| Шаг | Результат |
|-----|-----------|
| III.1 | `orchestrator add-node --vm <ip> --port-base 9089` — одна команда для добавления ноды |
| III.2 | Автоматизация: SSH setup, копирование бинарника, генерация конфига, запуск enclave, health check |
| III.3 | `orchestrator remove-node --vm <ip>` — чистое выведение из эксплуатации |

## 6. Бюджет Azure VM

> **Ограничение общего ресурса (согласовано с Phoenix PM, 2026-04-18):**
> 3 Azure DCsv3 VM (sgx-node-1/2/3) являются **общими** между perp-DEX
> и Phoenix PM. Ни один проект не может односторонне их деаллоцировать.
> Два условия должны быть выполнены для отключения:
>
> 1. Phoenix PM завершил интеграцию XRPL и BTC кластера
>    (Фазы X + B в плане PM).
> 2. Perp-DEX достиг аналогичной dual-mode готовности (Фазы I-III
>    в этом плане).
>
> Пока оба условия не выполнены, VM работают для разделения затрат.
> Свойство «выключены по умолчанию» в dual-mode — это **целевое
> конечное состояние**, а не текущая реальность.

Что актуально сейчас:

| Фаза | Azure VM нужны | Доп. стоимость |
|------|----------------|----------------|
| Фаза I (Hetzner dual-instance) | Уже включены (shared) | €0 доп. |
| Фаза II (lifecycle деплоя) | Уже включены (shared) | €0 доп. |
| Фаза III (тест add-node) | Уже включены (shared) | €0 доп. |
| **Конечное состояние (оба проекта готовы)** | **Выключены по умолчанию** | €0 (цель) |
| Тестирование мультисига (конечное) | Включены временно | ~€5/сессия |
| Тестирование DCAP (конечное) | Включены временно | ~€5/сессия |

**В текущей фазе:** Azure VM работают 24/7 (shared cost с PM).
Никаких дополнительных затрат от нашей инфраструктурной работы.

**После достижения dual-mode обоими проектами:** Azure можно
деаллоцировать между активными тест-сессиями, резко снижая затраты.
Но это требует симметричной готовности — ни один проект не может
принудительно отключить shared VM.

### Регрессионное тестирование мультисига при разработке

Следуя подходу PM: FROST/мультисиг должен проверяться периодически,
а не только в конце.

- **По умолчанию для разработки:** single-signer режим на Hetzner dev
  инстансе. Быстро, без зависимости от Azure, полный локальный цикл.
- **Периодические регрессии:** минимум раз за завершение Фазы,
  запуск prod-mode end-to-end:
  1. Убедиться что Azure VM включены (в текущей фазе они включены).
  2. Создать SignerListSet 2-of-3 мультисиг транзакцию на testnet.
  3. Проверить что все 3 SGX signer'а участвовали через P2P relay.
  4. Подтвердить транзакцию на XRPL testnet.
- **Перед любым production cutover:** полный prod-mode regression suite
  должен пройти, не только dev-mode тесты.

## 7. Последствия для участников

### Frontend (xperp.fi)

- **Без перебоев в Фазах I-III.** Вся инфраструктурная работа
  на стороне сервера.
- Контракт `api-perp.ph18.io` не меняется.
- После опустошения эскроу API вернёт пустые балансы для
  существующих пользователей — ожидаемое поведение.
- Dev инстанс (порт 3001) не проброшен через nginx — только внутренний.

### Tom (8Baller)

- **Сначала вывод XRP.** Нужен твой XRPL-адрес получателя и адрес
  kupermind.
- **Твой архитектурный документ** не блокирован этой инфраструктурной
  работой. Мы рассмотрим его после завершения Фазы I.
- **Решения по vault/AMM/pricing** независимы — улучшения
  инфраструктуры полезны при любой модели мэтчинга.

### Аудиторы безопасности / грантовые ревьюеры

- `deployment-dilemma.md` документирует все стратегии обновления
  enclave с анализом поверхности атаки (8 векторов, каждый с защитой).
- Стратегия 2 (MRSIGNER) и Стратегия 3 (Recovery) явно ОТКЛОНЕНЫ
  с обоснованием.
- Стратегия 4 (rolling key rotation) — цель для mainnet, но ещё не
  реализована. Стратегия 1 (ручная dual-server) достаточна для MVP
  с доверенным оператором.
- 11/11 сценариев отказов верифицированы на живом кластере.

## 8. Открытые вопросы (не блокируют этот план)

1. **Hetzner как 4-я нода кластера** — после переключения mainnet на
   testnet Hetzner присоединяется к Azure mesh. Hetzner имеет SGX1
   (нет DCAP), поэтому может подписывать, но не может генерировать
   attestation quote'ы. Принять как non-DCAP peer или исключить из
   подписи? PM выбрал «state-only peer, без подписи на Hetzner» —
   нужно принять то же решение.

2. **Репликация состояния enclave** — состояние perp engine живёт
   sealed внутри enclave, не в PostgreSQL. Репликация требует либо
   (a) протокол enclave-to-enclave синхронизации, либо (b) принять
   что только секвенсер имеет авторитетное perp-состояние, а
   валидаторы — read-only relay. Вариант (b) проще и достаточен
   для текущего масштаба.

3. **Судьба master key** — после вывода средств: отключить master key
   на существующем эскроу и настроить правильный 2-of-3 мультисиг?
   Или создать свежий эскроу с нуля с отключённым master key? Свежий
   эскроу чище.

4. **Singleton runner** — vault MM, price feed и liquidation loop
   сейчас работают на каждой ноде. Нужно перенять паттерн singleton
   из PM (запускать только на секвенсере) чтобы избежать дублирования
   ордеров и конфликтующих ликвидаций. Не срочно при N=1, но
   обязательно до N>1.
