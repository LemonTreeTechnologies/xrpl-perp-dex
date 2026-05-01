# API environment policy — Tom integration

**Status:** Принято (2026-05-01).
**Audience:** internal team и dev-perp; будущие maintainer'ы.
**Companions:** [`development-operating-model.{en,ru}.md`](development-operating-model.ru.md) (operating modes + mainnet sync procedure). Companion API-boundary rule (Том владеет clients black-box; мы владеем API) summarise'на inline ниже где это важно.

Этот документ фиксирует policy для того в каком **окружении** работают компоненты Тома во время разработки, и условия при которых компонент graduate'ится с testnet на mainnet-sandbox. Companion document определяет что в scope Тома vs наш; этот документ определяет где каждая сторона запускается.

---

## 1. Pairing rule

Компонент Тома подключается к **нашему instance того же окружения**, никогда не пересекает окружения:

- Tom-**testnet** client → наш **testnet** API (Azure DCsv3 кластер, escrow `rbqCUxgi…`, faucet-funded, multi-operator FROST 2-of-3, текущий код на `master` HEAD).
- Tom-**mainnet-sandbox** client → наш **mainnet-sandbox** API (Hetzner mainnet stack) — только после graduation gate в §4.
- Tom-**production** client → out of scope этого документа; production имеет свой launch playbook per `development-operating-model.md` §1.3.

Cross-environment подключение (Tom-testnet client стучится в наш mainnet, или наоборот) запрещено. И API surface, и data model environment-tagged; bridging их рискует загрязнить реальные средства тестовыми сигналами или наоборот.

Single-mode-check rule (каждый cross-node subcommand работает идентично на testnet и mainnet, никаких environment branches в committed коде) держит testnet и mainnet на одном коде, что значит **API contract идентичен** между ними — никаких testnet-only branches, никаких mainnet-only quirks. Pairing rule поэтому не code-level enforcement; это operational discipline.

---

## 2. Default — testnet first

Для каждого нового компонента Тома (новый bot, новый screen, новое демо, новый synthetic trader), default и единственный разрешённый dev environment — testnet. Graduation gate (§4) — то что разблокирует следующее окружение.

Почему testnet first:

- **Cost asymmetry.** Testnet faucet-funded и сбрасывается дёшево; mainnet использует реальные XRP held by the operator-of-record для sandbox использования. Client-side баг который часто стреляет — на testnet ничего не стоит, на mainnet стоит реальных средств даже на sandbox масштабе.
- **Audit cadence.** Per `development-operating-model.md` §3, каждый mainnet-sandbox sync требует audit cycle (REQ-N → RESP-N PASS). Быстрая итерация на client-side Тома вскрывает API-проблемы, требующие наших патчей, что требует re-audit'а — гонять этот цикл на каждую итерацию компонента Тома операционно нереально. Testnet поглощает итерацию без audit-cycle стоимости.
- **Bug isolation.** Когда проблема появляется на testnet, участвующие стороны: client Тома + наш orchestrator + наш enclave. Когда та же проблема появляется на mainnet во время dev-итерации, в игру входят дополнительные переменные (real XRPL state, real fees, real network conditions). Testnet — calibration step говорящий какие проблемы Тома vs наши.
- **Reversibility.** Testnet "сломан" → redeploy. Mainnet "сломан" с реальными XRP → никакой redeploy не вернёт потерянные средства.

---

## 3. Calm rebuttal list — когда Том просит mainnet напрямую

Это conversation moves, не отказы. Цель — держать boundary, уважая реальную потребность Тома (которая обычно "iteration speed" или "realistic flow").

| Том просит | Наш ответ | Что это разблокирует |
|---|---|---|
| "Just for now, mainnet быстрее" | "Testnet запускает тот же код; разница в скорости — deploy step ~30 мин, который мы всё равно делаем для sandbox graduation." | Мы поглощаем deploy delta на нашей стороне вместо того чтобы тащить client Тома в mainnet. |
| "Testnet нет realistic flow" | "Right — потому synthetic-trader path + V1 vault обеспечивают flow. Когда V1 имплементирован, testnet имеет continuous quoting и counter-flow." | Мы committ'имся делать testnet realistic enough для компонента Тома, вместо того чтобы тащить Тома в mainnet ради потока. |
| "Я хочу демо инвестору" | "Демо запускаются на mainnet-sandbox ПОСЛЕ testnet валидации. Sandbox — small-XRP и controlled. Investor demos спокойны by design — мы не гоняем их на raw mainnet." | Мы даём Тому путь к демо, просто не тот что обходит gate. |
| "Mainnet норм, я буду осторожен" | "Реальные XRP не grade'ят на осторожность. Audit cycle — gate делающий 'осторожен' enforceable; обход его убирает единственный механизм ловящий неосторожный случай." | Мы называем gate, не дисциплину Тома, реальным механизмом. |
| "На хакатоне работало" | "Работало потому что audit gate ещё не существовал — и цена этого был chaos который мы договорились никогда не повторять. Сегодня у нас есть testnet потому что мы построили его именно для этого случая." | Мы treat'им хакатон как прецедент мотивирующий gate, не как прецедент для пропуска его. |

