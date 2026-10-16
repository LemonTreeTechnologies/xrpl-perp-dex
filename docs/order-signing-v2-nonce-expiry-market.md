# Order Signing v2 — add `market`, `nonce`, `expiry` to the client-signed order body

**Status: DRAFT.** Pending the auditor's security-design review of the nonce scheme (below); only
after that should Tom implement the client side and we implement the server side. This is our API
contract (our scope); the client is Tom's black box built against it.

Related: `docs/audit/REQ-matcher-determinism-q3-impl.md` (private enclave repo) — this is the
"nonce/expiry/market" part-2 item named there (auditor item-1 Challenge 2).

---

## EN

### Why
A client-signed order today binds its *terms*, so the sequencer cannot alter or invent one — but
a **replayed genuine order re-executes a fill the user authorised once**, which manufactures a
fill the user did not authorise (forged intent). The signed body also names no market (cross-market
injection once more than one market exists) and has no validity bound (a signed order can be
replayed indefinitely). v2 closes all three: uniqueness (`nonce`), a validity window (`expiry`),
and market binding (`market`).

### v1 recap (mechanism UNCHANGED)
- Headers: `X-XRPL-Address`, `X-XRPL-PublicKey` (compressed hex), `X-XRPL-Signature`
  (secp256k1 DER, or ed25519 64-byte raw), `X-XRPL-Timestamp` (unix seconds, ±30s drift).
- Signature is over `SHA-256(<raw JSON body bytes> || <X-XRPL-Timestamp ASCII>)`; the XRPL
  SHA-512Half wrap is also accepted. The signature covers the **exact body bytes sent**.
- Body (v1): `{"user_id","side","type","size","leverage","time_in_force","reduce_only"[,"price"]}`.

### v2 — three new REQUIRED fields in the signed body
| field | type | meaning |
|---|---|---|
| `market` | string | e.g. `"XRP-USD-PERP"`. Server refuses if it ≠ the server's configured market. |
| `nonce` | integer (unsigned) | Strictly increasing per signer. The enclave tracks the highest `nonce` it has APPLIED per user in sealed, replicated state; an order with `nonce ≤ last_applied` is refused (replay/duplication). |
| `expiry` | integer (unix seconds) | The order is invalid once the enclave's ATTESTED clock passes it. Checked against the derived attested close-time, NOT wall-clock. |

The headers, the signing algorithm, and the `X-XRPL-Timestamp` drift window are unchanged; only
the JSON body gains the three fields (so the signature now covers them).

### Server/enclave checks (added, on top of the v1 owner==signer + terms checks)
1. `market == configured_market`, else refuse.
2. `nonce > last_applied_nonce[signer]`, else refuse (replay); on apply set `last_applied_nonce[signer] = nonce`.
3. `attested_close_time_unix ≤ expiry`, else refuse (expired). Also bound `expiry ≤ now + MAX_ORDER_TTL`
   (proposed 24h) to cap the replay window. If the attested clock is not live, matching is already
   paused (see increment 1), so this never reads a wall-clock.

### Nonce management (client side — Tom)
The client MUST send a strictly-increasing `nonce` per user. To survive client restarts and
concurrent tabs, the enclave is the source of truth: we add the user's current `last_applied_nonce`
to `GET /v1/account/balance`, and the client sends `nonce = max(local_next, server_last + 1)`.
Concurrent orders from one user must serialise nonce assignment client-side.

### Impact (who builds what)
- **Enclave (ours, private repo):** per-user `last_applied_nonce` in sealed perp state; the three
  checks in the order-auth path; expose `last_applied_nonce`.
- **Orchestrator (ours):** carry `market/nonce/expiry` in the signed body + `OrderMessage`;
  `verify_replicated_order` re-checks them against the re-parsed signed body; validators track the
  same nonce state (derived, not received — like the batch-time check).
- **Client (Tom):** include the three fields in the signed body + signature; manage the nonce via
  the exposed `last_applied_nonce`.

### Open for the auditor (security design) — resolve BEFORE Tom implements
1. Nonce scheme: monotonic per-user u64 (this draft) vs a seen-set/window. Monotonic is minimal
   derivable state (one u64/user) but needs the client to serialise nonce assignment.
2. Nonce state placement: per-user u64 in the sealed/replicated perp state (part of the ordered
   log, per the item-1 ruling) — confirm.
3. Expiry vs attested clock (refuse if `attested_unix > expiry`) + the `MAX_ORDER_TTL` bound — confirm.
4. Market binding by string equality vs an integer id — confirm.
5. Keep the `X-XRPL-Timestamp` drift window alongside `nonce`+`expiry`? (Request-freshness and
   order-uniqueness are different layers; this draft keeps both.)

