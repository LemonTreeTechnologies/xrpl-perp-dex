# TEE vs Smart Contract: Why We Can't Be Robbed Like Drift

**Date:** 2026-04-02
**Context:** Drift Protocol (Solana perp DEX) lost $200M+ due to admin private key compromise

---

## What Happened to Drift (Detailed Analysis)

The attack was significantly more complex than "key theft". Drift **had a multisig** (Security Council, 5 signers) — but it didn't help.

**Timeline:**

1. **March 23:** The attacker created 4 wallets with **durable nonces** (a Solana mechanism for deferred transaction execution). Two wallets were linked to Security Council members.
2. **March 30:** Drift performed a Security Council rotation. The attacker adapted — created a new wallet matching the updated multisig parameters.
3. **April 1:** Drift performed a legitimate test withdrawal from the insurance fund. **Within ~1 minute** the attacker activated two pre-authorized transactions, gaining administrative rights.
4. **Withdrawal:** $155.6M JPL, $60.4M USDC, $11.3M CBBTC, $4.7M WETH, $4.5M DSOL, $4.4M WBTC, $4.1M FARTCOIN and others. **Total: ~$280M.**

**How the attack bypassed multisig:**

The attacker used **social engineering** — convinced at least 2 of the 5 Security Council signers to approve transactions. Durable nonces allowed preparing transactions in advance and executing them automatically.

**Root causes:**
- Multisig signers are **people**, susceptible to social engineering
- Durable nonces allow **pre-signing** transactions without immediate execution
- No verification that the signed transaction is **reasonable** (margin check, amount limit)
- Monitoring failed to detect the preparation a week before the attack

---

## Why This Is Impossible in Our Architecture

### 0. Signers Are Hardware, Not People

```
Drift multisig:                  Our architecture:
┌──────────┐                     ┌──────────────────┐
│ Person 1 │ ← social           │ SGX Enclave A    │
│ Person 2 │    engineering      │ SGX Enclave B    │
│ Person 3 │    possible         │ SGX Enclave C    │
│ Person 4 │                     │                  │
│ Person 5 │                     │ (hardware, not   │
└──────────┘                     │  susceptible to  │
     │                           │  persuasion)     │
  2 of 5 convinced →            └──────────────────┘
  full access                          │
                                 Enclave will ONLY sign
                                 if margin check passes
                                 and tx is valid per code
```

**Drift's multisig was defeated by social engineering.** The attacker convinced 2 people to sign. In our architecture the signers are SGX enclaves. You cannot "convince" a processor to sign an invalid transaction.

### 1. The Key Does Not Exist Outside SGX

```
Drift:                           Our architecture:
┌──────────┐                     ┌──────────────────┐
│ Admin key│ ← stored            │ SGX Enclave      │
│ in file/ │    somewhere        │ ┌──────────────┐ │
│ in HSM/  │    accessible       │ │ ECDSA Key A  │ │
│ in memory│    to operator      │ │ (sealed,     │ │
└──────────┘                     │ │  never leaves│ │
     │                           │ │  enclave)    │ │
     │ stolen →                  │ └──────────────┘ │
     │ full access               └──────────────────┘
     ▼                                    │
  $280M withdrawal               Operator CANNOT
                                  extract the key
```

In SGX the private key is **generated inside the enclave** and **never leaves** it. The operator launches the enclave but physically cannot read the enclave memory contents — this is a guarantee at the Intel CPU level.

### 2. Multisig 2-of-3 — No Single Key

```
Drift: 1 admin key → full control

Our architecture:
  Operator A (Azure): ECDSA Key A — inside SGX
  Operator B (Azure): ECDSA Key B — inside SGX
  Operator C (Azure): ECDSA Key C — inside SGX

  XRPL Escrow: SignerListSet [A, B, C], quorum=2
  Master key: DISABLED

  Any withdrawal requires 2 of 3 signatures.
  Each key is inside its own SGX enclave.
  Operators are on different servers, different providers.
```

Even if the attacker **fully compromises** one server (root access, physical access) — they only gain access to one enclave. To withdraw funds they need to compromise **two enclaves on two different servers**.

### 3. Enclave Code Defines the Rules — the Operator Cannot Bypass Them

```
Drift: admin key can do anything
       (transfer all funds to own address)

Our architecture:
  Enclave code (attested, open-source):
    - Withdrawal only after margin check
    - Signing only for a specific user + amount
    - Rate limit on withdrawals
    - Spending guardrails (signature count limit)

  The operator CANNOT force the enclave to sign
  an arbitrary transaction — the enclave code forbids it.
```

### 4. DCAP Attestation — Code Is Verified

```
Drift: users trust that the smart contract does
       what is written (but admin key bypasses everything)

Our architecture:
  1. Enclave publishes MRENCLAVE (code hash)
  2. Intel signs SGX Quote (DCAP)
  3. Anyone can verify:
     - Enclave code = published open-source code
     - Running on genuine Intel SGX
     - Operator has not modified the code

  If the operator tries to run a modified enclave
  — MRENCLAVE changes → attestation fails
  → users see the substitution
```

