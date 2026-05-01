# AUDIT-PROTOCOL — REQ-N / RESP-N audit cycle

**Version**: 1.0 (2026-05-01)
**Owner**: security-audit-playbook
**Scope**: cross-project audit cycle for in-house and internal-client audits operated by this playbook's auditor identity.

---

## 0. Purpose

This protocol formalizes the audit cycle between a maintainer (BE — backend / build engineering) and the auditor as a sequence of **append-only markdown documents in git**. It eliminates chat-based audit communication, makes verification reproducible, and produces a self-documenting historical record per round.

It applies to:
- 77ph/SGX_project (EthSignerEnclave)
- 77ph/xrpl-perp-dex (perp-DEX + enclave)
- 77ph/phoenix-rs (Rust SGX rewrite)
- valory-xyz/wildcard
- valory-xyz/autonolas-* family (tokenomics, governance, registries, watchdog)
- All future internal audits where this playbook's auditor identity is engaged.

For external bug bounty engagements (Immunefi, Sherlock, C4A, Cantina), use the platform's native submission mechanism — this protocol does NOT supersede platform requirements.

---

## 1. Folder structure (per repo)

Every repo using this protocol creates a top-level audit folder:

```
docs/audit/
├── PROTOCOL.md              # thin wrapper: references this playbook protocol + repo-specific quirks
├── REQ-1.md                 # Round 1 audit request from BE
├── RESP-1.md                # Round 1 audit response from auditor
├── QUERIES-1.md             # in-round clarifications (append-only, optional)
├── REQ-1-supplement.md      # optional new-area scope additions
├── REQ-2.md                 # Round 2
├── RESP-2.md
├── ...
└── HISTORY.md               # optional: closed-round digest, severity tally per round
```

**Naming**:
- `REQ-<N>.md` and `RESP-<N>.md` — N is the round number, not the date. Date appears in document header. This avoids filename drift across timezones and re-rounds.
- `QUERIES-<N>.md` and `*-supplement.md` use the same N.

**Existing audits not in this protocol** (e.g. `audits/SECURITY-AUDIT-2026-04-22.md` style flat files for SGX_project rounds 1-2) are **not retroactively migrated**. They stay as historical artifacts. New rounds start with the next N (e.g. SGX_project starts at REQ-3).

Each repo's `PROTOCOL.md` is a thin wrapper:

```markdown
# This repo's audit protocol

Follows [security-audit-playbook AUDIT-PROTOCOL.md v1.0](<reference-to-playbook>).

Repo-specific quirks:
- Severity scheme: default (C/H/M/L/I)
- Disclosure mode default: private (this repo contains attacker recipes)
- PoC requirement: required for all C/H findings; opt-out per finding
- <any other repo-specific notes>
```

---

## 2. REQ-N — required template

