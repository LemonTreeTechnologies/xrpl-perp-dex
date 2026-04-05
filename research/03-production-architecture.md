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
│                              ▼                                │
│                     ┌──────────────────┐                      │
│                     │  SGX Enclave     │                      │
│                     │  :9088           │                      │
│                     │  TCSNum=1        │                      │
│                     │  ECDSA key       │                      │
│                     └──────────────────┘                      │
│                                                               │
│   Orchestrator также:                                         │
│       ├──► XRPL Mainnet (deposit monitor)                    │
│       ├──► Binance/CEX (price feed)                          │
│       └──► P2P gossipsub (order replication)                 │
└──────────────────────────────────────────────────────────────┘
```

**Архитектура одного оператора:** nginx → Orchestrator → Enclave.
- **nginx** терминирует TLS, проксирует к Orchestrator (:3000)
- **Orchestrator** (Rust, multi-threaded) управляет concurrency — сериализует запросы к enclave через Mutex
- **Enclave** (TCSNum=1, однопоточный) — получает один запрос за раз от Orchestrator

Этот документ описывает архитектуру **одного оператора (одного сервера)**.
Multi-operator координация (3 оператора, XRPL multisig 2-of-3, P2P) описана
в [04-multi-operator-architecture](04-multi-operator-architecture.md).

---

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
    server_name api-perp.ph18.io;

    # === Whitelist (public после запуска, пока — ограниченный доступ) ===
    # include /etc/nginx/perp_whitelist.conf;

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

    # CORS preflight
    if ($request_method = 'OPTIONS') {
        add_header 'Access-Control-Allow-Origin' '*';
        add_header 'Access-Control-Allow-Methods' 'GET, POST, OPTIONS';
        add_header 'Access-Control-Allow-Headers' '*';
        add_header 'Content-Length' 0;
        add_header 'Content-Type' 'text/plain';
        return 204;
    }

    listen 443 ssl; # managed by Certbot
    ssl_certificate /etc/letsencrypt/live/api-perp.ph18.io/fullchain.pem; # managed by Certbot
    ssl_certificate_key /etc/letsencrypt/live/api-perp.ph18.io/privkey.pem; # managed by Certbot
    include /etc/letsencrypt/options-ssl-nginx.conf; # managed by Certbot
    ssl_dhparam /etc/letsencrypt/ssl-dhparams.pem; # managed by Certbot
}

server {
    if ($host = api-perp.ph18.io) {
        return 301 https://$host$request_uri;
    } # managed by Certbot

    listen 80;
    server_name api-perp.ph18.io;
    return 404; # managed by Certbot
}
```

Опционально — IP whitelist через `/etc/nginx/perp_whitelist.conf` (аналогично PM API).
На этапе тестирования доступ ограничен, после запуска — открыт для всех (whitelist убирается).

**Whitelist-подход:** nginx проксирует **только явно перечисленные** public endpoints.
Всё остальное (включая internal endpoints вроде `/v1/perp/deposit`, `/v1/perp/price`,
`/v1/pool/generate` и т.д.) попадает в `location /` → `return 403`. Это безопаснее
чем blacklist: если в enclave появится новый endpoint, он **не будет доступен** извне
пока его явно не добавят в nginx конфигурацию.

**Concurrency:** Orchestrator использует `tokio::sync::Mutex` для сериализации
запросов к enclave. Это гарантирует что однопоточный enclave
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

### 2. SGX Enclave (perp-dex-server)

- Один инстанс на порту 9088
- TCSNum=1 (single-threaded)
- ECDSA ключ генерируется внутри enclave (не извлекаем)
- State sealed на диск (партиционированно, 5 частей по <64KB)
- Слушает на 127.0.0.1 (не доступен извне)
- DCAP remote attestation (Azure DCsv3)

### 3. Orchestrator (Rust binary)

- Один процесс, слушает на :3000 (localhost, за nginx)
- Подключается **напрямую** к enclave (localhost:9088)
- Сериализует запросы через `tokio::sync::Mutex` (один запрос за раз)
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

- Escrow аккаунт контролируется SGX ECDSA ключом
- RLUSD collateral на escrow
- Deposits: пользователь → Payment → escrow → Orchestrator детектит → enclave кредитует
- Withdrawals: enclave проверяет margin → подписывает → Orchestrator отправляет на XRPL
- Multi-operator (multisig 2-of-3) — см. [04-multi-operator-architecture](04-multi-operator-architecture.md)

---

## Сетевые правила

```
# Enclave — только localhost
iptables -A INPUT -p tcp --dport 9088 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088 -j DROP

# nginx — публичный
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — слушает :3000 только localhost
iptables -A INPUT -p tcp --dport 3000 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 3000 -j DROP
# Исходящие: localhost:9088, XRPL (51234), Binance (443)
```

---

## Порты

| Порт | Сервис | Доступ |
|------|--------|--------|
| 443 | nginx (public API) | Internet |
| 3000 | Orchestrator | localhost only |
| 9088 | SGX Enclave | localhost only |
| 8085-8087 | Phoenix PM (не трогать) | localhost only |
