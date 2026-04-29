# ADR — Почему мы используем SGX-FROST для cluster signing, а не threshold ECDSA

**Status:** Принято (соответствует коду в production на testnet, 2026-04-29).
**Audience:** будущие аудиторы, инвесторы, технические ревьюеры спрашивающие «почему не MPC/TSS вместо SGX?», будущие maintainer'ы рассматривающие замену in-enclave signing path'а.
**Companions:** `docs/cluster-trust-model-decision.md` (выбор DCAP cross-attestation), `docs/sgx-enclave-capabilities-and-limits.md` (SGX trust assumptions), `docs/multi-operator-architecture.md` (общая trust model).

## 1. Решение

В системе мы используем два отдельных threshold механизма; ни один из них — не MPC threshold-ECDSA:

1. **XRPL native multisig** для on-chain escrow account'а. Enclave каждого оператора держит свой per-operator ECDSA (secp256k1) ключ; каждый оператор независимо подписывает один и тот же unsigned XRPL transaction своим ключом; полученный массив `Signers[]` отправляется через `submit_multisigned`. On-chain `SignerQuorum` enforces `K-of-N` threshold. Multi-party computation между signer'ами отсутствует — каждая подпись это полная ECDSA подпись под одним независимым ключом.

2. **FROST внутри SGX** (Flexible Round-Optimized Schnorr Threshold) для любой cluster-level Schnorr подписи которая нужна системе. Три enclave'а проводят Pedersen DKG; каждый enclave seal'ит одну долю; вместе они могут произвести одну BIP340-style group signature. Group pubkey expose'ится один раз через `frost_group_id`; share material никогда не покидает ни один enclave в plaintext'е. Cross-machine share transport (Path A v2) — ECDH+AES-GCM keyed на DCAP-attested ECDH identity per `docs/cluster-trust-model-decision.md`.

Threshold ECDSA (CGGMP, DKLS или родственные MPC семейства) **не** принят ни на одном из слоёв.

## 2. Контекст — что заменил бы threshold-ECDSA-MPC

Архитектурная альтернатива была бы: заменить SGX enclave на threshold-ECDSA MPC протокол (CGGMP или DKLS), чтобы те же два security property (никто один не держит ключ; никто один не может подписать) обеспечивались MPC математикой вместо SGX железа. Технический доклад Сбера от апреля 2026 «Threshold ECDSA технические аспекты: глубина кроличьей норы или на сколько верна гипотеза Эскобара?» (`https://www.youtube.com/watch?v=ZqHfLjlJBww`, слайды в `references/MPC-TSS.pdf`) проходит ровно по этому семейству протоколов. Ниже — наш summary прочитанного.

### 2.1 Schnorr threshold тривиален

Для Schnorr (и ГОСТ) partial signature — `sig_i = k_i + λ_i · s_i · e`. Aggregate — `Σ sig_i`. Два раунда коммуникации, никаких zero-knowledge proof'ов, batch verification работает. Это то что делает FROST.

### 2.2 ECDSA threshold — нет

ECDSA подпись — `(e + r_x · s) / r`. Построить её из secret share'ов требует перемножить два секрета (`s · b` и `r · b`) не раскрывая ни одного. Multiplication of secret shares без раскрытия — та сложная задача которую доклад называет «кроличьей норой». Два семейства протоколов её решают:

- **CGGMP** использует Paillier гомоморфное шифрование плюс stack zero-knowledge proof'ов на каждую подпись: range, knowledge of discrete log, корректно выбранные простые числа, корректное зашифрование, knowledge of discrete log снова — пять различных ZK в версии разобранной в докладе, причём count растёт по мере того как новые атаки приземляют патчи. Baseline 2018 года (GG18) патчили в 2019 (range proof'ы), снова в 2021 (CMP — выбор простых чисел), а в 2024 на Black Hat показали что аудиторское допущение («атакующий восстанавливает максимум 1 бит ключа») композируется в полное восстановление ключа через 256-кратное повторение эксплуатируя modulus games между `Z_q`, `Z_N`, `Z_{N²}`. Paillier-side proof'ы теперь — заплатки на заплатках; вердикт доклада: «не известно безопасно ли это; интуиция подсказывает что сложно в таких условиях обосновать».
- **DKLS** использует пятислойный protocol stack: RVOLE → OT-extension (например SILENT-OT или SoftSpoken-OT) → SubfieldVOLE → MPFSS → DPF. Каждый слой чисто композируется в UC framework — это сильная сторона по сравнению с CGGMP. Цена: понимание stack'а требует прочитать 3–4 плотных paper последовательно; per audience commentary доклада, KOS protocol (один из OT-extension кандидатов) получил в 2025 году paper показывающий что его security-theorem proof был неполным и требует дополнительной конструкции для CD-security. Audit surface намного шире чем выглядит описание протокола.

### 2.3 Вердикт доклада по обоим

> «Не известно [как обосновать безопасность], интуиция подсказывает, что сложно в таких условиях обосновать.»
> (дословная цитата докладчика; завершает обзор обоих семейств — CGGMP и DKLS).

Working position докладчика — что threshold ECDSA сейчас research-grade конструкция, не ship-ready primitive в том же смысле в котором им является Schnorr threshold.

## 3. Почему это ведёт нас к SGX-FROST, а не к threshold-ECDSA

Три причины, в порядке приоритета.

### 3.1 SGX даёт нам trust anchor который делает Schnorr threshold практичным

Задача threshold signing слоя — обеспечить что никто один не может forge подпись. SGX даёт это property через seal'инг доли в enclave в который оператор не может заглянуть. Раз enclave обеспечивает sealed-share property, threshold signing protocol поверх него может быть простейшим из доступных — Schnorr threshold (FROST) — потому что security argument больше не опирается только на threshold математику. Enclave обрабатывает половину «никто один не читает долю»; FROST обрабатывает половину «никто один не подписывает в одиночку» через threshold construction.

