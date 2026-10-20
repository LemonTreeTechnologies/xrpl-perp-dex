# Order Signing v2 — add `market`, `nonce`, `expiry` to the client-signed order body

**Status: FINALIZED — auditor-ruled 2026-10-20** (see "Auditor rulings" at the end; the draft
below stands with the tightenings noted there). This is our API contract (our scope); Tom builds
the client as a black box against it. Enclave + orch impl pending; auditor will IMPL-review when built.

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


---

## Auditor rulings (FINAL, 2026-10-20) — the format is now stable for Tom

1. **nonce** = monotonic per-user u64 (NOT a seen-set/window). One u64/user is trivially re-derivable on replay (take the max); a window adds replicated state + an eviction boundary. Client serialises nonce assignment (a UX cost, not a security one).
2. **state + check** = per-user u64 in SEALED, replicated perp state, applied in ORDER-LOG order. The check-and-set lives in the DETERMINISTIC APPLY path, **never at ingress** (an ingress-only check isn't re-derivable → the co-sign would form on unverified nonces). SEALED is load-bearing: if `last_applied` reset on restart, every past nonce replays again.
3. **expiry** = refuse if `attested_close_time > expiry`, checked **at APPLY against the BATCH's attested time** (what the matcher stamped), not ingress against a live clock (else replay is non-deterministic). A resting order re-checked across batches uses each batch's attested time. `MAX_ORDER_TTL`: set as SHORT as the real max order lifetime allows (24h conservative default; tighten toward the true longest legitimate resting-order lifetime). nonce kills a leaked order when a later order applies; expiry kills it on wall-time regardless — complementary.
4. **market** = EXACT-BYTES string equality to the one configured market: no case-fold, trim, or unicode normalization (a canonicalization gap = cross-market injection). Not an integer id (an id needs a drift-prone registry). If many markets ever justify an id, it must be a stable append-only registry (ids never reused).
5. **X-XRPL-Timestamp** = KEPT, separate layer. It is the general request-auth replay guard over EVERY signed request (incl. non-order balance/account calls that have no nonce/expiry) — transport-freshness at ingress vs the server clock (±30s, non-deterministic by design, a filter). Do NOT collapse it into expiry (deterministic state check at apply vs the attested clock).

**Three cross-cutting (load-bearing):**
- All three fields live in the SIGNED BODY, never a header — in a header they are sequencer-forgeable and the checks are theater.
- The validator RE-PARSES the signed bytes and re-derives market/nonce/expiry from them, never from a struct the sequencer parsed (derived-not-received, same as the batch-time check).
- The exposed `last_applied_nonce` (on `GET /v1/account/balance`) is a CONVENIENCE for the client to pick the next nonce, NOT a trust input; the binding check is apply-time `nonce > last_applied` (fail-closed on a stale client nonce).

## Вердикты аудитора (ФИНАЛ, 2026-10-20) — формат стабилен для Tom

1. **nonce** = монотонный u64 на пользователя (НЕ seen-set). Тривиально воспроизводим на реплее (max); окно = лишнее реплицируемое состояние + граница вытеснения. Клиент сериализует назначение nonce (UX-цена, не security).
2. **состояние + проверка** = u64 на пользователя в ЗАПЕЧАТАННОМ реплицируемом perp-состоянии, применяется в порядке ORDER-LOG. Check-and-set в ДЕТЕРМИНИРОВАННОМ APPLY-пути, **не на ingress** (ingress-проверка не воспроизводима → co-sign соберётся на непроверенных nonce). SEALED критично: если `last_applied` сбросится на рестарте — все прошлые nonce снова проиграются.
3. **expiry** = отказ если `attested_close_time > expiry`, проверка **на APPLY против аттестованного времени БАТЧА** (что проштамповал матчер), не на ingress против живых часов (иначе реплей недетерминирован). `MAX_ORDER_TTL`: как можно КОРОЧЕ (24ч консервативно; ужимать к реальному максимуму жизни ордера). nonce убивает утёкший ордер при следующем ордере; expiry — по времени независимо.
4. **market** = ТОЧНЫЕ БАЙТЫ, строковое равенство настроенному рынку: без case-fold/trim/нормализации (разрыв канонизации = межрыночная инъекция). Не числовой id.
5. **X-XRPL-Timestamp** = ОСТАВИТЬ, отдельный слой: общий anti-replay транспорта над КАЖДЫМ подписанным запросом (вкл. balance/account без nonce/expiry), свежесть на ingress против серверных часов (±30с). НЕ сливать с expiry.

**Три сквозных:** все три поля в ПОДПИСАННОМ ТЕЛЕ, не в заголовке; валидатор ПЕРЕПАРСИВАЕТ подписанные байты (derived-not-received); отданный `last_applied_nonce` — удобство, не доверенный вход (связывает apply-time `nonce > last_applied`, fail-closed).