### 5. XRPL Settlement — Funds on L1, Not in a Contract

```
Drift: all funds inside a smart contract on Solana
       admin key = full access to contract = full access to funds

Our architecture:
  Funds: RLUSD on XRPL escrow account
  Control: SignerListSet 2-of-3 (not a smart contract)

  XRPL — fixed protocol, no upgradeable contracts.
  SignerListSet — native XRPL feature, not our code.
  No admin key, no upgrade function, no proxy pattern.
```

---

## Attack Comparison Table

| Attack Vector | Drift ($280M, actual attack) | Our Architecture (TEE + Multisig) |
|---|---|---|
| **Social engineering multisig** | ✅ 2 of 5 people convinced to sign | ❌ Signers = SGX hardware, not people |
| **Durable nonces (pre-signed tx)** | ✅ Transactions prepared a week in advance | ❌ Enclave signs only at the moment of request, each time with margin check |
| **Rehearsal (preparation)** | ✅ Test wallets, adaptation to rotation | ❌ DCAP attestation — code is immutable, no "adaptation" |
| **Timing attack** | ✅ Attack during a legitimate operation | ❌ Enclave does not distinguish "legitimate" from "attack" — checks the same way |
| **Admin key compromise** | ✅ Full control via Council | ❌ No admin key. Master key disabled on XRPL |
| **Insider threat** | ✅ Council members = potential insiders | ❌ Operators have no access to keys (SGX hardware) |
| **Supply chain attack** | ✅ Swap out contract upgrade | ❌ MRENCLAVE changes → DCAP attestation fail |
| **Rug pull** | ✅ Council withdraws everything | ❌ Enclave will sign withdrawal ONLY after margin check |

---

## What If SGX Is Compromised?

Theoretical side-channel attacks on SGX exist (Spectre, Foreshadow). However:

1. **One compromised SGX = one key** out of three. Withdrawal requires 2.
2. **Key rotation:** upon discovering a vulnerability — new keys, new SignerListSet, transfer funds.
3. **Intel microcode updates:** fix known side-channels.
4. **Time window:** the attacker needs to compromise 2 SGX instances simultaneously, before key rotation.

Compare: in Drift, once a key is stolen — it's stolen **forever**. In our architecture — even if one SGX is compromised, we have time for key rotation.

---

## What If an Operator Is Malicious?

| Operator Action | Drift | Our Architecture |
|---|---|---|
| Withdraw all funds | ✅ One tx (admin key) | ❌ Requires 2-of-3 + enclave will only sign valid tx |
| Swap out code | ✅ Upgrade contract | ❌ MRENCLAVE changes → DCAP attestation fail |
| Delay withdrawals | ✅ Pause contract | ⚠️ Can delay if they are the sequencer, but 2 other operators continue |
| Front-run users | ✅ MEV (sees all tx) | ❌ Orders encrypted for enclave |
| Forge prices | ✅ Modify oracle | ⚠️ Median from 3 operators, one cannot influence |

---

## Practical Recommendations

### For users of our DEX:

1. **Verify attestation** before depositing: `POST /v1/attestation/quote` → verify MRENCLAVE
2. **Check SignerListSet** on XRPL: ensure escrow has quorum=2, master disabled
3. **Make sure operators are on different providers** (Azure, OVH, Hetzner)
4. **Monitor key rotation** — if MRENCLAVE changed, check why

### For operators:

1. **Never store keys outside SGX** — all keys are generated inside the enclave
2. **Disable master key** on escrow account — always
3. **Monitoring:** alerting on unusual withdrawals, spending limit guardrails
4. **Regular key rotation** — don't wait for an incident
5. **DCAP attestation** — publish MRENCLAVE, let users verify

---

## Summary

| | Drift (actual attack, $280M) | Our Architecture |
|---|---|---|
| Security model | Multisig 2-of-5 (people) | TEE Multisig 2-of-3 (SGX hardware) |
| Attack vector | Social engineering of 2 people | Impossible — hardware is not susceptible to social engineering |
| Preparation | Durable nonces a week in advance | Enclave does not store pre-signed tx |
| Minimum for theft | Convince 2 people | Compromise 2 SGX on different servers |
| Time to react | ~1 minute (between legit op and drain) | Available (key rotation, 2-of-3 continues operating) |
| Code verification | Audit report (static) | DCAP attestation (runtime, Intel-signed) |
| Funds | In smart contract (Council control) | On XRPL L1 (SignerListSet, master disabled) |
| Recovery | $280M gone, protocol on the brink of death | Key rotation + new escrow + fund transfer |

**The $280M Drift hack is impossible in a TEE + Multisig architecture.**
Not because we are smarter — but because **the signers are hardware, not people**. Social engineering doesn't work on processors.