Если после этого разговора у Тома всё ещё есть реальная причина которую ничто из этого не адресует — путь surface'ить это как spec gap: Том называет чего testnet'у не хватает, dev-perp чинит gap, Том продолжает на testnet.

---

## 4. Graduation plan — testnet → mainnet-sandbox

Каждый компонент Тома graduate'ится отдельно. Последовательность:

1. **Итерация на testnet** до "demonstrably stable" — definition: компонент Тома проходит свой happy-path repeatedly + operator-side spot review подтверждает что surface area такой как заявлено.
2. **API contract check** — dev-perp подтверждает что API surface от которого зависит компонент Тома соответствует documented spec (`docs/frontend-api-guide.md`, OpenAPI). Любое отклонение чинится в наших docs/code до graduation; мы не shipping graduation требующий чтобы Том зависел от undocumented behaviour.
3. **Mainnet-sandbox sync gate** — per `development-operating-model.md` §3, orchestrator + enclave на mainnet на том же коде что testnet через Mode S sync. Audit verdict для того sync — PASS. Без этого, никакого graduation.
4. **Sandbox connection** — Том переключает API endpoint своего компонента на mainnet-sandbox URL. Volume small XRP only (definition: суммы которые если потеряны полностью — acceptable cost-of-learning per `product-sandbox-single-operator` mode в `development-operating-model.md` §1.1).
5. **Sandbox observation period** — dev-perp + Том + operator-of-record смотрят на behaviour deltas (любая разница между testnet и sandbox — signal что code-paths или environments не actually идентичны, и graduation откатывается). Длина периода: минимум одна полная sandbox session до того чтобы considered компонент "graduated."

Никакого автоматического promotion в production. Production — отдельное launch event со своим playbook; sandbox graduation — не тот же шаг.

---

## 5. Что dev-perp делает чтобы enforce это

- Поддерживает API contract docs (`docs/frontend-api-guide.md`, OpenAPI spec) так чтобы testnet behaviour и mainnet-sandbox behaviour документированы идентично. Single-mode-check rule гарантирует что это также true at runtime.
- Поддерживает API-level tests в `orchestrator/tests/` exercising contract. Failing test = API сломан на affected environment regardless of который client (Тома или наш) репортит.
- Mainnet-sandbox endpoint gated audit verdict'ом per operating model. dev-perp не enable Tom's mainnet-sandbox path до того как gate открыт.
- Когда Том репортит testnet API issue — response dev-perp'а инспектировать/чинить API и его docs, не suggest'ить restructure'ить client Тома.

---

## 6. Чем этот документ НЕ является

- Не правило против демо. Демо запускаются на appropriate environment (testnet для раннего, sandbox для позднего). Policy — "match the environment", не "no demos".
- Не дисциплина imposed на Тома. Policy симметричная — dev-perp тоже не подключает наш test infra к mainnet во время итерации.
- Не permanent block на mainnet. Graduation gate существует именно чтобы компоненты могли двигаться; gate — `audit-verdict + Mode S sync`, не subjective discretion.
- Не negotiable per-iteration. Conditions в §4 — gate; если они met, graduation proceeds. Если не — dev-perp чинит gap.

---

## 7. References

- [`development-operating-model.{en,ru}.md`](development-operating-model.ru.md) §1 (operating modes), §3 (mainnet sync procedure) — authoritative environment + sync-gate definitions.
- [`multi-operator-architecture.{en,ru}.md`](multi-operator-architecture.ru.md) §1 (trust model), §10 (subcommand classes) — same-code-on-testnet-and-mainnet implicit в этой архитектуре.
- Прецедент хакатона: Paris 2026-04-12 demo предшествует audit gate; "the chaos we agreed never to repeat" — операционная reference для §3 row 5.
