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
