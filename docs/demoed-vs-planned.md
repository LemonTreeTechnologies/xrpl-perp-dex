# Demoed vs. Planned

*Every item below is backed by the code (verified 2026-07-24). See
[status-and-features.md](status-and-features.md) for the fuller feature list.*

## Demoed (Hack the Block Paris — mainnet demo; production runs on testnet today)
- XRP-PERP, RLUSD margin + XRP collateral, up to 20x
- CLOB: limit + market orders, long + short
- Liquidations + 8h funding mechanism
- Crossmark / GemWallet integration
- Real-time WebSocket feed
- Attested enclave (SGX DCAP), M-of-N escrow multisig
- Market Making vault deployed — auto two-sided liquidity (V1 sign-off pending review)

## Hardened since (post-demo)
- Atomic state-preserving migration performed — live 3-node enclave cluster, all customer state preserved (May 2026)
- Cluster is authority over its own signer set — the XRPL SignerList is now a confirmed downstream projection (sync-before-spend + drift-halt), live
- Escrow key signs only typed, in-enclave-verified transactions — no "sign-any-hash" oracle (audit-passed, deploying next)
- Governed enclave-version trust — operator quorum + reproducible-build proof from ≥2 independent operators (audit-passed, deploying next)
- 20+ external audit review rounds (REQ/RESP) across the membership-authority + upgrade-path work

## Planned
- First live enclave-version upgrade on testnet (state-preserving migration) → external audit → mainnet relaunch
- Anti-MEV: enclave-key-encrypted order flow
- Delta Neutral vault (hedged spread + funding)
- Delta One vault (rate arbitrage)
- BTC-PERP (month 4)