```markdown
# REQ-<N> — <repo-name> audit verification request

**Round**: <N>
**Date**: YYYY-MM-DD
**Baseline commit**: <40-char SHA>
**Audit target HEAD**: <40-char SHA>
**Disclosure mode**: private | public-at-delivery
**Scope**: verify-only | sweep | new-area | mixed
**Severity scheme**: <ref to playbook §13.1 default, or override>
**PoC requirement**: required-for-C/H | opt-out (see per-finding overrides below)
**Expected RESP turnaround**: <e.g. 5 working days>

---

## 1. Per-finding verification table

(Required when `scope` includes verification of prior findings.)

| ID    | Title                                | Severity (origin) | BE-claimed status | Fix commit(s) (1-3 SHA) | Expected verification |
|-------|--------------------------------------|-------------------|-------------------|-------------------------|----------------------|
| S-C1  | FROST endpoint without session_key  | Critical          | FIXED             | a1b2c3d, e4f5g6h        | session_key gate present in all 4 ecalls |
| S-C2  | ECDH identity reuses wallet key     | Critical          | FIXED             | i7j8k9l                 | independent ECDH keypair sealed at boot |
| ...   | ...                                  | ...               | ...               | ...                     | ...                  |

**Rules:**
- Each row is one prior finding. EVERY prior finding from the round under verification appears here, even ones BE believes were closed in a prior round.
- `Fix commit(s)`: 1-3 SHA per finding. If a fix spans more than 3 commits, BE consolidates into a fewer-commits PR before opening REQ.
- `Expected verification`: 1-line description of what auditor should confirm. Not a fix recipe; a verification expectation.
- BE-claimed statuses: FIXED | MITIGATED | DEFERRED | ACKNOWLEDGED-AS-DESIGN | DISPUTED.

## 2. Diff scope

```
git diff <baseline>..<HEAD> -- <audit-relevant-paths>
```

Where `<audit-relevant-paths>` is a precise list of paths the auditor should review. Excludes:
- `node_modules/`, `vendor/`, lockfiles
- Documentation / docstring-only changes (unless documenting a finding fix)
- Test infrastructure renames / utility refactors
- Changelog / release-notes commits

If the diff is unavoidably large (>1000 LOC across non-trivial files), BE provides a **consolidation summary** at the top of REQ explaining which commits group with which finding.

## 3. Look especially at

(Free-form list of focus areas. Not findings — focus directions.)

- New attack surface introduced since baseline (e.g. "Tier-3 cluster libp2p mesh")
- Areas where BE's confidence is lower
- Areas where review has historically been thin
- Recent third-party changes the BE consumed (CVEs, dependency upgrades)

## 4. BE concerns / deferred / risk-accepted items

For EACH item BE chose not to fix or chose to defer:

### Item: <ID + short title>
- **Loss surface**: <what an exploit costs>
- **Likelihood**: <attacker effort vs payoff; concrete preconditions>
- **Alternative considered**: <what else BE looked at>
- **Acceptance reason**: <why this is acceptable>
- **Trigger to revisit**: <condition under which this should be re-evaluated>

This is BE's structured risk-acceptance, not a hand-wave. Auditor either signs off ("AGREED" in RESP) or pushes back ("DISAGREED — here's why").

## 5. Incremental scope (out-of-round-N)

(Optional. New attack surfaces or context items NOT being verified as part of round N. Auditor may take into round if bandwidth permits, otherwise defers to REQ-N+1.)

- Item 1: <description>
- Item 2: <description>

## 6. BE process notes (optional)

(Anything the BE wants to flag about the round itself: scope tradeoffs, time pressure, open questions to discuss in QUERIES-N.md.)

---

*End of REQ-<N>.*
```

**Per-finding PoC opt-out**: if BE wants the auditor to skip PoC for a specific Critical/High finding (e.g. compute-bound or operationally infeasible to PoC), mark `PoC: opt-out — <reason>` in the per-finding table. Auditor may still produce PoC if disagreed.

---

## 3. RESP-N — required template

```markdown
# RESP-<N> — <repo-name> audit verification response

**Round**: <N>
**Date**: YYYY-MM-DD
**Auditor identity**: <git identity>
**REQ reference**: REQ-<N>.md @ commit <SHA>
**Baseline confirmed**: <baseline SHA from REQ — auditor confirms reading from this point>
**Audit target HEAD confirmed**: <HEAD SHA>

---

## 1. Per-finding verdicts

EVERY row in REQ-<N>.md §1 receives a verdict. No omissions.

| ID    | Title                                | Verdict     | Notes                                                       |
|-------|--------------------------------------|-------------|-------------------------------------------------------------|
| S-C1  | FROST endpoint without session_key  | ✅ CLOSED   | session_key check present at pool_handler.cpp:1612, etc.    |
| S-C2  | ECDH identity reuses wallet key     | ⚠ PARTIAL  | new ECDH key, but legacy callsite at file:line still reuses |
| S-H3  | DCAP /tmp TOCTOU                    | ❌ NOT CLOSED | unlink missing for `.dcap_helper` itself                  |
| S-M4  | <other>                             | ↩ REGRESSED | fix from round 1 reverted in commit X                      |
| ...   | ...                                  | 🔍 SCOPE-OUT | not in this round per BE concerns §4 — auditor agreed     |

**Verdict legend:**
- ✅ **CLOSED** — fix verified, finding closed
- ⚠ **PARTIAL** — fix partial; remaining issue described in Notes
- ❌ **NOT CLOSED** — claimed fix does not address the finding; details in Notes
- ↩ **REGRESSED** — previously closed, now broken again
- 🔍 **SCOPE-OUT** — explicitly deferred per REQ §4 or §5; auditor agreed (not closed, but not blocking PASS)

## 2. New findings (this round)

For EACH finding raised this round, full card:

### F-<round>-<severity-letter><index>: <title>
- **Severity**: Critical | High | Medium | Low | Info
- **File**: <file:line>
- **Root cause** (1-3 sentences)
- **Quoted code** (verbatim from the file)
- **Exploit scenario** (numbered, walked through)
- **Result of exploitation** (operator terms — required for C/H)
- **PoC** (required for C/H by default; per-finding opt-out from REQ acknowledged here)
- **Suggested fix** (concrete code direction)
- **Confidence**: Confirmed | Likely | Speculative
- **Pattern family** (cross-reference to playbook DEFI-ATTACK-PATTERNS.md or category)

ID format: `F-<round>-<C|H|M|L|I><index>` (e.g. `F-3-H1`, `F-3-M2`). The round prefix prevents ID collisions across rounds.

## 3. BE concerns evaluation

For each item from REQ-<N>.md §4:

### Item: <ID + title>
- **Acceptance**: AGREED | DISAGREED | NEEDS-MORE-INFO
- **If DISAGREED**: <reason; suggested risk-acceptance path or required mitigation>
- **If NEEDS-MORE-INFO**: <specific question; if not resolved here, raise in QUERIES-N>

## 4. Verdict

- **PASS** — round closed. No further round required for the items in scope.
- **NEEDS-ANOTHER-ROUND** — list of items to address, with required severity tier (e.g. "address F-3-H1 + F-3-H2 before next REQ").

## 5. Diff-check trigger condition (only when verdict = PASS)

When should the next round be triggered? Examples:

- Enclave rebuild → MRENCLAVE change
- Signing path touched (any file under `src/core/wallet/` or analogue)
- More than N commits since baseline (e.g. 50)
- Dependency bump for: <list>
- External CVE in: <list>
- Calendar trigger: every X months regardless

If verdict ≠ PASS, omit this section.

## 6. Process feedback (optional)

- What BE could improve in next REQ (e.g. "consolidate fix commits", "tighten diff scope")
- Auditor self-critique (e.g. "did not have bandwidth for §5 incremental scope this round")
- Suggested protocol adjustments (these go to playbook AUDIT-PROTOCOL.md as PRs)

---

*End of RESP-<N>.*
```

