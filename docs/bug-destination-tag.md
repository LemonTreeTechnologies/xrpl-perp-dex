# Bug: XRPL Destination Tag not handled in deposit/withdraw flow

**Severity:** High — affects correctness of user fund attribution  
**Found:** 2026-04-18  
**Status:** Open  
**Affects:** perp-dex-orchestrator (likely also Phoenix PM)

---

## Problem

The deposit scanner (`xrpl_monitor.rs`) identifies deposits by **sender address** (`tx["Account"]`). The `DestinationTag` field in incoming Payment transactions is completely ignored.

This means:

1. **Custodial wallets (exchanges):** If two users deposit from the same exchange hot wallet, both deposits are credited to the same `user_id` (the exchange address). Funds are misattributed.

2. **User identification:** There is no mechanism for a user to specify *which DEX account* a deposit should credit. The only identity is the sender's XRPL address.

3. **Withdrawals to exchanges:** When a user withdraws to an exchange address, the signed XRPL Payment transaction does not include a `DestinationTag`. The exchange cannot route the funds to the user's exchange account → funds stuck in exchange limbo.

## Scope of the bug

| Component | File | Issue |
|-----------|------|-------|
| Deposit scanner | `orchestrator/src/xrpl_monitor.rs:155` | `sender = tx["Account"]` — DestinationTag not read |
| Deposit event | `orchestrator/src/xrpl_monitor.rs:12` | `DepositEvent` has no `destination_tag` field |
| Deposit credit | `orchestrator/src/main.rs:960` | `deposit(&deposit.sender, ...)` — user_id = sender address |
| Withdraw signing | `Enclave/Enclave.cpp` (ecall_perp_withdraw_check_and_sign) | Signed Payment has no DestinationTag |
| User registration | — | No mapping table `destination_tag ↔ user_id` exists |

## How XRPL Destination Tags work

- `DestinationTag`: `u32` field on Payment transactions (0–4294967295)
- Used by exchanges and custodial services to identify the recipient within a shared address
- `RequireDest` flag (`asfRequireDest`): account-level flag that **rejects incoming Payments without a DestinationTag** — we should set this on the escrow account
- The tag is part of the transaction, not the address — it's not a separate address

## Proposed fix

### 1. Escrow account: enable `RequireDest`

Set `asfRequireDest` flag on escrow account via `AccountSet` transaction. This makes XRPL itself reject any Payment without a destination tag — prevents unattributable deposits at the protocol level.

### 2. Deposit flow

```
DepositEvent {
    sender: String,
    amount: String,
    tx_hash: String,
+   destination_tag: Option<u32>,  // from tx["DestinationTag"]
}
```

User identification logic:
- If `destination_tag` is present → look up `user_id` from a registration mapping
- If no tag and `RequireDest` is set → XRPL rejects the tx (never reaches us)
- Fallback (tag present but unknown) → hold in pending, don't credit

### 3. User registration

Need a mapping: `destination_tag (u32) → user_id (String)`

Options:
- **Simple:** tag = hash(user_xrpl_address) mod 2^32, displayed in UI at registration
- **Explicit:** user registers on DEX, gets assigned a unique tag, must include it in deposits
- **Account-based:** keep using sender address as primary ID, tag is optional qualifier

### 4. Withdraw flow

When signing a withdrawal Payment, include `DestinationTag` if the user has specified one (e.g., withdrawing to an exchange).

```
// In the XRPL Payment transaction blob:
{
    "TransactionType": "Payment",
    "Destination": "<user_withdraw_address>",
    "DestinationTag": <user_specified_tag>,  // NEW
    "Amount": "...",
    ...
}
```

## For Phoenix PM

Check if your deposit scanner has the same issue. If you're using `account_tx` and keying deposits by sender address, you have the same bug. Any user depositing from a centralized exchange will have their funds credited to the exchange's hot wallet address instead of their own account.

Quick grep: search for `DestinationTag` in your deposit monitoring code. If zero hits — you have this bug.

---

# Баг: XRPL Destination Tag не обрабатывается в потоке депозитов/выводов

**Серьёзность:** Высокая — влияет на корректность атрибуции средств пользователей  
**Найден:** 2026-04-18  
**Статус:** Открыт

## Проблема

Сканер депозитов идентифицирует пользователя по **адресу отправителя**. Поле `DestinationTag` во входящих Payment транзакциях полностью игнорируется.

Последствия:
1. Два пользователя отправляют с одной биржи → оба депозита зачисляются на один `user_id` (адрес горячего кошелька биржи)
2. Нет механизма указать, на какой аккаунт DEX зачислить депозит
3. Вывод на биржу без `DestinationTag` → средства зависают в лимбо биржи

## Исправление

1. Включить флаг `RequireDest` на escrow аккаунте — XRPL будет отклонять платежи без тега
2. Парсить `DestinationTag` в сканере депозитов
3. Создать маппинг `tag → user_id` при регистрации пользователя
4. Включать `DestinationTag` в транзакции вывода
