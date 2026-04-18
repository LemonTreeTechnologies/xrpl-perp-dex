# Architectural Bugs — 2026-04-17

Found during failure-mode testing. These are not "nice-to-haves" — the system cannot operate in multi-operator mode without fixing them. Every workaround used during testing (SSH tunnels, ad-hoc Python, manual kill) is evidence of a missing architectural component.

**Principle:** if a test requires a workaround that isn't part of the system, that's not a testing gap — it's an architectural bug.

---

## Bug 1: No network path for cross-operator signing

**Symptom:** To test 2-of-3 multisig withdrawal, we had to create SSH tunnels from Hetzner to each enclave's localhost:9088.

**Root cause:** The enclave HTTP server binds to `127.0.0.1:9088`. The orchestrator's `withdrawal.rs` calls `signer.enclave_url` for each remote signer. These two facts are incompatible — there is no production-grade network path from orchestrator-A to enclave-B.

**Why it matters:** This is the entire withdrawal system. Without cross-signing, multisig doesn't work. Without multisig, user funds can't be withdrawn safely.

**Options (pick one):**

| Option | Change | Trust model impact | Complexity |
|--------|--------|--------------------|------------|
| A. Enclave binds to vnet interface | `config.json` → `bind_address: "10.0.0.x"` + Azure NSG rules | Expands enclave attack surface to vnet peers | Low — config change |
| B. Orchestrator signing proxy | New `POST /v1/proxy/sign` endpoint on orchestrator (port 3000, already open) that forwards to local enclave | No new attack surface on enclave; orchestrator authenticates peers via P2P identity | Medium — new endpoint |
| C. P2P signing relay | Signing requests travel over existing gossipsub mesh | Reuses existing authenticated channel; no new ports | High — protocol change |

**Decision: Option C — P2P signing relay.**

The gossipsub mesh already exists, already authenticated via libp2p peer identity, already carries heartbeats and state replication. Using it for signing requests is using the right component for the right purpose:

- No new ports to open
- No new attack surface (enclave stays localhost-only)
- Authentication is already solved (peer_id verification)
- Retry and peer discovery already implemented
- If a peer is unreachable for signing, it's also unreachable for consensus — the system already handles this

Option A expands the enclave attack surface. Option B adds a new unauthenticated HTTP endpoint. Option C reuses what's already there.

**Implementation:**
1. Add `SigningRequest { hash, signer_account }` and `SigningResponse { der_signature, pubkey }` message types to P2P protocol
2. Orchestrator-A sends `SigningRequest` to orchestrator-B via gossipsub
3. Orchestrator-B receives it, calls its local enclave `/v1/pool/sign`, returns `SigningResponse`
4. Orchestrator-A collects responses until quorum, assembles multisig tx
5. Timeout per signer = same as heartbeat timeout (15s) — if peer is dead for signing, election will handle it

---

## ~~Bug 2: No operator onboarding path~~ — FIXED

**Fixed:** 4 CLI subcommands now cover the full lifecycle:

| Command | Purpose |
|---------|---------|
| `operator-setup` | Generate keypair in enclave, derive XRPL address, output signer entry JSON |
| `config-init` | Combine multiple entry files into `signers_config.json` with quorum |
| `escrow-setup` | Submit `SignerListSet` + optionally disable master key on XRPL |
| `operator-add` | Add new operator to existing config + optionally re-submit SignerListSet |

Full workflow:

```bash
# 1. Generate identity on each enclave
./orchestrator operator-setup --enclave-url https://node-1:9088/v1 --name node-1 --output node-1.json

# 2. Combine into config
./orchestrator config-init --entries node-1.json node-2.json node-3.json \
  --escrow-address rXXX --quorum 2

# 3. Set up escrow on XRPL
./orchestrator escrow-setup --signers-config signers_config.json --escrow-seed sEdXXX --disable-master

# 4. Add operator later
./orchestrator operator-add --enclave-url https://node-4:9088/v1 --name node-4 \
  --config signers_config.json --xrpl-url https://... --escrow-seed sEdXXX
```

---

## ~~Bug 3: No deployment lifecycle~~ — FIXED

All components implemented:

| Component | Status | Location |
|-----------|--------|----------|
| systemd unit files | Done | Hetzner: `/etc/systemd/system/perp-dex-{enclave,orchestrator}-dev.service`; Azure: `scripts/perp-dex-orchestrator.service` |
| Health endpoint | Done | `GET /v1/health` → `{"status","version","role","peers","enclave","uptime_secs"}` |
| Deploy script | Done | `scripts/deploy.sh` — preflight, backup, atomic swap, health check, auto-rollback on failure |
| Version tracking | Done | `CARGO_PKG_VERSION` in health endpoint; `deploy.log` on each node |
| Rollback procedure | Done | `./deploy.sh rollback [node\|all]` restores `.prev` binary |

---

## Status

All 4 architectural bugs resolved:

| Bug | Status | Fixed |
|-----|--------|-------|
| Bug 1: Cross-signing | FIXED | P2P signing relay via gossipsub (<10ms quorum) |
| Bug 2: Operator onboarding | FIXED | 4 CLI commands: operator-setup, config-init, escrow-setup, operator-add |
| Bug 3: Deployment lifecycle | FIXED | systemd + deploy.sh with rollback + health endpoint + version tracking |
| Bug 4: CLI test tooling | FIXED | sign-request, withdraw, balance subcommands |

---

## ~~Bug 4: No CLI test tooling for authenticated endpoints~~ — FIXED

Three CLI subcommands implemented in `cli_tools.rs`:

| Command | Purpose |
|---------|---------|
| `sign-request` | Generate signed curl command for any endpoint |
| `withdraw` | Submit authenticated withdrawal |
| `balance` | Query account balance with auth |

```bash
./orchestrator sign-request --seed sEdXXX --url http://localhost:3000/v1/orderbook
./orchestrator withdraw --seed sEdXXX --amount 1 --destination rXXX
./orchestrator balance --seed sEdXXX
```
