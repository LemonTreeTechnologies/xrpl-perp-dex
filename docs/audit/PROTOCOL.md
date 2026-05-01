# xrpl-perp-dex audit protocol

This repo follows [AUDIT-PROTOCOL.md v1.0](./AUDIT-PROTOCOL.md).
Canonical source: `~/llm-work/security-audit-playbook/AUDIT-PROTOCOL.md` (auditor side).

## Repo identity

- **BE**: `dev-perp`
- **Visibility**: PUBLIC
- **Role in split-repo**: BACK-PORT TARGET — receives `X-*` findings on verdict=PASS
- **Sister repo**: `77ph/xrpl-perp-dex-enclave` (private, canonical audit cycle)

## Conventions

- **Severity scheme**: default (C/H/M/L/I per playbook §13.1)
- **Disclosure mode default**: `public-at-PASS` (reviewer-spec form, no attacker recipes)
- **PoC requirement**: PoC referenced (not inlined) since this repo is public; full PoC stays in private sister
- **Audit-relevant paths** (default diff filter):
  - `orchestrator/src/` — host-side code (auth, API, orderbook, P2P, signing relay, governance, DKG coordinator, deploy, withdrawal, xrpl monitor)
  - `orchestrator/Cargo.toml` — dependency surface
  - `orchestrator/tests/` — integration tests that lock wire shape (audit-bar per Appendix B/C)
  - `docs/multi-operator-architecture.{en,ru}.md` — trust model authority
  - `docs/development-operating-model.{en,ru}.md` — operating mode + sync procedure
  - `docs/cluster-trust-model-decision.md` — DCAP cross-attestation ADR
  - `docs/why-not-threshold-ecdsa.{en,ru}.md` — SGX-FROST vs CGGMP/DKLS ADR
  - `docs/sgx-enclave-capabilities-and-limits.{md,-ru.md}` — SGX trust framing
  - `docs/deployment-procedure{,-ru}.md` — ceremonial steps
  - `docs/accepted-platform-risks.md` — known risks accepted (P-1 etc.)
  - `coverage-baseline.json` + `scripts/check-coverage.py` — coverage gate
  - `.github/workflows/` — CI definitions
- **Out-of-scope paths** (default exclusions on top of universal ones):
  - `references/` — cited external PDFs only
  - `o1/` — non-project stash (untracked)
  - `audit-reviews/` — interim directory superseded by `docs/audit/` (retained for history; new artefacts go to `docs/audit/`)
  - `mainnet-sync-log.md` — operational log, not code
  - `SECURITY-AUDIT.md` + `SECURITY-REAUDIT-{1,2,3,4}.md` + `SECURITY-REAUDIT-4-FIXPLAN.md` — pre-protocol informal audit history (read-only historical record; new rounds use `docs/audit/REQ-N.md` + `RESP-N.md` per protocol)
  - `docs/post-audit-status.md` — summary of pre-protocol audit closure (read-only)
  - `docs/frontend-api-guide.md`, `docs/perp-dex-faq*.md` — user-facing docs
  - `docs/btc-perp-dex-feasibility*.md`, `docs/clob-vs-amm-alignment*.md`, `docs/post-hackathon-specs.md` — product roadmap discussion docs
  - `docs/xls-survey-for-perp-dex*.md`, `docs/sgx-vs-tdx-roi.md`, `docs/comparison-arch-network*.md` — research / reference comparisons

## ID prefixes

- `X-*` — DEX findings (this repo)
- `E-*` — enclave findings (NOT back-ported here; live exclusively in `77ph/xrpl-perp-dex-enclave`)
- **No Criticals here** per split-repo rule. If a finding's exploitation requires both repos, the canonical writeup stays in the private sister; only the public reviewer-spec form back-ports here on PASS.

## Round history

| Round | Status | Public artifact | Notes |
|---|---|---|---|
| 1 | informal (closed) | [`SECURITY-AUDIT.md`](../../SECURITY-AUDIT.md) + [`SECURITY-REAUDIT.md`](../../SECURITY-REAUDIT.md) | Pre-protocol informal audit |
| 2 | informal (closed) | [`SECURITY-REAUDIT-2.md`](../../SECURITY-REAUDIT-2.md) | Pre-protocol informal audit |
| 3 | informal (closed) | [`SECURITY-REAUDIT-3.md`](../../SECURITY-REAUDIT-3.md) | Pre-protocol informal audit |
| 4 | informal (closed) | [`SECURITY-REAUDIT-4.md`](../../SECURITY-REAUDIT-4.md) + [`SECURITY-REAUDIT-4-FIXPLAN.md`](../../SECURITY-REAUDIT-4-FIXPLAN.md) | Pre-protocol; baseline `b4b07ce` is the public-side reference for protocol Round 5 |
| 5 | open (REQ-5 lives in private sister) | (pending PASS — back-port arrives here on verdict=PASS) | First protocol round; back-port pulls X-* findings on PASS into this repo's `docs/audit/RESP-5.md` |

## Notes for back-port from private sister

When verdict=PASS lands in `77ph/xrpl-perp-dex-enclave/docs/audit/RESP-N.md`, BE will create `docs/audit/RESP-N.md` here containing **only** the X-* findings reformulated in reviewer-spec form (no attacker recipes, no PoC bodies — references to private repo by commit SHA). E-* findings are not back-ported per split-repo rule.

## Migration note

`audit-reviews/` was created on 2026-05-01 as the interim directory under the original `docs/development-operating-model.md` §2 format. The cross-project AUDIT-PROTOCOL.md v1.0 supersedes that format with `docs/audit/REQ-N.md` + `RESP-N.md` + `QUERIES-N.md`. The interim directory is retained empty for historical reference; all new audit artefacts in this repo go to `docs/audit/`.
