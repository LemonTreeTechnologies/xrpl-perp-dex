# Development Operating Model

**Status:** Accepted (2026-05-01).
**Audience:** the human team (Andrey, Tom, Alex), the AI-Auditor, future maintainers, future external auditors.
**Replaces / consolidates:** stand-alone notes about mainnet readiness, deploy procedure caveats, and audit workflow that previously lived only in chat history and memory files.
**Companions:** `docs/multi-operator-architecture.md` (the aspirational trust model), `docs/deployment-procedure.md` (the ceremonial steps), `SECURITY-REAUDIT-4.md` (the audit baseline).

This document fixes the operational reality of how this project is run **today**, and how that reality differs from the architecture documents. Both the architecture and the reality are correct; they describe different time horizons. This document is the bridge.

---

## 0. Why this document exists

The project has two concurrent timelines:

1. **Architecture** — `docs/multi-operator-architecture.md` describes the trust model, coordination protocols, and lifecycle invariants that hold **when the cluster is run by N independent human operators with zero trust between them**.
2. **Operational reality** — today, one physical operator (Andrey) plays all N roles. The architecture's §1 trust model ("operator-vs-operator zero trust") cannot be enforced when N=1 in human terms.

Both are correct for their respective horizons. The honest move is to document the gap, define the operating modes that bracket it, and write a procedure that lets us synchronise mainnet with development-state without pretending the gap doesn't exist.

This document is a living policy reference; it is updated alongside actual changes to the operating model.

---

## 1. The three operating modes

### 1.1 `product-sandbox-single-operator` — current mode

- **One physical operator** (Andrey) signs as node-1, node-2, and node-3.
- **Architecture invariants from `multi-operator-architecture.md` §1 are formally suspended** for this mode. The "operator-vs-operator zero trust" property cannot exist when there is only one operator.
- **No real customer funds are at risk.** Mainnet escrow holds only seed/test funds tracked by the founder; trading is not open to external customers.
- **Mainnet sync is a checkpoint, not a launch.** We synchronise mainnet with the development tip on a cadence (typically every few weeks), not as a product release.
- **Open product questions do not block sync.** AMM-vs-CLOB, BTC perp design, vault redesign, frontend integration scope — all proceed at their own cadence and never gate a mainnet sync.

This mode is honest about what we have. It is not a violation of the architecture; it is a documented pre-architecture state.

### 1.2 `product-sandbox-multi-operator` — transitional mode

- **At least two physical operators** independently run nodes; the architecture invariants from `multi-operator-architecture.md` §1 are now **enforced**, not suspended.
- The cluster runs the same code as in 1.1; the difference is purely human (different humans hold different operator seeds).
- Still pre-product: no real customer funds, mainnet sync is still a checkpoint cadence, AI-auditor gate (§2) still applies.
- **Trigger to enter:** a second human operator formally commits to running their node, has their `node-bootstrap` artefacts published, and has been added to the on-chain `SignerList` via `signerlist-update`.

### 1.3 `production` — final mode (not yet defined)

- Real customer funds. Multi-operator architecturally and operationally. Third-party human audit. Formal launch playbook.
- This mode's procedures are intentionally not specified in this document. When the project is ready to enter production mode, a separate launch playbook will be written; it will inherit most of §3 and §2 with strictness ratchets and a real third-party audit replacing the AI-Auditor as the primary gate.
- **Production-mode unlock is gated on TWO foundation invariants** (see `multi-operator-architecture.md` §1 invariants 5 and 7): (a) a working enclave-software-upgrade mechanism that preserves real customer state across MRENCLAVE bumps — Path A, currently being implemented at REQ-8; (b) reproducible MRENCLAVE produced independently by ≥N operators with bit-identical result — currently being instantiated via the GHA build-gate (`docs/build-requirements.{en,ru}.md` §5). Either gate failing means production-mode is unreachable; both must hold.

### 1.4 Mode transitions

