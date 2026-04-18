# Perp DEX Cluster — Dual-Mode + Infrastructure Status

**Date**: 2026-04-18
**Status**: Current state + plan for review.
**Author**: AL + Claude.

This document mirrors the Phoenix PM `cluster_dual_mode_status` format
to give colleagues a single source of truth on:
- what's built and what's missing in the perp-DEX infrastructure
- the Hetzner/Azure topology and budget
- the dev/prod operating modes
- the order of remaining infrastructure work

## 1. Operating modes — Dev vs Prod

### Dev mode (Azure VMs deallocated, mainnet escrow empty)

This is the **target state after fund withdrawal** (pending Tom +
kupermind confirmation).

- Hetzner runs two isolated instances side by side:

```
Hetzner EX44 (94.130.18.162) — Xeon E3-1275 v6, 64GB RAM, SGX1 HW
├── MAINNET instance
│   ├── Enclave (perp-dex-server): port 9088, SGX HW mode
│   ├── Orchestrator:              port 3000, XRPL mainnet
│   ├── Sealed data:               /tmp/perp-9088/
│   └── Database:                  perp_dex (PostgreSQL)
│
└── DEV/STAGING instance
    ├── Enclave (perp-dex-server): port 9089, SGX HW mode
    ├── Orchestrator:              port 3001, XRPL testnet
    ├── Sealed data:               /tmp/perp-9089/
    └── Database:                  perp_dex_dev (PostgreSQL)
```

- libp2p mesh N=1 (Hetzner only). Hetzner is sequencer trivially.
- Single-signer cryptography: XRPL single-account ECDSA (one
  SGX-held key on Hetzner). No multisig — no Azure enclaves needed.
- Enclave and orchestrator can be updated freely on the dev instance.
  Mainnet instance updated only after dev testing passes.
- **Azure cost: €0.**

### Prod mode (Azure VMs running)

- 4-node cluster: Hetzner (priority=0, preferred sequencer) + 3 Azure
  DCsv3 VMs (priority 1/2/3).
- All 4 nodes participate in: gossipsub mesh, sequencer election,
  P2P signing relay.
- Distributed signing available:
  - **XRPL SignerListSet 2-of-3** across 3 Azure SGX enclaves.
  - Cross-host withdrawal signing via gossipsub (<10ms for quorum).
- DCAP remote attestation available (Azure DCsv3 only — 4,734-byte
  Intel-signed quotes).
- **Azure cost: 3× DCsv3 VMs ≈ €300/month.**

### Switching modes

- `az vm deallocate -g SGX-RG -n sgx-node-{1,2,3}` → Dev mode.
- `az vm start -g SGX-RG -n sgx-node-{1,2,3}` → Prod mode.
- No orchestrator restarts on Hetzner. libp2p mesh adapts: peers
  come and go, the whitelist stays.
- Signing mode degrades: 2-of-3 multisig unavailable in dev mode,
  single-signer fallback.
- ~30s window during Azure VM boot until enclaves reattach.

## 2. Today's situation — what's built vs what's missing

### Infrastructure (chain-agnostic)

| Component | Status | Notes |
|-----------|--------|-------|
| libp2p mesh + Noise mTLS | ✅ Built | gossipsub, per-peer whitelist, persistent peer_id |
| Sequencer election | ✅ Built | priority + heartbeat, tested with split-brain |
| P2P signing relay | ✅ Built | <10ms quorum on 3-node testnet cluster |
| Deploy script | ✅ Built | `deploy.sh` — rolling deploy to Azure via Hetzner bastion |
| systemd units (Azure) | ✅ Built | `perp-dex-orchestrator.service` on all 3 Azure nodes |
| systemd units (Hetzner) | ❌ Missing | Hetzner runs via nohup, no auto-restart |
| Health endpoint | ✅ Built | `/v1/health` — enclave status, peers, role, uptime, version |
| Rollback mechanism | ❌ Missing | Manual binary swap only, no automated rollback |
| Version tracking | ❌ Missing | No `version.json` per node, no MRENCLAVE tracking |
| Signer count monitor | ❌ Missing | No alert when SignerListSet count ≠ expected |

### Perp DEX (application layer)

