# Path A migration ceremony — operator runbook (EN)

**Status:** production-grade. The ceremony has been executed on real SGX hardware (Azure DCsv3) across a 3-node cluster multiple times — the bundled HW E2E (2026-05-15), the populated-state re-run (2026-05-18), and the REQ-17 seal-policy cutover (2026-05-22). This runbook reflects what those runs taught. Reference spec: `docs/audit/REQ-7.md` (private repo).

---

## 1. What Path A is

A cluster-coordinated upgrade of the perp-dex enclave to a new MRENCLAVE **without losing customer state** (operator XRPL keys, FROST shares, perp positions, vault balances, account pool). Each operator runs the ceremony locally on their own machine; there is no cross-operator SSH.

**Path A delivers:**
- Funds in escrow stay accessible — XRPL signing keys survive the MRENCLAVE bump through the migration ceremony.
- Customer perp state survives — clients see no reset of balances or open trades.
- The quorum-of-operators governs whether an upgrade may proceed (delegation bundle).

**Path A does NOT cover:**
- Initial bootstrap (`node-deploy` without `--side-by-side`).
- Disaster recovery if an OLD enclave dies before its ceremony completes.
- Cross-host migration — Path A is platform-bound to the same physical SGX CPU.

**When you run it:** any MRENCLAVE bump that must preserve live state — a security release, the seal-policy re-key (REQ-17), or any enclave-code change deployed to a cluster already holding customer state.

---

## 2. Pre-requisites

1. **Reproducible build, MRENCLAVE agreed.** Each operator independently builds the NEW enclave from the agreed git ref via the GHA pipeline and confirms the resulting MRENCLAVE is bit-identical across operators (INV-BUILD-1). The build also runs the seal-policy gate (`check-seal-policy.sh`) — a build that fails the gate must not be deployed. **The git ref MUST be at or after REQ-16 + REQ-17** — older builds carry known migration defects (undersized host buffers; every-boot manifest re-verification). See §7.

2. **Quorum of operators agree** on the NEW MRENCLAVE, out-of-band (call / signed message), BEFORE anyone pre-stages the NEW unit. The ceremony later collects a **delegation quorum** of signatures — this quorum equals the escrow's XRPL SignerListSet quorum (**2-of-3** on the current 3-node cluster). Each operator preparing the ceremony must know how many co-signers to expect. The delegation bundle proves the agreement cryptographically; the pre-agreement is operator discipline.

3. **OLD enclave running and healthy** on port 9088, sealed state in `/home/<user>/perp/accounts/`. Verify: `systemctl is-active perp-dex-enclave` → `active`.

4. **Build artefacts staged** per operator: `enclave.signed.so`, `perp-dex-server`, `build-manifest.txt` with the expected MRENCLAVE pinned.

5. **One-time NEW-side machine setup** (first ceremony on a given machine only): install `perp-dex-enclave-next.service`, `systemctl daemon-reload`, create `/home/<user>/perp-next/`, pre-stage `perp-next/config.json` (port 9089). Permanent — reused every subsequent cycle.

---

## 3. The ceremony — PARALLEL across all nodes

> **§11.10 invariant — the ceremony MUST run in parallel on every node. Do NOT migrate one node to completion before starting the next.**
>
> Why: the ceremony's delegation-quorum step needs the operator quorum to sign approval, and an operator signs via their enclave's `/pool/sign` route — which a **retired** OLD refuses (RETIRED-gated). If you finish node-1 first, node-1's OLD retires and can no longer co-sign; once N-1 OLDs have retired, the last node's ceremony cannot reach delegation quorum. Every OLD must stay live until every node has passed its export+delegation phase. Run all nodes together. Recovery from a violation is in §7.1 — but prevention is the only tested guarantee.

**3.1 — Side-by-side deploy (every node, may be sequential — non-destructive).**
Each operator, on their own node:
```
perp-dex-orchestrator node-deploy --side-by-side --enclave-dist ./dist-new
```
Installs NEW artefacts to `perp-next/` and starts the NEW enclave on port 9089 alongside OLD (9088). NEW boots with empty sealed state + an ephemeral migration keypair generated inside SGX. Verify on each node: `curl -k https://localhost:9089/version` → NEW MRENCLAVE.

**3.2 — Fire the ceremony on ALL nodes concurrently.**
Each operator, on their own node, at the same time:
```
curl -sS -X POST http://127.0.0.1:7095/admin/migrate-state \
  -H 'Content-Type: application/json' \
  -d '{"expected_mrenclave_new":"<hex 64>"}'
```
(`--migrate-admin-listen 127.0.0.1:7095` must be set on the orchestrator; it is gated by `--signers-config`.)

Per node the driver: asks NEW for target_info + a fresh ceremony nonce → asks NEW to generate the ephemeral keypair → collects the delegation quorum (§2.2) over the libp2p signing-relay → asks OLD to verify NEW's LA report + delegation + encrypt state → asks NEW to verify OLD's LA report, decrypt, M3 durability self-check, write the M4 manifest LAST → asks OLD to verify the completion report and seal its retired-marker.

