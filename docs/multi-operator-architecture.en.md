# Multi-Operator Cluster Architecture

**Status:** authoritative for any change to cluster bootstrap, deployment, DKG, or membership.
**Audience:** anyone writing or reviewing code that crosses node boundaries.
**Companion:** `testnet-enclave-bump-procedure.{en,ru}.md` (concrete operator runbook for testnet today).

## 0. Why this document exists

This system is a perpetuals DEX whose security guarantee is "no operator can move user funds alone." That guarantee survives only if the cluster runs the **same code paths** on testnet and mainnet. Two-mode codebases (one path for testnet, another for production) leak assumptions: testnet validates one set of behaviors, audit then objects to a different set in production, we rewrite, the rewrite is again partly temporary, and we loop. Every loop is observable as a "we'll redo this for production later" comment, a `--testnet-only` flag, an SSH chain that orchestrates code that should orchestrate itself.

This document fixes the rules of the road. Rules are stated as commitments, not aspirations.

**Vow.** Every subcommand and module that crosses a node boundary passes a **single-mode check** before merging:

1. Does it work identically on testnet and mainnet?
2. Does it embed any cross-operator assumption — SSH to a peer's VM, shared filesystem, central bastion — that production cannot honor?

If the answer to (1) is no or to (2) is yes, the change does not merge. We rewrite.

**Operator workflow is NOT system code.** The human operator (or an AI assistant acting on their behalf) may SSH into their own VMs, run Ansible playbooks, fan-out scripts via parallel-ssh, drive deployments from a central bastion they control — that is their sysadmin tooling. None of it lives in our repository. When this document says "the system does X", it means our committed code does X. When it says "the operator does X", it means a human (possibly with an AI assistant) does X using their personal tooling, off our repo.

The critical implication: testnet today, where one human runs all three nodes and SSHes between them freely, is not a different mode. The system code that runs on testnet is the same code that runs on mainnet. The only difference is operator topology: one human owns three nodes (testnet) versus three independent humans owning one node each (mainnet).

## 1. Trust model

The trust we place in each component:

| Component | Trusted? | What it can do | What it cannot do |
|---|---|---|---|
| Intel SGX enclave (after DCAP attestation) | Yes | Hold sealed FROST shares, sign with them, perform AEAD over ECDH-derived keys | Persist anything that survives MRENCLAVE bump; communicate without going through host |
| Host OS on each operator's VM | No | Read/write any file outside the enclave; manipulate network; observe enclave timing | Read enclave memory; forge a DCAP quote; produce shares matching a different MRENCLAVE |
| Operator (human running their node) | No relative to other operators | Operate their own VM; observe their own network; substitute or stop their own host binary | Access another operator's VM; sign multisig transactions alone (quorum is 2-of-N for N≥3); produce another operator's enclave-bound key material |
| XRPL ledger | Yes (within Byzantine-resilience of the consensus) | Reflect SignerListSet, AccountSet, Payment transactions; provide durable public storage for `AccountSet.Domain` operator entries | Be unilaterally rewritten by any single party; resolve operator-internal disputes |
| libp2p mesh between orchestrators | Authenticated, not confidential | Carry gossipsub topics that nodes publish to and listen on; provide peer-discovery | Be a substitute for end-to-end attested encryption (we layer Path A v2 on top for share transport) |

Cluster invariants that fall out of this:

1. **No single operator can sign a withdrawal.** XRPL multisig with quorum ≥ 2 prevents it; the SignerList is the on-chain authority and is updatable only by quorum-met multisig.
2. **No single operator can produce a valid FROST signature.** FROST 2-of-N (or higher) prevents it; `frost_group` state is sealed inside each enclave.
3. **No single operator can rotate operator membership.** The on-chain SignerList is the membership source of truth; SignerListSet updates require existing-quorum multisig.
4. **No single operator can deny progress alone.** N-1 of N operators can sign and execute (when quorum is met).

## 2. Operator scope

Each operator owns:

