# Hack the Block Paris — 36-Hour Plan

**Team:** Alex, Andrey, Tom
**Track:** Challenge 2 — Impact Finance
**Project:** Perpetual Futures DEX on XRPL with RLUSD settlement

---

## Positioning (what judges see)

**Public narrative:** the first perpetual futures exchange that settles
natively in RLUSD on XRPL mainnet, powered by **Intel SGX Trusted
Execution Environments**. User funds held in XRPL `SignerListSet` 2-of-3
multisig between independent SGX operators. DCAP remote attestation
proves the enclave runs authentic code. No sidechain, no bridge, no
custodial trust.

**What we talk about freely:**
- Intel SGX as the technology (it's a well-known standard)
- DCAP attestation: "anyone can verify the enclave is real"
- Architecture: User → Orchestrator → SGX Enclave → XRPL
- MRENCLAVE hash, quote size, Azure DCsv3 as the platform
- 2-of-3 multisig, margin enforcement in hardware

**What we DON'T share:**
- The enclave source code (`xrpl-perp-dex-enclave` repo stays private)
- Internal implementation details of the margin engine C/C++ code
- Sealed state format, key derivation specifics

If judges ask to see the enclave code:
> "The orchestrator and all integration code is open source (BSL 1.1).
> The enclave binary is published for attestation verification — anyone
> can hash it and compare against the MRENCLAVE in a DCAP quote. The
> source code for the enclave will be published post-mainnet audit.
> Right now you can verify the binary, just not read the source."

---

## What we already have (DO NOT build during hackathon)

Everything below is **live and verified** as of April 10, 2026:

- Live API: `api-perp.ph18.io` (nginx, TLS, CORS)
- Secure computation module with margin engine, position tracking, ECDSA signing
- Rust orchestrator: CLOB orderbook, P2P gossipsub, sequencer election
- 2-of-3 multisig withdrawal via XRPL native SignerListSet — working, verified on testnet
- WebSocket with Fill/OrderUpdate/PositionChanged + channel subscriptions
- PostgreSQL trade replication across 3 operators
- Resting order persistence + failover recovery
- 16/16 E2E tests passing, 9/9 failure mode scenarios with 10 on-chain tx proofs
- Full grant application ready

**Strategy: we don't build the core at the hackathon. It's done. We build
the DEMO LAYER that makes judges go "wow, this is real and it works on
XRPL right now".**

---

## 36-Hour Timeline

### Hours 0-2: Setup & alignment (all 3)

- [ ] Connect to venue WiFi, test SSH to servers
- [ ] Verify `api-perp.ph18.io` responds from venue network
- [ ] Run smoke test: `curl markets`, `wscat wss://api-perp.ph18.io/ws`
- [ ] Align on task split (below) and commit to deliverables

### Hours 2-14: Build sprint (parallel tracks)

**Track A — Frontend trading UI (Tom, ~12h)**

Build a minimal but polished web UI at `perp.ph18.io`:
- [ ] Connect wallet (XRPL via GemWallet or Crossmark browser extension)
- [ ] Display live mark price + funding rate from REST API
- [ ] Show orderbook depth (bids/asks) from REST + WebSocket updates
- [ ] Submit limit/market orders via authenticated REST (sign with wallet)
- [ ] Show user's open orders + positions (polling /v1/orders, /v1/account/balance)
- [ ] Show real-time fills via WebSocket `user:rXXX` channel subscription
- [ ] "Verify Enclave" button → calls `/v1/attestation/quote` → shows MRENCLAVE + quote size + "Intel SGX ✅"
- [ ] "About" section: "Intel SGX enclave, XRPL settlement, 2-of-3 multisig, RLUSD native"

**Stack suggestion:** React or Next.js, Tailwind CSS, lightweight. No
backend — pure API client. WebSocket for live data. API is CORS-enabled.

**Minimum viable for demo:** price display + submit order + see fills
live. The "Verify Enclave" button is the wow-factor — proves hardware trust.

**Track B — Live trading demo setup (Andrey, ~4h)**

- [ ] Fund 2 test wallets on XRPL testnet
- [ ] Deposit funds to the escrow account for both wallets
- [ ] Place initial maker orders at realistic prices (spread around Binance mid)
- [ ] Set up a simple market-making bot (Python loop: quote bid/ask every 5s)
  - 50 lines using `tools/xrpl_auth.py` for signing
  - Creates liquidity so the demo looks alive, not an empty book
- [ ] Test full flow: deposit → maker quote → taker crosses → WS fill → withdraw multisig
- [ ] Record backup asciinema in case venue internet is flaky
- [ ] Prepare pre-funded wallets with saved seeds (offline backup)

**Track C — Pitch, materials, networking (Alex, ~6h)**

- [ ] Build attestation verifier page `verify.ph18.io` (or `perp.ph18.io/verify`):
  - "Fetch quote from live node" button → calls `/v1/attestation/quote`
  - Parse and display: MRENCLAVE, quote size (4,734 bytes), "Intel SGX ✅"
  - Compare MRENCLAVE against published enclave binary hash
  - Note: verifier shows the quote is real but does NOT expose enclave source
- [ ] Build "how it works" section on `perp.ph18.io/about`:
  - Architecture diagram (User → API → Orchestrator → SGX Enclave → XRPL)
  - "2-of-3 SGX operator multisig protects your funds"
  - "DCAP remote attestation — anyone can verify the enclave"
  - Link to XRPL testnet explorer with escrow account
- [ ] Refine 5-minute pitch for judges:
  - Problem (no DeFi derivatives on XRPL) → Solution (off-chain matching,
    on-chain settlement) → Why XRPL (RLUSD, SignerListSet, no MEV) →
    Live demo → "funds are verifiable on XRPL right now" → Call to action
  - Practice twice with timer
- [ ] Prepare Q&A cheat sheet (top 10 expected questions + 1-sentence answers)
- [ ] 1-page project summary for networking

### Hours 14-18: Integration & polish (all 3)

- [ ] Connect frontend to live API — end-to-end from UI
- [ ] Fix CSS/UX issues
- [ ] Market-maker bot keeps book populated
- [ ] Run through full demo flow together:
  1. Open `perp.ph18.io` → show live prices
  2. Connect wallet
  3. Submit limit order → visible in orderbook
  4. Crossing order from second wallet → fill on WebSocket
  5. Click "Verify Enclave" → DCAP quote → MRENCLAVE → "Intel SGX verified"
  6. Show XRPL escrow on testnet explorer → "funds are here, on XRPL"
  7. Withdraw via multisig → show tx hash on explorer
- [ ] If time: record 2-minute video walkthrough as backup

### Hours 18-24: Sleep + buffer

Be realistic — 6 hours of sleep. Don't skip it.

### Hours 24-30: Final polish

- [ ] Fix bugs from overnight cooling
- [ ] Tom: responsive UI, error states, loading spinners
- [ ] Andrey: infra check — servers alive, tunnels up, orchestrator healthy
- [ ] Alex: finalize pitch deck, match demo flow order
- [ ] Practice full demo 2x (3-min and 5-min versions)
- [ ] Prepare offline backup: screenshots, recorded demo, pre-filled tx hashes

### Hours 30-34: Demo prep

- [ ] Submit project to hackathon platform
- [ ] Prep demo laptop: tabs open, wallets connected, terminal ready
- [ ] Last smoke test from venue
- [ ] Huddle: who presents what, who answers questions
  - Alex: opening + problem + solution (2 min)
  - Tom: live demo walkthrough (2 min)
  - Andrey: architecture overview + Q&A (1 min)

### Hours 34-36: Presentations & judging

- [ ] Present
- [ ] Network with judges
- [ ] Collect contacts (VCs, teams, XRPL community)

---

## What NOT to do during the hackathon

1. **Don't rewrite the backend** — it works, 16/16 E2E passing
2. **Don't rebuild the SGX enclave** — rebuild + signing cycle is long
3. **Don't add new trading features** (order types, markets) — scope creep
4. **Don't share enclave source code** — binary is published, source is private until post-mainnet audit
5. **Don't try mainnet launch** — testnet is safe for live demo

## What to say (and not say)

**Talk freely about:**
- Intel SGX as a technology, DCAP attestation, MRENCLAVE
- Architecture: orchestrator (Rust, open source) talks to SGX enclave
- 2-of-3 multisig via XRPL SignerListSet
- Azure DCsv3 as the hosting platform
- DCAP quote verification flow

**Don't share:**
- Enclave C/C++ source code (repo is private, binary is published)
- Internal margin engine implementation details
- Sealed state format, key derivation specifics

| Question | Answer |
|---|---|
| "Can we see the enclave code?" | "The orchestrator is fully open source (BSL 1.1). The enclave binary is published for DCAP verification — you can hash it and compare MRENCLAVE. Source will be published post-mainnet audit." |
| "Is this an MPC?" | "No — each operator runs its own SGX enclave. The multisig is XRPL-native SignerListSet, not a threshold signature." |
| "Can the operator steal funds?" | "No. The enclave enforces margin checks in hardware-isolated memory. A compromised operator can't make the enclave sign an undercollateralized withdrawal." |
| "How do users verify?" | "Two ways: DCAP attestation proves the enclave is real, and XRPL escrow account is publicly visible in any explorer." |
| "Is this audited?" | "52 findings, 50 fixed, 2 by-design. Report in the repo." |
| "Open source?" | "Orchestrator: BSL 1.1 → Apache 2.0 in 4 years, public on GitHub. Enclave binary: published. Enclave source: post-mainnet." |

---

## Judging criteria (typical for Impact Finance)

- **Technical execution** (40%) — does it work? live demo?
- **Innovation** (25%) — novel for XRPL ecosystem?
- **Impact potential** (20%) — who benefits?
- **Presentation quality** (15%) — clear, confident, within time

Our pitch angle: **"This is not a prototype. This is a working product
with 12 verified transactions on XRPL testnet, 3-operator multisig
custody, and an API you can hit right now."**

---

## Emergency kit

| Problem | Fix |
|---|---|
| Servers down | Restart from Hetzner (scripts saved) |
| Tunnels dropped | Re-create SSH tunnels |
| Testnet faucet down | Pre-funded wallets (save seeds) |
| Venue WiFi blocks SSH | Phone hotspot |
| Live demo fails on stage | Recorded backup video |

---

## Key numbers for the pitch

- **$280M** — Drift Protocol loss from social engineering on human multisig (April 2026)
- **4,734 bytes** — Intel-signed DCAP attestation quote from our SGX enclaves
- **2-of-3** — XRPL native SignerListSet multisig between independent SGX operators
- **16.5 sec** — sequencer failover time (live tested on 3-node Azure cluster)
- **3 sec** — network partition reconvergence (live tested)
- **12** — verified multisig transactions on XRPL testnet
- **16/16** — E2E test pass rate
- **52** — security audit findings (50 fixed, 2 by-design)
- **$150K** — grant application ready for XRPL Grants Spring 2026
