# Production Architecture

**Дата:** 2026-03-30
**Статус:** Проектирование

---

## Обзор

```
┌──────────────────────────────────────────────────────────────┐
│                        Internet                               │
│                                                               │
│   User/Trader ─── HTTPS ───► nginx :443                      │
│                          (api-perp.ph18.io)                   │
│                               │                               │
│                               ▼                               │
│                     ┌──────────────────┐                      │
│                     │  Orchestrator    │                      │
│                     │  (Rust :3000)    │                      │
│                     │  Order book      │                      │
│                     │  Auth (XRPL sig) │                      │
│                     │  Mutex → enclave │                      │
│                     └────────┬─────────┘                      │
│                              │ HTTPS (localhost)              │
│                     ┌────────┼────────┐                      │
│                     ▼        ▼        ▼                      │
│                  :9088    :9089    :9090                      │
│                ┌────────────────────────┐                    │
│                │  SGX Enclave Instances  │                    │
│                │  (perp-dex-server)      │                    │
│                │  TCSNum=1, однопоточные │                    │
│                │  XRPL multisig 2-of-3   │                    │
│                └────────────────────────┘                    │
│                              │                                │
│   Orchestrator также:                                         │
│       ├──► XRPL Mainnet (deposit monitor)                    │
│       ├──► Binance/CEX (price feed)                          │
│       └──► P2P gossipsub (order replication)                 │
└──────────────────────────────────────────────────────────────┘
```

**Архитектура:** nginx → Orchestrator → Enclave.
- **nginx** терминирует TLS, проксирует к Orchestrator (:3000)
- **Orchestrator** (Rust, multi-threaded) управляет concurrency — сериализует запросы к enclave через Mutex
- **Enclave** (TCSNum=1, однопоточный) — получает один запрос за раз от Orchestrator

Enclave instances не доступны напрямую из интернета (только localhost).

---

## MVP vs Production: зачем 3 инстанса enclave

### MVP (текущее состояние)

```
nginx :443 → Orchestrator :3000 → Enclave :9088 (один инстанс)
```

Один enclave instance достаточен для полной демонстрации всей логики:
deposit → trade → liquidation → funding → withdraw. Escrow аккаунт на XRPL
контролируется одним ключом (обычная подпись).

### Production (целевое состояние)

```
nginx :443 → Orchestrator :3000 → Enclave :9088 ─┐
                                 → Enclave :9089 ─┤ SignerListSet 2-of-3
                                 → Enclave :9090 ─┘
```

3 инстанса нужны **не для производительности** (каждый всё равно однопоточный),
а для **безопасности вывода средств**:

1. **XRPL native multisig (SignerListSet quorum=2).** Escrow аккаунт настроен
   так, что для отправки Payment (withdrawal) нужны подписи от минимум 2 из 3
   инстансов. Master key отключён — единственная точка отказа устранена.

2. **Защита от компрометации одного инстанса.** Даже если злоумышленник получит
   доступ к памяти одного enclave (side-channel атака, уязвимость SGX), он получит
   только 1 из 3 ключей — этого недостаточно для подписи withdrawal транзакции.

3. **Отказоустойчивость.** Если один инстанс падает или его sealed state
   повреждён, оставшиеся 2 продолжают обслуживать withdrawals (2-of-3).

4. **Независимая верификация.** Каждый инстанс независимо проверяет margin
   перед подписью. Orchestrator собирает 2 подписи и формирует XRPL Signers
   array для отправки на ledger.

> **Примечание:** Все 3 инстанса запускают идентичный `enclave.signed.so`
> (одинаковый MRENCLAVE), что верифицируется через DCAP remote attestation.
> Разница только в сгенерированных ECDSA ключах.

---

## API разделение: Public vs Internal

### Public API (через nginx, доступно пользователям)

| Method | Endpoint | Описание |
|--------|----------|----------|
| GET | `/v1/perp/balance` | Баланс и позиции пользователя |
| POST | `/v1/perp/position/open` | Открыть позицию |
| POST | `/v1/perp/position/close` | Закрыть позицию |
| POST | `/v1/perp/withdraw` | Вывод средств (margin check + SGX signing) |
| GET | `/v1/perp/liquidations/check` | Посмотреть ликвидируемые позиции |
| GET | `/v1/pool/status` | Статус enclave |
| POST | `/v1/pool/report` | Attestation report (legacy) |
| POST | `/v1/attestation/quote` | DCAP remote attestation (SGX Quote v3, Azure DCsv3 only) |

### Internal API (только localhost, недоступно извне)

