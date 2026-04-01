# Failure Modes and Recovery

**Date:** 2026-03-29
**Status:** Design
**Context:** XRPL native multisig 2-of-3 (SignerListSet), 3 SGX operators, full infrastructure stack

---

## 1. Base Model: Full Stack per Operator

Each operator runs a full stack of 4 components + external dependencies:

```
                          Internet
                             │
                         DNS / LB
                      (CloudFlare, Route53)
                             │
                             ▼
┌─────────────────────────────────────────────────────────────┐
│                     Operator N                               │
│                                                              │
│   User ──► HAProxy :443 (public frontend)                   │
│                  │                                            │
│                  ▼                                            │
│            ┌───────────┐    ┌───────────┐    ┌───────────┐  │
│            │ Enclave 1 │    │ Enclave 2 │    │ Enclave 3 │  │
│            │ :9088     │    │ :9089     │    │ :9090     │  │
│            │ ECDSA Key │    │ ECDSA Key │    │ ECDSA Key │  │
│            │ TCSNum=1  │    │ TCSNum=1  │    │ TCSNum=1  │  │
│            └───────────┘    └───────────┘    └───────────┘  │
│                  ▲                                            │
│                  │                                            │
│            HAProxy :9443 (internal frontend, 127.0.0.1)     │
│                  ▲                                            │
│                  │                                            │
│            ┌─────────────────────────┐                       │
│            │    Orchestrator (Rust)  │                       │
│            │  ┌─ Order Book         │                       │
│            │  ├─ Price Feed         │──► Binance API        │
│            │  ├─ Deposit Monitor    │──► XRPL Mainnet       │
│            │  ├─ Liquidation Engine │                       │
│            │  ├─ Funding Rate       │                       │
│            │  └─ Sequencer/Validator│                       │
│            └────────────┬───────────┘                       │
│                         │                                    │
│                    P2P (libp2p gossipsub)                    │
│                         │                                    │
└─────────────────────────┼────────────────────────────────────┘
                          │
            ┌─────────────┼──────────────┐
            ▼             ▼              ▼
      Operator A    Operator B     Operator C
     (Sequencer)   (Validator)    (Validator)
            │             │              │
            └─────────────┼──────────────┘
                          ▼
                    XRPL Mainnet
                  (escrow account)
         SignerListSet: [rA, rB, rC], quorum=2
```

### Components per Operator

| # | Component | Port | Description |
|---|-----------|------|-------------|
| 1 | **HAProxy** (public) | :443 | TLS termination, rate limiting, blocking internal endpoints |
| 2 | **HAProxy** (internal) | :9443 (127.0.0.1) | Full access for orchestrator, maxconn 1 per enclave |
| 3 | **SGX Enclave** x3 | :9088-9090 | perp-dex-server: margin engine, ECDSA keys, sealed state |
| 4 | **Orchestrator** | none (outbound only) | Rust binary: order book, price feed, deposit monitor, sequencer/validator |

### External Dependencies

| Dependency | Purpose | Criticality |
|------------|---------|-------------|
| **XRPL Mainnet** | Settlement, deposit monitor, escrow | Critical for deposits/withdrawals |
| **Binance API** | Price feed (mark price) | Critical for liquidations and funding |
| **DNS/LB** | User routing to operators | Critical for availability |
| **P2P gossipsub** | State replication, price consensus, heartbeat | Critical for multi-operator operation |

---

## 2. Component Failure Matrix

What happens when each component fails. Assumption: failure on **one** operator, the other two are alive.