| Component | Status | Notes |
|-----------|--------|-------|
| Perp engine (enclave) | ✅ Built | 11 ecalls: deposit, withdraw, open/close position, liquidation, funding, state persistence |
| CLOB order book | ✅ Built | In-enclave matching, reduce_only IOC for closes |
| MM vault | ✅ Built | `vault:mm` as passive market maker on CLOB |
| Price feed | ✅ Built | Binance WebSocket → enclave `update_price` |
| Liquidation loop | ✅ Built | Periodic `check_liquidations` + auto-liquidate |
| Withdrawal flow | ✅ Built | Atomic margin check + ECDSA sign in enclave, 2-of-3 multisig collection |
| Deposit monitoring | ✅ Built | XRPL ledger polling for incoming payments |
| DCAP attestation | ✅ Built | Azure only — quote generation + verification |
| Shard-first architecture | ✅ Built | `shard_id` first-class in enclave state, N=1 for now |
| Partitioned sealing | ✅ Built | 5,000 users per sealed partition |
| Local XRPL signing | ✅ Built | Ed25519 + secp256k1 polymorphic signing, no Python dependency |
| Frontend API | ✅ Built | REST endpoints, HMAC auth, replay protection |

### Security / operations

| Item | Status | Notes |
|------|--------|-------|
| Mainnet master key | ⚠️ NOT disabled | Seed in plaintext JSON on Hetzner disk. Pending: withdraw funds → disable or regenerate. |
| Escrow SignerListSet | ✅ Configured | 2-of-3 on Azure enclave keys (testnet escrow `rLTFG...`) |
| Mainnet escrow SignerListSet | ⚠️ Cosmetic | Master key enabled → multisig bypassable |
| Enclave upgrade tooling | ❌ Not built | Strategy 4 (rolling key rotation) designed, not implemented |
| Enclave upgrade procedure | ✅ Documented | `deployment-dilemma.md` — 8 attack vectors analyzed |
| Failure mode testing | ✅ Complete | 11/11 scenarios passed on live cluster |
| Security audit | ✅ Complete | 52 findings, 50 fixed, 2 documented as by-design |

## 3. Current cluster state (live)

### Hetzner (94.130.18.162)

| Process | Port | Status | Since |
|---------|------|--------|-------|
| `ethsigner-server` (Phoenix PM enclave) | 8085 (HTTPS) | Running | Apr 03 |
| `perp-dex-server` (perp enclave) | 9088 (HTTPS) | Running | Apr 07 |
| `perp-dex-orchestrator` (mainnet) | 3000 | Running | Apr 12 |
| nginx | 80/443 | Running | — |

- SGX: HW mode (Kaby Lake, SGX1). No DCAP attestation.
- Enclave accounts: 48 sealed accounts in `/tmp/perp-9088/accounts/`
- Perp state: sealed to disk (users, positions, vaults, tx hashes)
- Escrow: `r4rwwSM9PUu7VcvPRWdu9pmZpmhCZS9mmc`, 108.36 XRP
- **No systemd** — manual nohup. Reboot = manual restart required.
- `p2p_identity.key` in `/tmp/perp-9088/` — survives reboot only by luck.
  Backup exists in `~/.secrets/`.

### Azure node-1 (20.71.184.176)

| Process | Port | Status | Role |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Running | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Running | **Sequencer** |

- Uptime: ~14h, version 0.1.0, 2 peers connected
- systemd: `perp-dex-orchestrator.service` enabled

### Azure node-2 (20.224.243.60)

| Process | Port | Status | Role |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Running | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Running | **Validator** |

- Uptime: ~14h, version 0.1.0, 2 peers connected
- systemd: `perp-dex-orchestrator.service` enabled

### Azure node-3 (52.236.130.102)

| Process | Port | Status | Role |
|---------|------|--------|------|
| `perp-dex-server` (enclave) | 9088 (HTTPS) | Running | SGX2+DCAP |
| `perp-dex-orchestrator` | 3000 | Running | **Validator** |

- Uptime: ~14h, version 0.1.0, 2 peers connected
- systemd: `perp-dex-orchestrator.service` enabled

### Why Hetzner is not in the P2P mesh

The Hetzner orchestrator runs with `--priority 0` but connects to
XRPL **mainnet**, while Azure nodes connect to **testnet**. They are
separate clusters that happen to share the same Hetzner machine.

After the mainnet escrow is emptied and Hetzner switches to testnet,
Hetzner can join the Azure mesh as a 4th node (priority=0, preferred
sequencer) — identical to the Phoenix PM architecture.