Each node returns `200 OK {"status":"ok", "ceremony_nonce_hex":…, "mrenclave_new_hex":…, "manifest_hash_hex":…}`. Distinct ceremony nonces per node are expected.

> **Point of no return:** the OLD enclave sealing its retired-marker — the final ceremony step. **Before** it, an abort is safe (OLD keeps serving, NEW is discarded). **After** it, OLD is retired and NEW has re-sealed state under the new MRENCLAVE — rollback to the OLD enclave is impossible (§4.6).

---

## 4. Post-migration verification

Run on every node before promotion. Do not promote on a partial result.

1. **NEW MRENCLAVE** — `curl -k https://localhost:9089/version` → expected NEW MRENCLAVE, all nodes.
2. **First-boot autoload** — restart NEW once; `enclave.log` shows `migration manifest verified — post-migration boot`, `sealed SignerList loaded`, `Auto-loaded ... account(s)`, `Auto-loaded perp state (chunked)`. The migration manifest is **consumed** on this first verify (rewritten `file_count=0`) so later boots after operational state changes do not trip the file-hash check.
3. **State survival** — probe a known user balance and a vault: `curl -k --get --data-urlencode "user_id=<r…>" https://localhost:9089/v1/perp/balance` and `…/v1/perp/vault/status?type=1`. Compare against pre-migration values.
4. **Sealed-file completeness + policy** — every sealed file in NEW's `accounts/` must be present (the expected per-shard set) and MRENCLAVE-policy. The `key_policy` is a uint16 LE at byte offset 2 of every `sgx_sealed_data_t` — `0x0001` = MRENCLAVE, `0x0002` = MRSIGNER. Audit every file, not a sample.
5. **OLD retired** — OLD returns `PATH_A_ERR_ECALL_RETIRED` (-150) for signing/state-mutating ecalls; `path_a_retired.sealed` present in OLD's `accounts/`; read-only routes (`/version`) still respond.
6. **Irreversibility** — the OLD enclave (different MRENCLAVE) can no longer unseal NEW's re-sealed files. This is the intended end-state: rollback to a pre-NEW enclave is impossible once NEW has re-sealed. Confirm it is understood as a conscious operational checkpoint.

**If any §4 check fails — see §7.2. Do not promote, do not decommission OLD.**

---

## 5. Promotion

Promote NEW into the canonical `:9088` slot so the orchestrator (which targets `--enclave-url …:9088`) drives NEW. Per node:

1. `sudo systemctl stop perp-dex-enclave-next perp-dex-enclave`.
2. Install NEW into `perp/`: replace `perp/enclave.signed.so` + `perp/perp-dex-server` with the NEW build. **Move OLD's sealed state aside — do not overwrite it:** `mv perp/accounts perp/accounts.OLD-<old-mrenclave>`. Then install NEW's: `cp -r perp-next/accounts perp/accounts`. The preserved `accounts.OLD-<old-mrenclave>` is the forensic copy until the §6 decommission.
3. Clear `perp-next/accounts/` (empty) — ready for the next migration cycle.
4. `sudo systemctl start perp-dex-enclave` → `:9088` now boots NEW. Leave `perp-dex-enclave-next` stopped.
5. Verify `:9088/version` = NEW MRENCLAVE; autoload chain green; the orchestrator reconnects.

---

## 6. OLD decommission — MANDATORY

Decommissioning the OLD enclave is **not optional and not "keep a backup for a few weeks."** `accounts.OLD-…` is a stale copy of customer state; an enclave binary for OLD's MRENCLAVE is reproducible from source, so OLD's sealed state is not cryptographically guaranteed inert. Stale sealed-state copies are a residue and an exposure surface — this is a direct lesson of the MRSIGNER seal-policy incident (INCIDENT-2026-05-20).

Per node, **after §4 verification passed and promotion is verified**:
1. OLD's process is already stopped (§5.1). Disable the OLD unit if it were ever separately defined.
2. **Scrub** OLD's sealed state: delete the `perp/accounts.OLD-<old-mrenclave>` directory preserved in §5.2, plus any archive/snapshot directories from prior cycles. Keep the audit record in `docs/` + memory, not as sealed blobs on disk.
3. If the migration retired an on-chain escrow as part of the change, check the old escrow's on-chain balance before scrubbing its key material — confirm nothing of value is stranded.
4. Re-scan: confirm only the active enclave's `accounts/` remains, all sealed files the expected policy, zero stale dirs.

Do not co-mingle other applications' sealed state into this scrub — only perp-dex directories.

---

## 7. Failure modes and recovery

