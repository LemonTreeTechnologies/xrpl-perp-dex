# Project Status & Features

*Verified against the codebase, 2026-09-15. Standard: no capability listed here is
one the code does not implement — and nothing the code does implement is left out.*

## Live features
- **XRP-PERP**, **RLUSD margin + XRP collateral** (XRP valued at a 90% haircut),
  up to **20×** leverage
- **CLOB**: limit + market orders, long + short, price-time-priority matching
- **Liquidations** + **8-hour funding** mechanism
- **Crossmark / GemWallet** integration (dual-mode signature verify, including the
  XRPL SHA-512Half wallet wrap)
- **Real-time WebSocket feed** — trades, orderbook, ticker, liquidations, and
  per-user fills / order-updates / position changes
- **Attested enclave** (Intel SGX DCAP) with **M-of-N escrow** multisig
  (XRPL SignerList, ECDSA — not a single aggregate key; escrow master key disabled)
- **Market-making vault** on the live cluster — automated two-sided liquidity
  (V1 formal sign-off pending the accessibility + spec-faithfulness review)
- **On-chain proof-of-liabilities** — the enclave signs a merkle root over its own
  sealed state and it is published hourly to a monotonic-epoch registry on
  Base-Sepolia through a 2-of-3 Safe. Anyone can verify that **their own account is
  included** in the published root
- **SPV-proven custody** — the custody figure is no longer asserted by the host: the
  enclave derives the escrow balance **itself** from an XRPL ledger attested by
  ≥80% of a validator set **anchored in the measurement**, verifying the validator
  manifests and the SHAMap inclusion proof in-enclave. A third party can re-check
  the figure directly against XRPL

## Hardened — done and live on the 3-node cluster
- **State-preserving enclave upgrades are routine, not a one-off** — **ten**
  performed on the live 3-node cluster (May–September 2026, most recently
  2026-09-15), each a full re-key with **all customer state preserved** and
  verified afterwards against on-chain evidence
- **Governed enclave-version trust, in use** — which build the cluster admits is an
  operator-quorum decision gated by a reproducible-build proof from ≥2 independent
  operators; it is exercised on every migration, not just specified
- **Distinct signing capability, in use** — the escrow key signs only a typed
  transaction the enclave re-verifies internally; there is no "sign any hash" oracle
- **Reproducible builds** — independent machines produce a **bit-identical**
  enclave measurement (the trust anchor for admitting a new build)
- **The cluster is the authority over its own signer set** — operators decide
  membership off-chain; the on-chain XRPL SignerList is a **confirmed downstream
  projection** of that decision, with sync-before-spend and drift-halt
- **Sustained external audit** — 90 REQ / 102 RESP review documents across the
  membership-authority, upgrade-path and proof-of-reserves work

## What the reserves artifact does *not* prove
Stated explicitly, because the useful claim is the precise one. Custody is
SPV-proven and independently checkable against XRPL. **Liability completeness is
not**: the root is an in-TEE assertion over the enclave's own sealed state, so a
third party can verify that their account is *included*, not that the root
enumerates every liability. The 2-of-3 Safe gate is key-custody plus a structural
publish gate — it is not independent economic validation of the figures.

## Roadmap
- **Anti-MEV** — order flow encrypted to the enclave's attested public key
  (not yet implemented)
- **Delta-Neutral vault** (hedged spread + funding) and **Delta-One vault**
  (rate arbitrage)
- **BTC-PERP** — a BIP-340 Taproot signing leg, key-separated from the XRPL
  escrow key

## Environment
Production runs on XRPL **testnet** today (committed policy: testnet-first). The
system is single-mode across testnet and mainnet — the same code, no
per-environment branches. The upgrade path that used to gate a mainnet move is
now routine (ten live migrations); what remains before mainnet is organisational
— operators, hosting, funded accounts — not code.