| Component | Trading | Deposits | Withdrawals | Prices | Liquidations | Recovery | Time |
|-----------|---------|----------|-------------|--------|--------------|----------|------|
| **HAProxy down** (one operator) | ✅ via others | ✅ | ✅ (2-of-3) | ✅ | ✅ | Restart HAProxy, DNS failover | ~30 sec |
| **Orchestrator crash** (on sequencer) | ⚠️ pause until failover | ⚠️ pause | ⚠️ pause | ⚠️ pause | ⚠️ pause | Heartbeat timeout 15s, validator becomes sequencer | ~15-30 sec |
| **Orchestrator crash** (on validator) | ✅ | ✅ | ✅ (2-of-3) | ✅ | ✅ | Restart orchestrator, resync state | ~10 sec |
| **Enclave crash** (orchestrator alive) | ⚠️ degraded | ✅ | ✅ (2-of-3 at operator level) | ✅ | ✅ | ecall_perp_load_state, orchestrator reconnect | ~5-15 sec |
| **Full operator down** | ✅ on remaining | ✅ | ✅ (2-of-3) | ✅ (median of 2) | ✅ | Sequencer failover + DNS redirect | ~15-30 sec |
| **P2P disconnect** (network partition) | ⚠️ **see Split-Brain** | ✅ each monitors | ❌ **blocked** until reconnect | ⚠️ no consensus | ⚠️ state divergence | Reconnect + state reconciliation | depends on partition |
| **Binance API unavailable** | ✅ (old price) | ✅ | ✅ | ❌ **frozen** | ⚠️ stale price risk | Switch to backup CEX / wait | ~1-60 min |
| **XRPL node unavailable** | ✅ | ❌ **not detected** | ❌ **not submitted** | ✅ | ✅ (internal) | Switch to backup XRPL node | ~10-30 sec |
| **DNS/LB failure** | ❌ for users | ❌ for users | ❌ for users | ✅ (internally) | ✅ (internally) | DNS failover / direct IP | ~1-5 min |

### Detailed Description of Each Failure

---

### 2.1. HAProxy down (on one operator)

**Cause:** HAProxy process crashed, OOM, failed configuration reload.

**Consequences:**
- Users of this operator lose API access
- This operator's orchestrator cannot reach enclave (internal frontend is also down)
- Enclave instances continue running but are unreachable

**Cascade:**
- If this operator = sequencer -> orchestrator cannot update state -> heartbeat timeout -> failover
- If validator -> loses ability to sign, but 2 other operators are sufficient

**Recovery:**
```
1. systemctl restart haproxy
2. HAProxy starts health check to enclave instances (/v1/pool/status)
3. Enclave instances are already running — instant recovery
4. DNS health check marks operator as alive
```

**Mitigation:** systemd watchdog for HAProxy, DNS health checks with TTL 30s.

---

### 2.2. Orchestrator crash (on sequencer)

**Cause:** Panic in Rust, OOM, unhandled error while processing XRPL/Binance response.

**Consequences:**
- Order book in RAM is lost (stateless, recoverable from open orders)
- Price feed stopped
- Deposit monitor stopped
- Heartbeat in P2P disappears

**Cascade:**
- After 15 seconds (3 missed heartbeats) validators detect the failure
- Validator B (next by priority) becomes sequencer
- Users are redirected to B via DNS

**Sequencer recovery:**
```
1. Validators detect missing heartbeat (3 x 5s = 15s)
2. Validator B assumes sequencer role (priority-based election)
3. B starts accepting orders, monitoring XRPL, publishing prices
4. Former sequencer A after restart:
   a. Orchestrator starts
   b. Connects to P2P mesh
   c. Requests current state from new sequencer
   d. Joins as validator
```

**Critical:** Order book in RAM is not persisted. Open orders (limit orders) must be recreated by users or recovered from state replication.

---

### 2.3. Orchestrator crash (on validator)

**Cause:** Same as for sequencer.

**Consequences:**
- One validator temporarily unavailable for signing
- State replication to it stops
- Does not affect trading (sequencer is alive)

**Recovery:**
```
1. Restart orchestrator
2. Connect to P2P, request missed state batches
3. State synchronization → ready for signing
```

**Downtime for users: 0** (sequencer continues operating, 2-of-3 signing is provided by remaining operators).

---

### 2.4. Enclave crash (orchestrator alive)

**Cause:** Segfault in enclave code, SGX exception, insufficient EPC memory.

**Consequences:**
- HAProxy health check (`/v1/pool/status`) marks instance as down
- HAProxy stops sending requests to the crashed instance
- Remaining 2 instances on this operator continue processing requests
- If all 3 local instances crash — operator degrades to "HAProxy down" state

