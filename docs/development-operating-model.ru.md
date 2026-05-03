# Development Operating Model

**Status:** Принято (2026-05-01).
**Audience:** человеческая команда (Andrey, Tom, Alex), AI-Auditor, будущие maintainer'ы, будущие external auditor'ы.
**Replaces / consolidates:** stand-alone заметки про mainnet readiness, deploy procedure caveats и audit workflow которые ранее жили только в chat history и memory файлах.
**Companions:** `docs/multi-operator-architecture.md` (the aspirational trust model), `docs/deployment-procedure.md` (the ceremonial steps), `SECURITY-REAUDIT-4.md` (the audit baseline).

Этот документ фиксирует operational reality того как проект ведётся **сегодня**, и как эта реальность отличается от architecture documents. И architecture, и reality — обе корректны; они описывают разные time horizons. Этот документ — мост.

---

## 0. Зачем существует этот документ

В проекте параллельно живут две timeline:

1. **Architecture** — `docs/multi-operator-architecture.md` описывает trust model, coordination protocols и lifecycle invariants которые держатся **когда кластер запускается N независимыми human operator'ами с zero trust между ними**.
2. **Operational reality** — сегодня один физический оператор (Andrey) играет все N ролей. Trust model из §1 архитектуры ("operator-vs-operator zero trust") не может enforce'иться когда N=1 в человеческих terms.

Обе корректны для своих horizons. Честный move — задокументировать gap, определить operating modes которые его bracket'ят, и написать процедуру которая позволит синхронизировать mainnet с development-state без притворства что gap'а нет.

Этот документ — living policy reference; обновляется параллельно с реальными изменениями operating model.

---

## 1. Три operating modes

### 1.1 `product-sandbox-single-operator` — текущий режим

- **Один физический оператор** (Andrey) подписывает как node-1, node-2, и node-3.
- **Architecture invariants из `multi-operator-architecture.md` §1 формально suspended** для этого режима. "Operator-vs-operator zero trust" не может существовать когда оператор один.
- **Реальные клиентские средства не под риском.** Mainnet escrow держит только seed/test funds трекаемые founder'ом; trading не открыт для external клиентов.
- **Mainnet sync — это checkpoint, не launch.** Мы синхронизируем mainnet с development tip по cadence (обычно каждые несколько недель), не как product release.
- **Open product questions не блокируют sync.** AMM-vs-CLOB, BTC perp design, vault redesign, frontend integration scope — все идут в своём cadence и никогда не gate'ят mainnet sync.

Этот режим честен относительно того что у нас есть. Это не нарушение архитектуры; это документированный pre-architecture state.

### 1.2 `product-sandbox-multi-operator` — переходный режим

- **Минимум два физических оператора** независимо запускают ноды; architecture invariants из `multi-operator-architecture.md` §1 теперь **enforce'ятся**, не suspended.
- Кластер запускает тот же код что в 1.1; разница чисто человеческая (разные люди держат разные operator seeds).
- Всё ещё pre-product: реальных клиентских средств нет, mainnet sync всё ещё checkpoint cadence, AI-auditor gate (§2) всё ещё применяется.
- **Trigger для входа:** второй human operator формально committed запускать свою ноду, его `node-bootstrap` artefacts опубликованы, и он добавлен в on-chain `SignerList` через `signerlist-update`.

### 1.3 `production` — финальный режим (пока не определён)

- Реальные клиентские средства. Multi-operator архитектурно и операционно. Third-party human audit. Формальный launch playbook.
- Процедуры этого режима намеренно не специфицированы в этом документе. Когда проект готов войти в production mode, отдельный launch playbook будет написан; он унаследует большую часть §3 и §2 со strictness ratchets и реальным third-party audit'ом заменяющим AI-Auditor как primary gate.
- **Production-mode unlock gated на ДВУХ foundation invariants** (см. `multi-operator-architecture.md` §1 invariants 5 и 7): (a) работающий enclave-software-upgrade mechanism сохраняющий real customer state across MRENCLAVE bumps — Path A, currently being implemented в REQ-8; (b) reproducible MRENCLAVE produced независимо ≥N operators с bit-identical result — currently being instantiated через GHA build-gate (`docs/build-requirements.{en,ru}.md` §5). Failure любого gate'а значит production-mode unreachable; оба должны hold.