- Their own hardware: typically a single SGX-capable VM (Azure DCsv3 in current testnet). They control its OS, network configuration, root access, and physical-or-virtual decommissioning.
- Their own enclave-internal keys (FROST share, ECDH identity). These exist only inside their enclave; even the operator cannot read them.
- Their own backups: enclave sealing means backups outside the enclave are useless against MRENCLAVE bump. The operator's "backup" is fundamentally social (rejoin the cluster after a fresh enclave).
- Their own XRPL operator account. Its master key generated by their enclave on first run, stays inside the enclave, signs multisig participations.

No operator has:

- Access — SSH, file, network, anything — to another operator's VM.
- A copy of another operator's FROST share. By DKG construction and Path A v2 transport invariants, only the holder enclave ever sees its own share.
- Authority to modify SignerList, escrow account state, or cluster membership without quorum-met multisig.
- The ability to know whether another operator's host is honest, malicious, or offline, except via observable on-chain or libp2p behavior.

The cluster is an emergent property of N independent operators that have agreed (off-chain) on a software release tag, deployed it independently, registered their public material on-chain, and met quorum on the resulting SignerList.

## 3. Cross-node coordination — what the system uses

Three channels, in priority order:

### 3.1 libp2p mesh + gossipsub

The orchestrator daemon on each node maintains a libp2p mesh (port 4001 in current testnet topology). Authenticated peer connections via libp2p's noise protocol. Gossipsub topics carry cluster-internal messages.

Topics in use:

| Topic | Direction | Payload | Existing |
|---|---|---|---|
| `perp-dex/path-a/peer-quote` | each node periodically publishes | DCAP quote bound to (shard, group_id, ECDH pubkey) | yes |
| `perp-dex/path-a/share-v2` | sender → recipient (filtered) | AEAD-wrapped FROST or DKG share envelope | yes |
| `perp-dex/cluster/dkg-step` | leader → all | DKG ceremony coordination signal (round-1-go, round-2-go, finalize) | new (Phase 2.1c-replacement) |

libp2p is the only cross-node coordination channel for live cluster operation. Anything that needs to happen between nodes during DKG, share rotation, or membership change goes through it. There is no "out-of-band SSH from a coordinator" path in production code.

### 3.2 DCAP cross-attestation (Path A v2)

Operators do not trust each other's hosts, but they trust each other's enclaves provided DCAP attests them. The Path A v2 protocol (`docs/path_a_ecdh_over_dcap.md` in the enclave repo) gives each enclave a verified peer ECDH identity, populates a per-peer attestation cache (5-minute TTL), and provides the AEAD primitive used by share transport. Path A v2 is the substrate beneath the libp2p share-v2 topic — libp2p delivers the envelope, Path A v2 makes it safe to deliver.

### 3.3 On-chain XRPL state

The XRPL ledger is the durable, public, tamper-evident memory of the cluster. We use it for three things:

| What lives on chain | Field | Updated by |
|---|---|---|
| Cluster membership | `SignerList` on the escrow account | SignerListSet via existing-quorum multisig |
| Each operator's ECDH pubkey | `AccountSet.Domain` on each operator's own XRPL account | Each operator independently |
| Cluster operations (withdrawals etc) | Payment / Escrow transactions on the escrow account | 2-of-N multisig signed by the orchestrators |

`Domain` is a 256-byte field on every XRPL account. We use it to publish each operator's ECDH identity public key. The format is structured: `xperp-ecdh-v1:<33-byte hex>`. Discovery for any node is: query `AccountObjects` of the escrow account → get the SignerList → for each entry, query `AccountInfo` → parse `Domain` → extract ECDH pubkey. No separate registry, no off-chain coordination for pubkey discovery.

### 3.4 Out-of-band — governance only