| Method | Endpoint | Описание | Вызывается |
|--------|----------|----------|------------|
| POST | `/v1/perp/deposit` | Кредит депозита | Orchestrator |
| POST | `/v1/perp/price` | Обновление цены | Orchestrator |
| POST | `/v1/perp/liquidate` | Исполнение ликвидации | Orchestrator |
| POST | `/v1/perp/funding/apply` | Применение funding rate | Orchestrator |
| POST | `/v1/perp/state/save` | Сохранение состояния | Orchestrator |
| POST | `/v1/perp/state/load` | Загрузка состояния | Orchestrator |
| POST | `/v1/pool/generate` | Генерация ключей | Admin |
| POST | `/v1/pool/sign` | Прямая подпись | Admin |
| POST | `/v1/pool/frost/*` | FROST operations (Bitcoin Taproot, не XRPL) | Admin |
| POST | `/v1/pool/dkg/*` | DKG operations (Bitcoin Taproot, не XRPL) | Admin |

---

## nginx конфигурация

```nginx
# /etc/nginx/sites-available/api-perp.ph18.io

server {
    listen 443 ssl http2;
    server_name api-perp.ph18.io;

    ssl_certificate     /etc/letsencrypt/live/api-perp.ph18.io/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/api-perp.ph18.io/privkey.pem;

    # === Public API → Orchestrator (:3000) ===
    # Orchestrator handles auth, orderbook, concurrency (Mutex → enclave)

    location /v1/perp/balance     { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/position/   { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/withdraw    { proxy_pass http://127.0.0.1:3000; }
    location /v1/perp/liquidations/check { proxy_pass http://127.0.0.1:3000; }
    location /v1/pool/status      { proxy_pass http://127.0.0.1:3000; }
    location /v1/pool/report      { proxy_pass http://127.0.0.1:3000; }
    location /v1/attestation/     { proxy_pass http://127.0.0.1:3000; }

    # WebSocket (orderbook, trades, liquidations)
    location /ws {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 86400;
    }

    # Block everything else — internal endpoints not exposed
    location / {
        return 403;
    }

    # Standard proxy headers
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;

    # Rate limiting
    limit_req zone=perp_api burst=20 nodelay;
}

# Rate limit zone (in http block)
# limit_req_zone $binary_remote_addr zone=perp_api:10m rate=10r/s;
```

**Concurrency:** Orchestrator использует `tokio::sync::Mutex` для сериализации
запросов к каждому enclave instance. Это гарантирует что однопоточный enclave
(TCSNum=1) не получит параллельных ecalls. nginx проксирует только к
Orchestrator — прямой доступ к enclave невозможен.

---

## Компоненты

### 1. nginx (reverse proxy)

- Терминирует TLS для пользователей (Let's Encrypt)
- Проксирует только public endpoints к Orchestrator (:3000)
- Блокирует всё остальное (return 403)
- WebSocket support для real-time данных
- Rate limiting на пользовательские endpoints

### 2. SGX Enclave Instances (perp-dex-server)

- 3 инстанса на портах 9088-9090
- Каждый со своим `enclave.signed.so` (одинаковый MRENCLAVE)
- TCSNum=1 (single-threaded per instance)
- XRPL native multisig (SignerListSet): каждый инстанс держит свой независимый ECDSA ключ
- State sealed на диск (per-instance)
- Слушают на 127.0.0.1 (не доступны извне напрямую)

### 3. Orchestrator (Rust binary)

- Один процесс, слушает на :3000 (localhost, за nginx)
- Подключается **напрямую** к enclave instances (localhost:9088-9090)
- Сериализует запросы через `tokio::sync::Mutex` (один запрос за раз к каждому instance)
- XRPL signature auth для пользовательских запросов
- CLOB orderbook с price-time priority
- libp2p gossipsub для репликации order flow между операторами
- Функции:
  - **Price feed**: Binance API → enclave price update (каждые 5 сек)
  - **Deposit monitor**: XRPL ledger → enclave deposit credit
  - **Liquidation**: enclave check → enclave liquidate (каждые 10 сек)
  - **Funding rate**: вычисление + применение (каждые 8 часов)
  - **State save**: периодическое сохранение (каждые 5 минут)

### 4. XRPL Mainnet

- Escrow аккаунт контролируется SGX (3 независимых ECDSA ключа, SignerListSet quorum=2, master key disabled)
- RLUSD collateral на escrow
- Deposits: пользователь → Payment → escrow → Orchestrator детектит → enclave кредитует
- Withdrawals: пользователь запрашивает → enclave проверяет margin → orchestrator собирает 2 ECDSA подписи от 2 инстансов → собирает Signers array → отправляет на XRPL

---

## Сетевые правила

```
# Enclave instances — только localhost
iptables -A INPUT -p tcp --dport 9088:9099 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088:9099 -j DROP

# nginx — публичный
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — слушает :3000 только localhost
iptables -A INPUT -p tcp --dport 3000 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 3000 -j DROP
# Исходящие: localhost:9088-9090, XRPL (51234), Binance (443)
```

---

## Порты

| Порт | Сервис | Доступ |
|------|--------|--------|
| 443 | nginx (public API) | Internet |
| 3000 | Orchestrator | localhost only |
| 9088 | Enclave instance 1 | localhost only |
| 9089 | Enclave instance 2 | localhost only |
| 9090 | Enclave instance 3 | localhost only |
| 8085-8087 | Phoenix PM (не трогать) | localhost only |