### 1.4 Mode transitions

| From | To | Trigger | Required artefacts |
|---|---|---|---|
| 1.1 → 1.2 | второй human operator присоединяется | его `node-bootstrap` опубликовал Domain on-chain; `signerlist-update` его добавляет; fresh DKG над новым membership | sync log запись именующая нового оператора (с consent); doc edit здесь маркирующий current mode = 1.2 |
| 1.2 → 1.3 | проект готов принимать реальные клиентские средства | отдельный launch playbook; third-party audit signoff; формальное go/no-go решение; это **не** sync — это launch | здесь не специфицировано |

Откат с 1.2 на 1.1 (оператор уходит, замена не найдена) разрешён; treat as `signerlist-update --remove` followed by sync log entry. Откат с 1.3 на что-либо — это major incident со своим playbook.

---

## 2. Audit workflow — AI-Auditor cycle

> **Authoritative protocol (2026-05-01).** Audit cycle управляется [`docs/audit/AUDIT-PROTOCOL.md`](audit/AUDIT-PROTOCOL.md) v1.0 (cross-project, cross-repo) и per-repo wrapper [`docs/audit/PROTOCOL.md`](audit/PROTOCOL.md). Этот §2 сохраняет conceptual rationale (зачем AI-Auditor; как gate композируется с §3 mainnet sync) но defer'ит все field/file/PR-shape specifics к protocol document. Где этот раздел и protocol document расходятся — protocol document каноничен.

### 2.1 Зачем AI-Auditor

Реальные third-party human audits дороги и медленны; мы не можем триггерить один на каждый mainnet sync (который случается каждые несколько недель). AI-Auditor — Claude Code instance запущенный на отдельной машине с audit-специализированной knowledge base, без exposure к dev-context — обеспечивает **independent review** между формальными third-party audits. Это не замена human audit на production-launch time; это higher-frequency gate во время sandbox phases.

Independence AI-Auditor'а структурная:
- Другая машина (нет shared memory или cache с dev instance).
- Knowledge base auditor-специализированная (prior audits, CVE patterns, attack libraries) а не dev-context.
- Operator-controlled (Andrey запускает обе стороны), но auditor instance получает только artefacts (commits, diffs, finding IDs) которые получил бы реальный аудитор — не dev-time conversation который их произвёл.

### 2.2 Цикл

1. **Trigger.** Mainnet sync (Mode S или F per §3) запрошен.
2. **Change-set assembly.** Dev side компилирует audit-input package: список commit hashes с последнего sync'а, mapping каждого к relevant finding IDs (`O-H1`, `O-M3`, `C-04`, etc.) или к "new functionality, no prior finding". Saved as `audit-reviews/<YYYY-MM-DD>-input.md`.
3. **Auditor review.** AI-Auditor читает input package + текущее code state, производит verdict file `audit-reviews/<YYYY-MM-DD>-verdict.md` содержащий:
   - per-finding-ID resolution (resolved / partial / not addressed / new finding)
   - sync gate decision: `approved` | `approved-with-conditions` | `blocked`
   - if `approved-with-conditions`: список conditions (например "must complete X before Mode F")
   - if `blocked`: список reasons + minimum scope to unblock
4. **Sync gate enforced.** Mainnet sync (§3) не выполняется без `approved` или `approved-with-conditions` verdict для current change-set.
5. **On block.** Dev side адресует reasons, лендит fixes, re-requests audit. Цикл до approved или escalated to human review.

### 2.3 Что идёт в `audit-reviews/`

```
audit-reviews/
  README.md                      — directory purpose + index
  2026-MM-DD-input.md            — change-set submitted to auditor
  2026-MM-DD-verdict.md          — auditor reply (verdict + conditions/blocks)
  2026-MM-DD-followup.md         — optional: dev response if verdict raised new questions
  ...
```

Файлы append-only: раз verdict commit'нут, он никогда не редактируется. Subsequent revisions идут в новый dated файл.

### 2.4 Граничные случаи

- **Auditor и dev не согласны по severity.** Документировать обе views в verdict file и follow-up. Если unresolved, escalate to human review (Andrey + future external auditor at next formal audit window).
- **Finding помечен "by design" dev'ом но disputed auditor'ом.** Same handling; produces documented disagreement который future human audit может revisit.
- **Audit-input забыл commit.** Treat as auditor's right to flag at any time; sync may need to roll back. Mitigation: dev compiles input package from `git log <last-sync-hash>..HEAD` без manual filtering.