---

## RU

### Зачем
Подписанный клиентом ордер уже привязывает свои *условия* — секвенсер не может их изменить или
выдумать ордер. Но **повторно проигранный НАСТОЯЩИЙ ордер исполняет сделку, которую пользователь
авторизовал один раз**, т.е. фабрикует неавторизованную сделку (подделка намерения). К тому же в
подписанном теле не назван рынок (межрыночная инъекция, когда рынков станет больше одного) и нет
границы валидности (подписанный ордер можно проигрывать бесконечно). v2 закрывает всё три:
уникальность (`nonce`), окно валидности (`expiry`), привязка к рынку (`market`).

### v1 (механизм НЕ меняется)
- Заголовки: `X-XRPL-Address`, `X-XRPL-PublicKey` (сжатый hex), `X-XRPL-Signature`
  (secp256k1 DER или ed25519 64 байта), `X-XRPL-Timestamp` (unix-секунды, дрейф ±30с).
- Подпись над `SHA-256(<сырые байты JSON-тела> || <X-XRPL-Timestamp ASCII>)`; принимается и
  обёртка XRPL SHA-512Half. Подпись покрывает **точные отправленные байты тела**.
- Тело (v1): `{"user_id","side","type","size","leverage","time_in_force","reduce_only"[,"price"]}`.

### v2 — три новых ОБЯЗАТЕЛЬНЫХ поля в подписанном теле
| поле | тип | смысл |
|---|---|---|
| `market` | строка | напр. `"XRP-USD-PERP"`. Сервер отказывает, если ≠ его настроенному рынку. |
| `nonce` | целое (беззнаковое) | Строго возрастает на пользователя. Анклав хранит максимальный ПРИМЕНЁННЫЙ `nonce` на пользователя в запечатанном реплицируемом состоянии; ордер с `nonce ≤ last_applied` отклоняется (повтор/дублирование). |
| `expiry` | целое (unix-секунды) | Ордер недействителен, как только АТТЕСТОВАННЫЕ часы анклава прошли это время. Проверяется против выведенного attested-времени, НЕ системного. |

Заголовки, алгоритм подписи и окно дрейфа `X-XRPL-Timestamp` не меняются; меняется только JSON-тело
(поэтому подпись теперь покрывает эти три поля).

### Проверки на сервере/в анклаве (в дополнение к v1: владелец==подписант + условия)
1. `market == настроенный_рынок`, иначе отказ.
2. `nonce > last_applied_nonce[подписант]`, иначе отказ (повтор); при применении `last_applied_nonce[подписант] = nonce`.
3. `attested_close_time_unix ≤ expiry`, иначе отказ (просрочен). Плюс ограничение
   `expiry ≤ now + MAX_ORDER_TTL` (предлагаю 24ч) — чтобы ограничить окно повтора. Если
   аттестованные часы не живые, матчинг и так на паузе (increment 1), системное время не читается.

### Управление nonce (клиент — Tom)
Клиент ОБЯЗАН слать строго возрастающий `nonce` на пользователя. Чтобы пережить перезапуски клиента
и параллельные вкладки, источник истины — анклав: добавляем текущий `last_applied_nonce` в
`GET /v1/account/balance`, клиент шлёт `nonce = max(local_next, server_last + 1)`. Параллельные
ордера одного пользователя должны сериализовать назначение nonce на стороне клиента.

### Кто что строит
- **Анклав (наше, приватный репо):** `last_applied_nonce` на пользователя в запечатанном
  perp-состоянии; три проверки в пути авторизации ордера; отдавать `last_applied_nonce`.
- **Оркестратор (наше):** нести `market/nonce/expiry` в подписанном теле + `OrderMessage`;
  `verify_replicated_order` перепроверяет их против перепарсенного подписанного тела; валидаторы
  отслеживают то же nonce-состояние (выведено, не получено — как проверка времени батча).
- **Клиент (Tom):** включить три поля в подписанное тело + подпись; управлять nonce через
  отданный `last_applied_nonce`.

### На решение аудитору (дизайн безопасности) — ДО того, как Tom начнёт
1. Схема nonce: монотонный u64 на пользователя (этот черновик) vs seen-set/окно.
2. Размещение состояния nonce: u64 на пользователя в запечатанном/реплицируемом perp-состоянии — подтвердить.
3. Expiry против аттестованных часов + ограничение `MAX_ORDER_TTL` — подтвердить.
4. Привязка рынка по равенству строк vs числовой id — подтвердить.
5. Оставить окно дрейфа `X-XRPL-Timestamp` вместе с `nonce`+`expiry`? (Свежесть запроса и
   уникальность ордера — разные слои; черновик оставляет оба.)
