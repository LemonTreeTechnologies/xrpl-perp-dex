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

**Проверить что Path A + DKG v2 endpoints в бинарнике** (ловит враньё BuildKit-кеша):

```bash
ssh andrey@94.130.18.162 "
  strings ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server \
    | grep -E '/v1/pool/(ecdh|attest|frost|dkg)' | sort -u
"
```

Минимум должны присутствовать v2-endpoints, требуемые §9:
- `/v1/pool/ecdh/pubkey`, `/v1/pool/ecdh/report-data`
- `/v1/pool/attestation-quote`, `/v1/pool/attest/verify-peer-quote`
- `/v1/pool/dkg/round1-generate`, `/v1/pool/dkg/round1-export-share-v2`, `/v1/pool/dkg/round2-import-share-v2`, `/v1/pool/dkg/finalize`
- `/v1/pool/frost/share-export-v2`, `/v1/pool/frost/share-import-v2`

Если хоть один отсутствует — сборка устаревшая, удалить dist-папку и пересобрать с `--no-cache`. Легаси-endpoints `/v1/pool/dkg/round1-export-share` и `/v1/pool/dkg/round2-import-share` (без `-v2`) остаются в бинарнике для обратной совместимости, но **их нельзя использовать кросс-машинно** — они падают с `SGX_ERROR_MAC_MISMATCH`. См. `feedback_dkg_cross_machine_bug.md`.

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

## 9. DKG-церемония (Pedersen, v2 ECDH-over-DCAP транспорт)

Легаси-endpoint `/v1/pool/dkg/round1-export-share` использует `sgx_seal_data` для share-блоба, а это привязывает seal-ключ к локальному CPU TCB — кросс-машинный `unseal` всегда возвращает `SGX_ERROR_MAC_MISMATCH (12289)`. Подтверждено эмпирически 2026-04-26 (см. `feedback_dkg_cross_machine_bug.md`). Кластер использует **v2**-endpoints — `/v1/pool/dkg/round1-export-share-v2` + `round2-import-share-v2` — которые повторяют формат Path A v2 (ECDH-over-DCAP key agreement + AES-128-GCM, AAD биндит `mrenclave_self || group_id || shard_id || ceremony_nonce || sender_pk || recipient_pk`). До bootstrap'а нет FROST `group_id`, поэтому используем `group_id = 32 нулевых байта` как sentinel; после §10 периодический announcer заберёт реальный ключ.

**Participant ID — 0-индексированы**: node-1 → pid 0, node-2 → pid 1, node-3 → pid 2. Enclave валидирует `my_participant_id < n_participants`; передача pid=3 при n=3 падает. `setup_testnet_escrow.py` пишет `signers_config.json` с тремя signer'ами `node-1/2/3`, но это **лейблы** — FROST pid маппинг позиционный в 0-based порядке.

У orchestrator'а пока нет DKG-драйвера (Phase 2.1 кодифицирует §9–§10 как Rust-subcommand). Сейчас это operator-driven curl. **Открой одну интерактивную bash-сессию на Hetzner и выполняй все блоки §9 в ней** — они шарят state через массивы `IPS`, `PK`, `QUOTE`, `GROUP_ZEROS`:

```bash
ssh andrey@94.130.18.162   # держи эту shell открытой; все блоки §9 выполняются внутри неё

GROUP_ZEROS='0000000000000000000000000000000000000000000000000000000000000000'
declare -A IPS=([0]=20.71.184.176 [1]=20.224.243.60 [2]=52.236.130.102)
declare -A PK QUOTE
mkdir -p ~/dkg-shares
```

**Round 0 — pre-DKG attestation round (group_id = zeros).** v2 export/import откажет если peer не находится в локальном `peer_attest_cache` для запрошенного `(shard_id, group_id, peer_pk)`. До DKG `frost_group_id` не сконфигурирован, периодический announcer спит. Прогоняем one-shot attestation вручную с bootstrap-sentinel'ом.

Для каждой ноды: собрать ECDH pubkey + DCAP report_data (привязанный к `(shard=0, group=zeros)`) + DCAP quote. Затем для каждой пары (sender, receiver) receiver верифицирует quote sender'а, заполняя свой кеш.

