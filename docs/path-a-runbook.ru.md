# Path A migration ceremony — операторский runbook (RU)

**Статус:** REQ-8 commit 11 draft. Ceremony driver сам по себе появляется в commit 12; этот runbook документирует видимую оператору процедуру, которую driver обёртывает. Спецификация: `docs/audit/REQ-7.md` (приватный репозиторий).

---

## Что такое Path A

Координированный кластером апгрейд perp-dex enclave-софта на новый MRENCLAVE без потери клиентского состояния (operator XRPL keys, FROST shares, perp-позиции, баланс vault'ов, account pool). Каждый оператор запускает церемонию локально на своей машине; никакого cross-operator SSH.

**Что Path A обеспечивает:**
- Средства в escrow остаются доступны — старые ключи XRPL подписи переживают апгрейд через миграционную церемонию.
- Клиентское perp-состояние сохраняется — клиенты не видят сброса балансов или открытых сделок.
- Кворум операторов решает, может ли апгрейд произойти.

**Что Path A НЕ покрывает:**
- Первичный bootstrap (`node-deploy` без `--side-by-side`).
- Disaster recovery если OLD enclave умер до завершения церемонии.
- Cross-host миграцию (Path A platform-bound — тот же физический SGX CPU).

## Pre-requisites

Перед запуском `node-deploy --side-by-side` на любом узле:

1. **OLD enclave запущен и работает** на порту 9088 с sealed state в `/home/azureuser/perp/accounts/`. Проверка: `systemctl is-active perp-dex-enclave` возвращает `active`.

2. **Кворум операторов согласовал** новый MRENCLAVE. Out-of-band consensus (Slack / call) до того как любой оператор pre-stages NEW unit. Delegation bundle, который orchestrator собирает позже, доказывает только факт согласия пост-фактум; pre-agreement — это операторская дисциплина.

3. **Build artefacts готовы**: свежесобранные `perp-dex-orchestrator`, `enclave.signed.so`, `perp-dex-server`, и желательно `build-manifest.txt` с pinned MRENCLAVE. Сборка через GHA pipeline на согласованный git ref. MRENCLAVE из build-manifest — это то, против чего `node-deploy --side-by-side` крест-чекает реально запустившийся NEW enclave через `/version` post-deploy.

4. **One-time per-machine NEW-side setup** выполнен (только самая первая церемония на данной операторской машине):

   а. Скопировать `scripts/path-a/perp-dex-enclave-next.service` в `/etc/systemd/system/perp-dex-enclave-next.service`.

   б. Выполнить `sudo systemctl daemon-reload`.

   в. Создать `/home/azureuser/perp-next/` (будет наполнено через `node-deploy --side-by-side`).

   г. Pre-stage `/home/azureuser/perp-next/config.json` из `scripts/path-a/config-next.json.sample`. Подкорректировать `ssl_*` пути если ваша TLS-топология отличается от OLD unit.

   д. Проверить что NEW unit виден: `systemctl status perp-dex-enclave-next` должно показать `inactive (dead)` (unit известен, ещё не стартовал).

   Этот setup permanent — последующие церемонии на той же машине переиспользуют его. После успешной post-promotion фазы оператор может оставить NEW unit definition для следующего цикла апгрейда.

## Церемония — взгляд оператора

**Ceremony driver** (commit 12) обёртывает шаги ниже. Операторское взаимодействие:

```
$ perp-dex-orchestrator node-deploy --side-by-side \
    --orchestrator ./perp-dex-orchestrator-new \
    --enclave-dist ./dist-azure-new
```

Это устанавливает NEW артефакты в `/home/azureuser/perp-next/` и стартует NEW enclave на порту 9089 параллельно с OLD (порт 9088). NEW поднимается с пустым sealed state и эфемерной migration keypair, сгенерированной внутри SGX enclave.

После того как все операторы независимо завершили `node-deploy --side-by-side`, **один оператор** запускает migration ceremony:

```
$ curl -k -X POST https://localhost:9088/admin/migrate-state \
    -H 'Content-Type: application/json' \
    -d '{"expected_mrenclave_new": "<hex 64>", "ceremony_nonce_request": true}'
```

(Семантика ceremony driver определена в commit 12.)

Driver:
1. Запрашивает у NEW (порт 9089) её target_info + свежий ceremony_nonce.
2. Просит NEW сгенерировать ephemeral migration keypair с binding'ами к (target_info_of_old, ceremony_nonce).
3. Собирает delegation signatures от кворума операторов через существующий libp2p signing-relay (тот же flow, который SignerListSet update уже использует).
4. Просит OLD (порт 9088) верифицировать NEW's LA report + delegation quorum, зашифровать state и эмитить (ciphertext, ephemeral_pk, tag, la_report_old).
5. Просит NEW (порт 9089) верифицировать la_report_old, расшифровать state, выполнить M3 sealed-file durability self-check, записать M4 manifest LAST, эмитить completion LA report.
6. Просит OLD (порт 9088) верифицировать completion LA report → seal retired-marker → flip in-memory retired flag.

При успехе ceremony driver возвращает 200 OK с записанным новым MRENCLAVE.

## Post-ceremony promotion

После успешной confirmation OLD будет возвращать `PATH_A_ERR_ECALL_RETIRED` (-150) на каждый signing или state-mutating ecall. `/version` OLD и read-only routes продолжают отвечать — полезно для подтверждения retired state.

Promotion sequence:
1. **Остановить OLD enclave**: `sudo systemctl stop perp-dex-enclave`.
2. **Перенаправить external traffic** на порт 9089 (reverse proxy оператора / TLS terminator / DNS — зависит от топологии).
3. **Проверить что NEW обслуживает**: `curl -k https://localhost:9089/version` возвращает новый MRENCLAVE.
4. **Отключить OLD unit** чтобы последующая перезагрузка случайно не запустила его обратно: `sudo systemctl disable perp-dex-enclave`.
5. **Опциональный cleanup** (на усмотрение оператора): после того как церемония подтверждена всеми операторами и зафиксирована, OLD `/home/azureuser/perp/accounts/` sealed state — forensic-only (NEW MRENCLAVE не может его расшифровать). Либо хранить как backup несколько недель, либо удалить после стабилизации кластера на NEW.

## Failure modes и recovery

| Симптом | Причина | Восстановление |
|---|---|---|
| `node-deploy --side-by-side` отклонил: "OLD service not active" | OLD enclave service остановлен или unhealthy | Запустить OLD (`sudo systemctl start perp-dex-enclave`); разобраться почему он остановился |
| `node-deploy --side-by-side` отклонил: "NEW unit not found" | Pre-requisite 4(а) пропущен | Установить systemd unit по pre-requisites |
| `node-deploy --side-by-side` отклонил: "perp-next/ not empty" | Половинчатая попытка предыдущей церемонии | `sudo rm -rf /home/azureuser/perp-next/*` и попробовать снова |
| `node-deploy --side-by-side` отклонил: "port 9089 not available" | Зависший процесс или старый NEW enclave всё ещё работает | `sudo systemctl stop perp-dex-enclave-next`; проверить `ss -ltn` на другие listeners |
| `node-deploy --side-by-side` отклонил: "MRENCLAVE mismatch" | Build artefacts не соответствуют build-manifest.txt | Разобраться КАКОЙ enclave запустился до retry; НЕ продолжать с mismatched build |
| Ceremony driver timeout на confirmation step | NEW не смог запечатать один из мигрируемых файлов | OLD НЕ zeroized (он не получил confirmation report) — escrow keys остаются доступны. Изучить enclave.log NEW; если recoverable, рестартовать NEW + перезапустить церемонию с fresh `ceremony_nonce` (OLD's `recent_ceremony_nonces` set отклонит повторное использование предыдущего) |
| Ceremony driver вернул `ERR_NONCE_REPLAY` | Оператор пытается повторить с тем же nonce | Сгенерировать fresh nonce; retry |
| Ceremony driver вернул `ERR_DELEGATION_QUORUM` | Недостаточно операторов подписали delegation | Координация с пропавшими операторами; собрать их подписи; retry |
| Церемония завершилась на NEW, но `verify_import_confirmation` вернул durability error | Confirmation от NEW пришёл, но OLD не смог запечатать retired-marker (disk full / FS bug) | OLD's in-memory state **не** retired (per implementation contract); оператор разбирается с disk health. Замечание: ceremony nonce consumed — любой retry ДОЛЖЕН использовать fresh nonce, и OLD's in-memory migration state всё ещё set, поэтому ceremony driver обязан перезапустить с начала с новым nonce после fix диска |

## Recent ceremony nonces — crash-window note (LR-IMPL-3)

OLD enclave's `recent_ceremony_nonces.sealed` обновляется ПОСЛЕ успешного encrypt + LA report production но ДО `verify_import_confirmation`. Есть **короткое crash-window** между encrypt-success и nonce-seal во время которого crash + restart мог бы оставить nonce незарегистрированным; если orchestrator затем попробует свежую церемонию с другим nonce, она нормально проходит — un-recorded nonce забыт, **но** captured ciphertext из первой попытки не может быть replay'нут, потому что он bound к первому nonce, который NEW уже принял (и consumed в собственном recent_nonces set).

На практике это окно — микросекунды (пара `sgx_seal_data + ocall_save_to_file`). Действия оператора: **никаких**. Defense-in-depth model держится — replay path не открывается.

Если вы наблюдаете `ERR_NONCE_REPLAY` от церемонии со свежесгенерированным nonce (что подсказало бы что-то не так с random source), захватите оба OLD's и NEW's `recent_ceremony_nonces.sealed` для forensics и свяжитесь с `dev-perp` до продолжения.

## Reference

- Спецификация: `docs/audit/REQ-7.md` (приватный репо `77ph/xrpl-perp-dex-enclave`)
- Реализация: `EthSignerEnclave/Enclave/path_a.cpp` (приватный)
- Orchestrator integration: `orchestrator/src/node_deploy.rs` + ceremony driver в commit 12
- Audit cycle: REQ-8 R1 verdict в audit channel; R2 verdict ожидается после commit 12
