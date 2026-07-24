# Project Status & Features

*Verified against the codebase, 2026-07-24. Standard: no capability listed here is
one the code does not implement.*

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

## Hardened — done and live on the 3-node cluster
- **Atomic state-preserving enclave migration performed live** — a full enclave
  re-key across the running cluster with **all customer state preserved** (May 2026)
- **Reproducible builds** — independent machines produce a **bit-identical**
  enclave measurement (the trust anchor for admitting a new build)
- **The cluster is the authority over its own signer set** — operators decide
  membership off-chain; the on-chain XRPL SignerList is a **confirmed downstream
  projection** of that decision, with sync-before-spend and drift-halt
- **Sustained external audit** across the membership-authority and upgrade-path work

## Audit-passed, deploying next
*(implementation review complete; not yet on the cluster)*
- **Distinct signing capability** — the escrow key signs only a *typed
  transaction it re-verifies inside the enclave*; there is no "sign any hash"
  oracle, so a fault in the value path cannot forge a governance change (and
  vice-versa)
- **Governed enclave-version trust** — which enclave build the cluster admits is
  an **operator-quorum decision**, gated by a **reproducible-build proof from ≥2
  independent operators**: no single party can admit an unaudited binary

## Roadmap
- **First live enclave-version upgrade on testnet** via the state-preserving
  migration (retiring rip-and-replace) → **external audit** → **mainnet relaunch**
- **Anti-MEV** — order flow encrypted to the enclave's attested public key
  (not yet implemented)
- **Delta-Neutral vault** (hedged spread + funding) and **Delta-One vault**
  (rate arbitrage)
- **BTC-PERP** — a BIP-340 Taproot signing leg, key-separated from the XRPL
  escrow key

## Environment
Production runs on XRPL **testnet** today (committed policy: testnet-first). The
system is single-mode across testnet and mainnet — the same code, no
per-environment branches — so mainnet relaunch follows the first live
upgrade-migration and an external audit, not a rewrite.
