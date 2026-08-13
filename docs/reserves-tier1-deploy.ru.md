# Доказательство обязательств — рунбук деплоя Tier-1 (Base-Sepolia)

> Честная маркировка: это **доказательство обязательств одним аттестованным
> enclave**, НЕ «2-of-3» и НЕ «reserves». Настоящий 2-of-3 (независимый пересчёт
> корня на каждой ноде) — это **Tier-2**, гейт на полную репликацию состояния
> (AC-R2-3). «Reserves» (активы ≥ обязательства) требует Tier-2 аттестации
> XRPL-баланса. См. `docs/audit/RESP-commitment-r2-state-replication-gap.md`.

## Что делает
**Enclave секвенсера** — единственный держатель авторитетного perp-состояния —
считает исчерпывающий per-asset merkle-корень обязательств над своим sealed-
состоянием, отказывает при `custody < liabilities` (по каждому активу) и
подписывает Gnosis-Safe EIP-712 `SafeTxHash` для
`ReservesRegistry.publishReserves(epoch, root, snapshotHash)` **внутри enclave**.
Оркестратор лишь ретранслирует подпись владельца в Safe и платит газ — корень он
не считает и подделать не может (AC-R2-1).

## Предпосылки
- **EVM-адрес пула секвенсера** — владелец Safe. Это `local_signer.address` из
  `signers_config.json` ноды (тот же 0x…-адрес, что для governance-подписи).
- **Gas-EOA** — *hot*-ключ, который только платит газ Base-Sepolia и является
  `msg.sender` для `execTransaction`. Подделать коммит **не может** (Safe
  проверяет подпись владельца-enclave). Сгенерировать свежий ключ; пополнить
  Base-Sepolia ETH. **Никогда** не переиспользовать ключи enclave/escrow.
- **QuickNode Base-Sepolia RPC** (содержит API-ключ — секрет).
- Foundry (`forge`) для деплоя реестра; контракт+скрипт лежат в **enclave**-репо:
  `EthSignerEnclave/contracts/reserves_registry/`.

## Шаги
1. **Взять EVM-адрес пула секвенсера** (владелец Safe):
   `jq -r '.local_signer.address' <signers_config.json>`  → 0x…
2. **Задеплоить Safe 1-of-1** на Base-Sepolia с этим владельцем и порогом 1
   (через app.safe.global или safe-cli). Записать `SAFE=0x…`.
3. **Задеплоить ReservesRegistry** с `authority = SAFE` (скрипт уже есть):
   ```
   cd EthSignerEnclave/contracts/reserves_registry
   RESERVES_AUTHORITY=$SAFE \
   forge script script/DeployReservesRegistry.s.sol \
       --rpc-url "$BASE_SEPOLIA_RPC" --broadcast --private-key "$DEPLOYER_PK"
   ```
   Записать `REGISTRY=0x…`. (`$DEPLOYER_PK` — пополненный deployer-ключ, только env.)
4. **Пополнить gas-EOA** Base-Sepolia ETH.
5. **Включить publisher** на **секвенсере** через `Environment=` в его **systemd**-
   юните (не shell-export, не в репозиторий — правило no-manual-shell-deploys):
   ```
   RESERVES_PUBLISH=1
   RESERVES_RPC_URL=https://…base-sepolia.quiknode.pro/<KEY>/   # секрет
   RESERVES_GAS_KEY=0x<приватный ключ gas-EOA>                   # секрет, hot-key
   RESERVES_REGISTRY=<REGISTRY>
   RESERVES_SAFE=<SAFE>
   RESERVES_CHAIN_ID=84532
   RESERVES_INTERVAL_SECS=3600
   ```
   `systemctl daemon-reload && systemctl restart <юнит оркестратора>`.
6. **Проверка** (E-1):
   - Секвенсер логирует `reserves-commit published to Base-Sepolia tx=0x…`.
   - On-chain `ReservesRegistry.latestReserves()` возвращает те же `(epoch, root,
     snapshotHash)`, что произвёл enclave; `epoch` монотонно растёт.
   - Пересчитать корень off-chain по набору листьев enclave и сверить (проверка
     включения для известного аккаунта).

## Tier-1 → Tier-2 (позже, после полной репликации AC-R2-3)
Добавить EVM-ключи enclave остальных нод как владельцев Safe и поднять порог до
2-of-3 — это governance-действие Safe (**добавить владельца + сменить порог**),
**без смены контракта и без ре-аудита** реестра. Реестр остаётся
`onlyAuthority(Safe)`.

## Замечания по безопасности
- Gas-EOA — **hot-key**: наименьшие привилегии (только газ), ротируемый,
  изолирован от ключей enclave/escrow. Компрометация ⇒ максимум DoS/слив газа,
  никогда не подделка коммита.
- `RESERVES_RPC_URL` + `RESERVES_GAS_KEY` — **секреты**: только systemd env, не
  CLI-аргументы (видны в ps), не в репозиторий.
- Publisher — **только секвенсер** и **opt-in** (`RESERVES_PUBLISH=1`); по
  умолчанию выключен.