```bash
for pid in 0 1 2 ; do
  ip=${IPS[$pid]}
  PK[$pid]=$(ssh azureuser@$ip 'curl -k -s https://localhost:9088/v1/pool/ecdh/pubkey' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["pubkey"].removeprefix("0x"))')
  rd=$(ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"shard_id\":0,\"group_id\":\"$GROUP_ZEROS\"}' \
    https://localhost:9088/v1/pool/ecdh/report-data" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["report_data"].removeprefix("0x"))')
  QUOTE[$pid]=$(ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"user_data\":\"$rd\"}' \
    https://localhost:9088/v1/pool/attestation-quote" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["quote_hex"].removeprefix("0x").lower())')
done

NOW=$(date +%s)
for tgt in 0 1 2 ; do
  ip=${IPS[$tgt]}
  for src in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 -c "import json; open('/tmp/v_${tgt}_${src}.json','w').write(json.dumps({
      'quote':'${QUOTE[$src]}','peer_pubkey':'${PK[$src]}',
      'shard_id':0,'group_id':'$GROUP_ZEROS','now_ts':$NOW}))"
    scp /tmp/v_${tgt}_${src}.json azureuser@$ip:/tmp/vbody.json >/dev/null
    rc=$(ssh azureuser@$ip 'curl -k -s -o /dev/null -w %{http_code} -X POST \
      -H "Content-Type: application/json" --data-binary @/tmp/vbody.json \
      https://localhost:9088/v1/pool/attest/verify-peer-quote')
    echo "pid=$tgt verifies pid=$src → HTTP $rc"
  done
done
```

Все 6 верификаций должны вернуть `HTTP 200`. `400 quote must be non-empty hex` — это lstrip foot-gun (см. Приложение A); `403` означает что DCAP collateral устарел или quote невалидна.

**Round 1 — VSS commitment.** Каждая нода генерирует свой commitment polynomial; результат публичный.

```bash
for pid in 0 1 2 ; do
  ip=${IPS[$pid]}
  ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"my_participant_id\":$pid,\"threshold\":2,\"n_participants\":3}' \
    https://localhost:9088/v1/pool/dkg/round1-generate" > /tmp/r1_$pid.json
done
```

Каждый `vss_commitment` это `threshold × 33 байта` сжатых-pubkey'ев в hex (132 chars + `0x`). Не секрет, должен попасть к каждой другой ноде чтобы они могли верифицировать share которую ты сейчас отправишь.

**Round 1.5 — share export (v2).** Каждая нода экспортирует один ECDH-обёрнутый envelope на каждого peer'а.

```bash
NOW=$(date +%s)
for src in 0 1 2 ; do
  ip=${IPS[$src]}
  for tgt in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 -c "import json; open('/tmp/exp_${src}_${tgt}.json','w').write(json.dumps({
      'target_participant_id': $tgt, 'peer_pubkey': '${PK[$tgt]}',
      'shard_id': 0, 'group_id': '$GROUP_ZEROS', 'now_ts': $NOW
    }))"
    scp /tmp/exp_${src}_${tgt}.json azureuser@$ip:/tmp/expbody.json >/dev/null
    ssh azureuser@$ip 'curl -k -s -X POST -H "Content-Type: application/json" \
      --data-binary @/tmp/expbody.json \
      https://localhost:9088/v1/pool/dkg/round1-export-share-v2' > ~/dkg-shares/exp_${src}_to_${tgt}.json
  done
done
```

Каждый ответ несёт `{status, target_participant_id, my_participant_id, envelope: {ceremony_nonce, iv, ct, tag, sender_pubkey}}`. Envelope понятен только целевому enclave (его ECDH identity в AAD).

**Round 2 — import + verify (v2).** Каждая нода импортирует два envelope'а адресованных ей, прикладывая публичный `vss_commitment` отправителя чтобы enclave мог верифицировать share.

```bash
NOW=$(date +%s)
for tgt in 0 1 2 ; do
  ip=${IPS[$tgt]}
  for src in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 - <<PYEOF > /tmp/imp_body.json
import json
exp = json.load(open('$HOME/dkg-shares/exp_${src}_to_${tgt}.json'))
r1  = json.load(open('/tmp/r1_${src}.json'))
print(json.dumps({
    'from_participant_id': $src,
    'sender_pubkey': '${PK[$src]}',
    'shard_id': 0,
    'group_id': '$GROUP_ZEROS',
    'now_ts': $NOW,
    'envelope': exp['envelope'],
    'vss_commitment': r1['vss_commitment'].removeprefix('0x'),
}))
PYEOF
    scp /tmp/imp_body.json azureuser@$ip:/tmp/impbody.json >/dev/null
    rc=$(ssh azureuser@$ip 'curl -k -s -o /dev/null -w %{http_code} -X POST \
      -H "Content-Type: application/json" --data-binary @/tmp/impbody.json \
      https://localhost:9088/v1/pool/dkg/round2-import-share-v2')
    echo "tgt=$tgt imports from src=$src → HTTP $rc"
  done
done
```