| From | To | Trigger | Required artefacts |
|---|---|---|---|
| 1.1 → 1.2 | a second human operator joins | their `node-bootstrap` published Domain on-chain; `signerlist-update` adds them; fresh DKG over new membership | sync log entry naming the new operator (with consent); doc edit here marking current mode = 1.2 |
| 1.2 → 1.3 | project is ready to take real customer funds | separate launch playbook; third-party audit signoff; formal go/no-go decision; this is **not** a sync — it is a launch | not specified here |

Reverting from 1.2 to 1.1 (an operator leaves, no replacement found) is allowed; treat it as a `signerlist-update --remove` followed by a sync log entry. Reverting from 1.3 to anything is a major incident with its own playbook.

---

## 2. Audit workflow — AI-Auditor cycle

> **Authoritative protocol (2026-05-01).** The audit cycle is governed by [`docs/audit/AUDIT-PROTOCOL.md`](audit/AUDIT-PROTOCOL.md) v1.0 (cross-project, cross-repo) and the per-repo wrapper [`docs/audit/PROTOCOL.md`](audit/PROTOCOL.md). This §2 retains the conceptual rationale (why an AI-Auditor; how the gate composes with §3 mainnet sync) but defers all field/file/PR-shape specifics to the protocol document. Where this section and the protocol document differ, the protocol document is canonical.

### 2.1 Why an AI-Auditor

Real third-party human audits are expensive and slow; we cannot trigger one for every mainnet sync (which happens every few weeks). An AI-Auditor — a Claude Code instance running on a separate machine with an audit-specialised knowledge base, no exposure to dev-context — provides an **independent review** between formal third-party audits. It is not a replacement for human audit at production-launch time; it is a higher-frequency gate during the sandbox phases.

The AI-Auditor's independence is structural:
- Different machine (no shared memory or cache with the dev instance).
- Knowledge base is auditor-specialised (prior audits, CVE patterns, attack libraries) rather than dev-context.
- Operator-controlled (Andrey runs both sides), but the auditor instance receives only the artefacts (commits, diffs, finding IDs) that a real auditor would receive — not the dev-time conversation that produced them.

### 2.2 Cycle

1. **Trigger.** A mainnet sync (Mode S or F per §3) is requested.
2. **Change-set assembly.** The dev side compiles the audit-input package: list of commit hashes since last sync, mapping each to relevant finding IDs (`O-H1`, `O-M3`, `C-04`, etc.) or to "new functionality, no prior finding". Saved as `audit-reviews/<YYYY-MM-DD>-input.md`.
3. **Auditor review.** AI-Auditor reads the input package + the current code state, produces a verdict file `audit-reviews/<YYYY-MM-DD>-verdict.md` containing:
   - per-finding-ID resolution (resolved / partial / not addressed / new finding)
   - sync gate decision: `approved` | `approved-with-conditions` | `blocked`
   - if `approved-with-conditions`: list of conditions (e.g. "must complete X before Mode F")
   - if `blocked`: list of reasons + minimum scope to unblock
4. **Sync gate enforced.** Mainnet sync (§3) does not execute without an `approved` or `approved-with-conditions` verdict for the current change-set.
5. **On block.** Dev side addresses the reasons, lands fixes, re-requests audit. Loop until approved or escalated to human review.

### 2.3 What goes into `audit-reviews/`

```
audit-reviews/
  README.md                      — directory purpose + index
  2026-MM-DD-input.md            — change-set submitted to auditor
  2026-MM-DD-verdict.md          — auditor reply (verdict + conditions/blocks)
  2026-MM-DD-followup.md         — optional: dev response if verdict raised new questions
  ...
```

Files are append-only: once a verdict is committed, it is never edited. Subsequent revisions go into a new dated file.

### 2.4 Boundary cases

- **Auditor and dev disagree on severity.** Document both views in the verdict file and a follow-up. If unresolved, escalate to human review (Andrey + future external auditor at next formal audit window).
- **A finding is marked "by design" by dev but disputed by auditor.** Same handling; produces a documented disagreement that a future human audit can revisit.
- **Audit-input forgets a commit.** Treat as auditor's right to flag at any time; sync may need to roll back. Mitigation: dev compiles input package from `git log <last-sync-hash>..HEAD` with no manual filtering.

