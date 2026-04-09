# XRPL Grants application package

This directory contains the full grant application for `xrpl-perp-dex`
to the XRPL Grants Software Developer Grants Program.

## Files

| File | Purpose |
|---|---|
| `xrpl-grants-application.en.md` | Full application, English (primary submission) |
| `xrpl-grants-application.md` | Full application, Russian (per bilingual docs rule) |
| `proof-of-traction.md` | Single-file verification pack — every claim resolves to a URL/hash/commit |
| `intro-email.md` | Draft email to `info@xrplgrants.org` for early-awareness submission |

## Status as of 2026-04-09

- **XRPL Grants 2025 wave:** closed
- **Spring 2026 wave:** not yet announced
- **Our plan:** submit intro email now, use package for Hack the Block
  Paris (April 11-12, 2026, Challenge 2: Impact Finance), be ready to
  submit formally the day Spring 2026 opens.

## Requested amount

**USD $150,000 over 12 months**, structured per XRPL Grants guidelines
as 30% product/integration milestones + 70% growth milestones.

| Milestone | Type | Amount | Target month |
|---|---|---|---|
| M1 — Rust multisig integration | Product | $22,500 (15%) | Month 2 |
| M2 — Audited mainnet launch with RLUSD | Product | $22,500 (15%) | Month 5 |
| M3 — First $50K mainnet TVL | Growth | $30,000 (20%) | Month 7 |
| M4 — 500 unique wallets | Growth | $37,500 (25%) | Month 9 |
| M5 — $1M cumulative volume | Growth | $37,500 (25%) | Month 12 |

## Key selling points (for the 30-second version)

1. **Live MVP on real SGX hardware** — api-perp.ph18.io, three Azure
   DCsv3 nodes with verified DCAP attestation
2. **10 on-chain testnet transactions as proof** of the 2-of-3 multisig
   withdrawal flow, covering all 9 operator-level failure scenarios
   from `research/07-failure-modes-and-recovery.md`
3. **Unique XRPL integration** — our entire security model relies on
   `SignerListSet` native multisig, RLUSD settlement, and XRPL's
   no-MEV / no-mempool property. On any chain without these primitives
   the project would look completely different.
4. **Direct response to the Drift Protocol incident** (April 2026,
   $280M loss to social engineering on 5-of-N human multisig) — our
   signers are processors, not humans, and cannot be social-engineered.

## Full package word count

- English application: ~6,500 words
- Russian application: ~6,500 words
- Proof of traction: ~2,000 words
- Intro email: ~550 words
- **Total:** ~15,500 words across 4 documents

## How to use this package

1. **Before the wave opens:** send `intro-email.md` to
   `info@xrplgrants.org` (see notes at the end of the email file).
2. **For Hack the Block Paris pitch:** use
   `xrpl-grants-application.en.md` as the talk outline. The executive
   summary (§1) and the "Traction and proof points" (§5) are the
   5-minute version.
3. **When Spring 2026 wave opens:** submit
   `xrpl-grants-application.en.md` as the project narrative, attach
   `proof-of-traction.md` as supporting evidence, link to the
   GitHub repos for live code.
4. **For reviewer due diligence:** point them at
   `proof-of-traction.md` — it is specifically designed so a reviewer
   can verify 80% of the claims in under 30 minutes without contacting
   the team.
