# Test environment workflow

**Date:** 2026-05-26
**Status:** Active

System has three environments. They are not parallel choices the consumer makes — they are sequential stages in our delivery flow.

```
dev-perp iteration    │   pre-prod validation    │   consumer acceptance
─────────────────────────────────────────────────────────────────────────
Hetzner (single host) │   Azure 3-node cluster   │   Tom + frontend
                      │   (testnet-cluster)      │
                      │                          │
local enclave         │   real DCAP attestation  │
no multi-op signing   │   multi-operator FROST   │
fast iteration        │   failover / promote     │
accumulation bugs     │                          │
visible here          │                          │
```

## Flow per feature

1. **dev-perp implements + runs preflight on Hetzner.** Code verification, accumulation-class scenarios (state that builds up across restarts surfaces here), rapid debug-rebuild iteration. Hetzner enclave is single-node dev SGX, no DCAP — so attestation-shape bugs are invisible. Multi-operator signing is bypassed (single-operator path used).
2. **Deploy to Azure testnet-cluster (staging).** Binary copied to all 3 nodes, systemd swap. Cluster-only behavior gets validated here: sequencer failover, multi-operator FROST signing, real DCAP attestation, libp2p mesh dynamics. Production-shape behaviour appears here that Hetzner can't show by design.
3. **Tom (and we, alongside) test against `api-dev.xperp.fi`.** Through TLS + Hetzner nginx + passive failover (`proxy_next_upstream` on 503) the request lands on the current Azure sequencer. Tom never has to know about per-env behavior — for him this is one black box with full system features. Bugs Tom surfaces here go back to step 1 for fix, then 2, then 3 (round-trip).

## Why this resolves "Hetzner ≠ Azure equivalence"

Each env contributes a different surface of validation:

| Surface | Where it surfaces |
|---|---|
| Vault curve math, ladder placement, posture transitions | Hetzner + Azure (both have the code path) |
| Accumulation-class bugs (Q-19-4-like) | Hetzner mandatory (state accumulates across restarts naturally) |
| Failover / singleton respawn (X-19-2-like) | Azure mandatory (cluster-only) |
| Multi-operator FROST signing | Azure mandatory |
| Real DCAP attestation | Azure mandatory |
| External-consumer access path (TLS, auth, headers) | Tom acceptance through `api-dev.xperp.fi` |

The "two envs differ" fact stops being a synchronisation mine because the workflow is sequential, not parallel. Tom does not see the difference — he sees only the black box at `api-dev.xperp.fi`. The cross-env discipline lives with dev-perp.

## Production env (separate, not-yet-provisioned)

`api.xperp.fi` is reserved as the production hostname; it currently aliases dev temporarily and gets re-pointed when production VMs are provisioned. Production-shape requirements (Azure Load Balancer with health-checked dynamic upstream, real DCAP allowlist, codified Terraform / Azure CLI infrastructure) live in the production roadmap, not in V1 vault closure.

## Today (2026-05-26 architectural state)

- `api-dev.xperp.fi` → Hetzner nginx (TLS, Let's Encrypt) → `proxy_next_upstream` over Azure 3-node cluster on `:3000` → current sequencer → DCAP-attested enclave.
- Hetzner orchestrator on `:3003` continues running as our dev workshop; no public hostname.
- NSG `:3000` opened only to Hetzner IPv4 `94.130.18.162`.
- Failover verified live: kill Azure node-1, signed POST through `api-dev.xperp.fi` succeeds on the next sequencer (node-2).
