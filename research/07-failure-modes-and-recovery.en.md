# Failure Modes and Recovery

**Date:** 2026-03-31
**Status:** Design
**Context:** FROST 2-of-3 DKG, 3 SGX operators, XRPL mainnet settlement

---

## Base Model

```
Operator A (Hetzner)     Operator B (Azure)      Operator C (OVH)
┌──────────────┐         ┌──────────────┐         ┌──────────────┐
│ SGX Enclave  │         │ SGX Enclave  │         │ SGX Enclave  │
│ FROST Share 1│         │ FROST Share 2│         │ FROST Share 3│
│ Sealed State │         │ Sealed State │         │ Sealed State │
└──────────────┘         └──────────────┘         └──────────────┘
```

- **Escrow account** on XRPL: group public key = FROST 2-of-3
- **Signing threshold**: 2 out of 3 operators are sufficient to sign
- **State**: sealed inside each enclave (MRENCLAVE-bound)
- **Funds**: RLUSD on XRPL mainnet, controlled by group key

---

## 1. One Operator Offline

### Scenario
Operator C loses connectivity (server crash, network outage, maintenance).

### Impact
| Function | Status | Explanation |
|----------|--------|-------------|
| Trading | ✅ Works | Order book in orchestrator, not in enclave |
| Deposits | ✅ Works | XRPL monitoring by any live operator |
| Withdrawals | ✅ Works | FROST 2-of-3: A+B sign without C |
| Liquidations | ✅ Works | Any live operator executes |
| Funding | ✅ Works | Any live operator applies |
| State persistence | ✅ Works | Each instance saves its own state |

### Actions
- None. The system continues operating.
- Alert operator C to restore service.

### Recovery of C
1. C restarts the server
2. Enclave loads sealed state from disk (`ecall_perp_load_state`)
3. C replicates missed state updates from A or B
4. C returns to rotation

**User downtime: 0**

---

## 2. Two Operators Offline

### Scenario
Only Operator A is alive. B and C are unreachable.

### Impact
| Function | Status | Explanation |
|----------|--------|-------------|
| Trading | ✅ Works | Order book in orchestrator A |
| Deposits | ✅ Works | A monitors XRPL |
| **Withdrawals** | ❌ **Blocked** | FROST requires 2-of-3, A alone cannot sign |
| Liquidations | ⚠️ Partial | Internal liquidations work, but margin withdrawal does not |
| Funding | ✅ Works | |
| State persistence | ✅ Works | |

### Actions
- Trading continues, but withdrawals are suspended
- Funds are safe on XRPL escrow (A cannot withdraw alone)
- Wait for at least one of B/C to recover

### Criticality
- **Funds are not lost** — escrow on XRPL, key inside SGX
- **Withdrawal queue** — withdrawal requests accumulate, executed after recovery
- **Max downtime risk**: if the situation persists, users cannot withdraw funds

**Time without withdrawals: until one of B/C recovers**

---

## 3. All Three Operators Offline

### Scenario
All servers are simultaneously unreachable (disaster, coordinated attack, error).

### Impact
| Function | Status |
|----------|--------|
| Everything | ❌ Stopped |

### Fund Safety
- **RLUSD on XRPL escrow** — funds are on-chain, not on servers
- **Nobody can withdraw** — neither operators nor attacker (no 2-of-3 signature)
- **XRPL ledger** — immutable, funds are publicly visible

### Recovery
1. **Scenario A: servers come back** — each enclave loads sealed state, system restarts
2. **Scenario B: hardware destroyed** — Shamir backup recovery (see section 9)

---

## 4. One Malicious Operator

### Scenario
Operator B attempts to steal funds or manipulate trading.

### What B Can Do
| Action | Possible? | Why |
|--------|-----------|-----|
| Steal funds | ❌ No | Requires 2-of-3 FROST signatures, B has only 1 share |
| Sign a fake withdrawal | ❌ No | A and C will not sign an invalid transaction |
| Stop withdrawals | ⚠️ Partially | If B = one of two live operators, can refuse to sign. But A+C still = 2-of-3 |
| Manipulate price | ⚠️ Limited | If B = sequencer, can delay orders. Mitigation: sequencer rotation |
| See orders | ❌ No | Orders are encrypted for TEE (anti-MEV) |
| Extract key from SGX | ❌ No* | SGX hardware protection. *Theoretical side-channel attacks |

### Actions
- A and C detect anomaly (e.g., B refuses to sign valid withdrawals)
- A+C together = 2-of-3 → continue operating without B
- B is excluded from rotation

---

## 5. SGX Compromise (Side-Channel Attack)

### Scenario
Attacker extracts a FROST share from one enclave via a side-channel vulnerability (Spectre, Foreshadow, etc.).

### Impact
- Leak of 1 share out of 3 — **insufficient for signing**
- Attacker needs 2 shares for FROST 2-of-3
- Compromise of one SGX does not grant access to funds

### Actions
1. Intel releases a microcode update for the vulnerability
2. Update SGX microcode on the compromised server
3. Rebuild the enclave (new MRENCLAVE)
4. **Key rotation**: run new DKG → transfer funds to new escrow
5. Old shares are useless after key rotation

### Key Rotation Protocol
```
1. All 3 operators run new DKG → new group_pubkey → new XRPL address
2. Sign XRPL Payment: old escrow → new escrow (all RLUSD)
3. Update configuration
4. Old shares can be safely deleted
```

---

## 6. Hardware Failure (SGX CPU)

### Scenario
The CPU with SGX on server B has physically failed. Sealed data on disk cannot be decrypted (bound to MRENCLAVE + CPU key).