## 4. Comparison with Phoenix PM cluster

Both projects share the same enclave codebase and converge on the same
infrastructure patterns. Key differences:

| Aspect | Perp DEX | Phoenix PM |
|--------|----------|------------|
| **Distributed signing** | XRPL SignerListSet 2-of-3 (3 ECDSA sigs) via P2P relay | BTC FROST 2-of-3 (Schnorr) via libp2p + XRPL multisig via SSH tunnels |
| **SSH tunnels in signing** | ✅ Eliminated (P2P relay) | ⚠️ Still used for XRPL multisig |
| **State replication** | Not implemented (single-sequencer writes) | ✅ PG state-log + snapshot catch-up |
| **External RPC corroboration** | Not implemented | ✅ Race-corroboration aggregator |
| **Singleton runner** | Not implemented | ✅ "Run only on sequencer" abstraction |
| **DCAP attestation** | ✅ Working on Azure | ✅ Working on Azure |
| **Enclave FROST fix** | Not cherry-picked (not using FROST) | ✅ `9bd4f0d` — ECDH+AES-GCM cross-machine |
| **Production traffic** | ~0 TPS, 108 XRP in escrow | 38,031 markets on api.ph18.io |
| **Process management (Hetzner)** | ❌ nohup | ✅ systemd |

### What to adopt from Phoenix PM

1. **systemd on Hetzner** — we have it on Azure but not on Hetzner.
   PM has `phoenix-pm-enclave.service`, `phoenix-rs.service`. We should
   add `perp-dex-server.service` + `perp-dex-orchestrator.service`.

2. **Singleton runner** (`singleton.rs`) — "run task only on sequencer"
   abstraction. Useful for vault MM, price feed, liquidation loop.
   Currently these run on every node which is wasteful.

3. **State-log replication** — not urgent (our state lives in enclave,
   not PG), but useful for order book and trade history replication
   to validator nodes.

4. **FROST enclave fix** — cherry-pick when we need cross-machine
   FROST (not now, but when Strategy 4 key rotation is implemented).

## 5. Immediate infrastructure plan

### Priority 0: Empty mainnet escrow (blocked on Tom + kupermind)

- Tom and kupermind withdraw their XRP (~108.36 total)
- Method: simple XRPL Payment signed with master key (seed on disk)
- After withdrawal: Hetzner free for updates, no fund risk
- **Status: waiting for recipient addresses**

### Phase I: Hetzner dual-instance (after escrow empty, ~3 hours)

| Step | Deliverable | Risk |
|------|-------------|------|
| I.1 | Build new `perp-dex-server` + `perp-dex-orchestrator` from latest code | None — build only |
| I.2 | Create dev enclave data dir `/tmp/perp-9089/` with testnet config | None |
| I.3 | Start dev enclave on port 9089, dev orchestrator on port 3001 | Low — separate process |
| I.4 | Verify dev instance health, generate test account, run deposit/trade cycle on testnet | None |
| I.5 | Move `p2p_identity.key` to `~/.config/perp-dex/` (permanent location) | Low — restart needed |
| I.6 | Add systemd units for both mainnet and dev instances on Hetzner | Low |

### Phase II: Deployment lifecycle (~1 day)

| Step | Deliverable |
|------|-------------|
| II.1 | `deploy.sh rollback <node>` — restore previous binary from `.prev` |
| II.2 | `version.json` per node — binary hash, git commit, MRENCLAVE, deploy timestamp |
| II.3 | `deploy.sh status` — query all nodes: version, uptime, health, signer count |
| II.4 | SignerListSet signer count health check — alert on count ≠ expected |

### Phase III: Operator onboarding CLI (~1-2 days)

| Step | Deliverable |
|------|-------------|
| III.1 | `orchestrator add-node --vm <ip> --port-base 9089` — single command to add a node |
| III.2 | Automates: SSH setup, binary copy, config generation, enclave start, health check |
| III.3 | `orchestrator remove-node --vm <ip>` — clean decommission |

## 6. Azure VM budget