| Symptom | Cause | Recovery |
|---|---|---|
| `node-deploy --side-by-side` rejects "OLD service not active" | OLD enclave stopped/unhealthy | Restart OLD; investigate why it stopped |
| rejects "NEW unit not found" | One-time setup (§2.5) skipped | Install the systemd unit |
| rejects "perp-next/ not empty" | Half-completed prior attempt | Clear `perp-next/accounts/`, retry |
| rejects "MRENCLAVE mismatch" | Build artefacts ≠ build-manifest | Find which enclave is running before retry; never proceed on a mismatch |
| ceremony returns `ERR_DELEGATION_QUORUM` | Too few operators signed; OR a node was migrated to completion first and its OLD retired (§3 violation) | Collect the missing signatures; if an OLD was retired early → §7.1 |
| ceremony returns `ERR_NONCE_REPLAY` | Same nonce re-used | A fresh nonce is generated per call; if this fires on a genuinely fresh nonce, suspect the RNG — capture both nodes' `recent_nonces.sealed` and contact dev-perp |
| ceremony times out at confirmation | NEW failed to seal a migrated file | OLD did NOT retire (no confirmation received — abort is still safe, §3 point-of-no-return). Inspect NEW's `enclave.log`; restart NEW + retry with a fresh nonce |
| post-migration restart: enclave refuses to start, `PATH_A_ERR_FILE_HASH_MISMATCH` (-17) | Pre-fix builds re-verified the migration manifest every boot; an operational state change then broke the file-hash check | Current builds (≥ REQ-16) **consume** the manifest after the first successful verify — this does not occur. If seen on a current build, the manifest write-back failed: inspect `enclave.log`; the manifest can be manually removed (the enclave then treats it as pre-migration state) |
| export fails `PATH_A_ERR_BUFFER_TOO_SMALL` (-2) | Pre-REQ-16 host buffer too small for populated state | Fixed in current builds (4 MB export buffer, 16 MB transport buffer). Rebuild from a ref ≥ REQ-16 |
| partial cluster migration — some nodes on NEW, some on OLD | A node's ceremony failed mid-run | The cluster tolerates the split transiently. Re-run the ceremony on the failed node **while the other OLDs are still alive** (do not promote/decommission until all nodes are on NEW) |

### 7.1 Stuck node — a node cannot reach delegation quorum (§3 violated)

A node is "stuck" if its ceremony fails `ERR_DELEGATION_QUORUM` because N-1 OLDs have already retired. The retired OLDs cannot co-sign — `/pool/sign` is RETIRED-gated.

**Recovery (best-effort, not tested in production):**
1. A **promoted** NEW enclave is not retired and holds the operator's migrated pool key — it *can* serve a `/pool/sign` delegation request. So: fully **promote** every already-migrated node (§5) so its orchestrator drives NEW on `:9088`.
2. Re-run the stuck node's ceremony. Its delegation collection now reaches the promoted NEW enclaves over the libp2p relay; their signatures verify against the SignerList (operator identities are unchanged across migration).
3. If quorum is still unreachable, the stuck node has **no in-place recovery**: it must be re-bootstrapped as a fresh operator (full bootstrap — SignerListSet + DKG; see the bootstrap procedure), then re-joined.

This recovery path has **not been exercised on hardware**. The only tested guarantee is the §3 parallel discipline — treat §3 as **mandatory, not advisory**. Prevention is the recovery.

### 7.2 §4 verification fails after the ceremony returned 200 OK

By the time §4 runs, the ceremony has completed: OLD is retired and NEW has re-sealed state (the §3 point of no return is passed — rollback is impossible). If a §4 check fails (state survival, completeness, policy):

1. **Stop.** Do not promote (§5). Do **not** run the §6 OLD decommission — OLD's `accounts/` (still in `perp/accounts/` at this point, not yet moved) is the only remaining copy of pre-migration state and is forensic evidence.
2. Leave both enclaves as they are. OLD is retired (won't sign) but its sealed state is intact and readable by an OLD-MRENCLAVE enclave.
3. Escalate to dev-perp with: the failing §4 check, NEW's `enclave.log`, the ceremony's `manifest_hash_hex`, and both nodes' `accounts/` listings. Recovery from here is case-specific and is not a routine operator procedure.

---

## 8. Recent ceremony nonces — crash-window note (LR-IMPL-3)

OLD's `recent_nonces.sealed` is updated AFTER successful encrypt + LA-report production but BEFORE `verify_import_confirmation`. There is a microsecond crash-window between encrypt-success and nonce-seal; a crash there leaves the nonce un-recorded, but the captured ciphertext cannot be replayed — it is bound to a nonce NEW already consumed. Operator action: none. If `ERR_NONCE_REPLAY` ever fires on a freshly-generated nonce, capture both nodes' `recent_nonces.sealed` and contact dev-perp.

---

## 9. Reference

- Specification: `docs/audit/REQ-7.md`, REQ-7.5, REQ-8 (private repo `77ph/xrpl-perp-dex-enclave`).
- Cluster ordering invariant: `docs/deployment-procedure.md §11.10`.
- Seal-policy incident + cutover: `docs/audit/INCIDENT-2026-05-20-mrsigner-seal-policy.md`, REQ-16, REQ-17 (private).
- Implementation: `EthSignerEnclave/Enclave/path_a.cpp`; `orchestrator/src/node_deploy.rs`, `path_a_migrate_admin.rs`, `path_a_delegation.rs`.
- Project invariants: `docs/audit/PROJECT-INVARIANTS.md`.