Все 6 импортов должны вернуть `HTTP 200`. `403 sender not attested` — §9.0 не заполнил attest cache для этой пары (или TTL кеша 5 минут истёк — перезапусти §9.0). `403 AEAD failed` — envelope подменён в transit; abort, не retry-ить. `403 VSS verification failed` — peer построил share неконсистентную со своим commitment'ом — это сигнал злого peer'а, abort и расследуй.

**Finalize.** Каждая нода агрегирует полученные share и эмитит group pubkey.

```bash
for pid in 0 1 2 ; do
  out=$(ssh azureuser@${IPS[$pid]} 'curl -k -s -X POST https://localhost:9088/v1/pool/dkg/finalize')
  echo "pid=$pid → $out"
done
```

Все три должны выдать **byte-identical** `group_pubkey` (32 байта BIP340 x-only, 64 hex символа + `0x`). Если расходятся — DKG transcript подменён, прерывать.

Reference run (2026-04-27): `group_pubkey = 0x847151fe514df4c5e43914bbc0fcc560c70e91c2550198b1a97aa13a368a2293` на всех трёх нодах.

## 10. Конфигурация Path A группы + рестарт orchestrator'ов

Добавить 32-byte hex в `shards.toml` на каждой Azure ноде:

```toml
[[shards]]
shard_id = 0
enclave_url = "https://localhost:9088/v1"
frost_group_id = "<GROUP_ID_HEX из шага 9>"
```

Перезапустить каждый orchestrator. Path A peer-quote announcer проснётся (он спит когда `frost_group_id` не задан; см. `path_a_redkg.rs`).

## 11. Path A wire test (опциональный regression handle)

§9 уже прогоняет полный ECDH-over-DCAP транспорт: v2 export/import-код **тот же** для DKG-bootstrap и пост-DKG ротации share, отличается только источник данных (`dkg_session.my_shares[]` vs `frost_group.shares[signer_id]`). Если §9 финализировался чисто — wire-формат верифицирован.