**Recovery:**
```
1. HAProxy health check detects instance failure (~5s interval)
2. Restart enclave process (systemd restart)
3. Enclave loads sealed state: ecall_perp_load_state
4. HAProxy health check sees instance alive → returns to rotation
```

**Note:** Sealed state is bound to MRENCLAVE + CPU key. If MRENCLAVE changed (code update) — sealed data cannot be decrypted. State export/import via orchestrator is required.

---

### 2.5. Full operator down

Described in detail in section 3 (Operator-Level Scenarios).

---

### 2.6. P2P disconnect (network partition)

Described in detail in section 4 (Split-Brain / Network Partition).

---

### 2.7. Binance API unavailable

**Cause:** Binance maintenance, rate limit, geo-block, DDoS on Binance.

**Consequences:**
- Price feed freezes at last known price
- Liquidations operate on stale price -> **danger**: price may have moved significantly
- Funding rate is not updated
- Trading formally continues but with outdated price

**Protective mechanisms:**
- **Stale price timeout**: if price has not been updated for > 60 seconds, orchestrator switches system to **price freeze mode**:
  - New positions are prohibited
  - Closing positions is allowed
  - Liquidations are paused (to avoid liquidating at stale price)
  - Withdrawals are allowed
- **Backup price source**: switch to another CEX (Kraken, Bybit) or XRPL DEX oracle

**Recovery:**
```
1. Binance API recovers
2. Orchestrator receives fresh price
3. Stale price timeout resets
4. System returns to normal mode
5. Liquidations are checked against new price (possible liquidation spike)
```

**Risk:** If Binance is unavailable for an extended period and the market moves sharply — positions may become underwater. Mitigation: insurance fund covers bad debt.

---

### 2.8. XRPL node unavailable

**Cause:** XRPL node maintenance, network issue, XRPL amendment freeze.

**Consequences:**
- **Deposits not detected**: orchestrator cannot see new Payment transactions to escrow
- **Withdrawals not submitted**: multisig transactions cannot be submitted
- Trading continues (off-chain)
- Liquidations work (internal settlement)

**Recovery:**
```
1. Switch to backup XRPL node (list: s1.ripple.com, s2.ripple.com, own node)
2. On recovery — scan missed ledgers for deposits
3. Queued withdrawals are submitted
```

**Critical:** Deposit monitor must remember the last processed ledger index and on reconnect scan from that point, not from the current one. Otherwise missed deposits will be lost.

---

### 2.9. DNS/LB failure

**Cause:** DNS registrar downtime, CloudFlare incident, incorrect DNS configuration.

**Consequences:**
- Users cannot resolve operator IPs
- All user-facing operations are unavailable
- Internally the system operates normally (P2P, orchestrator, enclave)
- Liquidations, funding, deposit monitor — all continue

**Recovery:**
```
1. DNS provider recovers
2. Alternative: users connect via direct IP (published in documentation)
3. Backup DNS (multi-provider: CloudFlare + Route53)
```

**Mitigation:** Multi-provider DNS, low TTL (30-60 sec), publishing operator IP addresses for emergency access.

---

## 3. Operator-Level Scenarios

### 3.1. One operator fully offline

**Scenario:** Operator C loses connectivity (server crash, network down, maintenance).

**Impact:**
| Function | Status | Explanation |
|----------|--------|-------------|
| Trading | ✅ Works | Sequencer (A or B) is alive, order book in its orchestrator |
| Deposits | ✅ Works | XRPL monitoring by any alive operator |
| Withdrawals | ✅ Works | Multisig 2-of-3: A+B sign without C |
| Liquidations | ✅ Works | Any alive operator executes |
| Funding | ✅ Works | Any alive operator applies |
| Prices | ✅ Works | Median from 2 operators (less resistant to manipulation) |
| State replication | ⚠️ Degraded | Only between A and B, C falls behind |

**Actions:**
- System continues operating without intervention
- Alert to operator C for recovery