---

## 4. QUERIES-N.md — in-round clarifications

Append-only file for Q+A during a round. **Verdicts NEVER live here — only in RESP.**

```markdown
# QUERIES-<N> — <repo-name>

## 2026-05-01 14:23 UTC — auditor → BE
Q: Is `0xABC123...` the expected MRENCLAVE for v1.3.0 enclave at HEAD?

## 2026-05-01 14:30 UTC — BE → auditor
A: Yes. Signed by mrsigner key at commit `def456`. Reference: `enclave_ceremony_2026-04-22.md`.

## 2026-05-01 16:05 UTC — auditor → BE
Q: REQ-3 §1 row S-H4 lists fix commit `c44c32d` but that commit only renames a function. Where is the actual fix?

## 2026-05-01 16:18 UTC — BE → auditor
A: Apologies — actual fix is `c44c32e` (typo in REQ). Confirmed.
```

**Rules:**
- Each block: timestamp (UTC), direction (auditor → BE / BE → auditor), Q or A label.
- Append only. No edits to prior blocks.
- Every Q gets an A before round closes (else the question becomes part of RESP §3 NEEDS-MORE-INFO).
- Format clarification questions only. Verdicts and findings always go to RESP.

---

## 5. Cycle state machine

```
                     ┌──────────────────┐
                     │ BE writes REQ-N  │
                     │  on audit/REQ-N  │
                     │  branch + opens  │
                     │  PR vs main      │
                     └────────┬─────────┘
                              │
                              ▼
                  ┌───────────────────────┐
                  │ Auditor reads REQ,    │
                  │ uses QUERIES-N if     │
                  │ clarifications needed │
                  └───────────┬───────────┘
                              │
                              ▼
                  ┌───────────────────────┐
                  │ Auditor writes RESP-N │
                  │ on same branch        │
                  │ (private mode) OR     │
                  │ counter-PR (public)   │
                  └───────────┬───────────┘
                              │
              ┌───────────────┴────────────────┐
              ▼                                ▼
       ┌─────────────┐                ┌──────────────────┐
       │ verdict =   │                │ verdict =        │
       │ PASS        │                │ NEEDS-ANOTHER-   │
       │             │                │ ROUND            │
       └──────┬──────┘                └─────────┬────────┘
              │                                 │
              ▼                                 ▼
       ┌─────────────┐                ┌──────────────────┐
       │ Branch      │                │ BE addresses     │
       │ merged to   │                │ items in scope,  │
       │ main as     │                │ then opens       │
       │ historical  │                │ REQ-N+1          │
       │ record      │                │ (back to top)    │
       └─────────────┘                └──────────────────┘
```