Threshold ECDSA без SGX переложил бы всю trust burden на MPC математику. Математика сейчас имеет audit-surface и patches-on-patches проблемы описанные в §2.2. Принятие её означало бы обмен известного, проаудированного, deployed trust anchor (SGX/DCAP) на research-grade — для системы где SGX trust anchor уже оплачен orderbook'ом, margin engine'ом и per-user state живущими в том же enclave.

### 3.2 Audit surface намного шире

Мы отслеживаем эти audit-quality concerns которые доклад поднимает по поводу CGGMP/DKLS:

- **CGGMP требует re-verifying ZK proof'ов на каждую подпись.** Range proof'ы, correctly-chosen-primes proof'ы, correct-encryption proof'ы запускаются на каждом раунде подписи. Latency платится за каждую подпись, не amortise'ится на setup'е. Per доклад: «всё это надо делать внутри одной подписи, и практически каждый раз».
- **CGGMP ZK stack движется.** История 2018 → 2019 → 2021 → 2024 «patch found, новый ZK добавлен» — ровно тот структурный паттерн против которого предостерегает аудиторская guidance. Добавление нового tx-type или weight semantic может invalidate ранее доказанное property и никто не заметит.
- **DKLS audit surface широк.** Чтение протокола означает чтение RVOLE + OT-extension + SubfieldVOLE + MPFSS + DPF последовательно. Аудит означает аудит каждого слоя плюс композиции. KOS theorem-proof issue 2025 года — ровно тот тип finding'а который всплывает когда surface настолько широк.
- **Оба требуют O(N²) point-to-point сообщений между парами операторов.** Latency и bandwidth плохо масштабируются по сравнению с broadcast-friendly aggregation FROST'а.

У нас уже есть per-tx-type signing-policy hardening (`p2p::validate_signing_policy`, audit-shipped per re-audit-3 X-C1 hardening) — шесть строк business validation на каждый allowed tx type. Добавление ECDSA-MPC protocol code в threat model добавило бы на четыре порядка больше кода которое надо держать корректным.

### 3.3 XRPL multisig не нуждается в MPC вообще

Signing requirement on-chain escrow'а удовлетворяется native primitive XRPL `SignerListSet`: `K-of-N` operator-адресов, каждый независимо подписывает своим ECDSA ключом, chain enforces quorum. Это threshold scheme реализованная chain protocol'ом, не нами. Здесь нечего заменять MPC. (См. `orchestrator/src/signerlist_update.rs` и Phase 2.2 — как membership changes propagate.)

Единственное место где MPC threshold-ECDSA применялся бы — FROST слой (будущий Schnorr-несовместимый chain без on-chain multisig). Для него мы используем Schnorr внутри SGX.

## 4. Когда мы пересмотрим

Два сценария re-open'ят это решение:

1. **SGX trust assumptions существенно ослабевают.** Примеры которые имели бы значение: side-channel атака извлекающая sealed material at scale на production-supported hardware'е, решение Intel retire DCAP без TDX migration path на usable timeline, или regulatory решение запрещающее SGX как custody primitive в нашей юрисдикции. Roadmap response в `docs/sgx-vs-tdx-roi.md` — TDX migration first; threshold-ECDSA-MPC — second-tier fallback если ни один TEE option не остаётся usable.
2. **Non-Schnorr-supporting chain становится hard product requirement и on-chain native multisig недоступен.** Большинство ECDSA chains которые нас интересуют имеют multi-party signing primitives работающие без MPC (XRPL `SignerListSet`, EVM gnosis-safe-style multisig contracts, BTC native multisig + Taproot Schnorr после Tapscript). MPC ECDSA становится load-bearing только там где ни один из них не работает.

В обоих сценариях effort estimate из доклада — «research-grade, 4–6 месяцев чтобы ship reliably в первый раз, дольше чтобы defend против следующего раунда патчей». Это project work item, не in-flight refactor.

## 5. References

- Технический доклад Сбера, 2026-04-22. Слайды: `references/MPC-TSS.pdf` (27-slide Beamer deck, Russian, с эпиграфами из Стругацких). Запись: <https://www.youtube.com/watch?v=ZqHfLjlJBww>. Доклад presented как research-direction overview; speaker's `Σ ZK` count и framing «кроличья нора» informует §2.2 выше.
- CGGMP family — Gennaro, Goldfeder (2018) "Fast Multiparty Threshold ECDSA with Fast Trustless Setup" (GG18); Canetti, Gennaro, Goldfeder, Makriyannis, Peled (2020+) "UC Non-Interactive, Proactive, Threshold ECDSA with Identifiable Aborts" (CGGMP).
- DKLS family — Doerner, Kondi, Lee, Shelat (2018, 2019, 2023) — "Secure Two-party Threshold ECDSA from ECDSA Assumptions" и follow-ups; paper 2023 года консолидирует protocol используемый в текущих DKLS implementation'ах.
- KOS protocol — Keller, Orsini, Scholl (2015) для OT-extension; paper 2025 года ревизирующий KOS CD-security argument был упомянут аудиторией доклада (Михаил Воронов, в чате).
- `docs/cluster-trust-model-decision.md` — выбор DCAP cross-attestation и отказ от operator-signed roster.
- `docs/sgx-enclave-capabilities-and-limits.md` — SGX trust model + FROST 2-of-3 framing.
- `docs/multi-operator-architecture.md` §1 (trust model), §10 (subcommand classes).
- `SECURITY-REAUDIT-4.md` X-C1 hardening — per-tx-type signing-policy pattern bounding audit surface этого слоя.