**Recovery of C:**
```
1. C restarts server
2. HAProxy starts, health check to enclave instances
3. Enclave loads sealed state: ecall_perp_load_state
4. Orchestrator connects to P2P mesh
5. Requests missed state batches from A or B
6. After synchronization — C returns to rotation (validator)
```

**Downtime for users: 0**

---

### 3.2. Two operators offline

**Scenario:** Only Operator A is alive. B and C are unavailable.

**Impact:**
| Function | Status | Explanation |
|----------|--------|-------------|
| Trading | ✅ Works | Order book in orchestrator A |
| Deposits | ✅ Works | A monitors XRPL |
| **Withdrawals** | ❌ **Blocked** | Multisig requires 2-of-3, A alone cannot sign |
| Liquidations | ⚠️ Partial | Internal liquidations work, but margin withdrawal does not |
| Funding | ✅ Works | |
| Prices | ⚠️ Single point | Only A's price, no median — vulnerable to manipulation |

**Actions:**
- Trading continues, withdrawals are suspended
- Withdrawal queue: requests accumulate, executed after recovery
- Funds are safe on XRPL escrow (A cannot withdraw alone)

**Criticality:**
- **Funds are not lost** — escrow on XRPL, key inside SGX
- **Max downtime risk**: users cannot withdraw funds until at least one of B/C recovers
- **Price risk**: single price source, manipulation could cause incorrect liquidations

**Time without withdrawals: until recovery of one of B/C**

---

### 3.3. All three operators offline

**Scenario:** All servers simultaneously unavailable (catastrophe, coordinated attack, error).

**Impact:**
| Function | Status |
|----------|--------|
| Everything | ❌ Stopped |

**Fund safety:**
- **RLUSD on XRPL escrow** — funds are on-chain, not on servers
- **Nobody can withdraw** — neither operators nor an attacker (no 2-of-3 multisig signature)
- **XRPL ledger** — immutable, funds are publicly visible

**Recovery:**
1. **Servers come back** — each enclave loads sealed state, system restarts
2. **Hardware destroyed** — Shamir backup recovery (see section 3.9)

---

### 3.4. One malicious operator

**Scenario:** Operator B attempts to steal funds or manipulate trading.

| Action | Possible? | Why |
|--------|-----------|-----|
| Steal funds | ❌ No | Requires 2-of-3 ECDSA signatures (multisig), B has only 1 key |
| Sign fake withdrawal | ❌ No | A and C will not sign an invalid transaction (enclave verifies margin) |
| Block withdrawals | ⚠️ Partially | If B = one of two alive operators, can refuse to sign. But A+C = 2-of-3 |
| Manipulate price | ⚠️ Limited | Median from 3 operators provides protection. If B = sequencer, can delay orders |
| See orders | ❌ No | Orders are encrypted for TEE (anti-MEV) |
| Extract key from SGX | ❌ No* | SGX hardware protection. *Theoretical side-channel attacks |
| Tamper with enclave code | ❌ No | Remote attestation: users and other operators verify MRENCLAVE |
| Send fake state batch | ❌ No | Validators deterministically replay operations and verify state hash |

**Actions:**
- A and C detect anomaly (B refuses to sign, B sends invalid state batches)
- A+C = 2-of-3 -> continue operating without B
- B is excluded from rotation
- If necessary: key rotation, replace B with new operator D

---

### 3.5. SGX compromise (side-channel attack)

**Scenario:** An attacker extracts the ECDSA key from one enclave via a side-channel vulnerability (Spectre, Foreshadow, SGAxe, etc.).

**Impact:**
- Leak of 1 key out of 3 — **insufficient for signing** (requires 2-of-3 multisig)
- Attacker needs 2 keys for 2-of-3 multisig
- Compromising one SGX does not grant access to funds

**Actions:**
1. Intel releases microcode update for the vulnerability
2. Update SGX microcode on the compromised server
3. Rebuild enclave (new MRENCLAVE)
4. **Key rotation**: each instance generates a new ECDSA keypair -> update SignerListSet -> transfer funds to new escrow
5. Old keys are useless after key rotation