Some decisions are inherently social: who is in the initial set of operators, when to bump MRENCLAVE for a security release, how to respond to an incident. These happen on whatever channel the operator group chooses (Discord, mailing list, a separate governance contract). This document does not specify the off-chain channel because the system code never reads from it. The only output of off-chain governance that matters to the system is **on-chain XRPL transactions**. Governance happens, an `SignerListSet` lands on chain, the system observes it.

## 4. The operator workflow vs system code distinction

The system code in this repository is responsible for everything that happens between nodes during cluster operation. That is exhaustively covered by §3 above: libp2p, Path A v2, on-chain.

The operator workflow is what the human (or their assistant) does to operate their node:

- SSH into their own VM
- Run `systemctl` to control services
- Use Ansible / parallel-ssh / a one-off bash script to fan out the same node-local command to multiple of their own VMs (relevant on testnet where one human owns all three)
- Drive sequence-of-steps procedures by reading the runbook and executing each step in turn

The operator's workflow is theirs to design and maintain. The system code does not assume any specific workflow tooling exists. When this document references operator activity (e.g., "the operator deploys their node"), it describes a runbook step the operator executes using their own tools, not a Rust subcommand we ship.

The single-mode check from §0 prevents bleed: a Rust subcommand that requires `ssh user@another-operator's-host` to function is system code embedding a cross-operator workflow assumption. That fails the check.

## 5. Cluster lifecycle

Six events in the cluster's life. Each is described concretely in §6–§9.

| Event | Frequency | Drivers |
|---|---|---|
| **Genesis** | Once | All N founding operators independently + a designated founder for trusted-dealer escrow setup |
| **Steady state** | Continuous | Each orchestrator daemon runs; libp2p mesh persists; on-chain operations execute on multisig |
| **MRENCLAVE bump** | Per security release (rare) | Each operator independently; coordinated start signal off-chain; new DKG follows |
| **Operator addition** | Rare (governance) | Existing quorum + new operator; SignerListSet adds member; new DKG |
| **Operator removal** | Rare (governance / emergency) | Existing quorum (excluding removed); SignerListSet removes member; new DKG |
| **Disaster recovery** | Per-incident | Affected operator + remaining quorum; reduces to membership change + new DKG |

All five events past genesis are mechanically the same: governance produces a SignerListSet transaction signed by current quorum, the cluster observes the new membership, then a fresh DKG runs over libp2p. Genesis is special only because there is no current quorum to sign yet.

## 6. Bootstrap protocol — genesis

The N founding operators have agreed off-chain on a software release tag. They have agreed on each other's XRPL operator addresses. They have agreed on the quorum (typically 2-of-3 or 3-of-5). They have agreed on who plays the role of founder for the trusted-dealer escrow setup.

### 6.1 Each operator independently deploys their node

Each operator, on their own VM, performs:

1. `git checkout <release-tag>` of both repositories.
2. `docker build --no-cache -f Dockerfile.azure ...` for the enclave image. Verify the resulting `enclave.signed.so` SHA256 and MRENCLAVE match those published in the release tag (see §7.1).
3. `cargo build --release` for the orchestrator binary.
4. Place artefacts in their canonical paths on the VM (`/home/<user>/perp/`).
5. Configure the orchestrator's startup arguments: enclave URL (loopback only), libp2p listen address, libp2p peer addresses (the other N-1 operators' public addresses, agreed off-chain), database URL, no escrow address yet.
6. Start the enclave service. Run a single `node-bootstrap` command on this VM (see §10) to generate the operator's XRPL keypair inside the enclave; the enclave seals the private key and emits the public xrpl_address + compressed_pubkey.

After this step, each operator possesses (locally) a freshly-generated `node-<i>.json` containing their xrpl_address, compressed_pubkey, and an authentication session_key.

### 6.2 Each operator publishes their ECDH pubkey on-chain

Each operator queries their enclave for its ECDH identity public key (`/v1/pool/ecdh/pubkey`) and submits an `AccountSet` transaction on their XRPL operator account, setting `Domain` to a structured hex value: `xperp-ecdh-v1:<33-byte ECDH pubkey hex>`. Submitted from the operator's own machine via XRPL JSON-RPC; no SSH involved.