---

## 3. Mainnet sync procedure

### 3.1 Decoupling — Mode S vs Mode F

Mainnet sync — это две ортогональные операции. Любая может запускаться независимо от другой, и большинство sync'ов запускают только одну из двух.

- **Mode S — Software sync.** Update enclave + orchestrator binaries, MRENCLAVE bump, fresh DKG. **НЕ** модифицирует escrow `SignerList` или master-key state самой Mode S. **HONEST DESCRIPTION (perp REQ-6 discovery 2026-05-02):** в текущей реализации Mode S также **НЕ сохраняет sealed enclave state** при MRENCLAVE bump. Что это означает в operator/customer terms: (a) FROST доли пересоздаются fresh DKG step (шаг 6) — семантически корректно, ничего ценного не теряется; (b) per-enclave ECDH identity keys регенерируются на первом старте — peer-attest cache rebuilds в одном announcer cycle (~240 s), ничего ценного не теряется; (c) **per-operator XRPL signing keys сгенерированные СТАРЫМ enclave НЕ доступны новому enclave** — это значит что оператор делающий MRENCLAVE bump в production-mode без предварительного transfer-of-authority на НОВЫЙ XRPL ключ сделает существующий on-chain `SignerList` неподписываемым; (d) **perp state — позиции, маржа, vault state, account pool — сбрасывается на каждый Mode S sync** потому что новый MRENCLAVE не может unseal blob'ы запечатанные старым. Сегодня это допустимо потому что мы в §1.1 `product-sandbox-single-operator` mode где нет реальных клиентских позиций для потери и pre-Mode-S transfer-of-authority для mainnet escrow не понадобился (mainnet sync ещё не выполнялся — заблокирован на upgrade-mechanism work). Production-mode (§1.3) недостижим из этой точки без cross-enclave Local Attestation migration mechanism (Path A; spec будет authored как audit-cycle REQ-7); смотри foundation invariant в §1 — почему это non-negotiable. Mainnet Mode S sync поэтому postponed до приземления того mechanism'а.
- **Mode F — Funding / on-chain governance update.** Top up escrow, disable master key, change `SignerListSet` membership или quorum. **НЕ** трогает enclave binaries. Запускается как on-chain XRPL transactions only.

Большинство sync'ов — Mode S only. Mode F редкий и явный (например "add a second physical operator" → 1.1→1.2 transition).

### 3.2 Mode S — software sync steps

**Pre-flight:**

