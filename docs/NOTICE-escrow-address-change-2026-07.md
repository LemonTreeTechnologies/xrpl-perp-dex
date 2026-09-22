# NOTICE — testnet escrow (deposit address) is changing

**Status:** SUPERSEDED — historical record · **Original date:** 2026-07-06 · **Scope:** testnet only

---

> ## ⚠️ Superseded — read this first
>
> **EN.** This notice records the 2026-07-06 rotation. The address it announces as NEW
> (`rUY1bpf7X2…`) is **no longer the escrow**: a later genesis rotated again, and the current
> testnet deposit address is **`rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd`**. Do not copy any address
> out of this page — read `deposit_address` from `GET /v1/system/status`, which serves the live
> value.
>
> **This class of change is over.** A state-preserving MRENCLAVE migration (Path-A) now carries
> the sealed keys across an enclave upgrade, so an upgrade no longer forces a fresh escrow. The
> reason given below — "the SignerList cannot be re-pointed at the new keys" — no longer holds.
>
> **RU.** Это уведомление фиксирует ротацию от 2026-07-06. Адрес, объявленный в нём новым
> (`rUY1bpf7X2…`), **больше не является эскроу**: позже был ещё один генезис, и действующий
> адрес депозита на testnet — **`rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd`**. Не копируйте адрес с
> этой страницы — читайте поле `deposit_address` из `GET /v1/system/status`.
>
> **Сам класс таких переездов закончился.** Миграция MRENCLAVE с сохранением состояния (Path-A)
> переносит запечатанные ключи через обновление анклава, поэтому обновление больше не требует
> нового эскроу. Причина, названная ниже — «SignerList нельзя перенаправить на новые ключи», —
> больше не действует.

---

## EN (for Tom — copy/paste)

We are deploying the β cluster (the off-chain membership-authority build) to the
testnet cluster. This deploy bumps the SGX enclave measurement (MRENCLAVE), which
rotates every node's pool signing key. Because the current escrow's master key is
disabled on-chain, its SignerList cannot be re-pointed at the new keys — so the
testnet escrow is being **replaced with a fresh account**, not reused.

**What changes for you:** the **deposit address** for testnet moves.

- OLD (deprecated after cutover): `r3MfCgxJeP8JPpAySgFukoj92kJHnAF11E`
- NEW: `rUY1bpf7X2ySfY1AL2PFKCSXyQKNpMWQGD` (live as of 2026-07-06; also served by the API `deposit_address` field)

**What you need to do:**
1. The deposit address is already served live by the API as the `deposit_address`
   field of the status response (see `docs/frontend-api-guide.md`). **The robust
   fix is to read it live from that field and never hardcode it** — then this kind
   of change is a no-op for you.
2. If you hardcoded `r3MfCgxJeP8…` anywhere (frontend, bots, synthetic traders,
   fixtures), switch those to the NEW address above (or, better, to the live field).
3. Old testnet balances under the old escrow do not carry over — re-fund test
   accounts against the new address from the faucet as usual.

Nothing on mainnet is affected (there is no mainnet). This is testnet-only and
expected to recur on future MRENCLAVE bumps — reading `deposit_address` live is the
durable answer.

---

## RU (для Андрея)

Разворачиваем β-кластер (сборка, где кластер — хозяин своего состава) на
testnet-кластер. Деплой поднимает новую MRENCLAVE → у каждой ноды новый пул-ключ
подписи. Мастер-ключ текущего эскроу отключён on-chain, поэтому его SignerList
нельзя перенаправить на новые ключи — значит testnet-эскроу **заменяем на свежий
аккаунт**, а не переиспользуем.

**Что меняется:** адрес депозита (deposit address) на testnet.

- СТАРЫЙ (устаревает после cutover): `r3MfCgxJeP8JPpAySgFukoj92kJHnAF11E`
- НОВЫЙ: `rUY1bpf7X2ySfY1AL2PFKCSXyQKNpMWQGD` (в проде с 2026-07-06; также отдаётся полем API `deposit_address`)

**Что Тому сделать:** адрес уже отдаётся живьём в поле `deposit_address` статус-ответа
API — надёжное решение читать его оттуда и не хардкодить; если где-то захардкожен
`r3MfCgxJeP8…` — заменить на новый (или на живое поле). Балансы под старым эскроу не
переносятся — перезалить тестовые аккаунты с faucet на новый адрес.

Mainnet не затронут (его нет). Это только testnet и будет повторяться на будущих
bump'ах MRENCLAVE — читать `deposit_address` живьём и есть долговременный ответ.
