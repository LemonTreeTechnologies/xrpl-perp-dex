# Intro email to XRPL Grants team

**To:** `info@xrplgrants.org`
**From:** `<team contact>`
**Subject:** Perp DEX on XRPL via Intel SGX — early introduction for Spring 2026 wave
**Attachment:** link to grant package (or PDF export of `grant/xrpl-grants-application.en.md`)

---

Hello XRPL Grants team,

I understand the 2025 wave is closed and Spring 2026 programming is still
being finalized. I am reaching out now to introduce a project that I
believe fits the Software Developer Grants track tightly, and to ask
whether a pre-announcement conversation is possible.

**Project in one sentence:** a perpetual futures DEX that settles
natively in RLUSD on XRPL mainnet by using Intel SGX enclaves as the
"smart contract" layer, with user funds held in an XRPL-native
`SignerListSet` 2-of-3 multisig between three independent hardware
operators.

**Why this matters for XRPL:** XRPL does not support smart contracts,
so the DeFi derivatives stack currently either skips XRPL entirely or
relies on EVM sidechains that inherit Solidity's well-known attack
surface (re-entrancy, flash loans, MEV). We show that the combination
of XRPL's native multisig, fast finality, and RLUSD is sufficient to
build an institution-grade derivatives venue **without** a second chain
and **without** custodial trust, using an Intel SGX enclave as the
computation layer and XRPL as the settlement layer.

**Why I am reaching out now, before the wave opens:**

The project is already past MVP and well into verifiable traction. The
API is live at https://api-perp.ph18.io and I would like reviewers to
be able to poke at it before the formal application window opens,
rather than discover us in a pile of fresh submissions:

- **Ten verified on-chain transactions on XRPL testnet**, one per
  failure-mode scenario from our research document 07, including 2-of-3
  withdrawal, malicious-operator rejection, three SignerListSet rotation
  scenarios (SGX compromise, hardware failure, cloud migration), and a
  2-of-3 → 3-of-4 expansion.
  Escrow: https://testnet.xrpl.org/accounts/rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx

- **Three Azure DCsv3 nodes** with Intel SGX, all returning valid
  4,734-byte Intel-signed DCAP quotes. Full deployment recipe published
  with two newly-discovered Azure-specific gotchas documented.

- **Multi-operator sequencer election verified end-to-end** on the live
  3-node cluster, including a network partition (split-brain) test via
  `iptables DROP` — minority keeps its old leader, majority elects a
  new one, and reconvergence happens in 3 seconds after the network
  heals. Report: `research/election-split-brain-test-report.md`.

- **9 of 9 failure-mode scenarios** pass reproducibly with a single
  command against the live cluster (`python3 tests/scenarios_runner.py all`).
  No mocks; the signatures are real ECDSA produced inside the SGX
  enclaves.

- Full test coverage: 86 Rust unit tests, 22 Python e2e tests, 19
  enclave invariant tests, 9 scenarios. External security audit with
  52 findings, 50 fixed and 2 documented as by-design for single-
  operator MVP.

- Open source from day one under BSL 1.1 (auto-converts to Apache 2.0
  in four years). Both repositories public at
  https://github.com/LemonTreeTechnologies/xrpl-perp-dex and
  https://github.com/77ph/xrpl-perp-dex-enclave.

**The ask:**

1. Please accept this project into the "radar" for the Spring 2026
   wave so we are not lost in the queue when applications open.
2. If there is any pre-application conversation, office hours, or
   technical review offered before the wave opens, I would welcome it.
3. If a demo would be useful, I am available to join any call. I will
   also be presenting at **Hack the Block, Paris Blockchain Week,
   April 11-12, 2026** (Challenge 2: Impact Finance) — if anyone from
   the XRPL Grants team is attending, I would love to show the live
   DCAP attestation step and the on-chain multisig flow in person.

**Full application package** (single `.md` file, ~900 lines, including
problem statement, architecture, team, 5 milestones with 30% product +
70% growth split, budget breakdown to USD $150,000, 12-month roadmap,
sustainability plan, XRPL integration strategy, risks, and links to all
verifiable evidence): see attachment, or browse live at
`grant/xrpl-grants-application.en.md` in the
`xrpl-perp-dex` repository.

**Proof of traction pack** (every claim resolves to a URL, tx hash,
commit, or file): `grant/proof-of-traction.md` in the same repository.

Thank you for your time. I understand the timing is awkward given the
2025 wave just closed, but the project is moving fast and I would
rather introduce it now than wait for the announcement and lose a
month.

Best regards,

ph18.io team

---

## Notes for the sender (not part of the email)

- **Before sending:** replace `<team contact>` with the actual sender
  email. The reply-to should be a monitored address.
- **Attachment options:**
  1. PDF export of `grant/xrpl-grants-application.en.md` (pandoc or
     print-to-PDF from a markdown viewer). Recommended — reviewers
     prefer attachments over git links.
  2. Direct GitHub link — include a specific commit SHA so the
     reviewer sees the exact version you are pitching.
  3. Both (attach the PDF, include GitHub link as a backup).
- **Tone:** the email is deliberately long (~500 words) because the
  project has a lot of concrete proof points. If you want a shorter
  version, cut everything between "Why I am reaching out now" and
  "The ask" to produce a ~200-word variant.
- **Follow-up schedule:** if no reply within 10 business days, send
  a one-paragraph follow-up referencing the original message. Do not
  follow up twice — respect their queue.
- **If they reply asking for a call:** bring the asciinema recording
  (`presentation/demo.cast`) pre-loaded, and have an Azure VM ssh
  session ready to show a live DCAP attestation quote generation.
- **If you are replying to a "wave is closed, please wait" auto-reply:**
  that is fine. The goal of this email is to get on their radar, not
  to skip the queue. Ask them to confirm receipt so you know the
  intro landed.
