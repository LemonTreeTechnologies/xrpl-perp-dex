# Withdrawal authorisation v1 — the client contract

*Russian version: [withdrawal-authorization-v1.ru.md](withdrawal-authorization-v1.ru.md).*

A withdrawal request carries the account's own signature over exactly that withdrawal. Every
enclave that signs the escrow Payment rebuilds the message from the Payment it is about to sign
and verifies this signature itself; without a valid one, no enclave signs.

## Request fields (`POST /v1/withdraw`, in addition to `user_id`, `amount`, `destination`, `destination_tag`)

| field | type | meaning |
|---|---|---|
| `user_pubkey` | hex string, 33 bytes | the account's public key (`02`/`03`… secp256k1, `ED`… ed25519) — its XRPL address must be `user_id` |
| `user_sig` | hex string | signature over the digest below: DER (secp256k1) or 64 bytes (ed25519) |
| `auth_nonce` | **decimal string** of a u64 | exactly `last_withdraw_nonce + 1` (see Nonce) |
| `max_ledger` | integer (u32) | the last ledger the payment may land in |

A request without these four fields is refused with HTTP 400.

## The message (106 bytes, fixed layout, little-endian integers)

```
"PERP_WITHDRAW_AUTH_v1"          21 bytes, ASCII, no terminator
escrow AccountID                 20 bytes   (the cluster escrow address, decoded)
user AccountID                   20 bytes   (the account being debited = user_id, decoded)
amount_drops                      8 bytes   u64 LE — the withdrawal in drops (1 XRP = 1,000,000)
destination AccountID            20 bytes
has_tag                           1 byte    0 or 1
tag                               4 bytes   u32 LE (0 when has_tag = 0)
nonce                             8 bytes   u64 LE
max_ledger                        4 bytes   u32 LE
```

`digest = SHA-256(message)`. Sign `digest` with the account key. Either form verifies:
the digest directly, or `SHA-512Half(digest)` (the form XRPL wallets produce when asked to sign
a 32-byte hash) — the same dual form as the `X-XRPL-*` request headers.

`amount` in the request is the same withdrawal as a decimal XRP string with at most 6 decimals;
it must equal `amount_drops / 1,000,000` exactly.

## Nonce

Per account, strictly sequential: the first withdrawal is `1`, then `2`, and so on. Read the
current value as `last_withdraw_nonce` in the balance response (`GET /v1/account/balance`) and
send exactly that `+ 1`. A nonce that skips ahead or repeats is refused. Send one withdrawal at a
time per account (serialise them client-side). A withdrawal that was accepted and later expired
unpaid still used its nonce; a request refused before it was accepted did not.

## Deadline

`max_ledger` is a ledger index. The escrow Payment's `LastLedgerSequence` never exceeds it, so the
payment cannot land later. Use the current validated ledger plus a short window (for example
+60, about 3–4 minutes). An authorisation whose `max_ledger` is too close to the current ledger
is refused before anything happens; sign a new one.

## Test vectors

1. Message digest — escrow `0x11`×20, user `0x44`×20, amount 5,000,000 drops, destination
   `0x22`×20, tag 7, nonce 42, max_ledger 123456:
   `3686d0f64fe19d38036bb4f1ad9038a22300bdb2c8dcac1a99c1ec17b55fefec`.
2. Signed — XRPL test seed `snoPBrXtMeMyMHUVTgbuqAfg1SUTb` (address
   `rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh`), escrow `0x11`×20, amount 5,000,000, destination
   `0x22`×20, no tag, nonce 1, max_ledger 1000 → digest
   `a4c2fdc31fc03bfe1b7a28976d7e5b4eae1809765c3b865d89226e83d3967689`, public key
   `0330e7fc9d56bb25d6893ba3f317ae5bcf33b3291bd63db32654a313222f7fd020`, signature (RFC 6979,
   direct form)
   `304402205d53d458b0c38102e8d66ad1e43d7c0b4ce306b6deac566d62ed4cc4000a06b102205fec66580286af1ca7a4be16e27bc5d09a14701da8253426035a693a15d608de`.

The reference client is `perp-dex-orchestrator withdraw` (`orchestrator/src/cli_tools.rs`).