> **Shared resource constraint (aligned with Phoenix PM, 2026-04-18):**
> The 3 Azure DCsv3 VMs (sgx-node-1/2/3) are **shared** between
> perp-DEX and Phoenix PM. They cannot be unilaterally deallocated by
> either project. Two conditions must hold before they can go off-budget:
>
> 1. Phoenix PM has finished both XRPL and BTC cluster integration
>    (Phases X + B in PM's plan).
> 2. Perp-DEX has reached equivalent dual-mode capability (Phases I-III
>    in this plan).
>
> Until both conditions are met, the VMs stay on for cost-sharing
> reasons. The "deallocate by default" property of dual-mode is the
> **target end-state**, not the during-work reality.

What's actually in scope right now:

| Phase | Azure VMs needed | Incremental cost |
|-------|------------------|------------------|
| Phase I (Hetzner dual-instance) | Already on (shared) | €0 incremental |
| Phase II (deployment lifecycle) | Already on (shared) | €0 incremental |
| Phase III (add-node testing) | Already on (shared) | €0 incremental |
| **End-state (both projects done)** | **Off by default** | €0 (target) |
| Multisig testing (end-state) | On temporarily | ~€5/session |
| DCAP attestation (end-state) | On temporarily | ~€5/session |

**During current phase:** Azure VMs are on 24/7 (shared cost with PM).
No incremental burn from our infrastructure work.

**After both projects reach dual-mode:** Azure can be deallocated
between active test sessions, dropping cost dramatically. But this
requires symmetric readiness — neither project can force the other
off the shared VMs.

### Multisig regression testing during development

Following PM's approach: FROST/multisig must be exercised periodically,
not only at the end.

- **Default for dev work:** single-signer mode on Hetzner dev instance.
  Fast, no Azure dependency, full local debug loop.
- **Periodic regression:** at least once per Phase completion, run a
  prod-mode end-to-end:
  1. Confirm Azure VMs are on (during shared-budget phase they are).
  2. Create a SignerListSet 2-of-3 multisig transaction on testnet.
  3. Verify all 3 SGX signers participated via P2P relay.
  4. Confirm transaction on XRPL testnet.
- **Before any production cutover:** full prod-mode regression suite
  must pass, not just dev-mode tests.

## 7. Implications for stakeholders

### Frontend (xperp.fi)

- **No disruption during Phase I-III.** All infrastructure work is
  server-side.
- `api-perp.ph18.io` endpoint contract unchanged.
- After mainnet escrow is emptied, the API will return empty balances
  for existing users — expected behavior.
- Dev instance (port 3001) is not exposed via nginx — internal only.

### Tom (8Baller)

- **Withdraw XRP first.** We need your recipient XRPL address and
  kupermind's address.
- **Your architecture document** is not blocked by this infrastructure
  work. We will review it after Phase I is complete.
- **Vault/AMM/pricing decisions** are independent — infrastructure
  improvements benefit any matching model.

### Security auditors / grant reviewers

- `deployment-dilemma.md` documents all enclave upgrade strategies
  with attack surface analysis (8 vectors, each with defense).
- Strategy 2 (MRSIGNER) and Strategy 3 (Recovery) are explicitly
  REJECTED with rationale.
- Strategy 4 (rolling key rotation) is the mainnet target but not
  yet implemented — Strategy 1 (manual dual-server) is sufficient
  for MVP with trusted operator.
- 11/11 failure-mode scenarios verified on live cluster.

## 8. Open questions (not blocking this plan)

1. **Hetzner as 4th cluster member** — after mainnet switch to testnet,
   Hetzner joins the Azure mesh. Hetzner has SGX1 (no DCAP), so it
   can sign but cannot produce attestation quotes. Accept as non-DCAP
   peer or exclude from signing? PM chose "state-only peer, no signing
   on Hetzner" — we should decide the same.

2. **Enclave state replication** — perp engine state lives sealed
   inside the enclave, not in PostgreSQL. Replicating it requires
   either (a) enclave-to-enclave sync protocol or (b) accepting that
   only the sequencer has the authoritative perp state and validators
   are read-only relays. Option (b) is simpler and sufficient for
   current scale.

3. **Master key disposition** — after fund withdrawal, should we
   disable the master key on the mainnet escrow and set up proper
   2-of-3 multisig? Or create a fresh escrow from scratch with
   master key disabled from the start? Fresh escrow is cleaner.

4. **Singleton runner** — vault MM, price feed, and liquidation loop
   currently run on every node. Should adopt PM's singleton pattern
   (run only on sequencer) to avoid duplicate orders and conflicting
   liquidations. Not urgent at N=1 but required before N>1.