Пропусти эту секцию если только тебе не нужен отдельный regression handle для post-DKG share-rotation flow (например, при добавлении четвёртого оператора или share refresh без MRENCLAVE-bump'а). Post-DKG flow использует `/v1/pool/frost/share-export-v2` + `/v1/pool/frost/share-import-v2` (внимание: `/frost/`, не `/dkg/`) и драйвится loopback admin-route'ом orchestrator'а на `/admin/path-a/share-export`. Drop-in pattern для `--admin-listen 127.0.0.1:9099` и `systemctl revert` для удаления после теста — задокументированы в `path_a_redkg.rs` и оригинальном Phase 6b commit-message'е.

## 12. Smoke-тест multisig signing

End-to-end проверка что новый escrow + новый SignerList + новые operator-ключи работают вместе. Фандим свежий secp256k1 user-кошелёк, депозитим несколько XRP в новый escrow, выводим 1 XRP назад. Orchestrator собирает 2-of-3 multisig подписи через `/v1/pool/sign` на enclave каждого оператора и сабмитит через `submit_multisigned`. Success signal — `tesSUCCESS` validated tx на testnet.

**Шаг A — депозит 5 XRP с свежего secp256k1 кошелька.** Запустить на Hetzner:

```bash
ssh andrey@94.130.18.162 "
ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)
python3 - <<PYEOF
import json, time
from xrpl.clients import JsonRpcClient
from xrpl.wallet import Wallet, generate_faucet_wallet
from xrpl.constants import CryptoAlgorithm
from xrpl.models.transactions import Payment
from xrpl.transaction import submit_and_wait
from xrpl.utils import xrp_to_drops

client = JsonRpcClient('https://s.altnet.rippletest.net:51234')
fresh = Wallet.create(algorithm=CryptoAlgorithm.SECP256K1)
funded = generate_faucet_wallet(client, wallet=fresh, debug=False)
print('user_id =', funded.classic_address)
pay = Payment(account=funded.classic_address, destination='\$ESCROW_ADDR', amount=xrp_to_drops(5))
resp = submit_and_wait(pay, client, funded)
print('deposit_tx_hash =', resp.result.get('hash'))
print('seed =', funded.seed)
time.sleep(20)  # дать deposit-сканеру зачислить
PYEOF
"
```

xrpl-py по умолчанию использует `ED25519` для `Wallet.create()` — **обязательно** передай `algorithm=CryptoAlgorithm.SECP256K1`, иначе auth-flow orchestrator'а (который ждёт XRPL secp256k1 family generator) не верифицирует подпись. См. Приложение A.

**Шаг B — withdraw 1 XRP через CLI orchestrator'а на node-1.**

```bash
ssh andrey@94.130.18.162 "
  ssh azureuser@20.71.184.176 \
    \"~/perp/perp-dex-orchestrator withdraw \
      --api http://localhost:3000 \
      --seed '<seed из шага A>' \
      --amount 1.00000000 \
      --destination '<user_id из шага A>'\"
"
```

Успешный ответ несёт `xrpl_tx_hash`. Верифицируй on-chain:

```python
from xrpl.clients import JsonRpcClient
from xrpl.models.requests import Tx
client = JsonRpcClient('https://s.altnet.rippletest.net:51234')
r = client.request(Tx(transaction='<xrpl_tx_hash>')).result
assert r['meta']['TransactionResult'] == 'tesSUCCESS'
assert r['validated'] is True
assert len(r['tx_json']['Signers']) >= 2  # quorum=2 достигнут
```

Массив `Signers[]` должен содержать **2 из 3** operator-адресов — orchestrator останавливает сбор подписей по достижении quorum'а (слот неиспользованного оператора пропускается, не zero-padding).

Reference run (2026-04-27): user `rJWZfQuNvAqLDBBFR5eNGrdztbXSqpbipU`, withdrawal `0AD9913799EC94078CC36463B491B0CF1A7FD4AC8D951246958B6226289A856F` (`tesSUCCESS`, validated, 2 signers — node-1 + node-2; node-3 не понадобилась).

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

## Приложение A — типичные foot-gun'ы

Это конкретные ловушки в которые мы влетали прогоняя процедуру 2026-04-26/27. Никто из них не "баг в процедуре" — это места где Python/JS строковая семантика или дефолты XRPL молча выдают неверные значения которые выглядят правильно.

**`str.lstrip("0x")` это НЕ `str.removeprefix("0x")`.** `lstrip` берёт *множество* символов и удаляет любую комбинацию слева. На `"0x030002…"` оно удаляет `0x03` (matching `{0, x}` повторно), оставляя `"3002…"` — и контент неверный, и нечётная длина (поэтому последующий `hex_to_bytes` падает с "non-empty hex" или подобным запутывающим сообщением). Всегда `removeprefix("0x")`. Нас этот баг укусил в §9.0 attestation-раунде; симптом — `HTTP 400 quote must be non-empty hex` несмотря что quote-строка явно непустая.

**FROST participant_id 0-индексированы.** Enclave валидирует `my_participant_id < n_participants`. При `n_participants=3` валидны только pid'ы `{0, 1, 2}`; `pid=3` возвращает `HTTP 500 DKG round1 generate failed`. Раньше эта дока говорила "Participant ID 1–3" — было неверно; convention зачищен после §9.0 в bump'е 2026-04-27.

**`Wallet.create()` и `Wallet.from_seed()` по умолчанию используют ED25519 даже для secp256k1-seed'ов.** API xrpl-py молча выбирает ed25519 если не передать `algorithm=CryptoAlgorithm.SECP256K1`. Auth-flow orchestrator'а использует XRPL secp256k1 family generator (`derive_keypair_from_seed`); ed25519-ключи не декодируются. Симптом — `HTTP 401` от `/v1/withdraw` хотя seed/address пара выглядит валидной. Та же ловушка существует в `generate_faucet_wallet(client, wallet=…)` — передавай явно secp256k1 `wallet`-аргумент; НЕ вызывай `generate_faucet_wallet(client)` без `wallet` в надежде поменять алгоритм потом.

**`peer_attest_cache` TTL = 5 минут.** v2 export/import откажет если verified DCAP quote peer'а устарел. Если §9.4 finalize фейлит на rerun'е с `403 sender not attested` — обычно причина в том что §9.0 запускали более 5 минут назад; перезапусти его.

**SSH-shell-quoting клобирует большие hex-строки.** Передача 9.5 KB DCAP quote через три уровня shell-escaping (laptop → bash → ssh → bash → curl `-d`) теряет байты молча — принимающая сторона видит JSON-body где `quote` поле обрезано или пустое. Всегда пиши request body в файл через Python `json.dumps`, делай `scp` файла, потом `curl --data-binary @/tmp/body.json`. Паттерны в §9 используют это.

**`unix2dos -q` файлам где оригинал был CRLF.** Некоторые файлы в enclave-репо (`server/server.cpp`, `server/api/v1/pool_handler.hpp`) закоммичены с CRLF line endings. Любой инструмент который перепишет их с LF (`Path.write_text` в Python, итд) делает что `git diff --stat` показывает каждую строку как изменённую, утопляя реальную правку. Восстанавливай CRLF через `unix2dos -q <file>` после правки; diff схлопывается обратно к настоящему изменению.
