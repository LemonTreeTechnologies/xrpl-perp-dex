# Production Deployment Procedure — 3 Independent Operators

**Status:** Draft for review. Not yet implemented. Document captures the target architecture for production deployment when the FROST 2-of-3 threshold signing setup is operated by three independent persons (working assumption: Andrey, Alex, Tom), each controlling their own SGX node, with no operator having access to anyone else's server.

**Russian version:** `deployment-procedure-ru.md`. This document must be kept in sync with the Russian version per the bilingual docs policy.

---

## 1. Threat model

What we are protecting against, in priority order:

1. **Single rogue operator.** One of the three operators tries to push a malicious binary to their own node and uses it to extract keys, sign unauthorised XRPL transactions, or steal user funds. The 2-of-3 FROST scheme already mitigates the *signing* side of this — one rogue node cannot sign alone — but the *deployment* side must enforce the same property: one rogue operator cannot unilaterally change what runs on the network.

2. **Compromised operator workstation.** An operator's laptop is compromised (malware, stolen credentials, evil maid). The attacker can SSH to that operator's node but should not be able to push a release the other two operators have not approved.

3. **Compromised build environment.** The build machine (or the CI environment) is compromised and produces a binary that does not match the source. This is the SolarWinds scenario.

4. **Compromised hosting provider.** Hetzner / Azure compromises a single VM (or is legally compelled to). The other two nodes must continue safely; the compromised node's attestation should fail and be excluded from the FROST quorum.

5. **Coercion of a single operator.** An operator is forced (legal, physical, social) to deploy a specific binary. The 2-of-3 approval requirement ensures this single operator cannot succeed alone.

