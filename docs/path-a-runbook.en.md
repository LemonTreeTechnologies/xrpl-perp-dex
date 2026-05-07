# Path A migration ceremony — operator runbook (EN)

**Status:** REQ-8 commit 11 draft. Ceremony driver itself ships in commit 12; this runbook documents the operator-visible procedure that the driver wraps. Reference spec: `docs/audit/REQ-7.md` (private repo).

---

## What Path A is

A cluster-coordinated upgrade of the perp-dex enclave software to a new MRENCLAVE without losing customer state (operator XRPL keys, FROST shares, perp positions, vault balances, account pool). Each operator runs the ceremony locally on their own machine; no cross-operator SSH.

**What Path A delivers:**
- Funds in escrow remain accessible — old XRPL signing keys survive the upgrade through the migration ceremony.
- Customer perp state survives — clients see no reset of balances or open trades.
- The cluster's quorum-of-operators governs whether an upgrade may proceed.

**What Path A does NOT cover:**
- Initial bootstrap (`node-deploy` without `--side-by-side`).
- Disaster recovery if OLD enclave dies before the ceremony completes.
- Cross-host migration (Path A is platform-bound to the same physical SGX CPU).

## Pre-requisites

Before invoking `node-deploy --side-by-side` on any node:

1. **OLD enclave is running and healthy** on port 9088 with sealed state in `/home/azureuser/perp/accounts/`. Verify: `systemctl is-active perp-dex-enclave` returns `active`.

2. **Quorum of operators agree** on the new MRENCLAVE. Out-of-band consensus (Slack / call) before any operator pre-stages the NEW unit. The delegation bundle the orchestrator collects later only proves the agreement was real after the fact; pre-agreement is operator discipline.

3. **Build artefacts ready**: freshly-built `perp-dex-orchestrator`, `enclave.signed.so`, `perp-dex-server`, and ideally `build-manifest.txt` with the expected MRENCLAVE pinned. Build via the GHA pipeline against the agreed-on git ref. The build-manifest's MRENCLAVE is what `node-deploy --side-by-side` cross-checks against the running NEW enclave's `/version` post-deploy.

4. **One-time per-machine NEW-side setup** done (only the very first ceremony on a given operator machine):

   a. Copy `scripts/path-a/perp-dex-enclave-next.service` to `/etc/systemd/system/perp-dex-enclave-next.service`.

   b. Run `sudo systemctl daemon-reload`.

   c. Create `/home/azureuser/perp-next/` (will be populated by `node-deploy --side-by-side`).

   d. Pre-stage `/home/azureuser/perp-next/config.json` from `scripts/path-a/config-next.json.sample`. Adjust `ssl_*` paths if your TLS topology differs from the OLD unit's.

   e. Verify the NEW unit is recognised: `systemctl status perp-dex-enclave-next` should report `inactive (dead)` (unit known, not yet started).

   This setup is permanent — subsequent ceremonies on the same machine reuse it. After a successful ceremony's post-promotion phase the operator may keep the NEW unit definition for the next upgrade cycle.

## Ceremony — operator's view

The **ceremony driver** (commit 12) wraps the steps below. Operator interaction is:

```
$ perp-dex-orchestrator node-deploy --side-by-side \
    --orchestrator ./perp-dex-orchestrator-new \
    --enclave-dist ./dist-azure-new
```

This installs the NEW artefacts to `/home/azureuser/perp-next/` and starts the NEW enclave on port 9089 alongside OLD (port 9088). NEW comes up with empty sealed state and an ephemeral migration keypair generated inside the SGX enclave.

After all operators have completed `node-deploy --side-by-side` independently, **one operator** drives the migration ceremony:

```
$ curl -k -X POST https://localhost:9088/admin/migrate-state \
    -H 'Content-Type: application/json' \
    -d '{"expected_mrenclave_new": "<hex 64>", "ceremony_nonce_request": true}'
```

(Ceremony driver semantics defined in commit 12.)

The driver:
1. Asks NEW (port 9089) for its target_info + a fresh ceremony_nonce.
2. Asks NEW to generate the ephemeral migration keypair with bindings to (target_info_of_old, ceremony_nonce).
3. Collects delegation signatures from quorum of operators using the existing libp2p signing-relay (the same flow the SignerListSet update already uses).
4. Asks OLD (port 9088) to verify NEW's LA report + delegation quorum, encrypt state, and emit (ciphertext, ephemeral_pk, tag, la_report_old).
5. Asks NEW (port 9089) to verify la_report_old, decrypt state, run M3 sealed-file durability self-check, write the M4 manifest LAST, emit a completion LA report.
6. Asks OLD (port 9088) to verify the completion LA report → seal the retired-marker → flip in-memory retired flag.