### Impact
- B share is lost
- A + C = 2-of-3 → **system continues operating**
- But there is no redundancy now — losing one more operator = loss of signing capability

### Actions
1. **Immediately**: A+C continue operating (withdrawals, trading — all OK)
2. **Urgently**: key rotation to new 2-of-3 DKG (A+C+D, where D = new server)
3. Transfer funds to new escrow
4. Old escrow is empty, can be discarded

### Recovery Time
- If D is already prepared (standby): ~5 minutes (DKG + transfer)
- If D needs to be deployed: ~1-2 hours (provision Azure VM + install SGX + DKG)

---

## 7. Migration: Changing Cloud Provider

### Question from 8Baller: "Can operators change cloud provider?"

### Answer: Yes, without loss of funds.

### Procedure
```
Current: A (Hetzner), B (Azure), C (OVH)
Target:  A (Hetzner), B (AWS), C (OVH)   ← B migrates Azure → AWS

1. Deploy new SGX instance D on AWS
2. Run DKG 2-of-3 between A, D, C → new group_pubkey
3. FROST signing (A+C): transfer RLUSD from old escrow to new
4. Update configuration: D replaces B
5. Shut down B (Azure)

Migration time: ~30 minutes
Time without withdrawals: ~5 minutes (only during the transfer moment)
```

### Key Point
- **No need** to export keys from SGX
- **No need** to trust the new provider — key is generated INSIDE the new enclave
- Funds are always on XRPL — not on servers
- Remote attestation on D confirms the code is the same (MRENCLAVE)

---

## 8. Scaling: "Books Get Too Big"

### Question from 8Baller: "Can they move to a more performant box?"

### Answer: Yes, step by step.

### Order Book
The order book lives in the **orchestrator (Rust)**, not in the enclave. No SGX limitations:
- Horizontal scaling of the orchestrator
- In-memory order book → can move to a more powerful server at any time
- No sealed state for the order book — stateless restart

### Enclave State
The enclave stores only balances + positions + margin (~25 KB for PoC, ~5 MB for production):
- On growth: partitioned sealing (seal in parts)
- On rebalancing: key rotation to a more powerful server

### Upgrade Procedure
```
1. Deploy new powerful SGX server D
2. Key rotation: DKG(A,B,C) → DKG(A,D,C)  (B is replaced by D)
3. Transfer funds
4. Orchestrator on D handles a larger order book
```

---

## 9. Catastrophic Recovery: All 3 Servers Destroyed

### Scenario
All three operators simultaneously lost access to sealed data (data center fire, coordinated server seizure).

### Backup: Shamir's Secret Sharing for Master Key

During initial setup (DKG):
1. Each enclave generates an **encrypted state export** encrypted with a master key
2. The master key is split via Shamir 3-of-5 among trusted custodians
3. Encrypted backups are stored outside the enclave (on USB, in a safe, in a bank)

### Recovery
```
1. 3 of 5 custodians gather and provide Shamir shares
2. Reconstruct the master key INSIDE a new attested enclave
3. Decrypt backup → restore state + FROST shares
4. New enclaves begin operating
5. Key rotation is recommended after recovery
```

### Alternative: XRPL as Source of Truth
Even without Shamir backup:
- All deposits are visible on the XRPL ledger
- It is possible to reconstruct who deposited how much
- Open positions are lost (off-chain state), but collateral is safe
- **Worst case**: pro-rata distribution of escrow balance based on XRPL deposit history

---

## 10. Risk Summary Table

| # | Scenario | Trading | Withdrawals | Funds | Recovery |
|---|----------|---------|-------------|-------|----------|
| 1 | 1 operator offline | ✅ | ✅ (2-of-3) | ✅ | Automatic |
| 2 | 2 operators offline | ✅ | ❌ Waiting | ✅ | Wait for 1 to recover |
| 3 | All 3 offline | ❌ | ❌ | ✅ (XRPL) | Shamir / restart |
| 4 | 1 malicious | ✅ | ✅ (2 honest) | ✅ | Exclude from rotation |
| 5 | SGX side-channel | ✅ | ✅ | ✅ (1 share not enough) | Key rotation |
| 6 | Hardware failure | ✅ | ✅ (2-of-3) | ✅ | Key rotation to new DKG |
| 7 | Provider migration | ✅ | ⚠️ 5 min | ✅ | DKG + transfer |
| 8 | Scaling | ✅ | ⚠️ 5 min | ✅ | Key rotation |
| 9 | Catastrophic (all 3) | ❌ | ❌ | ✅ (XRPL) | Shamir 3-of-5 |

---

## 11. Infrastructure Guarantees

### What Is Protected by Hardware (Intel SGX)
- Private keys (FROST shares) — never leave the enclave
- State in memory — isolated from OS and operator
- Sealed data — encrypted with CPU key + MRENCLAVE

### What Is Protected by Protocol (FROST 2-of-3)
- No single operator can sign alone
- Stealing funds requires compromising 2 out of 3 SGX enclaves
- Key rotation without service interruption

### What Is Protected by XRPL
- Funds are always on-chain (RLUSD on escrow)
- Deposit history — permanent, auditable
- Settlement — atomic, final within 3-5 seconds

### What Is NOT Protected
- Off-chain state (positions, PnL) — loss of all 3 servers = loss of state
- Order book — lives in orchestrator RAM, not persistent
- Funding rate history — computed on the fly

### Mitigations for Unprotected Items
- Periodic state sealed backups (every 5 minutes)
- Encrypted state exports (Shamir backup)
- XRPL deposit history as last-resort source of truth
