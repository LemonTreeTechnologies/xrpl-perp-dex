# Процедура смены MRENCLAVE на testnet-кластере

**Статус:** авторитетный документ для обновлений энклейва на testnet.
**Область:** любое изменение C++ кода энклейва (= новый MRENCLAVE) на 3-узловом Azure testnet кластере.
**Mainnet:** см. `deployment-procedure-ru.md §11.5 — Путь B`. Этот документ её НЕ заменяет.

## 0. Инварианты

1. **Sealing привязан к MRENCLAVE.** Каждый sealed blob (`account.sealed`, `frost_share.sealed`, perp state, `ecdh_identity.sealed`) запечатан на MRENCLAVE. Новый MRENCLAVE не может распечатать ничего из старого. MRSIGNER-sealing полностью отклонён — см. `deployment-dilemma.ru.md §"Стратегия 2 — ОТКЛОНЕНА"` и `sgx-enclave-capabilities-and-limits-ru.md`.

2. **Поэтому каждое обновление энклейва = ротация ключей.** Все XRPL multisig ключи перегенерируются. On-chain SignerList становится невалиден в момент свопа бинарника, поэтому SignerListSet обязателен и безусловен.

3. **Testnet vs mainnet.** Testnet терпит одноволновой rip-and-replace потому что XRP под риском нет. Testnet escrow **тоже ротируется** при каждом bump'е — свежий faucet-funded escrow + свежий SignerListSet, не re-key существующего. Предыдущий escrow остаётся осиротевшим. Mainnet ведёт себя иначе: тот же escrow адрес forever, key rotation только через `deployment-procedure-ru.md §11.5` Путь B (staging-порт, peer DCAP verify, двухшаговый SignerListSet с буфером quorum→3, soak, shred, promote). **Не** переносить testnet-сокращения в mainnet.

