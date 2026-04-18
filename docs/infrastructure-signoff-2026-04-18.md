# Infrastructure Sign-Off — 2026-04-18

**Author:** AL + Claude  
**Status:** All infrastructure work complete. Ready to proceed to application-layer changes (Tom's design document).

---

## Executive Summary

All 4 architectural bugs identified during failure-mode testing on 2026-04-17 are resolved. The dev/prod dual-mode cluster is operational on Hetzner. Mainnet escrow has been emptied and the account deleted. The codebase is ready for the next phase: reviewing and implementing Tom's vault/pricing architecture.

---

## Infrastructure Completion Matrix

### Architectural Bugs (found 2026-04-17, all fixed 2026-04-18)

| # | Bug | Fix | Verified |
|---|-----|-----|----------|
| 1 | No network path for cross-operator signing | P2P signing relay via gossipsub — <10ms quorum on 3-node testnet | Yes — 11/11 failure scenarios passed |
| 2 | No operator onboarding CLI (8 manual steps) | 4 CLI subcommands: `operator-setup`, `config-init`, `escrow-setup`, `operator-add` | Yes — builds, help verified |
| 3 | No deployment lifecycle | systemd units + `deploy.sh` with rollback + `/v1/health` endpoint + version tracking | Yes — systemd active on Hetzner |
| 4 | No CLI test tooling for auth endpoints | `sign-request`, `withdraw`, `balance` subcommands | Yes — builds, used in testing |

### Infrastructure Items

| Item | Status | Notes |
|------|--------|-------|
| Mainnet escrow emptied | Done | AccountDelete TX `17D4F2E3...`, 103.36 XRP sent to kupermind |
| Mainnet orchestrator stopped | Done | PID 2044357 terminated, was running 6 days |
| Dev enclave (port 9089) | Running | systemd `perp-dex-enclave-dev.service`, XRPL testnet |
| Dev orchestrator (port 3003) | Running | systemd `perp-dex-orchestrator-dev.service`, testnet |
| Dev smoke test | Passed | Deposit, price update, order matching through CLOB with vault-mm |
| p2p_identity.key backup | Done | `~/.config/perp-dex/p2p_identity_{mainnet,dev}.key` |
| Escrow seed backup | Done | `~/.secrets/multisig_escrow_mainnet.json` (chmod 600) |
| FROST enclave fix (cherry-pick) | In codebase | Commit `82e809a` — ECDH+AES-GCM cross-machine transport. Deployed on next enclave rebuild |
| Singleton runner | In codebase | Vault MM/DN wrapped in singleton; main loop gated by `is_sequencer`. Deployed on next orchestrator push |
| State event replication | In codebase | Deposits, funding, liquidations broadcast via P2P `perp-dex/events` topic. Deployed on next push |
| deploy.sh with rollback | Done | `./deploy.sh rollback [node]` restores `.prev` binary |
| systemd (Hetzner dev) | Active | Both services enabled, auto-restart on crash/reboot |
| systemd (Azure) | Active | `perp-dex-orchestrator.service` on all 3 VMs |
| Health endpoint | Done | `/v1/health` → version, role, peers, enclave status, uptime |
| Tests | Passing | 78 unit + 6 integration, all green |

### PM Project Alignment

| Improvement from Phoenix PM | Status | Notes |
|-----------------------------|--------|-------|
| systemd on Hetzner | Done | Two service files created and enabled |
| Singleton runner | Done | `singleton.rs` — role-aware spawn/abort on promotion/demotion |
| State-log replication | Done | Events topic via gossipsub (lighter than PM's full PG state-log — our critical state is in SGX sealed storage, not PG) |
| FROST enclave fix | Done | Cherry-picked `9bd4f0d` — cross-machine FROST share transport |

---

## What's NOT Done (and why it's OK)

| Item | Reason |
|------|--------|
| Enclave rebuild with FROST fix | Not blocking — we don't use FROST yet. Fix is in the codebase for the next build |
| Deploy new orchestrator to Azure | Not blocking — Azure VMs are deallocated (dev mode). Will deploy when scaling to prod mode |
| Signer count health monitoring | Not urgent until N > 3 operators |
| `/v1/health` behind auth | By design — all orchestrator endpoints require XRPL wallet signature |
| Hetzner mainnet systemd unit | Not needed — mainnet instance will be replaced by dev after testing |

---

## Next Phase: Tom's Architecture Document

All infrastructure work that does NOT depend on Tom's design document is complete. The remaining work is application-layer:

- Vault architecture (vAMM vs CLOB — resolved: CLOB preserved)
- Margin system review
- XRP collateral pricing
- Maker rebate program
- API restructuring

**I am now switching to Tom's design document review and implementation.**

---

## Test Evidence

```
$ cargo test
test result: ok. 78 passed; 0 failed; 0 ignored
test result: ok. 6 passed; 0 failed; 0 ignored

$ systemctl is-active perp-dex-enclave-dev perp-dex-orchestrator-dev
active
active

$ curl -sk https://localhost:9089/v1/perp/balance?user_id=vault:mm
{"status":"success","data":{"margin_balance":"20000.00000000",...}}
```
