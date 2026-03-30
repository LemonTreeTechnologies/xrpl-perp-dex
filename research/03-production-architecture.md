# Production Architecture

**Дата:** 2026-03-30
**Статус:** Проектирование

---

## Обзор

```
┌─────────────────────────────────────────────────────────────┐
│                        Internet                              │
│                                                              │
│   User/Trader ──── HTTPS ────► HAProxy :443                 │
│                                   │                          │
│                          ┌────────┼────────┐                │
│                          ▼        ▼        ▼                │
│                       :9088    :9089    :9090                │
│                     ┌────────────────────────┐              │
│                     │  SGX Enclave Instances  │              │
│                     │  (perp-dex-server)      │              │
│                     │  FROST 2-of-3 signing   │              │
│                     └────────────────────────┘              │
│                          ▲                                   │
│                          │ localhost only                    │
│                     ┌────┴─────┐                            │
│                     │Orchestrator│                           │
│                     │  (Rust)    │                           │
│                     └────┬──┬───┘                           │
│                          │  │                                │
│                    ┌─────┘  └──────┐                        │
│                    ▼               ▼                         │
│              XRPL Mainnet    Binance/CEX                    │
│            (deposit monitor)  (price feed)                  │
└─────────────────────────────────────────────────────────────┘
```

---

## API разделение: Public vs Internal

### Public API (через HAProxy, доступно пользователям)

| Method | Endpoint | Описание |
|--------|----------|----------|
| GET | `/v1/perp/balance` | Баланс и позиции пользователя |
| POST | `/v1/perp/position/open` | Открыть позицию |
| POST | `/v1/perp/position/close` | Закрыть позицию |
| POST | `/v1/perp/withdraw` | Вывод средств (margin check + SGX signing) |
| GET | `/v1/perp/liquidations/check` | Посмотреть ликвидируемые позиции |
| GET | `/v1/pool/status` | Статус enclave |
| POST | `/v1/pool/report` | Attestation report |

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
| POST | `/v1/pool/frost/*` | FROST operations | Admin |
| POST | `/v1/pool/dkg/*` | DKG operations | Admin |

---

## HAProxy конфигурация

```haproxy
frontend perp-dex
    bind *:443 ssl crt /etc/ssl/perp.pem
    mode http

    # Block internal endpoints
    acl is_internal path_beg /v1/perp/deposit
    acl is_internal path_beg /v1/perp/price
    acl is_internal path_beg /v1/perp/liquidate
    acl is_internal path_beg /v1/perp/funding
    acl is_internal path_beg /v1/perp/state
    acl is_internal path_beg /v1/pool/generate
    acl is_internal path_beg /v1/pool/sign
    acl is_internal path_beg /v1/pool/frost
    acl is_internal path_beg /v1/pool/dkg
    acl is_internal path_beg /v1/pool/load
    acl is_internal path_beg /v1/pool/unload
    acl is_internal path_beg /v1/pool/schnorr
    acl is_internal path_beg /v1/pool/musig
    acl is_internal path_beg /v1/pool/regenerate
    acl is_internal path_beg /v1/pool/validate
    acl is_internal path_beg /v1/pool/recovery
    http-request deny if is_internal

    default_backend enclave_instances

backend enclave_instances
    mode http
    balance roundrobin
    option httpchk GET /v1/pool/status
    server enclave1 127.0.0.1:9088 check ssl verify none
    server enclave2 127.0.0.1:9089 check ssl verify none
    server enclave3 127.0.0.1:9090 check ssl verify none
```

---

## Компоненты

### 1. HAProxy/nginx (reverse proxy)

- Терминирует TLS для пользователей
- Блокирует internal endpoints
- Round-robin между enclave instances
- Health check через `/v1/pool/status`
- Rate limiting на пользовательские endpoints

### 2. SGX Enclave Instances (perp-dex-server)

- 3 инстанса на портах 9088-9090
- Каждый со своим `enclave.signed.so` (одинаковый MRENCLAVE)
- TCSNum=1 (single-threaded per instance)
- FROST 2-of-3: каждый инстанс держит свой share
- State sealed на диск (per-instance)
- Слушают на 127.0.0.1 (не доступны извне напрямую)

### 3. Orchestrator (Rust binary)

- Один процесс, работает на localhost
- Подключается к enclave instance напрямую (127.0.0.1:9088)
- Функции:
  - **Price feed**: Binance API → enclave price update (каждые 5 сек)
  - **Deposit monitor**: XRPL ledger → enclave deposit credit
  - **Liquidation**: enclave check → enclave liquidate (каждые 10 сек)
  - **Funding rate**: вычисление + применение (каждые 8 часов)
  - **State save**: периодическое сохранение (каждые 5 минут)

### 4. XRPL Mainnet

- Escrow аккаунт контролируется SGX (приватный ключ внутри enclave)
- RLUSD collateral на escrow
- Deposits: пользователь → Payment → escrow → Orchestrator детектит → enclave кредитует
- Withdrawals: пользователь запрашивает → enclave проверяет margin → SGX подписывает Payment → XRPL

---

## Сетевые правила

```
# Enclave instances — только localhost
iptables -A INPUT -p tcp --dport 9088:9099 -s 127.0.0.1 -j ACCEPT
iptables -A INPUT -p tcp --dport 9088:9099 -j DROP

# HAProxy — публичный
iptables -A INPUT -p tcp --dport 443 -j ACCEPT

# Orchestrator — не слушает портов, только исходящие:
#   → localhost:9088 (enclave)
#   → XRPL nodes (port 51234)
#   → Binance API (port 443)
```

---

## Порты

| Порт | Сервис | Доступ |
|------|--------|--------|
| 443 | HAProxy (public API) | Internet |
| 9088 | Enclave instance 1 | localhost only |
| 9089 | Enclave instance 2 | localhost only |
| 9090 | Enclave instance 3 | localhost only |
| 8085-8087 | Phoenix PM (не трогать) | localhost only |