4. **Сервисы на Hetzner вне кластера.** Dev enclave на `:9089` и dev orchestrator на `:3003` (юниты `perp-dex-enclave-dev.service` и `perp-dex-orchestrator-dev.service`) — НЕ часть testnet-кластера: другой escrow, другой p2p-порт. Mainnet enclave на `:9088` (PID с прошлой загрузки, без orchestrator'а) тоже вне scope. **Все три не трогать.** Stop/start команды этой процедуры адресованы ТОЛЬКО трём Azure VM.

## 1. Пре-флайт

**Самодостаточные prerequisites.** Эта процедура не требует никаких секретов которые держит в голове оператор. Testnet escrow seed лежит по каноническому пути `~/.secrets/perp-dex-xrpl/escrow-testnet.json` на Hetzner (mode 0600). Если файла нет или он stale — шаг 7 создаёт свежий testnet escrow через faucet и записывает новый seed туда же. Никакой зависимости от человеческой памяти. См. `feedback_secrets_canonical_files.md` для общего правила.

Запускать с локального ноутбука. Все команды read-only.

```bash
# 1.1 Подтвердить что мы на ноутбуке, не Hetzner. Сборка будет на Hetzner.
hostname

# 1.2 Оба репо на ожидаемых tip'ах на Hetzner.
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl && git fetch && git log --oneline -1 origin/master
  cd ~/xrpl-perp-dex-enclave && git fetch && git log --oneline -1 origin/main
"

# 1.3 Инвентарь 3 Azure VM — все должны быть достижимы через бастион.
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  ssh andrey@94.130.18.162 "ssh -o ConnectTimeout=5 azureuser@$ip 'hostname'"
done
```

## 2. Сборка на Hetzner

Согласно `feedback_enclave_build_gate.md`: ноутбук — не build gate; считается только Hetzner.

```bash
ssh andrey@94.130.18.162 "
  set -e
  # Безопасность: отказаться затирать незакоммиченные правки в любом репо.
  for repo in ~/llm-perp-xrpl ~/xrpl-perp-dex-enclave; do
    cd \$repo
    if [ -n \"\$(git status -uno --porcelain)\" ]; then
      echo \"\$repo working tree dirty — закоммитить/застэшить перед продолжением\" >&2
      exit 1
    fi
  done

  # Синхронизация рабочих копий до tip'ов origin
  cd ~/llm-perp-xrpl && git checkout master && git pull --ff-only
  cd ~/xrpl-perp-dex-enclave && git checkout main && git reset --hard origin/main

  # Orchestrator (~30 с с кешем, ~2 мин с нуля)
  cd ~/llm-perp-xrpl/orchestrator
  ~/.cargo/bin/cargo build --release

  # Enclave — ВСЕГДА --no-cache. BuildKit-кеш COPY уже вводил в заблуждение;
  # лишние 5–10 минут стоят определённости.
  cd ~/xrpl-perp-dex-enclave/EthSignerEnclave
  TAG=phase7-pathA-\$(date +%Y%m%d-%H%M%S)
  docker build --no-cache -f Dockerfile.azure -t perp-dex-enclave:\$TAG .

  # Извлечение в свежую dist-папку (не затирать предыдущий baseline)
  mv dist-azure dist-azure.prev-\$(date +%Y%m%d-%H%M%S) 2>/dev/null || true
  mkdir -p dist-azure
  cid=\$(docker create perp-dex-enclave:\$TAG)
  docker cp \$cid:/build/out/enclave.signed.so dist-azure/
  docker cp \$cid:/build/out/perp-dex-server   dist-azure/
  docker rm \$cid

  # Зафиксировать build manifest: git_sha + sha256 + timestamp.
  cat > dist-azure/build-manifest.txt <<EOF
git_sha=\$(git rev-parse --short HEAD)
build_date=\$(date -u +%Y-%m-%dT%H:%M:%SZ)
image=perp-dex-enclave:\$TAG
enclave_sha256=\$(sha256sum dist-azure/enclave.signed.so | awk '{print \$1}')
server_sha256=\$(sha256sum  dist-azure/perp-dex-server  | awk '{print \$1}')
EOF
  cat dist-azure/build-manifest.txt
"
```

**Проверить что Path A endpoints в бинарнике** (ловит враньё BuildKit-кеша):

```bash
ssh andrey@94.130.18.162 "
  strings ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server \
    | grep -E '/v1/pool/(ecdh|attest|frost)' | sort -u
"
```

Должны быть видны все 16 endpoints, включая `/v1/pool/ecdh/pubkey`, `/v1/pool/attest/verify-peer-quote`, `/v1/pool/frost/share-export-v2`, `/v1/pool/frost/share-import-v2`. Если хоть один отсутствует — сборка устаревшая, удалить dist-папку и пересобрать с `--no-cache`.

## 3. Остановка кластера (одна волна)

На testnet нет окна для rolling-upgrade. Останавливаем всё перед свопом — нода которая поднимется новой пока peer'ы старые, опубликует в `perp-dex/path-a/peer-quote` для непонимающих слушателей и зашумит логи.

Останавливаем **только 3 Azure VM**. Hetzner-юниты (`perp-dex-enclave-dev`, `perp-dex-orchestrator-dev`) и mainnet enclave на `:9088` не трогаем — см. §0 инвариант 4.

Параллельно сохраняем текущий `signers_config.json` каждой Azure-ноды чтобы шаг 13 мог откатить чисто если новая процедура зафейлится.

```bash
ssh andrey@94.130.18.162 "
  TS=\$(date +%Y%m%d-%H%M%S)
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    echo == \$ip ==
    ssh azureuser@\$ip \"
      sudo systemctl stop perp-dex-orchestrator perp-dex-enclave
      cp -a /home/azureuser/perp/signers_config.json /home/azureuser/perp/signers_config.json.prev-\$TS
    \"
  done
"
```

## 4. Своп бинарников

Хранить предыдущий артефакт рядом с новым. Папка `accounts/` на каждой ноде тоже копируется с timestamp'ом — несмотря на то что новый MRENCLAVE её не unseal-ит, оставляем как forensic evidence, не удаляя вслепую.

Чтобы избежать трёхуровневого shell-экранирования (ноутбук → Hetzner → Azure), собираем per-Azure swap-скрипт один раз на Hetzner через heredoc, потом `scp` и `bash` на каждой VM. Все переменные раскрываются на Hetzner до отправки.

```bash
ssh andrey@94.130.18.162 'bash -s' <<'OUTER'
set -e
TS=$(date +%Y%m%d-%H%M%S)
cat > /tmp/swap.sh <<SCRIPT
#!/bin/bash
set -e
cd /home/azureuser/perp
mv enclave.signed.so       enclave.signed.so.prev-${TS}
mv perp-dex-server         perp-dex-server.prev-${TS}
mv perp-dex-orchestrator   perp-dex-orchestrator.prev-${TS}
cp -a accounts             accounts.prev-${TS}
SCRIPT
chmod +x /tmp/swap.sh

for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  echo "== $ip =="
  scp /tmp/swap.sh azureuser@$ip:/tmp/swap.sh
  ssh azureuser@$ip 'bash /tmp/swap.sh'
  scp ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/enclave.signed.so azureuser@$ip:/home/azureuser/perp/
  scp ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server   azureuser@$ip:/home/azureuser/perp/
  scp ~/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator    azureuser@$ip:/home/azureuser/perp/
  ssh azureuser@$ip 'rm -rf ~/perp/accounts && mkdir ~/perp/accounts && rm -f /tmp/swap.sh'
done
rm -f /tmp/swap.sh
OUTER
```

## 5. Старт только enclave'ов

Orchestrator'ы остаются выключенными — они пока не могут аутентифицироваться против live testnet escrow (старый SignerList ещё on-chain).

```bash
ssh andrey@94.130.18.162 "
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    ssh azureuser@\$ip 'sudo systemctl start perp-dex-enclave && sleep 2 && curl -k -s https://localhost:9088/v1/health'
  done
"
```

Должен прийти здоровый ответ от каждого enclave на `:9088`. Новый enclave стартует с пустым sealed state.

## 6. Генерация свежих keypair'ов на каждой ноде

Бинарник orchestrator'а одновременно работает как operator CLI. С каждой Azure ноды обращаемся к локальному enclave на `:9088`:

```bash
ssh andrey@94.130.18.162 "
  mkdir -p ~/phase7-entries
  for i in 1 2 3; do
    case \$i in
      1) ip=20.71.184.176 ;;
      2) ip=20.224.243.60 ;;
      3) ip=52.236.130.102 ;;
    esac
    ssh azureuser@\$ip \"
      cd ~/perp
      ./perp-dex-orchestrator operator-setup \\
        --enclave-url https://localhost:9088/v1 \\
        --name node-\$i \\
        --output /tmp/node-\$i.json
    \"
    scp azureuser@\$ip:/tmp/node-\$i.json ~/phase7-entries/
  done
"
```

Каждый `/tmp/node-N.json` содержит новый XRPL address, compressed pubkey, session key. Соответствующий приватный ключ enclave seal-ит локально.

## 7. Создание свежего testnet escrow + регистрация signer'ов (one-shot)

На testnet мы **не** сохраняем escrow между enclave-bump'ами. Seed предыдущего escrow редко captured (исходный `setup_testnet_escrow.py` печатал в stdout, см. `feedback_secrets_canonical_files.md`), а faucet-escrow бесплатный. Каждый bump = свежий testnet escrow со свежим seed-файлом.

Пропатченный `setup_testnet_escrow.py` делает всё одним действием: faucet-fund нового escrow, SignerListSet для 3 новых node-адресов, отключение master key, запись seed'а в `~/.secrets/perp-dex-xrpl/escrow-testnet.json` (0600).

```bash
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl

  # Отодвинуть в сторону предыдущий testnet seed-файл (с прошлого bump'а)
  if [ -f ~/.secrets/perp-dex-xrpl/escrow-testnet.json ]; then
    mv ~/.secrets/perp-dex-xrpl/escrow-testnet.json \\
       ~/.secrets/perp-dex-xrpl/escrow-testnet.json.prev-\$(date +%Y%m%d-%H%M%S)
  fi

  # Достать 3 новых xrpl_address из вывода operator-setup
  N1=\$(jq -r .xrpl_address ~/phase7-entries/node-1.json)
  N2=\$(jq -r .xrpl_address ~/phase7-entries/node-2.json)
  N3=\$(jq -r .xrpl_address ~/phase7-entries/node-3.json)

  python3 orchestrator/scripts/setup_testnet_escrow.py \\
    --signer node-1=\$N1 \\
    --signer node-2=\$N2 \\
    --signer node-3=\$N3 \\
    --quorum 2
"
```

Вывод печатает `ESCROW_ADDRESS=r…` и `SEED_FILE=…`. Проверить на https://testnet.xrpl.org что новый адрес имеет quorum 2 с тремя node-адресами и master key disabled.

Сохранить новый escrow address как shell-переменную для шага 8:

```bash
ESCROW_ADDR=$(ssh andrey@94.130.18.162 "jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json")
echo "$ESCROW_ADDR"
```

Единый `signers_config.json` для кластера собирается с **новым** escrow address:

```bash
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl/orchestrator
  ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)
  ./target/release/perp-dex-orchestrator config-init \\
    --entries ~/phase7-entries/node-1.json \\
              ~/phase7-entries/node-2.json \\
              ~/phase7-entries/node-3.json \\
    --escrow-address \$ESCROW_ADDR \\
    --quorum 2 \\
    --output ~/phase7-entries/signers_config.json
"
```

## 8. Распространение config'а + старт orchestrator'ов

Каждой ноде нужны:
- Копия `signers_config.json` со своим `local_signer` полем (структура: `FullSignersConfig` в `cli_tools.rs`).
- Обновлённый `start_orchestrator.sh` (или systemd unit ExecStart) с `--escrow-address` указывающим на **новый** escrow из шага 7.

```bash
ssh andrey@94.130.18.162 "
  cd ~/phase7-entries
  ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)

  for i in 1 2 3; do
    case \$i in
      1) ip=20.71.184.176 ;;
      2) ip=20.224.243.60 ;;
      3) ip=52.236.130.102 ;;
    esac

    # Собираем per-node signers_config с правильным local_signer указателем
    jq --argjson local \"\$(cat node-\$i.json)\" '. + {local_signer: \$local}' \\
      signers_config.json > /tmp/signers_config_node-\$i.json
    scp /tmp/signers_config_node-\$i.json azureuser@\$ip:/home/azureuser/perp/signers_config.json

    # Обновляем start_orchestrator.sh: меняем старый --escrow-address rUjzn... на новый.
    # sed по regex 'r[1-9A-HJ-NP-Za-km-z]{24,34}' (XRPL r-address shape).
    ssh azureuser@\$ip \"
      cp -a ~/perp/start_orchestrator.sh ~/perp/start_orchestrator.sh.prev-\$(date +%Y%m%d-%H%M%S)
      sed -i -E 's|--escrow-address +r[1-9A-HJ-NP-Za-km-z]{24,34}|--escrow-address \$ESCROW_ADDR|' ~/perp/start_orchestrator.sh
      grep -- '--escrow-address' ~/perp/start_orchestrator.sh
      sudo systemctl start perp-dex-orchestrator
    \"
  done
"
```

Подождать ~30 с, проверить p2p mesh:

```bash
ssh andrey@94.130.18.162 "
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    ssh azureuser@\$ip 'curl -s http://localhost:3000/v1/health'
    echo
  done
"
```

## 9. DKG-церемония (4-этапный Pedersen)

У orchestrator'а нет DKG-драйвера — это operator-driven curl. 4 этапа выполняются на каждой Azure ноде против её локального enclave на `:9088`. Participant ID = 1–3, threshold 2, n 3.

**Round 1 — VSS commitment.** Каждая нода генерирует свой commitment polynomial; он публичный.

```bash
# Запускать на каждой Azure ноде, подставляя MY_ID = 1, 2, 3
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round1-generate \
  -H 'Content-Type: application/json' \
  -d '{"my_participant_id": MY_ID, "threshold": 2, "n_participants": 3}' \
  > /tmp/round1-MY_ID.json
```

В ответе `vss_commitment` (hex). Не секрет, должен быть отправлен двум другим нодам.

**Round 1.5 — попарный экспорт sealed share.** Каждая нода генерирует один sealed share для каждого peer'а.

```bash
# На ноде MY_ID, для каждого peer TARGET_ID ∈ {1,2,3} \ {MY_ID}:
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round1-export-share \
  -H 'Content-Type: application/json' \
  -d '{"target_participant_id": TARGET_ID}' \
  > /tmp/share-from-MY_ID-to-TARGET_ID.json
```

Теперь оператор вручную перетасовывает пары `(sealed_share, vss_commitment)` между Azure VM'ами через Hetzner-бастион (Azure-to-Azure SSH закрыт, открыт только для ключа Hetzner). Sealed share зашифрован под идентичность целевого enclave; vss_commitment публичный.

**Round 2 — импорт + verify.** Каждая нода импортирует две полученные share от peer'ов; enclave проверяет каждую share против соответствующего VSS commitment.

```bash
# На ноде TARGET_ID, для каждого FROM_ID ∈ {1,2,3} \ {TARGET_ID}:
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round2-import-share \
  -H 'Content-Type: application/json' \
  -d "{
    \"from_participant_id\": FROM_ID,
    \"sealed_share\":      \"$(jq -r .sealed_share share-from-FROM_ID-to-TARGET_ID.json)\",
    \"vss_commitment\":    \"$(jq -r .vss_commitment round1-FROM_ID.json)\"
  }"
```

500 здесь означает что VSS verification fail-нул — peer либо misbehave, либо share повредился в transit. **Прервать и стартовать с Round 1**. Не делать silent retry; сначала разобраться.

**Finalize.** Каждая нода финализирует; все три должны выдать одинаковый `group_pubkey` (32 байта BIP340 x-only).

```bash
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/finalize > /tmp/finalize.json
GROUP_ID_HEX=$(jq -r .group_pubkey /tmp/finalize.json)
echo "$GROUP_ID_HEX"  # 64 hex символа
```

Cross-check: значение `group_pubkey` должно быть byte-identical на всех трёх нодах. Если расходятся — DKG transcript подменён, прерывать.

## 10. Конфигурация Path A группы + рестарт orchestrator'ов

Добавить 32-byte hex в `shards.toml` на каждой Azure ноде:

```toml
[[shards]]
shard_id = 0
enclave_url = "https://localhost:9088/v1"
frost_group_id = "<GROUP_ID_HEX из шага 9>"
```

Перезапустить каждый orchestrator. Path A peer-quote announcer проснётся (он спит когда `frost_group_id` не задан; см. `path_a_redkg.rs`).

## 11. Path A wire test

Выбрать одну ноду как ceremony sender — пусть node-1. Перезапустить её orchestrator с `--admin-listen 127.0.0.1:9099`. Две другие без изменений. По дизайну безопасности admin-listen по умолчанию выключен и биндится только loopback.

Добавляем флаг через systemd drop-in. Сначала смотрим текущий ExecStart на node-1 чтобы знать что воспроизводить:

```bash
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'systemctl cat perp-dex-orchestrator | grep ExecStart'"
```

Создаём drop-in переопределяющий ExecStart на текущую строку + новый флаг:

```bash
ssh andrey@94.130.18.162 'ssh azureuser@20.71.184.176 "sudo systemctl edit perp-dex-orchestrator"'
# В редакторе вставить:
#   [Service]
#   ExecStart=
#   ExecStart=<вставить текущий ExecStart буквально, дописать> --admin-listen 127.0.0.1:9099
# Сохранить, выйти. Затем:
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator'"
```

Пустая строка `ExecStart=` обязательна: она сбрасывает унаследованное значение перед установкой нового.

Подождать ~5 минут пока периодический peer-quote announcer (interval 240 с) сделает все три peer'а видимыми в attest cache друг друга. Проверить через `/v1/pool/attest/peer-lookup` если нужно.

Триггерим share-export на node-1:

```bash
ssh azureuser@20.71.184.176 "
  curl -s -X POST http://127.0.0.1:9099/admin/path-a/share-export \\
    -H 'Content-Type: application/json' \\
    -d '{
      \"shard_id\": 0,
      \"group_id\": \"$GROUP_ID_HEX\",
      \"signer_id\": 1,
      \"targets\": [
        \"<node-2 ECDH pubkey из /v1/pool/ecdh/pubkey>\",
        \"<node-3 ECDH pubkey из /v1/pool/ecdh/pubkey>\"
      ]
    }'
"
```

На node-2 и node-3 в orchestrator-логах видим `verified peer quote` потом `imported v2 FROST share`. На node-1 в response body `published: 2, refused: 0, errored: 0`.

После теста снимаем override и перезапускаем чтобы admin-listen вернулся в off:

```bash
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'sudo systemctl revert perp-dex-orchestrator && sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator'"
```

`systemctl revert` удаляет все drop-in'ы и возвращает базовый юнит. Admin surface не должна оставаться live.

## 12. Smoke-тест multisig signing

End-to-end проверка что новый SignerList работает. Сабмит любого мелкого testnet withdrawal через API orchestrator'а — он триггерит multisig flow который ходит в `/pool/sign` на каждом enclave и сабмитит через `submit_multisigned`. Подписанный + подтверждённый tx hash = success signal.

## 13. Откат

Если что-то фейлится между шагами 5 и 12 и кластер не recovery'абелен:

1. `systemctl stop perp-dex-enclave perp-dex-orchestrator` на всех 3 Azure нодах.
2. Вернуть `*.prev-<timestamp>` артефакты сохранённые ранее:
   - бинарники из шага 4 (`enclave.signed.so.prev-…`, `perp-dex-server.prev-…`, `perp-dex-orchestrator.prev-…`)
   - sealed state из шага 4 (`accounts.prev-<TS>` → `accounts/`)
   - `signers_config.json.prev-<TS>` сохранённый в шаге 3
   - `start_orchestrator.sh.prev-<TS>` сохранённый в шаге 8 (восстанавливает старый `--escrow-address`)
3. Если §11 был достигнут — снять admin-listen override: `sudo systemctl revert perp-dex-orchestrator` на ноде-отправителе.
4. Перезапустить enclave'ы + orchestrator'ы. Предыдущий testnet escrow (с до-bump'а) всё ещё в чейне со своим старым SignerList — восстановленные бинарники + восстановленный config будут работать против него как раньше.
5. **Если §7 уже создал новый escrow:** on-chain rollback не нужен — это faucet-funded testnet, бросаем. Seed-файл `~/.secrets/perp-dex-xrpl/escrow-testnet.json` отодвигаем (он уже был отодвинут до начала §7, см. `escrow-testnet.json.prev-<TS>` в `~/.secrets/perp-dex-xrpl/`).

Задокументировать failure mode в этом файле в новой секции. Future-вы скажут спасибо.

## Что НЕ покрывает эта процедура

- **Mainnet** обновления — см. `deployment-procedure-ru.md §11.5 — Путь B`.
- **DKG без смены enclave'а** (например, добавление четвёртого оператора в существующую группу) — это отдельный документ; этот предполагает полный reset.
- **Recovery после потери share'ов** — flow recovery (`ecall_generate_account_with_recovery`) вне scope.