After this, all N operator accounts publicly carry their ECDH pubkey. Any party (including peers, observers, auditors) can query this with a standard XRPL `AccountInfo` request.

### 6.3 Off-chain agreement on initial SignerList

Operators exchange their `node-<i>.json` files via their off-chain channel (Discord, email). Each verifies the content matches what they themselves submitted. Final agreed SignerList composition is the set of N xrpl_addresses with quorum K.

This is the only step that is NOT enforced by the system code. It is governance. The system observes the resulting on-chain SignerListSet and trusts only it.

### 6.4 Founder runs trusted-dealer escrow setup

The designated founder runs `escrow-init` (see §10) from any machine with internet access. This subcommand:

1. Generates a fresh secp256k1 XRPL wallet (the escrow account).
2. Faucet-funds it (testnet) or has the founder fund it from their own XRP holdings (mainnet — exact mechanism is governance, not specified here).
3. Submits `SignerListSet` with the N agreed operator addresses and quorum K.
4. Submits `AccountSet asfDisableMaster`. From this point the founder has no special authority over the escrow.
5. Writes the seed to a canonical path (`~/.secrets/perp-dex-xrpl/escrow-<env>.json`, mode 0600).
6. Prints the escrow address.

The seed file output of step 5 is unique to this command — it documents that the founder once held it, but post-disable the seed has no operational power. We keep it to prove provenance and for forensics.

After this step the escrow address is public, the SignerList is on-chain, master is disabled.

### 6.5 Operators publish escrow address to their orchestrators

Each operator updates their orchestrator's startup configuration to include the escrow address (the value is broadcast to operators by the founder; verifiable on-chain by anyone). They restart their orchestrator. The orchestrator now boots, joins the libp2p mesh, observes the on-chain SignerList, and begins discovering peers' ECDH pubkeys via `AccountInfo` queries against the SignerList members.

### 6.6 libp2p mesh forms

Each orchestrator dials the other N-1 known peer addresses. A gossipsub mesh forms automatically. No manual bootstrap step. Existing topics are subscribed.

### 6.7 Cross-node DCAP attestation rounds

The existing periodic peer-quote announcer (one task per FROST group, per orchestrator, period 240s) publishes DCAP quotes bound to `(shard_id, group_id, ECDH_pubkey)`. At genesis no FROST `group_id` exists yet, so the announcer uses the bootstrap sentinel `group_id = 32 zero bytes`. Other orchestrators receive the announcement, call `verify-peer-quote` against their local enclave to populate `peer_attest_cache` for the sentinel.

Within ~10 minutes of all operators online, every enclave's attestation cache is populated for every other operator's ECDH pubkey under the sentinel.

### 6.8 DKG ceremony via libp2p