**Key Rotation Protocol:**
```
1. All 3 instances generate new ECDSA keypair → new XRPL addresses (rA', rB', rC')
2. Create new escrow account with SignerListSet: [rA', rB', rC'], quorum=2
3. Multisig signature (2-of-3 with old keys): transfer RLUSD from old escrow to new
4. Update configuration
5. Old keys can be safely deleted
```

---

### 3.6. Hardware failure (SGX CPU)

**Scenario:** The SGX CPU on server B has physically failed. Sealed data on disk cannot be decrypted (bound to MRENCLAVE + CPU key).

**Impact:**
- ECDSA key B is lost
- A + C = 2-of-3 multisig -> **system continues operating**
- No margin: loss of one more operator = loss of signing capability

**Actions:**
1. **Immediately**: A+C continue operating (withdrawals, trading — all OK)
2. **Urgently**: deploy operator D, generate new ECDSA key -> update SignerListSet to [rA, rD, rC]
3. Transfer funds to new escrow (or update SignerListSet on existing one)

**Recovery time:**
- Standby operator D is prepared: ~5 minutes (keygen + SignerListSet update)
- D needs to be deployed from scratch: ~1-2 hours (provision VM + install SGX + keygen + SignerListSet)

---

### 3.7. Migration: changing cloud provider

**Procedure:**
```
Current: A (Hetzner), B (Azure), C (OVH)
Target:  A (Hetzner), B (AWS), C (OVH)   ← B migrates Azure → AWS

1. Deploy new SGX instance D on AWS
2. D generates ECDSA keypair inside enclave → address rD
3. Update SignerListSet: [rA, rD, rC], quorum=2 (multisig signature A+C)
4. D connects to P2P mesh, synchronizes state
5. Update DNS: B → D
6. Shut down B (Azure)

Migration time: ~30 minutes
Time without withdrawals: ~5 minutes (moment of SignerListSet update)
```

**Key point:**
- No need to export keys from SGX
- No need to trust the new provider — key is generated INSIDE the new enclave
- Remote attestation on D confirms identical MRENCLAVE

---

### 3.8. Scaling: adding operators

**Order book:** lives in orchestrator (Rust), not in enclave. No SGX limitations:
- Horizontal scaling of orchestrator
- In-memory order book -> can move to a more powerful server
- Stateless restart (order book recovers from open orders)

**Enclave state:** only balances + positions + margin (~25 KB for PoC, ~5 MB for production)

**Increasing the number of operators:**
```
Current: 2-of-3 [A, B, C]
Target:  3-of-5 [A, B, C, D, E]

1. D and E generate ECDSA keypair in their enclaves
2. Update SignerListSet: [rA, rB, rC, rD, rE], quorum=3
3. D and E connect to P2P mesh
4. State synchronization
5. XRPL SignerListSet supports up to 32 signers — no limitations
```

---

### 3.9. Catastrophic recovery: all 3 servers destroyed

**Scenario:** All three operators simultaneously lost access to sealed data.

**Backup: Shamir's Secret Sharing for master key**

During initial setup:
1. Each enclave generates an encrypted state export, encrypted with a master key
2. Master key is split via Shamir 3-of-5 among trusted custodians
3. Encrypted backups are stored outside the enclave (USB, safe, bank)

**Recovery:**
```
1. 3 of 5 custodians provide Shamir shares
2. Reconstruct master key INSIDE a new attested enclave
3. Decrypt backup → restore state + ECDSA keys
4. New enclaves begin operation
5. Key rotation is recommended after recovery
```

**Alternative: XRPL as source of truth**

Even without Shamir backup:
- All deposits are visible on the XRPL ledger
- It is possible to reconstruct who deposited how much
- Open positions are lost (off-chain state), but collateral is safe
- **Worst case**: pro-rata distribution of escrow balance based on XRPL deposit history

---

## 4. Split-Brain / Network Partition

### 4.1. Problem

P2P gossipsub between operators can be disrupted: firewall, provider, BGP incident. The result is two (or more) isolated clusters, each considering itself the primary.