---

## 6. Branch / PR shape

### BE side

1. Branch `audit/REQ-<N>` off main.
2. Add `docs/audit/REQ-<N>.md` (and supplements / queries file).
3. Open PR vs main with title: `Audit round <N> — verification request (REQ-<N>)`.
4. PR body: 1-paragraph summary + link to `docs/audit/REQ-<N>.md`.
5. PR labels (recommended): `audit`, `audit-round-<N>`, severity-of-prior-round (if any).

### Auditor side — private disclosure mode

1. Push `RESP-<N>.md` (and `QUERIES-<N>.md` updates) to the same `audit/REQ-<N>` branch.
2. Do not open a counter-PR.
3. PR remains open until verdict = PASS, at which point BE merges to main.
4. Post-merge: branch is deleted; history retained in main.

### Auditor side — public-at-delivery mode

1. Auditor opens **counter-PR** off the same `audit/REQ-<N>` branch (or off main, if private branch was already merged).
2. Counter-PR title: `Audit round <N> — response (RESP-<N>)`.
3. PR body: severity tally + link to RESP-<N>.md.
4. Findings written in **reviewer-spec form** (no attacker recipes). Severity reasoning explicit.
5. BE merges when verdict = PASS, OR a follow-up REQ-N+1 supersedes.

### Why not just chat?

- Git history is the source of truth. Future auditors (rotating, re-audit, post-incident) can replay the cycle from REQ-1/RESP-1 forward.
- Auditor's RESP commits are signed by the auditor's git identity — accountability symmetric with BE's commit signature.
- No information loss across rotations (different auditor, different BE engineer).

---

## 7. Severity scheme

Default scheme — used unless REQ overrides:

- **Critical** — non-trivial loss-of-funds, remote takeover, signature forgery, key custody compromise. Production-blocker.
- **High** — exploitable confidentiality/integrity breach with bounded-but-non-trivial impact. Frequently production-blocker.
- **Medium** — exploitable but bounded by small economic ceiling, or operationally hard to trigger.
- **Low** — defense-in-depth, hardening, edge-case operational risk.
- **Info** — observation, future-proofing, pattern-family note, regression-monitor trigger.

For external engagements (Sherlock, C4A, Cantina, Immunefi), the platform's scheme overrides this default. State the override in REQ §header.

---

## 8. Auditor identity & signing

- Auditor's git identity in commits **must be distinct** from BE's identity.
- For multi-tenant setups: auditor uses a single dedicated identity across all repos (e.g. `audit-claude` or whatever maintainer-supplied identity is used).
- For Olas / Valory internal audits: auditor identity = whatever maintainer's repo policy requires (usually a Valory-owned dedicated email).
- Commits SHOULD be GPG-signed when the repo's branch protection requires it; auditor announces signing key in the first round's RESP.

---

## 9. Disclosure mode rules

REQ-N.md `Disclosure mode` field MUST be one of:

### `private`

- Findings may be written in attacker-recipe form (full PoC, exploit sequences, MRENCLAVE values, key derivation steps).
- Branch stays unpublished or behind branch protection until verdict = PASS.
- After PASS:
  - Option A (default for SGX-style projects): merge to main as historical record. The repo becomes the disclosed audit trail.
  - Option B: rebase to scrub the most sensitive attacker recipes, then merge. Use only if maintainer / regulator demands.

### `public-at-delivery`

- Findings written in **reviewer-spec form**:
  - Describe the bug class, not the exploit walkthrough.
  - Severity reasoning explicit (so judges/external readers can independently verify).
  - PoC may live in a separate private gist; reference rather than inline.
- Counter-PR shape preferred.
- After verdict = PASS, branch may be merged immediately as the public audit record.

### Mode change during round

If BE flips disclosure mode during a round (rare — usually because the maintainer realized the finding warranted a public CVE), the change is recorded as a **new commit on the branch** with a one-line note. Auditor re-formats RESP-N for the new mode and pushes a new commit.

---

## 10. Failure modes / dispute path

### Disagreement on a verdict

