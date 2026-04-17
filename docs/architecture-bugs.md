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

## Bug 2: No operator onboarding path

**Symptom:** Setting up the 2-of-3 XRPL escrow required an ad-hoc Python script that:
- Called each enclave's `/v1/pool/generate`
- Manually compressed the uncompressed secp256k1 pubkey
- Derived the XRPL r-address via SHA-256 + RIPEMD-160 + Base58Check
- Used `xrpl-py` to submit `SignerListSet` and `AccountSet` (disable master key)
- Manually wrote `signers_config.json`

None of these steps exist as a reproducible tool.

**Root cause:** The system has no concept of "operator identity setup". Each piece exists in isolation:
- Enclave can generate keys (`/v1/pool/generate`)
- Rust code can derive XRPL addresses (`xrpl_signer.rs`)
- Orchestrator can read `signers_config.json`

But there is no tool that connects them: generate → derive → register → configure.

**What's needed:** A single CLI command or script:

```
./operator-setup \
  --enclave-url https://localhost:9088/v1 \
  --xrpl-url https://s.altnet.rippletest.net:51234 \
  --output signers_config.json
```

That:
1. Calls enclave `/v1/pool/generate` → gets pubkey
2. Compresses pubkey → derives XRPL r-address (using the same Rust code as `xrpl_signer.rs`)
3. Outputs the signer entry JSON (address, compressed_pubkey, xrpl_address, session_key)

And a separate escrow setup command:

```
./escrow-setup \
  --signers-config signers_config.json \
  --xrpl-url https://s.altnet.rippletest.net:51234 \
  --escrow-seed sEdXXX \
  --quorum 2
```

That:
1. Reads all signer XRPL addresses from config
2. Submits `SignerListSet` to XRPL
3. Disables master key on escrow
4. Verifies the setup

---

## Bug 3: No deployment lifecycle

**Symptom:** During testing, old orchestrator processes (PIDs 81288, 80769) from a previous deployment were still running, binding port 4001, and interfering with new deployments. Required manual `kill -9` to clean up.

**Root cause:** No process management. No deployment script. No health verification. The current "deployment" is:

```bash
scp binary azureuser@$ip:~/perp/
ssh azureuser@$ip "pkill old; ./new-binary [args] &"
```

**What's missing:**

| Component | Status | Impact |
|-----------|--------|--------|
| systemd unit files | Not created | No auto-restart on crash, no clean stop |
| Health endpoint | Not implemented | No way to verify readiness |
| Deploy script | Not written | No reproducibility, no audit trail |
| Version tracking | Not implemented | No way to know what's running |
| Rollback procedure | Not defined | Can't recover from bad deploy |

**What's needed:**

1. **systemd units** for `perp-dex-server` and `perp-dex-orchestrator` on each Azure VM
2. **`GET /v1/health`** on orchestrator returning `{"status": "ok", "version": "...", "role": "sequencer|validator", "peers": N, "enclave": "ok|error"}`
3. **`scripts/deploy.sh`** that: builds on Hetzner → verifies binary → stops old via systemd → deploys new → starts → health check → logs deployment

---

## Dependency chain

```
Bug 1 (cross-signing) blocks:
  └── End-to-end multisig withdrawal via orchestrator API
  └── Production-grade operator config

Bug 2 (onboarding) blocks:
  └── Adding new operators without ad-hoc scripting
  └── Key rotation flow
  └── Grant milestone M1 demo (clean path, not workaround)

Bug 3 (deployment) blocks:
  └── Reproducible cluster setup
  └── Failure recovery (auto-restart)
  └── CI/CD pipeline
```

**Priority order:** Bug 1 → Bug 2 → Bug 3 → Bug 4. Bug 1 is the foundation — without it, the withdrawal system only works through SSH tunnels.

---

## Bug 4: No CLI test tooling for authenticated endpoints

**Symptom:** To test the `/v1/withdraw` endpoint, we had to write a throwaway Python script that:
- Generates a secp256k1 keypair
- Derives XRPL r-address
- Signs the request body with SHA-256 + ECDSA + DER encoding + low-S normalization
- Sets X-XRPL-Address, X-XRPL-PublicKey, X-XRPL-Signature, X-XRPL-Timestamp headers

This is the same pattern as Bug 2 — every test of an authenticated endpoint requires a custom script.

**Root cause:** The system requires XRPL wallet signature auth (correct for production), but provides no tooling to generate signed requests outside of the Crossmark browser extension. There is no `curl`-equivalent for testing.

**What's needed:** A CLI tool (part of orchestrator binary or standalone):

```
./perp-cli withdraw \
  --api http://localhost:3000 \
  --seed sEdXXX \
  --amount 1 \
  --destination rXXX
```

That handles auth internally. Or at minimum, a `sign-request` subcommand:

```
./perp-cli sign-request \
  --seed sEdXXX \
  --body '{"user_id":"rXXX","amount":"1","destination":"rYYY"}'
# Outputs: curl -H "X-XRPL-Address: ..." -H "X-XRPL-PublicKey: ..." ...
```

This unblocks:
- Automated integration testing
- Operator-level failure testing without ad-hoc scripts
- CI/CD pipeline health checks