### 4.2. Partition scenarios

```
Scenario 1: [A] | [B, C]     ← A is isolated
Scenario 2: [A, B] | [C]     ← C is isolated
Scenario 3: [A] | [B] | [C]  ← full fragmentation
```

### 4.3. Two sequencers simultaneously (split-brain)

**How it occurs:**
1. A = sequencer, B and C = validators
2. Network splits: [A] | [B, C]
3. B and C do not receive heartbeat from A (15 seconds)
4. B becomes sequencer by priority
5. Now: A considers itself sequencer, B also considers itself sequencer

**Problem:** Two sequencers build different state (different order sequencing, different liquidations).

### 4.4. Split-brain resolution

**Principle: Majority wins.**

| Partition | Who continues | Who stops | Why |
|-----------|--------------|-----------|-----|
| [A] vs [B,C] | B,C (2 operators) | A (1 operator) | Majority at [B,C] |
| [A,B] vs [C] | A,B (2 operators) | C (1 operator) | Majority at [A,B] |
| [A] vs [B] vs [C] | Nobody | All | No majority |

**Mechanism:**

1. **Quorum check during sequencer election:** A validator becomes sequencer only if it sees a majority of operators (>= 2 out of 3). If B and C see each other but not A -> B becomes sequencer (sees majority).
2. **Isolated operator self-demotion:** If A stops seeing at least 1 other operator and is not part of the majority -> A switches itself to **read-only mode**:
   - Accepts read requests (balances, positions)
   - Rejects write requests (opening/closing positions, withdrawals)
   - Logs: "isolated, waiting for reconnect"
3. **Reconnect reconciliation:** Upon connectivity restoration:
   ```
   1. Isolated operator (A) requests current state hash from majority
   2. If state diverges — A discards its state, accepts majority state
   3. A returns as validator
   ```

### 4.5. Withdrawals during partition

- **[A] vs [B,C]:** B+C = 2-of-3 -> withdrawals work. A cannot sign (no quorum for signing).
- **[A,B] vs [C]:** A+B = 2-of-3 -> withdrawals work. C cannot sign.
- **[A] vs [B] vs [C]:** Nobody can sign (requires 2-of-3). Withdrawals are blocked.

### 4.6. Protection against double-spending during partition

**Risk:** If split-brain is not detected instantly, both sequencers may have approved conflicting withdrawals.

**Protection:** XRPL Sequence number on the escrow account. Each transaction increments the Sequence. If both clusters attempt to send a transaction:
- The first one is included in the ledger
- The second one is rejected with `tefPAST_SEQ` or `tefMAX_LEDGER`

**Additional protection:** The orchestrator checks the current Sequence before sending a withdrawal. In case of conflict — one of the two withdrawals is delayed until reconciliation.

---

## 5. Cascading Failures

### 5.1. Scenario: Cascade via overload

```
Timeline:
T+0:    Orchestrator A (sequencer) crash
T+5s:   HAProxy A health check fails on /v1/pool/status
        (orchestrator does not restart enclave, but enclave is still alive)
T+15s:  Validators B,C do not receive heartbeat → B = new sequencer
T+16s:  DNS health check sees A down → all users redirected to B
T+17s:  B receives 3x normal traffic (its own + former A's)
T+20s:  HAProxy B: queue overflow (maxconn 1 x 3 instances = 3 concurrent)
        Latency grows: 5s → 15s → timeout
T+30s:  Users see timeouts
T+60s:  Some users go to C → C also becomes loaded
```

### 5.2. Cascading failure mitigations

**Rate limiting on HAProxy:**
```haproxy
frontend perp-public
    # No more than 50 req/s per IP
    stick-table type ip size 100k expire 30s store http_req_rate(10s)
    http-request deny deny_status 429 if { sc_http_req_rate(0) gt 500 }
```

**Connection queue management:**
```haproxy
backend enclave_instances
    timeout queue 5s          # Do not wait longer than 5 seconds in queue
    option redispatch         # If instance down — redirect to another
    retries 1                 # Maximum 1 retry
```