The agreed-upon ceremony leader (per off-chain agreement, often `pid=0`) initiates DKG by publishing on the new `perp-dex/cluster/dkg-step` topic: a typed message stream covering round-1-start, round-1.5-export, round-2-import-status, finalize. Each orchestrator handles each step locally (calling the enclave's `/pool/dkg/round1-generate`, `/pool/dkg/round1-export-share-v2`, etc., on its own loopback enclave) and publishes step-completion acks on the same topic. Round-1.5 envelopes are published on the existing `perp-dex/path-a/share-v2` topic with a discriminant indicating DKG-bootstrap context.

When all N orchestrators acknowledge finalize and the leader has cross-checked their `group_pubkey` outputs (broadcast over the topic), the leader publishes a final-ack message containing the canonical group_pubkey. Every orchestrator records it locally and updates its `frost_group_id` configuration.

The full ceremony completes in the same time bound as the testnet manual procedure (~35 seconds wall-clock for N=3) but without any operator SSH involvement.

### 6.9 Steady state

Orchestrators are running with:
- libp2p mesh stable
- `frost_group_id` known and configured
- Periodic peer-quote announcer running with the real `frost_group_id`
- `frost_group` initialized in each enclave; can sign

The cluster is now ready for withdrawals (multisig submitted via XRPL `submit_multisigned`), order processing, deposit detection, etc.

## 7. MRENCLAVE bump (coordinated upgrade)

### 7.1 Reproducible build is the foundation

Every release carries a Git tag. The release process produces a deterministic MRENCLAVE that any operator can reproduce by running:

```
git checkout <tag>
docker build --no-cache -f Dockerfile.azure -t perp-dex:<tag> .
docker run --rm perp-dex:<tag> sha256sum /build/out/enclave.signed.so
docker run --rm perp-dex:<tag> sgx_sign dump -enclave /build/out/enclave.signed.so -out /tmp/sigstruct -dumpfile /tmp/sigstruct.txt
# Read the MRENCLAVE field from /tmp/sigstruct.txt
```

The release tag includes the expected MRENCLAVE in its release notes / signed announcement. An operator whose local build produces a different MRENCLAVE has a non-reproducible build — they investigate and fix before deploying.

A reproducible build is what makes "we all run the same MRENCLAVE" verifiable without any of us trusting any of the others. Each operator independently confirms.

### 7.2 Each operator independently deploys

Each operator, on their own VM:

1. Verifies their built MRENCLAVE matches the release tag's published MRENCLAVE.
2. Runs `node-deploy` (see §10) locally on their VM. This stops the local services, swaps binaries with backup, restarts the enclave only.

Sealed state on each enclave does not survive an MRENCLAVE bump. Each operator's `frost_group` data is gone after the swap (preserved only as a forensic backup directory). This is by design: the enclave has no concept of decryption keys for a peer enclave's old shares.

### 7.3 Coordinated rollout

The bump is coordinated via off-chain governance: operators agree on a deploy window. Each operator deploys within that window. The libp2p mesh detects "N orchestrators online with new MRENCLAVE" via the periodic peer-quote announcer (peer-quote messages now attest to the new MRENCLAVE).

### 7.4 Fresh DKG follows

Once N operators are online with the new MRENCLAVE and their attestation caches are populated for the sentinel `group_id`, the cluster runs a fresh DKG via §6.8 above. New `group_pubkey`. Old escrow remains on-chain — same address, same SignerList — just a new FROST group underneath. The escrow's existing SignerList is unaffected (XRPL multisig uses per-operator XRPL keys, which were generated in §6.1 and seal across MRENCLAVE bumps only if the operator chose to preserve them; in current testnet they do not, so a separate operator-rekey runs alongside; in mainnet operators may choose differently, but that is a per-operator decision).

## 8. Membership changes

### 8.1 Add an operator

Off-chain, the existing operators agree to add operator M+1. The new operator independently builds and deploys (§6.1). Publishes their ECDH pubkey on-chain (§6.2). Existing operators verify the new MRENCLAVE matches (§7.1). One existing operator drafts a `SignerListSet` with N+1 signers and circulates it for signature; existing-quorum multisig signs and submits.

After the SignerListSet lands, the cluster observes new membership. A fresh DKG follows (§6.8) over the N+1 operators with the new threshold (typically `ceil((N+1)*2/3)` or per governance).

### 8.2 Remove an operator

Same shape. Existing-quorum (excluding the removed operator) drafts and signs a SignerListSet with N-1 signers. Lands on-chain. Fresh DKG over N-1 operators.

### 8.3 Why this is the same primitive as MRENCLAVE bump from a code perspective

Both events end with: "the cluster's effective membership changed; fresh DKG required". The system code has one DKG ceremony driver (§6.8). It runs after any membership-affecting event. There is no special-case "we were 3, now we're 4" path.

## 9. Disaster recovery

Three scenarios, all reducing to a membership change.

### 9.1 Operator X loses their share (host crash, MRENCLAVE bump, etc.)

If only X is affected and N-1 operators still hold valid shares: the cluster can still sign at quorum K (assuming K ≤ N-1). For X to rejoin, run §8.1 — treat X as a new addition (with a new ECDH identity post-fresh-enclave). Equivalently a fresh DKG with the same membership set, just X starting from scratch.

If multiple operators are affected and we drop below quorum: the on-chain escrow is unsignable. The cluster must rebuild from genesis (§6) with the existing escrow account address — operators submit a new SignerListSet via... they cannot, because they cannot meet quorum. This case is the founder's continuing on-chain backstop: the escrow's master key was disabled in §6.4, so the founder cannot help. Recovery is an XRPL-level operation that is out of scope here; consult XRPL recovery mechanisms (none unilateral). This scenario is why the cluster MUST maintain at least quorum live operators at all times.

### 9.2 Operator X's key is compromised (host malicious, attacker has X's enclave)

Existing operators detect the compromise via on-chain misbehavior or off-chain signal. They run §8.2: remove X via SignerListSet signed by the N-1 honest operators (assuming N-1 ≥ K). Fresh DKG with N-1.

If quorum cannot be reached without X (K = N), the cluster is at risk and must operate cautiously until membership grows (§8.1) or X is recovered (§9.1).

### 9.3 Operator X is offline indefinitely

Indistinguishable from §9.2 from the cluster's perspective. Same recovery procedure: run §8.2 to remove X. If the operator returns later, treat them as a new addition (§8.1).

## 10. Subcommand model

The system exposes four classes of subcommand. Every committed subcommand fits one class.

### 10.1 Node-local — runs on a single node

A node-local subcommand affects only the node it runs on. It can read the local enclave (loopback HTTPS), write the local filesystem, query XRPL JSON-RPC (read-only or signing with this node's keys). It does NOT take node addresses or SSH targets as input.

Examples:
- `operator-setup` (existing, will be renamed to `node-bootstrap`): generate this node's operator keypair in the local enclave; emit `node-<i>.json`.
- `node-config-apply` (new): query the on-chain SignerList; for each member, query their `Domain`; build local `signers_config.json` with `local_signer` set to this node's entry; restart the local orchestrator service.
- `node-deploy` (new): swap local binaries, manage local systemd services. Replaces the SSH-driven `cluster-deploy`.

### 10.2 XRPL-only — runs anywhere with internet

An XRPL-only subcommand does not talk to any enclave. It uses XRPL JSON-RPC to query state or submit transactions.

Examples:
- `escrow-init` (new): faucet-fund the escrow, submit SignerListSet, submit AccountSet asfDisableMaster, write seed file. Replaces `setup_testnet_escrow.py`.
- `domain-set` (new, sub-step of `node-bootstrap`): submit AccountSet on this operator's account with `Domain = xperp-ecdh-v1:<hex>`. Optionally bundled into `node-bootstrap`.

### 10.3 Cluster-coordinated — runs on a single node, drives via libp2p

A cluster-coordinated subcommand runs on one node and drives a cluster-wide operation through gossipsub. It does NOT use SSH. The other nodes participate by virtue of their orchestrator daemon listening on the relevant gossipsub topic.

Examples:
- `dkg-coordinate` (new): on one node (the leader), publishes `perp-dex/cluster/dkg-step` messages; followers respond by calling local enclave endpoints; leader waits for finalize-acks; emits group_pubkey on success. Replaces SSH-driven `dkg-bootstrap`.

### 10.4 What does NOT exist

There is no subcommand that takes a list of "remote nodes" and SSHes to them. There is no "fan-out" mode. There is no testnet-only path. Operators who want to run a node-local subcommand on multiple of their own VMs do so with their own sysadmin tooling outside this repository.

## 11. Current code mapped to model

### 11.1 Production-grade primitives

These exist and are correct under the multi-operator model:

- `xrpl-perp-dex-enclave`: ECDH identity, DCAP cross-attestation, Path A v2 share transport, DKG v2 wire format, FROST signing primitives.
- `orchestrator/src/p2p.rs`: libp2p mesh + gossipsub topics for `peer-quote` and `share-v2`.
- `orchestrator/src/path_a_redkg.rs`: existing share-export driver (for share rotation, not bootstrap DKG). Loopback admin route. Honors the no-cross-operator-SSH rule.
- `orchestrator/src/withdrawal.rs`: multisig withdrawal flow via XRPL `submit_multisigned`. Each operator's enclave signs with its own key; aggregation is on-chain.
- `orchestrator/src/cli_tools.rs::operator_setup`: node-local. Generates the operator's keypair via the local enclave, emits `node-<i>.json`. Will be promoted to `node-bootstrap` and extended to also publish `Domain`.

### 11.2 Marked as debt — must be replaced

The following are present in the repository but violate the model. They are marked with `[DEPRECATED — violates multi-operator architecture; replace per Phase 2.1c]` doc-comments at module top. They are kept temporarily because we need testnet operations during the transition; they are NOT to be relied on for any non-throwaway state.

- `orchestrator/src/dkg_bootstrap.rs` (commit `5fe5aa1`+`78710a9`): SSH-driven DKG ceremony. Replacement: `dkg-coordinate` (cluster-coordinated, libp2p-driven).
- `orchestrator/src/cluster_deploy.rs` (commit `fefd3c9`): SSH-driven multi-node binary swap + service lifecycle. Replacement: `node-deploy` (node-local). Coordinated MRENCLAVE bumps are governance, not a single command.

### 11.3 Replacement plan (Phase 2.1c continuation)

The replacement work happens in this order:

| Phase | Subcommand | Class | Replaces | Time |
|---|---|---|---|---|
| 2.1c-A | `node-bootstrap` | node-local | `operator-setup` (rename + extend with Domain publish) | ~1 day |
| 2.1c-B | `escrow-init` | XRPL-only | `setup_testnet_escrow.py` | ~½ day |
| 2.1c-C | `node-config-apply` | node-local | new (no current equivalent) | ~½ day |
| 2.1c-D | `dkg-coordinate` | cluster-coordinated | `dkg-bootstrap` (SSH) | ~2 days |
| 2.1c-E | `node-deploy` | node-local | `cluster-deploy` (SSH) | ~½ day |
| 2.1c-F | retire `dkg_bootstrap.rs` + `cluster_deploy.rs` | (delete) | (retiring debt) | ~½ day |

Total: ~5 days. During this time the testnet operates on the deprecated SSH-driven path; mainnet does not exist yet. Deprecated-path operations during the transition are explicitly understood as testnet-developer-convenience, not system-code, and never inform mainnet design.

## 12. Glossary

| Term | Definition |
|---|---|
| Operator | A party that runs one node in the cluster. Owns their VM, their enclave, their XRPL operator account. May be a human or an organization. |
| Node | One operator's VM running enclave + orchestrator. |
| Enclave | An Intel SGX TEE running our `enclave.signed.so` with a specific MRENCLAVE. Sealed state is bound to MRENCLAVE. |
| MRENCLAVE | The SHA256-based identity of a specific enclave binary. Same source + same toolchain produces the same MRENCLAVE (reproducible build). |
| Founder | The party that performs the trusted-dealer escrow setup at genesis. After AccountSet asfDisableMaster, the founder has no on-going authority. |
| Quorum | The on-chain SignerList's required-weight threshold for multisig. K-of-N, with K ≥ 2 in any realistic configuration. |
| FROST group | The set of operators that participated in a single DKG ceremony. Each participant holds a sealed share; together they can produce one BIP340-style signature. |
| Path A v2 | The ECDH-over-DCAP protocol for cross-machine share transport. Wraps shares in AES-128-GCM keyed on enclave-pair ECDH after both sides have DCAP-attested each other. |
| ECDH identity | A per-enclave secp256k1 keypair used only for Path A transport. Distinct from FROST signing keys. Public key published on-chain via `AccountSet.Domain`. |
