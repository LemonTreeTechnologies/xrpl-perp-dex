# XRPL Grants Application — Perpetual Futures DEX on XRPL via Trusted Execution Environments

**Project name:** Perp DEX on XRPL (working title: `xrpl-perp-dex`)
**Applicant:** ph18.io
**Application date:** 2026-04-09
**Requested amount:** USD $150,000 (12-month program)
**Funding track:** Software Developer Grants Program
**Live demo:** https://api-perp.ph18.io
**GitHub:**
- https://github.com/77ph/xrpl-perp-dex (Rust orchestrator, tests, research)
- https://github.com/77ph/xrpl-perp-dex-enclave (C/C++ SGX enclave, perp engine)

---

## 1. Executive summary

XRPL does not support smart contracts. That gap blocks the entire DeFi
derivatives stack — perpetual futures, options, margin lending — from being
built natively on the ledger. Projects either leave for EVM sidechains
(sacrificing XRPL's security and performance guarantees) or build
centralized custodial services (sacrificing the trustless property that
makes DeFi interesting).

We replace smart contracts with **Intel SGX Trusted Execution Environments**.
An enclave executes the same logic a smart contract would — margin engine,
position tracking, liquidation, funding rate, withdrawal signing — but with
hardware-enforced integrity. XRPL is used for what it is best at: settlement.

The result is a **perpetual futures DEX that settles natively in RLUSD on XRPL
mainnet**, with user withdrawals gated by a 2-of-3 multisig of independent SGX
operators using XRPL's native `SignerListSet`. No sidechain, no bridge, no
custom L1.

The PoC is **live today** at `https://api-perp.ph18.io`. We have a working
margin engine, CLOB, price feed, WebSocket gateway, DCAP remote attestation
on Azure DCsv3, and a verified end-to-end 2-of-3 multisig withdrawal flow on
XRPL testnet with **10 on-chain transaction hashes as proof** across all
nine failure-mode scenarios from our research document 07.

The grant would fund the 12-month path from working testnet PoC to
**audited XRPL mainnet launch with RLUSD settlement**.

---

## 2. Problem

### 2.1 XRPL has no DeFi derivatives

XRPL has a native spot DEX (CLOB) since inception and an AMM since XLS-30,
but it cannot support synthetic assets, leverage, perpetual futures, or any
instrument that requires Turing-complete state. The constraints:

| XRPL mainnet | Status |
|---|---|
| Built-in CLOB DEX (spot) | Working since launch |
| AMM (XLS-30) | Working since 2024 |
| RLUSD stablecoin | Live, >$1.2B market cap |
| **Smart contracts** | **Not supported** |
| **Hooks** | **Not on mainnet** (Xahau sidechain only) |
| **Derivatives** | **Not possible without off-ledger logic** |

The consequences are tangible: RLUSD holders who want yield or hedging
exposure must bridge to Ethereum or Solana, losing XRPL's settlement
guarantees and regulatory positioning, and paying bridge fees and delays.

### 2.2 The "off-chain services" alternatives have their own problems

Teams that don't want smart contracts end up in one of three camps, all
with their own pitfalls:

1. **Centralized order book** with custodial user deposits — trustful, single
   point of failure, regulatory exposure
2. **EVM sidechain** (XRPL EVM Sidechain, Xahau) — loses XRPL's settlement
   guarantees, inherits all Solidity attack surface (re-entrancy, flash
   loans, MEV) and the operational burden of a second chain
3. **Multi-operator multisig with human signers** — the social-engineering
   target that cost Drift Protocol $280M in April 2026 when attackers spent
   six months building trust with multisig members and then used Solana
   durable nonces to drain the protocol during a routine insurance-fund test

All three approaches accept trade-offs that contradict the whole point of
building on a trust-minimized ledger.

---

## 3. Solution: SGX enclave as the "smart contract"

### 3.1 Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                       User browser                           │
└────────────────────────────┬─────────────────────────────────┘
                             │ TLS (X-XRPL-Signature auth)
                             ▼
               ┌──────────────────────────────┐
               │  nginx (api-perp.ph18.io)    │
               │  rate-limit + block internal │
               └──────────────┬───────────────┘
                              │
                 ┌────────────▼──────────────┐
                 │  Orchestrator (Rust)      │
                 │  • CLOB order book        │
                 │  • Price feed (Binance)   │
                 │  • WebSocket push         │
                 │  • Sequencer election     │
                 │  • Validator replay       │
                 └─────┬───────────────┬─────┘
                       │ HTTPS         │ libp2p gossipsub
                       ▼               │
               ┌────────────────┐      │   ┌─────────────┐
               │  SGX Enclave   │      └──▶│  Peer ops   │
               │  (C/C++)       │          │  (2 of 3)   │
               │  • margin      │          └─────────────┘
               │  • positions   │
               │  • liquidation │
               │  • ECDSA sign  │
               │  • sealed state│
               └───────┬────────┘
                       │ secp256k1 + DER
                       ▼
                ┌─────────────────┐
                │   XRPL Mainnet  │
                │  SignerListSet  │
                │  2-of-3 escrow  │
                │  RLUSD tokens   │
                └─────────────────┘
```

### 3.2 Why SGX instead of smart contracts

| Smart contracts (Solidity, Move) | TEE (Intel SGX) |
|---|---|
| Require chain support | Works with any chain including XRPL |
| Public code at runtime (MEV, front-running) | Code runs in encrypted memory |
| Gas on every operation | No gas — compute is paid at host |
| Upgrade requires chain governance | Upgrade requires new DCAP attestation |
| Re-entrancy, flash-loan attack surface | Attack surface is hardware side-channel, not application logic |

### 3.3 Why XRPL is the right settlement layer

- **RLUSD** — a regulated stablecoin with >$1.2B market cap and real
  institutional counterparties
- **3-4 second** finality — fast enough for perp liquidations
- **Native multisig** via `SignerListSet` — a ledger primitive designed
  exactly for N-of-M operator custody, no smart contract required
- **Fees under $0.001** per transaction — fee impact on PnL is negligible
- **No mempool, no MEV** — XRPL's consensus eliminates the frontrunning
  surface that plagues Ethereum-based DEXs

### 3.4 Why SGX is the right TEE

- **Intel SGX with DCAP remote attestation** is the only widely-deployed
  hardware enclave with cloud availability (Azure DCsv3) and a mature
  attestation stack verifiable without contacting Intel at runtime
- **Azure THIM** (Trusted Hardware Identity Management) provides
  provisioned PCK certificates so any third party can verify a 4,734-byte
  SGX Quote v3 against Intel's root of trust
- **Sealed data** binds persistent state to a specific MRENCLAVE — a
  rogue operator cannot tamper with sealed margin state without the
  enclave crashing on boot

---

## 4. Uniqueness and defensibility

### 4.1 We are not Drift

Drift Protocol on Solana lost $280M on April 1, 2026 to a six-month social
engineering attack targeting its 5-of-N Security Council human multisig.
Our entire design is a direct response to that class of failure:

- **Signers are processors, not humans** — the multisig quorum in our
  design is 3 SGX enclaves on geographically separated Azure DCsv3 hosts.
  The enclaves cannot be "convinced" to sign an invalid withdrawal, because
  they run deterministic code that reproves margin on every signing request.
- **Private keys never exist outside the CPU** — the secp256k1 key material
  is generated inside the enclave, sealed to MRENCLAVE, and used only via
  an in-enclave ECDSA routine. The host process has no access.
- **MRENCLAVE is publishable and verifiable** — any user can request an
  attestation quote before trusting the system, hash our open-source build,
  and compare.

### 4.2 We are not Hyperliquid

Hyperliquid runs its own L1 with its own validator set, consensus, and
economic security assumptions. That is a ~$50M engineering investment. We
reuse XRPL's ~$30B economic security by using XRPL as the settlement
layer and treating the enclave strictly as a computation layer. Our
"validator set" is off-the-shelf DCsv3 instances and the "consensus" is
XRPL itself — we only need to prove that each operator's signature came
from a verifiable enclave, which DCAP gives us for free.

### 4.3 We are not a custodial CEX

User funds live in an XRPL-native escrow account secured by a
`SignerListSet` 2-of-3 multisig between 3 independent SGX operators. No
single operator — and no pair colluding against the third — can move user
funds without the enclaves approving. A full audit trail of every deposit
and withdrawal lives on XRPL, not on our servers. Catastrophic loss of all
three operator machines is recoverable from XRPL ledger history alone.

---

## 5. Traction and proof points (as of 2026-04-09)

**Everything below is live and independently verifiable.** URLs and
transaction hashes are included where applicable.

### 5.1 Live infrastructure

| Component | Evidence |
|---|---|
| Public API | https://api-perp.ph18.io (TLS via nginx, CORS, rate limit) |
| OpenAPI spec | `GET /v1/openapi.json` |
| Live WebSocket | `wss://api-perp.ph18.io/ws` pushing ticker events now |
| 3 SGX enclaves | Azure DCsv3 (`20.71.184.176`, `20.224.243.60`, `52.236.130.102`) |
| DCAP attestation | All 3 nodes return 4,734-byte SGX Quote v3, Intel-signed, verified on 2026-04-08 |

### 5.2 On-chain testnet transactions (XRPL testnet)

Escrow account: [`rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx`](https://testnet.xrpl.org/accounts/rM44W1FkXrvwiQ6p4GLNP2yfERA12d4Qqx)

Ten verified transactions across the nine failure-mode scenarios from
`research/07-failure-modes-and-recovery.md`:

| Scenario | Event | XRPL testnet tx hash |
|---|---|---|
| 3.1 One operator offline | 2-of-3 withdrawal succeeds | `FC9C32DBCC422368888518FE143AA0715BA1D1FD6E26B5EC45DA52B471D8A1D0` |
| 3.4 Malicious operator | Honest retry after garbage sig rejected | `90819EC6CBA5ACC549DEF5496B665BBFA959C8FA7887C4500D779B2B917ED25F` |
| 3.5 SGX compromise | Key rotation via SignerListSet | `23E1C0EE68EB000A5C922E8D6D8A68AC042A024D1CF2FB8A437D65D47F11E809` |
| 3.5 | Withdrawal with rotated key | `8274091DFB2A0FA83B33473E31A60E90E24C832DA9B12788259D4C5DD5887F14` |
| 3.6 Hardware failure | SignerListSet after wipe | `B056CA00778054F041335F05054B106E6CBA22713A33611CCCFC924B19E1A686` |
| 3.6 | Withdrawal with replacement key | `F455EBCCBC7B858854E1A1DCCEC86430C53897319BFE6C1D11083B280C68F7BE` |
| 3.7 Cloud migration | SignerListSet rotation | `8899292EE67D800A136741C9A3950054248658625156AD7C23E4CD24E7CA3E23` |
| 3.7 | Post-migration withdrawal | `8931EFAA52AED2351D7BAE0313B84B6157386CD7B73389C4190FF5E4B975CBBB` |
| 3.8 Scaling 2-of-3 → 3-of-4 | SignerList expansion | `B992884B8A19204014AD59D899B0C66D5F3AFF69625C7795E1A5D7E3D80DAA8B` |
| 3.8 | Post-expand 3-of-4 withdrawal | `49629B8F72E8E1624B6C781DEBED3E5D734BA27530492A7A5B2D2C2AFCC7E38F` |

Full report: `research/failure-modes-test-report.md`

### 5.3 Multi-operator sequencer election verified live

On 2026-04-09, the sequencer election state machine (`orchestrator/src/election.rs`)
was verified end-to-end on the live 3-node Azure cluster, including network
partition (split-brain) via `iptables DROP` on port 4001. The test report is
in `research/election-split-brain-test-report.md`. Summary:

- Stable cluster: 5 minutes, zero false failovers
- Kill sequencer → failover in 16.5 s (timeout = 15 s)
- Restart sequencer → reclaim in 8 s
- Network partition → majority elects new leader, minority keeps old (both sides correct)
- Reconnect → single sequencer restored in 3 s
- Persistent libp2p identity (peer_id stable across restarts)
- Heartbeat-level debug observability for operational forensics

### 5.4 Test coverage

- **Rust unit tests**: 86 (auth, orderbook, election, p2p, trading, types, WebSocket)
- **Python integration / e2e**: 22 (auth + trading + WebSocket + multisig)
- **Enclave invariant tests**: 19 (FP8 arithmetic correctness)
- **Failure mode scenarios**: 9/9 pass with on-chain proofs
- **Security audit**: 52 findings, 50 fixed, 2 by-design (documented in `SECURITY-REAUDIT*.md`)

### 5.5 Public demo and documentation

- Asciinema recording of live trading flow + DCAP attestation step
- Marp slide deck (RU + EN) for Hack the Block Paris (April 11-12, 2026)
- Bilingual research documents: 10 topics in `research/` (RU + EN)
- Frontend API guide in `docs/frontend-api-guide.md`
- Russian FAQ for non-trader developers in `docs/perp-dex-faq-ru.md`

---

## 6. Team

**ph18.io** is a two-person team with previous production SGX experience from
a separate signing-infrastructure project (`EthSignerEnclave`). The SGX
enclave used as the base for the perp DEX is a cleaned-up fork of that
production system — we did not start the SGX work from scratch for this
grant.

- **Lead developer / architect** — 10+ years of systems engineering
  experience, primary author of the SGX enclave infrastructure, XRPL
  integration, and Rust orchestrator.
- **Operations / security** — previous production SGX deployment
  experience (Azure DCsv3, DCAP attestation), XRPL testnet multisig
  operations, security audit response.

Both team members are active on the codebase (see `git log` in the public
repositories). Detailed team bios and contact information are available on
request.

---

## 7. Milestones

Per the XRPL Grants structure described in the program FAQ, milestones
split roughly **30% product/integration + 70% growth**. We propose five
milestones over 12 months totaling USD $150,000.

### M1 — Production orchestrator multisig integration (Product, 15% / $22,500)

**Target:** 2026-Q2 end (month 2)

- Port the Python multisig coordinator (`tests/multisig_coordinator.py`) into
  the Rust orchestrator as a first-class withdrawal flow. Current
  `orchestrator/src/withdrawal.rs` is an explicit single-operator MVP
  stub; this milestone replaces it with peer-to-peer multisig signing
  over libp2p gossipsub.
- End-to-end integration test from the orchestrator REST
  (`POST /v1/withdraw`) to an on-chain 2-of-3 multisigned Payment.
- All 9 failure mode scenarios from research doc 07 re-verified against
  the new integrated flow.

**Deliverables:** commit on `master`, passing tests, updated API guide.

### M2 — Audited XRPL mainnet launch with RLUSD (Product, 15% / $22,500)

**Target:** 2026-Q3 mid (month 5)

- Independent security audit of both repositories (`xrpl-perp-dex` and
  `xrpl-perp-dex-enclave`). Audit scope: margin engine, ECDSA signing
  path, XRPL transaction construction, multisig quorum logic,
  DCAP attestation flow.
- All critical and high findings fixed and re-audited.
- XRPL mainnet escrow account provisioned with SignerListSet 2-of-3.
- RLUSD trustlines established for the escrow.
- Mainnet launch of the perp DEX in "restricted beta": open
  invite-only at start, public opening after 2 weeks of live monitoring.
- Public DCAP attestation endpoint documented for user verification.

**Deliverables:** audit report (published), mainnet escrow address, live
public API on mainnet.

### M3 — First $50,000 TVL on mainnet (Growth, 20% / $30,000)

**Target:** 2026-Q3 end (month 7)

- Reach $50,000 total value locked in the XRPL mainnet escrow.
- At least 30 unique user XRPL addresses depositing.
- At least 1,000 orders executed (open + close) on mainnet.
- Trading history queryable from XRPL ledger via `account_tx`.

**Measurement:** independently verifiable on XRPL mainnet via any XRPL
explorer. We will publish a live dashboard at `dashboard.ph18.io`
including all of the above metrics.

### M4 — 500 unique wallets (Growth, 25% / $37,500)

**Target:** 2026-Q4 end (month 9)

- Reach 500 unique XRPL addresses that have opened at least one position.
- Funding-rate system live for 30 consecutive days without manual
  intervention.
- WebSocket gateway sustains >50 concurrent clients without backpressure.

**Measurement:** same dashboard as M3 + public WebSocket connection
statistics endpoint.

### M5 — $1,000,000 cumulative mainnet volume (Growth, 25% / $37,500)

**Target:** 2027-Q1 end (month 12)

- Reach $1M total trading volume on XRPL mainnet since launch.
- 3-of-4 multisig expansion exercised at least once (adding a fourth
  operator).
- Public quarterly post-mortem covering operational incidents.
- Monthly state commitment published to an EVM mainnet (Ethereum or
  Sepolia) via existing CommitmentRegistry contract.

**Measurement:** on-chain volume sum from escrow's XRPL tx history,
verified against the operator's trade database.

---

## 8. Budget breakdown

Total grant request: **USD $150,000 over 12 months**

| Category | Amount | Notes |
|---|---|---|
| Security audit (external) | $45,000 | Joint SGX enclave + Rust orchestrator audit by a reputable blockchain security firm |
| Development (lead dev, 6 months, 50%) | $36,000 | Rust multisig integration, frontend work, operational tooling |
| Development (ops/security, 6 months, 50%) | $24,000 | Azure / XRPL operations, incident response, monitoring |
| Azure DCsv3 hosting (3 × DC2s_v3) | $6,000 | $0.20/hour × 3 nodes × 12 months ≈ $5,256 + headroom for short-lived experimental nodes |
| XRPL mainnet operating costs | $2,000 | Escrow funding, reserve, account activation, fee buffer for ~1M tx |
| Frontend (trading UI) | $15,000 | Contractor for production UI on `perp.ph18.io` |
| Legal / compliance review | $10,000 | Custody structure, XRPL multisig as operator model, terms of service for restricted beta and public opening |
| Monitoring & ops tooling | $5,000 | Grafana / Loki / Prometheus stack, alerting, on-call setup for 3 operators |
| Community & documentation | $5,000 | Developer docs, integration tutorials, XRPL developer Discord presence |
| Contingency / buffer (~1.5%) | $2,000 | Unallocated reserve for unforeseen items |
| **Total** | **$150,000** | |

Funds are disbursed per milestone completion per the XRPL Grants program
schedule: 30% across M1–M2 (product/integration) and 70% across M3–M5
(growth).

---

## 9. 12-month roadmap

```
Month 1  ──▶ M1: Rust multisig integration, internal test pass
Month 2  ──▶ M1 COMPLETE. Audit kickoff.
Month 3  ──▶ Audit rounds 1-2, frontend contractor hired.
Month 4  ──▶ Audit round 3, critical fixes, testnet regression.
Month 5  ──▶ M2: mainnet restricted beta launch with RLUSD.
Month 6  ──▶ Public mainnet opening (end of restricted beta).
Month 7  ──▶ M3: first $50K TVL.
Month 8  ──▶ Funding rate loop stabilized, WebSocket scaled.
Month 9  ──▶ M4: 500 unique wallets.
Month 10 ──▶ Expand to 3-of-4 multisig (add 4th operator).
Month 11 ──▶ Incident retrospective, ops tooling hardening.
Month 12 ──▶ M5: $1M cumulative volume, quarterly post-mortem published.
```

---

## 10. Financial sustainability after the grant

Our post-grant revenue model is the fee take from the DEX itself. The
orchestrator already collects a **5 bps taker fee** and a **2 bps maker
fee** (see `orchestrator/src/trading.rs`), which are paid into a fee
account inside the enclave and bookkept per-operator.

At the M5 target of $1M monthly volume, a 5 bps gross take generates
**$500 per month**. That is not enough to be sustainable on its own, so
the 12-month post-grant plan is:

1. **Volume-based fee take** scales with mainnet adoption. Breakeven for
   a two-person team (roughly $120K/year engineering cost) is ~$2M
   monthly volume at 5 bps, which is realistic in Q3–Q4 2027 if M4–M5
   trajectories hold.
2. **Additional markets**: ETH-RLUSD-PERP and BTC-RLUSD-PERP can be
   added with minimal additional engineering since the margin engine and
   orchestrator are market-agnostic. Each market adds an independent fee
   stream.
3. **Protocol staking** (`PerpStake` module already implemented): users
   can stake XRP for fee discounts, with stake fees flowing to operators.
4. **Vault LP income** (already designed in research doc 02 section 6):
   liquidity providers earn funding rate and maker rebates while the
   protocol takes a skim.
5. **Institutional RFQ** as an off-book product once mainnet liquidity
   allows.

We will publish monthly financials on `ph18.io/transparency` during and
after the grant period.

---

## 11. XRPL integration strategy

This grant is **not** asking XRPL Grants to fund a DEX that happens to
settle to XRPL — it is asking XRPL Grants to fund a DEX that is only
possible because of specific XRPL primitives:

1. **SignerListSet native multisig** — our security story relies on
   XRPL's native 2-of-3 quorum. On a chain without native multisig we
   would need an on-chain smart contract, which is exactly what we are
   avoiding. On XRPL, the quorum is a ledger-enforced primitive.
2. **RLUSD** — our settlement currency. Our target users are exactly the
   RLUSD holders who today have no way to use RLUSD for perps without
   bridging off XRPL.
3. **Sub-4-second finality** — fast enough to include liquidation
   settlements on-chain in real time, which is not possible on a chain
   with minute-scale finality.
4. **No mempool / no MEV** — our users are explicitly protected from
   sandwich and frontrunning attacks because XRPL's consensus does not
   expose an ordered pending-tx pool to bidders.
5. **Ledger-as-audit-trail** — the catastrophic-recovery scenario (3.9)
   relies on XRPL's 50-year archival guarantees. Every deposit, every
   SignerListSet update, every withdrawal is persisted on XRPL and
   recoverable without any of our servers.
6. **Low fees** — <$0.001 per tx means fee-to-PnL ratio is negligible
   for a perp DEX, which is not true on Ethereum mainnet.

If XRPL did not have SignerListSet and RLUSD, this project would be
building on a different chain.

---

## 12. Open-source licensing

- **Code**: Business Source License 1.1 (BSL 1.1), converting to **Apache
  2.0** after 4 years. BSL permits non-commercial use and review by any
  third party during the restricted period; commercial use requires a
  separate license. Apache 2.0 kicks in automatically on the license
  change date.
- **Research documents** (`research/*.md`): Creative Commons
  Attribution-NonCommercial-NoDerivatives 4.0 International.
- **Design diagrams**: same as research documents.

Both repositories are public from day one. All commits are signed by the
development team's verified GitHub identities.

The BSL-with-automatic-conversion-to-Apache-2.0 model is a common pattern
for DeFi protocols (used by Uniswap v3, Mysten, and others) and gives us
a defensible window against fork-and-dump competitors while guaranteeing
the community will inherit a permissively licensed codebase in 4 years.

---

## 13. Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| SGX side-channel research breaks our isolation assumption | Low | High | Maintain active tracking of Intel TCB updates; migrate to AMD SEV-SNP or AWS Nitro as fallback; publish monthly patch status |
| Azure deprecates DCsv3 family | Low | Medium | Proven portability: the same enclave runs on non-Azure bare-metal (Hetzner) for development. Document production migration path as part of M4 deliverables |
| RLUSD regulatory status changes | Medium | Medium | Contract design does not depend on RLUSD-specific features; can swap to USDC/USDT trustlines on XRPL without protocol changes |
| Low mainnet adoption after launch | Medium | High | Growth milestones M3–M5 are explicitly measurable; if we miss them we do not receive those payments. Grant is structured to protect XRPL Grants from adoption risk |
| Operator collusion (all 3 operators compromised together) | Very Low | Catastrophic | Catastrophic-recovery scenario 3.9 is already tested: funds recoverable from XRPL ledger history alone. Mitigation is growth to 5+ operators with geographic and jurisdictional diversity by end of grant period |
| Independent audit finds critical bugs | Medium | Medium | M2 explicitly includes audit + fix iterations before mainnet launch. Launch gated on clean audit re-review |

---

## 14. What we are NOT asking for

- **Equity or token allocation** — the XRPL Grants program is explicitly
  non-dilutive and we are not issuing a governance token.
- **Marketing budget** beyond documentation and community development.
- **Office or hardware** — all development and operations are remote.
- **Salary top-up** beyond the development line item.

---

## 15. Contact and next steps

**Primary contact:** ph18.io team (contact email in GitHub profile)
**Demo:** https://api-perp.ph18.io (live), https://github.com/77ph/xrpl-perp-dex
**Demo video:** `presentation/demo.cast` (asciinema) in the repository
**Appearing at:** Hack the Block, Paris Blockchain Week, April 11-12, 2026
(Challenge 2: Impact Finance)

We understand the 2025 wave is closed and that Spring 2026 programming is
yet to be announced. This application package is intended as an early-
awareness submission for the next wave and as the pitch material for Hack
the Block Paris. We would welcome a conversation with the XRPL Grants team
at `info@xrplgrants.org` in advance of the next formal application window.

---

*Application compiled 2026-04-09. All code, tests, and on-chain evidence
referenced above is public and independently verifiable at the URLs listed.*