**Graceful degradation:**
- Under overload — HAProxy returns 503 with Retry-After header
- Frontend shows "system overloaded, please try again in 30 seconds"
- Critical operations (withdrawals, liquidations) have priority in queue

**Auto-scaling enclave instances:**
- Under load — launch additional enclave instances (9091, 9092...)
- HAProxy dynamically adds new backends
- Limitation: EPC memory per CPU (typically 128-256 MB)

### 5.3. Scenario: Cascade via Binance API

```
Timeline:
T+0:    Binance API rate limit (429) for operator A
T+5s:   A switches to backup source (Kraken)
T+10s:  Kraken also rate limited (all operators switched)
T+15s:  All 3 operators have stale price
T+60s:  Price freeze mode on all operators
T+??:   Market moves, positions become underwater
```

**Mitigation:**
- Each operator uses its own API key for Binance
- Staggered requests (A queries at :00, B at :02, C at :04 seconds)
- Multiple backup sources: Kraken, Bybit, XRPL DEX, CoinGecko
- Circuit breaker: if > 2 sources are unavailable, automatic price freeze

### 5.4. Scenario: Cascade via XRPL

```
Timeline:
T+0:    XRPL node of operator A lost connectivity
T+1s:   Deposits not detected on A
T+5s:   A switches to backup XRPL node
T+6s:   Backup node also unavailable (XRPL amendment freeze, all nodes updating)
T+10s:  A cannot send withdrawal tx
T+15s:  B,C also lose XRPL connectivity
T+??:   All withdrawals and deposits blocked globally
```

**Mitigation:**
- Multiple XRPL nodes (s1.ripple.com, s2.ripple.com, own node, xrplcluster.com)
- Deposit monitor buffers: on reconnect scans missed ledgers
- Withdrawal queue: requests accumulate, submitted upon recovery
- XRPL amendment freeze — rare, typically < 15 minutes

---

## 6. Infrastructure Guarantees

### What is protected by hardware (Intel SGX)
- Private ECDSA keys — never leave the enclave
- State in memory — isolated from OS and operator
- Sealed data — encrypted with CPU key + MRENCLAVE
- Remote attestation — users verify that code is unchanged

### What is protected by HAProxy
- Users do not have access to internal endpoints (deposit, price, liquidate, state)
- Request serialization to single-threaded enclave instances (maxconn 1)
- Health check — automatic removal/return of enclave instances
- Rate limiting — protection against DDoS and overload

### What is protected by Orchestrator
- Deposit monitor — detects all incoming payments to escrow
- Price feed — mark price update every 5 seconds
- Liquidation engine — margin check every 10 seconds
- State save — periodic saving (every 5 minutes)
- Sequencer/validator logic — ordering, state replication, heartbeat

### What is protected by protocol (XRPL SignerListSet 2-of-3)
- No single operator can sign alone (quorum=2)
- Stealing funds requires compromising 2 out of 3 SGX instances
- Key rotation via SignerListSet update without service interruption

### What is protected by P2P (gossipsub)
- State replication — all operators have consistent state
- Price consensus — median from multiple sources
- Sequencer election — automatic failover
- Heartbeat — failure detection within 15 seconds

### What is protected by XRPL
- Funds are always on-chain (RLUSD on escrow)
- Deposit history — permanent, auditable
- Settlement — atomic, final within 3-5 seconds
- Sequence number — protection against double-spending during split-brain

### What is protected by DNS/LB
- User routing to the nearest/alive operator
- Health check — automatic failover when an operator goes down
- DDoS protection (CloudFlare)

### What is NOT protected (requires external mitigations)
| Element | Risk | Mitigation |
|---------|------|------------|
| Off-chain state (positions, PnL) | Loss of all 3 servers = loss of state | Periodic sealed backups + Shamir |
| Order book | Lives in orchestrator RAM | Stateless restart, recreation from open orders |
| Funding rate history | Computed on the fly | Logging, recovery from logs |
| Price feed | Dependency on Binance API | Multiple backup sources, price freeze mode |
| P2P connectivity | Partition = split-brain | Quorum check, majority wins, self-demotion |

