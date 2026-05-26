# V1 vault E2E отчёт — 2026-05-26

**Branch:** `LemonTreeTechnologies/xrpl-perp-dex` — `feat/v1-vault-vamm` @ `6815958`
**Кластер:** Azure 3-node testnet-cluster (`testnet-cluster` env)
**Tom-facing endpoint:** `https://api-dev.xperp.fi` (Hetzner nginx TLS-fronts Azure cluster, passive failover via `proxy_next_upstream`)
**Контекст:** REQ-19 PASS + RESP-19-addendum PASS уже на `private:main`; E2E запущен post-closure для product-spec-faithfulness validation и готовности к Tom handoff.

## 0. Вердикт

**PASS для V1 vault scope** (Tom-spec соответствие, external consumer accessibility, корректность failover'а, persistent state через respawn).

Один реальный баг найден и исправлен во время E2E (E-2E-1, nginx config). Три сценария explicitly deferred с обоснованием.

## 1. Запущенные сценарии

| ID | Сценарий | Вердикт | Заметки |
|---|---|---|---|
| T1.1 | Hysteresis (UtilHot enter → close → resume below threshold) | PASS | util 0.152 → 0.028 через reduce cycle, no flapping |
| T1.3 | Multi-fill ladder consumption (один IOC через 4 уровня) | PASS | 3 позиции tracked, UtilHot picks largest-first |
| T2.2 | Sequencer kill mid-active-vault-state | PASS | nginx transparent retry, 5/5 signed POST 200 @ 67-153ms во время failover'а |
| T2.3 | Restart с активными позициями | PASS (через T2.2) | Vault respawn re-initializes Curve + recomputes posture из live balance |
| T1.2 | Refresh threshold ≥5bps | SKIPPED | Unit-tested (`vamm::tests`); deterministic external repro hard |
| T1.4 | Posture boundary util=0.0499 vs 0.0501 | SKIPPED | То же что T1.2 |
| T1.5 | DeltaHot posture | SKIPPED | Требует `delta_cap` config override; не exercised smoke TOML |
| T2.1 | Sequencer kill mid-pending-close | COVERED через T2.2 | Тот же code path (singleton respawn re-fires UtilHot с нового sequencer'а) |
| T2.4 | Mark feed glitch handling | DEFERRED | Естественно наблюдается (`XRPL account_tx error: Ledger indexes invalid` warns) — vault loop выживает |

## 2. E2E находки

### E-2E-1 — nginx `proxy_next_upstream` пропускал `non_idempotent`

**Surfaced by:** external `api-dev.xperp.fi` probe pattern (loopback testing missed by design).

**Симптом:** signed POST `/v1/orders` возвращал 503 «this node is not the sequencer» в ~3 из 5 попыток. nginx round-robin выбирал одну из трёх Azure нод; validator возвращал 503; nginx **НЕ retry'ил** хотя `proxy_next_upstream error timeout http_503` был выставлен.

**Корневая причина:** nginx по умолчанию treat'ит POST как non-idempotent — `proxy_next_upstream` НЕ retry'ит POST requests без явного ключевого слова `non_idempotent`. Раньшний failover test (kill node-1 → signed POST succeeded на node-2) работал только потому что *connection failed до отправки request* (TCP refused = `error`, always retryable). Когда нода accept'ит и *отвечает* 503 на POST — nginx по умолчанию НЕ retry'ит.

**Fix:** добавить `non_idempotent` к upstream directive:
```nginx
proxy_next_upstream error timeout http_503 non_idempotent;
proxy_next_upstream_tries 3;
```

**Verified post-fix:** 5/5 signed POSTs вернули 200 OK, затем 5/5 returned 200 OK во время deliberate node-1 kill / failover.

**Methodology fit:** ровно тот class который INV-OPS-2 / direction-1b (g) был designed catch — invisible from loopback testing, surfaces только из external network position with consumer'ского actual request pattern (POST with body).

**Operational impact:** Tom (и любой external client) без этого fix'а видел бы intermittent 503 errors в ~2/3 write requests без observable паттерна (round-robin randomized). Легко misdiagnose как «cluster instability» вместо «nginx config gap».

### E-2E-2 — vault closes остаются open без external liquidity (expected)

vault'ский UtilHot reduce path использует `submit_close_order` (REQ-19-addendum X-19-1 fix) который шлёт market IOC. Без opposing-side liquidity на orderbook'е, IOC cancels без fill'ов. vault'ская `UtilHot persistent — reducing` log message fires каждый tick пока liquidity не появится (или другой fill drain'ит position).

Это **НЕ баг** — корректная семантика «cancel-first / close-only-if-still-over». Документируется для awareness'а Tom'а: vault под UtilHot нуждается в external counterparty для position relief. В production'е multi-LP / arb-bot environment ensure'ит что это always exists.

## 3. State после run'а

Vault на Azure node-1 (sequencer): 3 open short positions (residual от T1.3 multi-fill), util ≈ 0.20, UtilHot active (ожидает taker liquidity). Acceptable для E2E end state; drain'ится naturally через taker activity или clearить'ся в следующем reset cycle.

## 4. Что НЕ было tested (deferred / known gaps)

- **Long-running stability (Tier 3 passive 24-48h)** — требует 24-часового observation window. Состояние: vault left running с active positions; check next session для memory growth, log volume drift, stuck states, election cycles.
- **DCAP attestation verification от Tom'а** — `/v1/attestation/*` endpoints существуют и proxy через nginx, но Tom-side independent verification против Intel CA chain не exercised. Tom'ский eventual quickstart должен включать это.
- **Multi-operator FROST withdrawal** — vault user может в principle initiate withdrawal; multi-op signing relay code exercised separate test paths, не vault-specific.
- **Mainnet-like XRPL endpoint flap** — production XRPL endpoint resilience needs dedicated testing.

## 5. Рекомендация для V1 vault closure

V1 vault `feat/v1-vault-vamm @ 6815958` is **product-spec-faithful, externally-accessible, failover-correct, multi-position-tracking-correct** для сценариев в scope.

**Готов для Tom review** на `https://api-dev.xperp.fi` против Tom-spec deliverables (`docs/post-hackathon-specs.md` + Tom's inline answers в `docs/post-hackathon-specs-response.md`). После Tom'а product-spec acceptance — готов merge'ить `feat/v1-vault-vamm` в `master`.

nginx `non_idempotent` fix (E-2E-1) — deployment-config change на Hetzner; НЕ часть orchestrator code branch'а, живёт в `/etc/nginx/sites-enabled/api-dev.xperp.fi`. Нужно codify в runbook entry чтобы future deployments preserve'или его.

## 6. Cross-references

- Branch: `feat/v1-vault-vamm @ 6815958` (PR #15 series; ещё не merged в master pending этот E2E + Tom review)
- Audit: `docs/audit/REQ-19.md` + `docs/audit/RESP-19.md` + `docs/audit/RESP-19-addendum.md` (private repo `main`)
- Workflow doc: `docs/test-env-workflow.{en,ru}.md` (public repo `master`)
- Methodology: AUDIT-PROTOCOL v1.3 (operability axis A-E + direction-1b prompt (g)); PROJECT-INVARIANTS v0.7 (INV-OPS-1 + INV-OPS-2)
- E-2E-1 anchor для INV-OPS-2: external-position smoke поймал config gap invisible from loopback. Это второй подтверждённый anchor для direction-1b (g) (первый был V1 vault E2E NSG-blocked-consumer discovery 2026-05-26 earlier same day). Two-anchor count начался; per RESP-ACC-1 §3, если 3rd surface в ~5 cycles, accessibility может promote из §1b footer в full Category F.