1. AI-Auditor verdict для current change-set — `approved` или `approved-with-conditions` (§2). Conditions, если есть, выполнены до continuing.
2. Reproducible build: подтвердить Hetzner-built enclave + orchestrator binaries, log MRENCLAVE.
3. Sync log entry начат (не commit'нут yet) с `Mode: S`, target MRENCLAVE, change-set hash range.

**Per-node steps (3 Azure mainnet VMs, последовательно node-by-node):**

4. `node-deploy` (existing subcommand) — node-local artefact distribute + systemd swap.
5. Peer attest cycle — verify что новый MRENCLAVE quote показывается в peer-attest cache на обеих других нодах.
6. После того как все 3 ноды deployed: fresh DKG ceremony через `dkg-coordinate` (libp2p) — производит новый group pubkey.
7. Cluster health check — group pubkey byte-identical across all 3 nodes; libp2p mesh stable; periodic peer-quote announcer firing.

**Wrap-up:**

8. Sync log entry commit'нут с: MRENCLAVE before/after, group pubkey before/after, AI-auditor verdict file ref, operator acknowledgement (§1.1: все операторы сыграны Andrey), evidence links.
9. Никакая on-chain транзакция не требуется Mode S. Escrow `SignerList` и balance unchanged.

### 3.3 Mode F — funding / on-chain governance steps

Mode F — это меню XRPL операций, каждая со своей sub-procedure. Ни одна не требует enclave change.

- **Top-up escrow:** простой `Payment` от funded source к mainnet escrow address.
- **Membership change:** `signerlist-update` admin route (Phase 2.2-C); existing-quorum подписывает новый `SignerListSet`. Использовать при входе в Mode 1.2 или ротации оператора.
- **Master-key disable:** `AccountSet asfDisableMaster` на escrow. One-way; запускается только когда готовы commit'ить к multisig-only governance forever для этого account'а. (Mainnet escrow's master не yet disabled per `reference_mainnet_escrow_seed.md` — это intentional во время sandbox phase.)
- **Quorum change:** `signerlist-update --quorum N` (без membership change).

Каждая Mode F операция производит свою линию в sync log под той же датой что и Mode S sync (если combined) или свою dated entry (если standalone).

### 3.4 Combined Mode S+F runs

Permitted но редко. Когда обе запускаются в один calendar day:

- Mode S идёт первым (software теперь current).
- Mode F следует (governance change сделан поверх current software).
- Sync log entry имеет `Mode: S+F` и листает оба kinds of evidence.

Если Mode F меняет membership, new operator's MRENCLAVE должен уже match (т.е. он запустил свой own Mode S first на своей own VM, или он joined fresh и запустил `node-bootstrap` против current MRENCLAVE).

---

## 4. Sync log

### 4.1 Где живёт

`mainnet-sync-log.md` в repo root. Append-only. Entries immutable раз commit'нуты.

### 4.2 Entry template

```markdown
## 2026-MM-DD — Mainnet sync #N
- **Mode:** S | F | S+F
- **Trigger:** <одно предложение: какое изменение мотивировало этот sync>
- **AI-Auditor verdict:** [`audit-reviews/2026-MM-DD-verdict.md`](audit-reviews/2026-MM-DD-verdict.md) — <approved | approved-with-conditions | n/a>
- **MRENCLAVE before → after:** `<hash>` → `<hash>` (или "n/a" для Mode F)
- **Group pubkey before → after:** `<hash>` → `<hash>` (или "n/a" для Mode F)
- **Escrow address:** `<rXXXX>` (unchanged unless Mode F changed it)
- **SignerList before → after:** <quorum>-of-<N> → <quorum>-of-<N> (или "unchanged")
- **Operators participated:**
  - node-1 = [Andrey] (`product-sandbox-single-operator` mode — see §1.1)
  - node-2 = [Andrey]
  - node-3 = [Andrey]
- **Acknowledgement:** Запущен под `product-sandbox-single-operator` mode. Architecture invariant §1 of `multi-operator-architecture.md` (operator-vs-operator zero trust) suspended per §1.1 этого документа. Реальных клиентских средств не под риском; это checkpoint deploy, не launch.
- **Verification:**
  - <evidence #1, e.g. "group_pubkey byte-identical across 3 nodes — log lines at <commit>">
  - <evidence #2, e.g. "on-chain tx <hash> landed tesSUCCESS">
  - <evidence #3>
- **Outcome:** clean | reverted | partial (if partial: что осталось)
- **Next:** <что триггернуло next planned sync, или "no next planned">
```

Когда проект transition'ит в Mode 1.2, "Operators participated" block будет именовать multiple humans; "Acknowledgement" block заметит что §1 теперь enforced а не suspended.

---

## 5. Open questions deliberately not addressed here

Этот документ — operating model, не product roadmap. Следующие — реальные open questions, но они принадлежат elsewhere и **не gate'ят никакой sync**:

- AMM-vs-CLOB direction (`docs/clob-vs-amm-alignment.md`)
- BTC perp feasibility (`docs/btc-perp-dex-feasibility.md`)
- Frontend integration questions (Tom / Alex / Tanya scope)
- State hash Merkle tree (M-05 в audit history)
- Build-gate decision (Hetzner self-hosted runner vs aligned-deps GHA)

Когда любой из них resolves в code change, тот change идёт через normal AI-Auditor cycle (§2) и лендит в next sync (§3); ни один из них не блокирует sync'и которые не включают их changes.

---

## 6. Initial state at this document's commit

- **Current mode:** `product-sandbox-single-operator`.
- **Mainnet sync number completed:** 0 (этот документ предшествует first cataloged sync).
- **Mainnet escrow на Hetzner** держит ~108 XRP per `reference_mainnet_escrow_seed.md`; master key пока не disabled. State acceptable для current mode.
- **Testnet stack** (3 Azure DCsv3 VMs) is current с `master` HEAD; mainnet stack нет. Первый scheduled sync закроет тот gap в Mode S.