---

## 3. Mainnet sync procedure

### 3.1 Decoupling — Mode S vs Mode F

Mainnet sync is two orthogonal operations. Either may run independently of the other, and most syncs run only one of the two.

- **Mode S — Software sync.** Update enclave + orchestrator binaries, MRENCLAVE bump, fresh DKG. Does NOT modify escrow `SignerList` or master-key state via Mode S itself. **HONEST DESCRIPTION (perp REQ-6 discovery 2026-05-02):** as currently implemented, Mode S also does NOT preserve sealed enclave state across the MRENCLAVE bump. What this means in operator/customer terms: (a) FROST shares are recreated by the fresh DKG step (step 6) — semantically correct, no value lost; (b) per-enclave ECDH identity keys are regenerated on first boot — peer-attest cache rebuilds within one announcer cycle (~240 s), no value lost; (c) **per-operator XRPL signing keys generated by the OLD enclave are NOT reachable by the new enclave** — meaning an operator who bumps MRENCLAVE in production-mode without first transferring authority to a NEW XRPL key would render the existing on-chain `SignerList` unsignable; (d) **perp state — positions, margin balances, vault state, account pool — is reset on every Mode S sync** because the new MRENCLAVE cannot unseal blobs sealed by the old. Today this is acceptable because we operate under §1.1 `product-sandbox-single-operator` mode where there are no real customer positions to lose and the mainnet escrow's pre-Mode-S transfer-of-authority hasn't been needed (mainnet sync hasn't yet been performed — gated on the upgrade-mechanism work). Production-mode (§1.3) is unreachable from this point without the cross-enclave Local Attestation migration mechanism (Path A; spec to be authored as audit-cycle REQ-7); see the foundation invariant in §1 for why this is non-negotiable. Mainnet Mode S sync is therefore postponed until that mechanism lands.
- **Mode F — Funding / on-chain governance update.** Top up escrow, disable master key, change `SignerListSet` membership or quorum. Does NOT touch enclave binaries. Runs as on-chain XRPL transactions only.

Most syncs are Mode S only. Mode F is rare and explicit (e.g. "add a second physical operator" → 1.1→1.2 transition).

### 3.2 Mode S — software sync steps

**Pre-flight:**

1. AI-Auditor verdict for the current change-set is `approved` or `approved-with-conditions` (§2). Conditions, if any, are met before continuing.
2. Reproducible build: confirm Hetzner-built enclave + orchestrator binaries, log MRENCLAVE.
3. Sync log entry started (not committed yet) with `Mode: S`, target MRENCLAVE, change-set hash range.

**Per-node steps (3 Azure mainnet VMs, sequentially node-by-node):**

4. `node-deploy` (existing subcommand) — node-local artefact distribute + systemd swap.
5. Peer attest cycle — verify the new MRENCLAVE quote shows up in the peer-attest cache on both other nodes.
6. After all 3 nodes are deployed: fresh DKG ceremony via `dkg-coordinate` (libp2p) — produces a new group pubkey.
7. Cluster health check — group pubkey byte-identical across all 3 nodes; libp2p mesh stable; periodic peer-quote announcer firing.

**Wrap-up:**

8. Sync log entry committed with: MRENCLAVE before/after, group pubkey before/after, AI-auditor verdict file ref, operator acknowledgement (§1.1: all operators played by Andrey), evidence links.
9. No on-chain transaction is required by Mode S. The escrow's `SignerList` and balance are unchanged.

### 3.3 Mode F — funding / on-chain governance steps

Mode F is a menu of XRPL operations, each with its own sub-procedure. None require an enclave change.

- **Top-up escrow:** simple `Payment` from a funded source to the mainnet escrow address.
- **Membership change:** `signerlist-update` admin route (Phase 2.2-C); existing-quorum signs new `SignerListSet`. Use when entering Mode 1.2 or rotating an operator.
- **Master-key disable:** `AccountSet asfDisableMaster` on the escrow. One-way; only run when ready to commit to multisig-only governance forever for that account. (Mainnet escrow's master is not yet disabled per `reference_mainnet_escrow_seed.md` — this is intentional during sandbox phase.)
- **Quorum change:** `signerlist-update --quorum N` (no membership change).

Each Mode F operation produces its own line in the sync log under the same date as the Mode S sync (if combined) or its own dated entry (if standalone).

### 3.4 Combined Mode S+F runs

Permitted but rare. When both run on the same calendar day:

- Mode S goes first (software is now current).
- Mode F follows (governance change made on top of current software).
- Sync log entry has `Mode: S+F` and lists both kinds of evidence.

If Mode F changes membership, the new operator's MRENCLAVE must already match (i.e. they ran their own Mode S first on their own VM, or they joined fresh and ran `node-bootstrap` against the current MRENCLAVE).

---

## 4. Sync log

### 4.1 Where it lives

`mainnet-sync-log.md` at repo root. Append-only. Entries are immutable once committed.

### 4.2 Entry template

```markdown
## 2026-MM-DD — Mainnet sync #N
- **Mode:** S | F | S+F
- **Trigger:** <one sentence: what change motivated this sync>
- **AI-Auditor verdict:** [`audit-reviews/2026-MM-DD-verdict.md`](audit-reviews/2026-MM-DD-verdict.md) — <approved | approved-with-conditions | n/a>
- **MRENCLAVE before → after:** `<hash>` → `<hash>` (or "n/a" for Mode F)
- **Group pubkey before → after:** `<hash>` → `<hash>` (or "n/a" for Mode F)
- **Escrow address:** `<rXXXX>` (unchanged unless Mode F changed it)
- **SignerList before → after:** <quorum>-of-<N> → <quorum>-of-<N> (or "unchanged")
- **Operators participated:**
  - node-1 = [Andrey] (`product-sandbox-single-operator` mode — see §1.1)
  - node-2 = [Andrey]
  - node-3 = [Andrey]
- **Acknowledgement:** Run under `product-sandbox-single-operator` mode. Architecture invariant §1 of `multi-operator-architecture.md` (operator-vs-operator zero trust) is suspended per §1.1 of this document. No real customer funds at risk; this is a checkpoint deploy, not a launch.
- **Verification:**
  - <evidence #1, e.g. "group_pubkey byte-identical across 3 nodes — log lines at <commit>">
  - <evidence #2, e.g. "on-chain tx <hash> landed tesSUCCESS">
  - <evidence #3>
- **Outcome:** clean | reverted | partial (if partial: what is left)
- **Next:** <what triggered the next planned sync, or "no next planned">
```

When the project transitions to Mode 1.2, the "Operators participated" block will name multiple humans; the "Acknowledgement" block will note that §1 is now enforced rather than suspended.

---

## 5. Open questions deliberately not addressed here

This document is the operating model, not the product roadmap. The following are real open questions, but they belong elsewhere and **do not gate any sync**:

- AMM-vs-CLOB direction (`docs/clob-vs-amm-alignment.md`)
- BTC perp feasibility (`docs/btc-perp-dex-feasibility.md`)
- Frontend integration questions (Tom / Alex / Tanya scope)
- State hash Merkle tree (M-05 in audit history)
- Build-gate decision (Hetzner self-hosted runner vs aligned-deps GHA)

When any of these resolves into a code change, that change goes through the normal AI-Auditor cycle (§2) and lands in the next sync (§3); none of them block syncs that don't include their changes.

---

## 6. Initial state at this document's commit

- **Current mode:** `product-sandbox-single-operator`.
- **Mainnet sync number completed:** 0 (this document precedes the first cataloged sync).
- **Mainnet escrow on Hetzner** holds ~108 XRP per `reference_mainnet_escrow_seed.md`; master key not yet disabled. State is acceptable for current mode.
- **Testnet stack** (3 Azure DCsv3 VMs) is current with `master` HEAD; mainnet stack is not. The first scheduled sync will close that gap in Mode S.