**Out of scope for this document:**
- Compromise of two or more operators simultaneously (the protocol's 2-of-3 trust assumption is broken — no deployment process can save you).
- Hardware attacks on SGX itself (separate threat model, covered by attestation policy).
- Initial DKG ceremony (separate document — this doc covers updates to *already-running* nodes).

## 2. Core principles

The deployment system must satisfy all of:

1. **No cross-server access.** Operator A has SSH/sudo only on Server A. Operator B only on Server B. Operator C only on Server C. There is no shared "deploy bot" account, no shared SSH key, no operator with access to all three nodes.

2. **2-of-3 release approval is mandatory.** No binary is deployed to *any* node until at least two of three operators have independently approved it by signing its hash with their hardware key. This must be enforced *on each node*, by the node itself, before swapping the binary — not by a central orchestrator (which would itself be a single point of compromise).

3. **Reproducible builds.** Two operators building the same source commit on different machines must produce byte-identical binaries. Without this, "I signed hash X" and "you signed hash Y" become incomparable and the whole approval scheme collapses.

4. **Hardware-key gated approval.** Approval signatures must come from a hardware key the operator physically possesses (YubiKey, NitroKey, or equivalent). A signature from a software key on a laptop fails principle 2 — it can be exfiltrated.

5. **Auditable history.** Every release, every approval, every deployment is recorded in an append-only log that all three operators can independently verify. No silent rollbacks, no unrecorded hotfixes.

6. **Automation up to but not including the human approval step.** Building, hashing, fetching, verifying signatures, swapping binaries, restarting — all automated. The signing of the release with a hardware key is the *only* manual human action and it stays manual on purpose.

## 3. Components

### 3.1 Reproducible build

We need byte-identical binaries from the same source on different machines. Current state of the repo:

- `rust-toolchain.toml` already pins Rust 1.88.0 ✅
- `Cargo.lock` is checked in ✅
- Build environment is *not* deterministic — depends on system libs, glibc version, build path. ❌

**To fix before this scheme is viable:**
- Build inside a fixed container image (specific Debian/Ubuntu version, pinned by SHA256 digest, not by tag).
- Strip build paths (`--remap-path-prefix`), strip timestamps, pin `SOURCE_DATE_EPOCH`.
- Verify reproducibility: two operators build commit `X` on different machines, hashes must match exactly. Add this as a CI gate.
- For the SGX enclave: MRENCLAVE must be reproducible. SGX SDK supports this if signed with a known key in deterministic mode. Worth a separate validation pass.

**Acceptance criterion:** all three operators can independently produce identical `sha256(perp-dex-orchestrator)` and identical `MRENCLAVE` from the same git commit, without coordinating.

### 3.2 Hardware keys

Recommended: **YubiKey 5 series** (PIV or OpenPGP slot), one per operator, two physical units per operator (primary + backup stored in a separate physical location).

Why YubiKey:
- Mature, audited, widely deployed.
- Both PIV (X.509 + ECDSA) and OpenPGP modes supported.
- Touch-to-sign requirement prevents silent malware-driven signing.
- Works with SSH (ssh-agent), Git (commit signing), and arbitrary file signing (gpg, age, minisign).

Each operator generates their signing keypair *on the YubiKey itself* — the private key never exists outside the secure element. Public keys are committed to the repo at `deploy/operators/<operator>.pub`. This file is itself signed by all three at first setup, so the public key list cannot be silently swapped.

**Backup key:** every operator has a second YubiKey with the *same* signing key (provisioned at the same time, at the same air-gapped ceremony) stored in a different physical location. If primary is lost, backup is activated. Losing both = operator triggers the rotation procedure (section 7).

### 3.3 Release manifest

A release manifest is a small signed file that says:

```
release_version: 1.4.2
git_commit: abcdef1234...
orchestrator_sha256: 9f8e7d...
enclave_mrenclave: 1234abcd...
enclave_mrsigner: 5678ef01...
build_timestamp: 2026-04-15T10:30:00Z
notes: "fix vAMM rebate accounting (issue #142)"
```

This manifest is what the operators sign with their hardware keys. The signatures (one file per operator) are stored alongside the manifest:

```
releases/1.4.2/manifest.toml
releases/1.4.2/signatures/andrey.sig
releases/1.4.2/signatures/alex.sig
releases/1.4.2/signatures/tom.sig
```

The release manifest + signatures live in a **dedicated public repository** (e.g. `LemonTreeTechnologies/xrpl-perp-dex-releases`). It is *separate* from the source repo so that release approval cannot be silently mixed with code commits, and so the release repo can have stricter branch protection (no force-push, signed commits required).

### 3.4 Per-node deploy agent

On each operator's server runs a small **deploy agent** — a tiny Rust or Go binary, written once, audited carefully, and changed almost never. Its job:

1. Periodically poll the release repo for new manifests.
2. When a new manifest appears, fetch it.
3. Verify there are at least 2 valid signatures from the known operator public keys.
4. Fetch the binary from a content-addressed location (the URL contains the sha256).
5. Verify the binary's sha256 matches the manifest.
6. Verify the SGX enclave's MRENCLAVE matches the manifest.
7. Stop the running orchestrator gracefully (drain orders, finish in-flight settlement).
8. Swap the binary atomically.
9. Restart and verify the new instance comes up healthy via the local health endpoint.
10. Record the deployment in the local audit log and report success/failure to the release repo (signed with the node's own attestation key, not the operator key).

The deploy agent has no operator interaction during normal deployment. The operator's only action was to sign the manifest hours or days earlier.

**The deploy agent itself must be deployed differently** — its own updates are rare, manually deployed by each operator on their own node, after a face-to-face or video call review. This is the trusted base.

### 3.5 Attestation cross-check

After deploy, each node verifies its peers via DCAP attestation before re-joining the FROST signing quorum. If node A comes up with an unexpected MRENCLAVE, nodes B and C will refuse to perform signing rounds with A. This is the second line of defence: even if the deploy gate is somehow bypassed on one node, the cluster will exclude it.

This requires the post-hackathon work on `feedback_dcap_subprocess_pattern.md` and `feedback_dcap_azure_two_bugs.md` to be production-stable, and remote attestation working on whichever cloud provider is chosen for production.

## 4. One-time setup procedure

Done once, in person if possible (or via secure video with screen sharing for the witness role).

### Step 1 — Hardware key provisioning ceremony

All three operators in one room (or on a recorded video call). Each operator:

1. Unboxes two new YubiKeys (primary + backup) from sealed packaging, witnesses verify packaging integrity.
2. Generates a fresh PIV/OpenPGP keypair on the primary YubiKey. Witnesses confirm the key never leaves the device.
3. Clones the same key material to the backup YubiKey (only opportunity — after this both devices are sealed).
4. Exports the public key to a USB stick.
5. Stores the backup YubiKey in a tamper-evident envelope, signed by all three, taken to a separate physical location after the ceremony.

The three public keys are committed to the source repo at `deploy/operators/{andrey,alex,tom}.pub` in a single commit, signed by all three operator keys. This commit is the **trust anchor** — every later release verification chains back to it.

### Step 2 — Reproducible build verification

All three operators independently:
1. Clone the source repo at the chosen commit.
2. Build using the canonical container image.
3. Compute sha256 of the orchestrator binary and the enclave MRENCLAVE.
4. Compare results with each other.

If hashes do not match, reproducibility is broken — must be fixed before proceeding. This is a hard gate.

### Step 3 — Deploy agent installation

Each operator, on their own server, manually:
1. Installs the deploy agent binary (built from the same reproducible build pipeline, hash verified).
2. Configures the agent with the URL of the release repo and the path to the operator pubkey directory.
3. Starts the agent under systemd with a restricted user.
4. Verifies it polls the release repo and idles correctly when no new manifest is present.

### Step 4 — End-to-end dry run

Push a no-op release (e.g. version `0.9.9-test`) through the full pipeline:
1. Build, compute hash, write manifest.
2. Each operator signs manifest with their YubiKey, commits signature to release repo.
3. After 2-of-3 signatures land, watch each deploy agent independently fetch, verify, and (in dry-run mode) report what it *would* deploy.
4. Do a real swap on a staging instance (not production) to verify graceful drain and restart.

Only after a successful dry run is the system considered ready for real releases.

## 5. Per-release procedure (the steady state)

This is the procedure you run every time you want to push a new version to production.

### Stage A — Build and propose

Done by **whichever operator is making the change** (could be any of the three; not a privileged role).

1. Push the change as a normal PR to the source repo. Get reviewed and merged to `main` as usual.
2. Tag the release: `git tag -s v1.4.2` (signed git tag, with the operator's hardware key).
3. Run the canonical build pipeline. Output: orchestrator binary, enclave .so, MRENCLAVE.
4. Generate the release manifest (`manifest.toml`) with the hashes.
5. Upload the binary + enclave to the content-addressed storage (the URL is `https://releases.example/{sha256}`, so the URL itself is verifiable).
6. Open a PR to the release repo adding `releases/1.4.2/manifest.toml`. This PR must NOT contain any signatures yet — those land separately, see Stage B.

### Stage B — Independent verification and signing

Each of the other two operators (and ideally the proposer too, for a full 3-of-3 in normal cases):

1. Pulls the manifest PR.
2. Checks out the source at the commit referenced by the manifest.
3. **Independently builds** the binary on their own machine. Reproducible build means their hash must match the manifest's hash exactly.
4. Verifies the SGX enclave MRENCLAVE matches.
5. Reads the actual diff vs the previous release. Asks questions if anything is unclear.
6. If satisfied, signs the manifest:
   - YubiKey is plugged in.
   - `signify -S -s yubikey:slot -m manifest.toml -x signatures/<operator>.sig` (or equivalent for chosen tool).
   - YubiKey blinks, operator touches it to confirm.
7. Commits the signature file to the release repo PR.

Once 2-of-3 signatures are present, the manifest PR is merged.

**Critical:** the verification step (3) is non-negotiable. An operator who signs without independently building is a single-operator-trust attack vector. The whole scheme rests on at least two operators independently confirming the binary matches the source.

### Stage C — Automated rollout

After the manifest merges to the release repo `main` branch, no human action is needed. Each deploy agent on each server will:

1. Detect the new manifest within its poll interval (e.g. 60s).
2. Verify signatures locally (offline, against the trust anchor pubkeys baked in at setup).
3. Fetch the binary and enclave from content-addressed storage.
4. Verify hashes.
5. Drain → swap → restart → health check.
6. Cross-attest with the other nodes. Once all three nodes report healthy attestation, signing quorum resumes.

**Rolling vs synchronised:** prefer **rolling** (one node at a time, with a 5-minute soak between) so that if a release breaks, you find out on node A while B and C are still on the old version and the cluster keeps signing. Implement this with a manifest field `rollout_order` that encodes which node deploys first / second / third, and have the agents wait their turn by watching the previous node's deployment receipt in the release repo.

### Stage D — Post-deploy verification

Each operator independently checks their own node's health (logs, metrics, attestation status). If any node reports failure, the **rollback** procedure (section 6) kicks in.

## 6. Rollback and incident response

### Automatic rollback

The deploy agent keeps the previous binary on disk. If after swap:
- Health check fails for N consecutive seconds, OR
- Cross-attestation with peers fails, OR
- The orchestrator panics on startup,

the agent automatically reverts to the previous binary, restarts, and posts a "rollback" receipt to the release repo (signed with the node's attestation key). The other two nodes see this and know one peer is on the old version.

### Manual rollback

If a release deploys successfully but reveals a bug in production (e.g. wrong fee accounting visible only after real trades), operators can deploy an older release:

1. Open a new manifest PR pointing at an earlier version's hashes (no rebuild needed, hashes already known).
2. 2-of-3 sign as normal.
3. Rollout proceeds.

This treats a rollback as just another release. There is no special "emergency rollback" path — the whole point of 2-of-3 is that no single operator has emergency override.

### Lost hardware key

Operator loses primary YubiKey. They activate the backup (stored at the separate physical location). No protocol change needed.

If operator loses *both* primary and backup, they cannot sign releases. The other two operators can still push releases (2-of-3 still satisfied), and the operator-rotation procedure (section 7) replaces the missing key.

### Coerced operator

If an operator is being coerced to sign a malicious release, they should refuse. The other two operators cannot be coerced simultaneously (out of scope per threat model). The coerced operator alone cannot push the release. If they reveal the coercion to the others, the others simply do not sign, and the malicious release goes nowhere.

A panic-button / duress code is *not* recommended at this layer — it's reasonable at the operator's local key unlock layer (PIV PIN with separate "wipe PIN") but adds complexity at the manifest layer with little benefit, since the 2-of-3 requirement already protects the protocol.

## 7. Operator rotation

Adding or removing an operator (e.g. Tom hands off to a new person, or one of the three steps away):

1. The replacement operator goes through the hardware key provisioning ceremony (section 4 step 1) with the remaining two operators present.
2. The remaining two operators sign a `deploy/operators/CHANGES` manifest entry adding the new pubkey and removing the old.
3. This change is itself a release manifest, deployed through the normal pipeline (2-of-3 sign with the *current* operator set, including the departing one if available).
4. After deployment, the deploy agents on all three nodes update their trust-anchor pubkey set.
5. From this point on, the new operator's signature is recognised and the old operator's is not.

If the departing operator is uncooperative or compromised, the remaining two can still execute the rotation (2-of-3 satisfied without them), provided the rotation manifest itself is treated as a normal release.

## 8. What automation we can and cannot have

| Step | Automatable? | Why |
|---|---|---|
| Source build | Yes | CI runs reproducible build pipeline |
| Manifest generation | Yes | Hash computation is deterministic |
| Signature collection | No | Hardware key + human touch is the trust gate |
| Manifest merge to release repo | Partially | Auto-merge once 2-of-3 signatures present |
| Deploy agent fetching new manifest | Yes | Polling |
| Signature verification on the node | Yes | Crypto, no human input |
| Binary hash verification | Yes | Crypto |
| Drain / swap / restart | Yes | Standard process management |
| Rollout ordering | Yes | Encoded in manifest |
| Cross-attestation | Yes | DCAP mechanics |
| Rollback on health failure | Yes | Local agent decision |
| Rollback on production bug | No | Requires human judgment + 2-of-3 signing |

The only step that stays manual is the human signing with the hardware key. That's the point — it's the gate that prevents the rest of the automation from being trusted unilaterally.

## 9. Open decisions to resolve before implementation

These are the things we don't yet have a strong opinion on and need to discuss:

1. **Signing tool.** `signify` (OpenBSD, very simple) vs `minisign` (signify-compatible, more widely packaged) vs `gpg` (full-featured but complex) vs `cosign` (sigstore ecosystem, integrates with OCI registries). Recommend **minisign** for simplicity, with YubiKey holding the private key via the OpenPGP slot bridge, OR **cosign** if we end up shipping container images anyway.

2. **Release repo hosting.** GitHub (convenient but SPOF), self-hosted Gitea (more control), or a non-git append-only log (sigstore Rekor, or a simple signed-append-only-file on each operator's server with gossip). Recommend **GitHub** with branch protection for now, plus a mirror to a self-hosted instance for resilience.

3. **Content-addressed binary storage.** S3-compatible bucket with public read (cheap, simple, but the hosting provider sees what you serve) vs IPFS (decentralised but operationally heavier) vs a small file server on each operator's node, with cross-replication. Recommend **S3-compatible bucket** (e.g. Cloudflare R2) for simplicity — content-addressing means storage doesn't need to be trusted.

4. **Container vs bare-metal deploy.** Right now Hetzner runs the orchestrator under nohup. Production should at minimum run under systemd, ideally inside a container with a tight seccomp/apparmor profile. The deploy agent procedure assumes a binary swap; container swap is also fine, choose one and stick with it.

5. **Reproducibility validation cadence.** How often do we run "all three operators rebuild the latest release and confirm hashes match" as an audit? Every release? Monthly? Recommend **every release**, since it's the literal precondition of the scheme.

6. **Deploy agent code review.** Since it's the most security-critical piece (it's what enforces the 2-of-3 gate on each node), it must be small, audited, and itself reproducibly built. Who writes it, who reviews it, when?

7. **Initial seed of trust.** The operator pubkeys committed at setup are trusted because all three operators were physically present. How do we prove this to *ourselves* a year later? Recommend a signed video recording of the ceremony, hashes posted publicly.

## 10. Estimated implementation order

Working from "we have a lot of time" — not a hackathon scramble:

1. **Reproducible builds** first. Without this, nothing else works. This is mostly engineering, no crypto.
2. **Hardware key tooling** — pick the signing tool, write the wrapper scripts, do a paper exercise of the signing flow.
3. **Release repo + manifest format** — define the file format, the trust anchor commit, the verification logic.
4. **Deploy agent v1** — small, focused, audited carefully.
5. **Single-node dry run** — prove the agent works on one machine before involving the others.
6. **Three-operator dry run** — full ceremony, full pipeline, no production traffic.
7. **Cross-attestation integration** — wire the existing DCAP work into the deploy agent's startup checks.
8. **Production rollout plan** — migrate from current `nohup`-based deploy to the new pipeline.
9. **Rotation drill** — practice removing and re-adding an operator before you ever need to do it for real.

At each step, the previous steps are running in the background and getting battle-tested.

## 11. Mainnet hotfix runbook

This section is the concrete step-by-step procedure an on-call operator runs when a new release has been signed 2-of-3 (section 5) and must go live on a cluster that is already holding real user funds on XRPL mainnet. Everything earlier in this document is about *how we decide what to deploy*; this section is about *how we deploy it without losing state or funds*.

### 11.1 Invariant: MRSIGNER sealing is not available

Every sealed blob on every production node (FROST key share, tx dedup table, recovery artefacts) is sealed to the **MRENCLAVE** of the enclave that wrote it. Binding is deliberate: MRSIGNER sealing was evaluated and rejected — see `deployment-dilemma.en.md` §"Strategy 2: MRSIGNER-based Sealing — REJECTED" for the full argument (attestation verifiers check MRENCLAVE not MRSIGNER, one-way door, compromised build pipeline → full key extraction).

The operational consequence of that invariant:

> **Any change that produces a new MRENCLAVE cannot unseal old state.** There is no file-swap upgrade path for the enclave. Every such change is an on-chain key rotation via `SignerListSet`, not a binary swap.

This is the single most important thing to internalise before running this runbook. If you catch yourself thinking "let's just copy the new `.signed.so` in place", stop and re-read this section.

### 11.2 Decision tree: which path am I on?

Before touching anything in production, classify the hotfix:

```
Did the release change any of:
  - Enclave/*.cpp, Enclave/*.h, Enclave/*.edl
  - PerpState.h or any struct persisted inside the enclave
  - Enclave/Enclave.config.xml (TCS, heap, stack, debug flag)
  - SGX SDK version or build flags that affect the enclave
  - The enclave signing key
?

No  → Path A: orchestrator-only hotfix
Yes → Path B: enclave change (new MRENCLAVE)
```

Fast sanity check: `sha256sum enclave.signed.so` on the release artefact vs on the currently running node. If the hashes match it is definitively Path A. If they differ it is Path B, full stop — there is no "tiny enclave change" exception.

### 11.3 Pre-flight (both paths)

Run through this list before touching any production node. If any item is missing, abort and fix it first — there is no "we'll deal with it during the window".

1. **Release manifest has 2-of-3 valid signatures** (section 5). Verify each signature locally against the trust-anchor pubkeys.
2. **Reproducible build hash matches** — you have independently built from the tagged commit and your `sha256` matches the manifest.
3. **Previous good manifest identified** — write down its version and commit hash. This is the rollback target; do not hunt for it under pressure.
4. **Testnet soak completed** — the exact same build has been running on the testnet cluster for ≥24h with healthy FROST rounds.
5. **Cluster state snapshot taken:**
   - Current MRENCLAVE on each node (from the local attestation endpoint).
   - Current XRPL account sequence for the escrow account.
   - Current signer list on chain (addresses + quorum).
   - Escrow XRP balance and any open positions.
   - FROST health: last successful quorum round timestamp.
6. **No in-flight withdrawals** — check the orchestrator's pending-tx table. If non-empty, either wait for them to confirm or abort.
7. **No unconfirmed deposits younger than 20 ledgers** — anything mid-validation should settle before you start.
8. **All three operators reachable** in a shared channel for the duration of the window. A path-B rotation requires live 2-of-3 FROST rounds mid-procedure.
9. **Rollback release is already built and hash-verified** on each node's disk as `.prev`. The rollback must not depend on a rebuild.

### 11.4 Path A — Orchestrator-only hotfix

Enclave keeps running throughout. Only the orchestrator binary is swapped. Sealed data is untouched. Keys are not rotated. No on-chain action.

Order of nodes: **Hetzner first** (canary — it is the node that feels "most ours"), then Azure node-1, node-2, node-3, with a 5-minute soak between each.

Per-node steps:

1. Confirm `sha256sum enclave.signed.so` on this node matches the production manifest. If not, this is not Path A — stop.
2. Stop the orchestrator:
   ```
   sudo systemctl stop perp-dex-orchestrator-prod
   ```
3. Verify the enclave host process on port 9088 is still alive (`pgrep -f enclave_signed`). If the enclave died with the orchestrator, something is wrong — abort before continuing.
4. Move the new binary into place, keeping the previous one as `.prev`:
   ```
   sudo mv /opt/perp-dex/bin/orchestrator /opt/perp-dex/bin/orchestrator.prev
   sudo install -m 755 /tmp/orchestrator-new /opt/perp-dex/bin/orchestrator
   ```
5. Start the orchestrator:
   ```
   sudo systemctl start perp-dex-orchestrator-prod
   ```
6. Health check must return OK continuously for 60s:
   ```
   for i in $(seq 1 60); do curl -fsS http://localhost:3003/v1/health || exit 1; sleep 1; done
   ```
7. Run a **dry-run FROST round** against the other two peers (no XRPL submission — the orchestrator has a diagnostic endpoint for this). Round must complete within 10 s.
8. Soak 5 minutes. Watch `journalctl -u perp-dex-orchestrator-prod -f` for errors.
9. If clean: move to the next node. If anything flaps: see §11.6 rollback.

Total wall-clock: ~25 minutes for four nodes (5 min per node + 5 min per inter-node soak).

### 11.5 Path B — Enclave change (new MRENCLAVE)

This is the full pre-planned key-rotation ceremony. Do not skip steps. Read `deployment-dilemma.en.md` §"Strategy 4: Pre-planned Key Rotation (recommended)" end-to-end before starting; the attack-surface analysis (vectors 1–6) is the justification for why each step exists.

Order of nodes: **Hetzner first** (canary), then Azure node-1, node-2, node-3. Never more than one node in the rotating state at a time. The cluster must always have ≥2 nodes on a consistent MRENCLAVE to keep signing.

Per-node steps:

1. **Start the new enclave on a staging port** (9089), leaving the old enclave on 9088 running and serving live traffic.
   ```
   sudo systemctl start perp-dex-enclave-prod-new
   ```
   The new enclave comes up with empty sealed state (no FROST share yet, no tx dedup history).

2. **New enclave generates a fresh keypair internally** and produces a DCAP attestation quote binding `new_MRENCLAVE` to the new XRPL address. Ecall: `ecall_generate_key_with_attestation`.

3. **Peers verify the attestation quote.** The orchestrator on the rotating node sends the quote to the orchestrators of the other two nodes, which forward it to their (old) enclaves. Each peer enclave verifies:
   - The quote chains to Intel's root.
   - The MRENCLAVE in the quote matches the `enclave_mrenclave` field in the currently merged release manifest.
   - The public key in the quote's report data matches what the orchestrator proposed.
   If any peer rejects, abort — the new enclave is lying about its identity. Stop the new enclave, do not delete old data, escalate.

4. **Co-sign SignerListSet #1 — add new signer, raise quorum to 3.**
   - Transaction adds the new address to the escrow account's signer list.
   - Quorum is simultaneously raised from 2 to **3** for the duration of the rotation window (defense against vector 1 from deployment-dilemma §Attack surface analysis).
   - `LastLedgerSequence` is set to `current_ledger + 20` (~60 s validation window).
   - 2-of-3 existing (old) enclaves co-sign.
   - If not confirmed within `LastLedgerSequence`: no key was added, state is unchanged, abort this node's rotation and escalate.

5. **Co-sign SignerListSet #2 — remove old signer, lower quorum to 2.**
   - Transaction removes the old address, keeping the list at 3 signers (the two unrotated peers + this node's new key).
   - Quorum returns from 3 to 2.
   - `Sequence = (tx #1 Sequence) + 1` — XRPL's per-account sequence enforces ordering (vector 4 defense).
   - Retry with exponential backoff up to 10 minutes total.
   - **If tx #2 has not confirmed after 10 minutes:** submit emergency SignerListSet #3 which removes the *new* key (returning to the original 3-signer list). Better to abort the upgrade than leave a 4-key window open indefinitely (vector 3 defense).

6. **Verify on-chain state.** Pull the signer list from XRPL and confirm: exactly 3 entries, quorum 2, new key present, old key absent.

7. **Stop the old enclave.**
   ```
   sudo systemctl stop perp-dex-enclave-prod-old
   ```
   At this point the new enclave on 9089 is the only enclave on this node, but it has not yet been promoted to the production port/service name.

8. **Soak before shredding (golden rule).** Run one real FROST round on mainnet (a no-op such as a trivial escrow memo payment of 1 drop back to the escrow itself, or a ping-style liveness tx agreed in advance). Wait for the tx to confirm on-chain. Confirm the new enclave signed correctly. This is the last safe window — do not skip.

9. **Shred old sealed data.**
   ```
   sudo shred -u /var/lib/perp-dex/prod/frost_share.sealed
   sudo shred -u /var/lib/perp-dex/prod/tx_dedup.sealed
   sudo shred -u /var/lib/perp-dex/prod/nonce_ctr.sealed
   sudo rm -f /opt/perp-dex/bin/enclave.signed.so.old
   ```
   (Sealed to old MRENCLAVE; unrecoverable even in theory, but shredded anyway to remove the tempting fiction of "we can roll back by restoring the old files".)

10. **Promote the new enclave.** Point the production systemd unit at the new binary and port; restart under the production service name.
    ```
    sudo systemctl stop perp-dex-enclave-prod-new
    sudo mv /opt/perp-dex/bin/enclave.signed.so.new /opt/perp-dex/bin/enclave.signed.so
    sudo systemctl start perp-dex-enclave-prod
    ```

11. **Cross-attest with peers.** Each of the other two nodes runs its attestation check against this node and must accept the new MRENCLAVE. If any peer rejects, this node is out of the quorum — see §11.7.

12. **Operational soak — 10 minutes minimum.** Watch for:
    - Healthy FROST rounds involving this node.
    - No unexpected errors in `journalctl -u perp-dex-enclave-prod -u perp-dex-orchestrator-prod`.
    - Attestation status remains "OK" on all three nodes.

13. **Move to next node** only after the soak passes cleanly. Rotate Hetzner → Azure-1 → Azure-2 → Azure-3 sequentially.

End state after all four nodes are rotated: signer list has 3 new addresses (all bound to new MRENCLAVE), quorum 2, same escrow balance, zero XRP moved during the ceremony.

Total wall-clock: ~60–75 minutes for the full cluster, assuming no rollbacks.

### 11.6 Rollback criteria

Listed from least to most drastic. Pick the narrowest one that matches the failure mode.

| Trigger | Scope | Action |
|---|---|---|
| Path A health check fails during soak | Single node | Agent auto-reverts to `.prev` binary, restarts, posts rollback receipt. Cluster unaffected. |
| Path B — add-signer tx not confirmed within `LastLedgerSequence` | Single node | Abort ceremony on this node. No key was added. Old enclave still running. Escalate. |
| Path B — remove-signer tx not confirmed within 10 min | Single node | Submit emergency SignerListSet #3 removing the *new* key. Return to original 3-signer list. Escalate. |
| Path B — new enclave fails DCAP attestation by peers | Single node | Stop new enclave. Old enclave still running, old sealed data intact. Investigate MRENCLAVE mismatch (build env drift, wrong manifest) before re-attempting. |
| Path B — cross-attestation after promotion fails on one peer | Single node | If soak has not yet shredded old data: restart old enclave from its sealed blobs. If shred already done: this node is out of the quorum, follow §11.7. |
| Operational failure during post-rotation soak (bad FROST rounds, corrupted state) | Cluster | Co-sign a SignerListSet that re-adds the previous node's old key and removes its new key. Cluster returns to pre-hotfix signer set. Quarantine the bad node. |
| Production bug discovered after full cluster rotation | Cluster | Treat as a new release: sign 2-of-3 a manifest pointing at the previous good version. Run this runbook again in reverse. |

No rollback path involves a single operator overriding the 2-of-3 requirement. If a situation seems to require that, you are either in the "two operators compromised" out-of-scope case or you have misclassified the failure.

### 11.7 Worst case: sealed data lost on one node

If §11.5 step 9 (shred) has completed and the new enclave then fails permanently on that node (hardware fault, MRENCLAVE mismatch that was not caught earlier, unrecoverable data corruption), the node has **no FROST share** — not old, not new.

This node is out of the FROST quorum permanently until a fresh DKG ceremony runs. The cluster continues signing with 2-of-2 of the remaining nodes (the list has 3 signers, quorum 2 — mathematically intact), but the margin of safety is now zero. A second node failing during this window would freeze the escrow.

Recovery procedure (separate from this runbook):

1. Reprovision the affected node from scratch (new VM, wipe and reinstall, or new hardware).
2. Run a fresh 3-party DKG with the two healthy nodes plus the replacement. This produces a new set of FROST shares bound to the current MRENCLAVE.
3. Submit a SignerListSet replacing the dead node's address with the replacement's address. Quorum stays at 2.
4. Resume normal operation.

DKG itself is a separate ceremony and is out of scope of this document. It is documented in `deployment-procedure.md` as a future addition (see section 1 "out of scope").

### 11.8 Golden rule

> **Do not shred old sealed data until the new enclave has signed at least one real FROST round on mainnet.**

Everything before the shred is reversible. The shred is the point of no return for that node. The 10-minute soak at §11.5 step 8 exists specifically so you have a last safe window to abort.

If for any reason you are unsure whether the new enclave is actually producing valid signatures (log noise, attestation flakiness, network oddness during the soak), **do not shred**. Extend the soak, investigate, and only proceed when every signal is clean. An extra hour of soak is always cheaper than a fresh DKG.

### 11.9 Post-hotfix checklist

After the cluster is fully on the new release:

1. All three nodes report the new MRENCLAVE in attestation.
2. XRPL signer list has exactly 3 entries, quorum 2, all three are the new addresses.
3. Escrow balance unchanged (modulo the liveness-ping tx from §11.5 step 8).
4. One successful real user-initiated operation (deposit, withdraw, or position change) signed by the new cluster.
5. Audit log entry committed to the release repo, signed by the on-call operator, recording: release version, start/end timestamps, node-by-node timing, any deviations from this runbook.
6. Previous release kept on disk as `.prev` for the standard rollback window (30 days suggested), then pruned.

---

## Appendix A — What this does NOT solve

Be honest about the limits:

- **Two-operator collusion.** If Alex and Tom collude, they have 2-of-3 and can deploy whatever they want. This is by design — the trust assumption of the protocol is that fewer than two operators are malicious.
- **Build supply chain.** A malicious dependency in `Cargo.lock` would be in everyone's reproducible build. Independent verification of hashes only proves all three got the *same* poison. Mitigation: dependency review on merge, `cargo deny`, vendored deps for critical paths.
- **YubiKey supply chain.** A backdoored YubiKey could leak private keys. Mitigation: buy from authorised resellers, verify packaging, ideally buy from different sources for the three operators.
- **DCAP/SGX trust.** The whole attestation layer trusts Intel. Out of scope of this document but worth remembering when reasoning about end-to-end trust.

## Appendix B — Connected memos and docs

- `feedback_dcap_subprocess_pattern.md` — the attestation primitive that backs the cross-attestation step
- `feedback_dcap_azure_two_bugs.md` — production gotchas if production runs on Azure
- `project_fork_and_deploy.md` — current ad-hoc deploy state (the thing we are replacing)
- `feedback_always_redeploy.md` — current "rebuild on Hetzner + redeploy to all 4 hosts" pattern, will be obsoleted by this scheme