On success, the ceremony driver returns 200 OK with the new MRENCLAVE recorded.

## Post-ceremony promotion

After confirmation succeeds OLD will return `PATH_A_ERR_ECALL_RETIRED` (-150) for every signing or state-mutating ecall. OLD's `/version` and read-only routes still respond — useful for confirming the retired state.

Promotion sequence:
1. **Stop OLD enclave**: `sudo systemctl stop perp-dex-enclave`.
2. **Reroute external traffic** to port 9089 (operator's reverse proxy / TLS terminator / DNS — depends on topology).
3. **Verify NEW is serving**: `curl -k https://localhost:9089/version` returns the new MRENCLAVE.
4. **Disable OLD unit** so a subsequent reboot doesn't accidentally restart it: `sudo systemctl disable perp-dex-enclave`.
5. **Optional cleanup** (operator's discretion): once the ceremony is verified across all operators and committed-to, the OLD `/home/azureuser/perp/accounts/` sealed state is forensic-only (NEW MRENCLAVE cannot unseal it). Either keep it as a backup for a few weeks or remove it after the cluster is stable on NEW.

## Failure modes and recovery

| Symptom | Cause | Recovery |
|---|---|---|
| `node-deploy --side-by-side` rejects: "OLD service not active" | OLD enclave service stopped or unhealthy | Restart OLD (`sudo systemctl start perp-dex-enclave`); investigate why it stopped |
| `node-deploy --side-by-side` rejects: "NEW unit not found" | Pre-requisite 4(a) skipped | Install the systemd unit per pre-requisites |
| `node-deploy --side-by-side` rejects: "perp-next/ not empty" | Half-completed prior ceremony attempt | `sudo rm -rf /home/azureuser/perp-next/*` then retry |
| `node-deploy --side-by-side` rejects: "port 9089 not available" | Stray process or earlier NEW enclave still running | `sudo systemctl stop perp-dex-enclave-next`; check `ss -ltn` for other listener |
| `node-deploy --side-by-side` rejects: "MRENCLAVE mismatch" | Build artefacts don't match build-manifest.txt | Investigate WHICH enclave is running before retry; do NOT proceed with mismatched build |
| Ceremony driver times out at confirmation step | NEW failed to seal one of the migrated files | OLD did NOT zeroise (it never received a confirmation report) — escrow keys remain accessible. Inspect NEW's enclave.log; if recoverable, restart NEW + retry the ceremony with a fresh `ceremony_nonce` (the OLD enclave's `recent_ceremony_nonces` set rejects re-use of the prior one) |
| Ceremony driver returns `ERR_NONCE_REPLAY` | Operator re-attempted with the same nonce | Generate a fresh nonce; retry |
| Ceremony driver returns `ERR_DELEGATION_QUORUM` | Insufficient operators signed the delegation | Coordinate with the missing operators; collect their signatures; retry |
| Ceremony succeeded on NEW but `verify_import_confirmation` returns durability error | NEW's confirmation arrived but OLD couldn't seal the retired-marker (disk full / FS bug) | OLD's in-memory state is **not** retired (per implementation contract); operator investigates disk health. Note: the ceremony nonce is consumed — any retry MUST use a fresh nonce, and OLD's in-memory migration state is still set, so ceremony driver must restart from the beginning with a new nonce after disk fix |

## Recent ceremony nonces — crash-window note (LR-IMPL-3)

The OLD enclave's `recent_ceremony_nonces.sealed` is updated AFTER successful encrypt + LA report production but BEFORE `verify_import_confirmation`. There is a **short crash-window** between encrypt-success and nonce-seal during which a crash + restart could leave the nonce un-recorded; if the orchestrator then attempts a fresh ceremony with a different nonce, that succeeds normally — the un-recorded nonce is forgotten, **but** the captured ciphertext from the first attempt cannot be replayed because it was bound to the first nonce, which NEW already accepted (and consumed in its own recent_nonces set).

In practice this window is microseconds (the `sgx_seal_data + ocall_save_to_file` pair). Operator action: **none required**. The defense-in-depth model holds — no replay path opens up.

If you observe an `ERR_NONCE_REPLAY` from a ceremony with a freshly-generated nonce (which would suggest something is wrong with the random source), capture both OLD's and NEW's `recent_ceremony_nonces.sealed` for forensics and contact `dev-perp` before continuing.

## Reference

- Specification: `docs/audit/REQ-7.md` (private repo `77ph/xrpl-perp-dex-enclave`)
- Implementation: `EthSignerEnclave/Enclave/path_a.cpp` (private)
- Orchestrator integration: `orchestrator/src/node_deploy.rs` + ceremony driver in commit 12
- Audit cycle: REQ-8 R1 verdict in audit channel; R2 verdict pending after commit 12