---

## 7. Risk Summary Table

| # | Scenario | Trading | Deposits | Withdrawals | Funds | Recovery | Time |
|---|----------|---------|----------|-------------|-------|----------|------|
| 1 | HAProxy down (1 operator) | ✅ | ✅ | ✅ | ✅ | Restart, DNS failover | ~30 sec |
| 2 | Orchestrator crash (sequencer) | ⚠️ pause | ⚠️ pause | ⚠️ pause | ✅ | Heartbeat failover | ~15-30 sec |
| 3 | Orchestrator crash (validator) | ✅ | ✅ | ✅ | ✅ | Restart, resync | ~10 sec |
| 4 | Enclave crash (orch. alive) | ⚠️ degrad. | ✅ | ✅ | ✅ | Load sealed state | ~5-15 sec |
| 5 | 1 operator fully down | ✅ | ✅ | ✅ (2-of-3) | ✅ | Automatic failover | ~15-30 sec |
| 6 | 2 operators down | ✅ | ✅ | ❌ waiting | ✅ | Wait for 1 recovery | variable |
| 7 | All 3 down | ❌ | ❌ | ❌ | ✅ (XRPL) | Shamir / restart | hours |
| 8 | P2P partition [1] vs [2] | ✅ (majority) | ✅ | ✅ (majority) | ✅ | Reconnect + reconcile | ~15-60 sec |
| 9 | P2P full fragmentation | ❌ read-only | ✅ (each) | ❌ | ✅ | Reconnect | variable |
| 10 | Binance API down | ⚠️ freeze | ✅ | ✅ | ✅ | Backup source / wait | ~1-60 min |
| 11 | XRPL node down | ✅ | ❌ not detect. | ❌ not submit. | ✅ | Backup XRPL node | ~10-30 sec |
| 12 | DNS/LB failure | ❌ for users | ❌ for users | ❌ for users | ✅ | DNS failover / direct IP | ~1-5 min |
| 13 | 1 malicious operator | ✅ | ✅ | ✅ (2 honest) | ✅ | Exclude from rotation | minutes |
| 14 | SGX side-channel | ✅ | ✅ | ✅ | ✅ (1 key insufficient) | Key rotation | hours |
| 15 | Hardware failure | ✅ | ✅ | ✅ (2-of-3) | ✅ | Key rotation + SignerListSet | 5 min - 2 hrs |
| 16 | Provider migration | ✅ | ✅ | ⚠️ 5 min pause | ✅ | Keygen + SignerListSet | ~30 min |
| 17 | Scaling | ✅ | ✅ | ⚠️ 5 min pause | ✅ | Key rotation + SignerListSet | ~30 min |
| 18 | Cascade: overload | ⚠️ latency | ⚠️ latency | ⚠️ latency | ✅ | Rate limiting, queue mgmt | ~1-5 min |
| 19 | Catastrophic (all 3 destroyed) | ❌ | ❌ | ❌ | ✅ (XRPL) | Shamir 3-of-5 | hours-days |

---

## 8. Threshold Flexibility: Not Just 2-of-3

XRPL SignerListSet supports up to 32 signers. Each signer has a weight, quorum is set arbitrarily.

| Scheme | Operators | To sign | Tolerated failures | For collusion | Use case |
|--------|-----------|---------|-------------------|---------------|----------|
| 2-of-3 | 3 | 2 | 1 | 2 (67%) | PoC, small team |
| 3-of-5 | 5 | 3 | 2 | 3 (60%) | Production |
| 5-of-9 | 9 | 5 | 4 | 5 (56%) | High decentralization |
| 7-of-11 | 11 | 7 | 4 | 7 (64%) | Maximum decentralization |
| 16-of-32 | 32 | 16 | 16 | 16 (50%) | Maximum XRPL SignerList |

**Recommendation:** t = ceil(n/2) + 1 (simple majority + 1).

> **Note:** FROST/DKG remains available in the enclave for Bitcoin Taproot use cases, but is not used for XRPL operations.
