# Demoed vs. Planned

*Every item below is backed by the code (verified 2026-09-15). See
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
- State-preserving enclave upgrades are a routine **testnet** operation — **ten** performed on this live 3-node cluster (May–September 2026), each preserving all customer state
- Cluster is authority over its own signer set — the XRPL SignerList is now a confirmed downstream projection (sync-before-spend + drift-halt), live
- Escrow key signs only typed, in-enclave-verified transactions — no "sign-any-hash" oracle, **live**
- Governed enclave-version trust — operator quorum + reproducible-build proof from ≥2 independent operators, **live and exercised on every migration**
- On-chain proof-of-liabilities — enclave-signed merkle root published hourly to Base-Sepolia via a Safe whose sole owner is the sequencer enclave's own key, at threshold 1 (2-of-3 across the three enclaves is planned, not live); you can verify your own account's inclusion
- SPV-proven custody **baseline** — the enclave derived the escrow balance itself from an XRPL ledger attested by ≥80% of a validator set anchored in the measurement, so custody is not a host assertion. It is a point-in-time baseline: flows since are enclave-tracked, not yet SPV-proven, and liability *completeness* remains an in-TEE assertion — see status-and-features.md
- 90 external audit review rounds (90 REQ / 102 RESP documents) across the membership-authority, upgrade-path and proof-of-reserves work

## Planned
- 2-of-3 Safe publish gate (add the other two enclaves as owners, raise the threshold) — gated on full-state replication
- Anti-MEV: enclave-key-encrypted order flow
- Delta Neutral vault (hedged spread + funding)
- Delta One vault (rate arbitrage)
- BTC-PERP (month 4)