If BE disagrees with auditor's verdict:
1. BE writes counter-position in **next REQ-N+1** under §4 (BE concerns) with explicit "we accept this risk because…".
2. Auditor responds in next RESP-N+1: AGREED | DISAGREED | NEEDS-MORE-INFO.
3. If DISAGREED twice, escalate to second-opinion auditor or external review. Final disposition recorded in RESP with rationale.

### Auditor unable to verify (insufficient info / tooling)

- RESP marks the finding 🔍 SCOPE-OUT with reason ("requires fork test infrastructure not available", "third-party contract bytecode unreadable", etc.).
- BE provides the missing infrastructure or accepts that this verdict carries lower confidence.

### Out-of-band emergency findings

If auditor discovers a Critical not in REQ scope, mid-round, with active exploit risk:
1. Pause the round.
2. Write a 1-page out-of-band advisory directly to BE (private channel — Slack / encrypted email — NOT git).
3. Once BE deploys mitigation, document the finding in next RESP-N or RESP-N+1 (if mitigation requires more code).
4. Reason: git is not always low-latency enough for active-exploit-risk scenarios.

---

## 11. Cross-project consistency

This protocol applies uniformly across:
- 77ph/SGX_project (EthSignerEnclave)
- 77ph/xrpl-perp-dex (perp-DEX + enclave)
- 77ph/phoenix-rs
- valory-xyz/wildcard
- valory-xyz/autonolas-* family
- All future internal audits

Each repo's `docs/audit/PROTOCOL.md` is a thin wrapper:

```markdown
# <Repo> audit protocol

This repo follows the [security-audit-playbook AUDIT-PROTOCOL.md v1.0](<reference>).

## Repo-specific quirks

- **Severity scheme**: default (C/H/M/L/I) unless overridden per-round
- **Disclosure mode default**: private | public-at-delivery
- **PoC requirement**: required for C/H by default
- **Audit-relevant paths** (default diff filter):
  - `EthSignerEnclave/Enclave/`
  - `EthSignerEnclave/App/`
  - `phoenix-orchestrator/src/`
  - <…>
- **Out-of-scope paths** (default exclusions on top of universal ones):
  - `<paths excluded for this specific repo>`

## Round history

| Round | Status | RESP verdict | Notes               |
|-------|--------|--------------|---------------------|
| 1     | closed | PASS         | <link to RESP-1>    |
| 2     | closed | PASS         | <link to RESP-2>    |
| 3     | open   | (in-flight)  | <link to REQ-3>     |
```

The wrapper is updated each time a round closes (HISTORY entry appended).

---

## 12. Deviations

When deviating from this protocol, REQ-N.md MUST state explicitly which fields/sections are skipped or modified, and why. Auditor may push back if deviation undermines verification quality.

Examples of acceptable deviations:
- Smaller scope round (e.g. single-finding verification): REQ §1 has 1 row, §3 / §4 / §5 may be empty.
- Time-boxed round (e.g. "audit only 2 hours of attention available"): REQ states the budget; auditor produces best-effort RESP within bound.
- Mixed-scope round: §1 verifies prior, §3 highlights new areas — both populated.

Examples of deviations auditor will reject:
- REQ omits per-finding fix commits ("see the diff" — not acceptable).
- REQ requests verification with no baseline SHA (cannot establish "as-of" point).
- REQ writes findings in RESP format pretending they're closed (BE-claimed status without code reference).

---

## 13. Versioning

This protocol is versioned alongside the security-audit-playbook.

- **v1.0** (2026-05-01) — initial protocol. SGX_project, perp-DEX, phoenix-rs, autonolas-*, wildcard.
- Future versions: changes recorded in playbook CHANGELOG; breaking changes bump major version.

A repo's `PROTOCOL.md` wrapper pins the playbook version it follows. Repos may upgrade at their own pace; auditor may push for upgrade if the older version's gaps materially impact verification.

---

## 14. Quick checklist for first-time adopters

When introducing this protocol to a new repo:

1. Create `docs/audit/` folder.
2. Write `docs/audit/PROTOCOL.md` (the thin wrapper, per §11 above).
3. List existing audit artifacts as **historical** (do not migrate to REQ-N format).
4. Decide round numbering: continue from prior count (e.g. SGX_project starts at REQ-3 since it had 2 informal rounds), or start fresh (REQ-1).
5. BE writes first REQ-N per §2 template.
6. Auditor responds with RESP-N per §3 template.
7. Update repo's `PROTOCOL.md` HISTORY table when round closes.

---

*End of AUDIT-PROTOCOL v1.0.*
