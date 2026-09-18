# Perp DEX on Bitcoin — architectural feasibility

**Audience:** technical team and investor.
**Status:** feasibility study, not a commitment. Bilingual: this document has a Russian counterpart at `btc-perp-dex-feasibility-ru.md`.
**Context:** this document exists because the question naturally arises from the project's own history. The enclave signing stack in `xrpl-perp-dex-enclave` (forked from the earlier `SGX_project`) already went through a journey from plain ECDSA to the full Bitcoin-grade signing surface — BIP340 Schnorr, MuSig2, FROST threshold signing — *before* we settled on XRPL as the first target. So the natural next question is: given that the signing primitives we already ship are exactly the ones Bitcoin uses, can the same architecture be pointed at Bitcoin and yield a real perp DEX?

The short answer is **yes, the architecture carries over cleanly, and it's a bounded engineering project rather than a new platform**. The long answer is below, written to be honest about both the opportunity and the work.

---

## 1. Executive summary (for the investor)

- **The signing stack we built is already Bitcoin-native.** The enclave implements ECDSA, BIP340 Schnorr, MuSig2, and FROST threshold signing on `libsecp256k1`. Those are the signature types Bitcoin actually uses, not approximations of them. Taproot spends from a FROST 2-of-3 vault work on Bitcoin today with the code we already have.
- **The non-chain-specific parts of the product port directly.** Margin engine, position state machine, liquidation loop, funding rate, CLOB matching, the 2-of-3 threshold-custody model (XRPL SignerList ECDSA today, FROST Schnorr on Bitcoin), SGX enclave deployment, release pipeline — all of this is chain-agnostic. The custody *model* ports; the signing *mechanism* does not — FROST signing would be new work, not a port. About 80% of the engineering already done for the XRPL version is reusable as-is.
- **The chain-specific work is bounded and well-understood.** Deposit/withdraw plumbing is different (Bitcoin transactions instead of XRPL, with Taproot multisig vaults and confirmation-based crediting), and the collateral-and-settlement design has to be explicit about BTC's quirks (10-minute blocks, no native stablecoin, variable fees, reorgs). None of this is research; all of it has known solutions shipping in production elsewhere.
- **The business opportunity is the largest liquidity pool in crypto.** BTC is roughly an order of magnitude larger than any altchain by market cap and daily derivatives volume. The L1-native decentralized BTC perp-DEX category is essentially empty — BitMEX and Deribit are centralized, GMX and Hyperliquid live on L2s and use wrapped BTC, and wrapped-BTC-on-Ethereum carries bridge risk the BTC community dislikes. A TEE-backed, FROST-custodied, L1-settled BTC perp DEX is a product category that does not meaningfully exist yet.
- **Timing.** This is not a replacement for the XRPL track. The recommendation is: finish XRPL, prove the architecture in production, *then* fork a BTC version — and in the meantime keep the codebase chain-agnostic so the fork is a 2-3 month project rather than a rewrite.
- **Optionality beyond BTC.** Every chain that signs transactions with `secp256k1` (Bitcoin, Bitcoin Cash, Litecoin, Dogecoin, Zcash transparent addresses, Ethereum tx envelope) can host the same architecture. Bitcoin is simply the most strategically valuable first target after XRPL.

---

## 2. Why the question arises from the project's own history

The current `xrpl-perp-dex-enclave` repo is a fork of `SGX_project`, which started life as a general-purpose "signing-as-a-service in an SGX enclave" codebase aimed at Ethereum-style ECDSA signing. Over the course of its evolution (as a signer for various chains, and then as we moved toward BTC-family signing) the enclave accumulated the following capability stack:

| Capability | Status in the enclave today | What Bitcoin needs |
|---|---|---|
| secp256k1 arithmetic (`libsecp256k1`) | Linked, used everywhere | Same library Bitcoin Core uses |
| ECDSA (pre-Taproot P2PKH/P2WPKH) | Implemented | Used in pre-Taproot Bitcoin addresses |
| **BIP340 Schnorr** | Implemented | Used in Taproot P2TR spends |
| **MuSig2** (2-party and n-party key aggregation) | Implemented, session management inside enclave | Enables multi-party Taproot spends producing single Schnorr signature |
| **FROST** threshold Schnorr (2-of-3) | Implemented, DKG supported | Enables 2-of-3 Taproot custody that appears on-chain as a single-sig Schnorr spend |
| SGX sealing of key material | Implemented | Same for any chain |
| FROST ceremony across 3 independent operators | Implemented | Same for any chain |

So when we pivoted from "generic BTC-family signer" toward "perp DEX on XRPL", we did not *remove* Bitcoin capability — we built the perp-DEX layer on top of it. The XRPL-specific pieces are the transaction format (STObject serialization, SHA-512Half pre-hash, DER-encoded ECDSA) and the on-chain settlement path (RLUSD escrow, XRPL testnet client, ledger monitoring). Those sit on top of a signing core that is strictly a superset of what Bitcoin needs.

This is not a retrofit argument. It is a statement about what the code *already does* today.

---

## 3. What ports from the XRPL version essentially unchanged

The following layers are chain-agnostic and would be reused directly in a BTC version:

**Enclave, signing, custody:**
- The entire `libsecp256k1` stack inside the enclave (ECDSA, Schnorr, MuSig2, FROST).
- FROST 2-of-3 DKG and threshold signing ceremony.
- SGX sealing of signing shares and enclave state (our whole Part 6 migration story from `sgx-enclave-capabilities-and-limits.md`).
- Remote attestation via DCAP, `deployment-procedure.md` release ceremony with YubiKey-gated 2-of-3 signing, reproducible builds, per-node deploy agents.
- Side-channel posture, CPUSVN handling, threat model.

**Trading and risk:**
- `PerpState` — the position state machine.
- Margin engine, including the fixed-point arithmetic and `fp_mul` / `fp_div` helpers.
- Open / close / liquidate flow.
- Funding rate application.
- Liquidation scanning loop.
- Insurance fund accounting.
- CLOB matching semantics, including `reduce_only` IOC for position closes (per `feedback_closes_must_route_clob.md`).

**Infrastructure and operations:**
- Orchestrator architecture (price feed pipeline, deposit monitor loop, liquidation loop, state-save cadence).
- REST / gRPC API surface for external clients.
- Monitoring, alerting, logging discipline.
- Multi-operator deployment model (3 independent persons, no cross-server access, FROST-signed releases).
- Failure-mode testing plan.

Counting generously, this is 80% of the work that went into the XRPL version. None of it needs to be rewritten or even significantly modified for a BTC target — the chain-specific code lives in a thin layer at the top (transaction construction and monitoring) and at the bottom (orchestration to a different chain client).

---

## 4. What is genuinely different on Bitcoin

This section exists to be honest about the chain-specific engineering that a BTC version requires. None of these items is research; all of them have production-grade reference implementations elsewhere. But they are not zero and they must be budgeted.

### 4.1 Deposit latency

XRPL settles in ~3 seconds and gives us deterministic finality. Bitcoin blocks arrive every ~10 minutes and carry probabilistic finality — the standard rule of thumb is 1 confirmation for low-value credit, 3-6 confirmations for larger amounts. This has direct UX implications:

- A user cannot start trading on Bitcoin the moment they sign a deposit transaction. They have to wait at least until the tx lands in a block, and for meaningful amounts until it is buried under several more blocks.
- The orchestrator must model **pending balance** separately from **confirmed margin**, and the enclave margin engine must be told when a pending deposit transitions to confirmed.
- Reorgs do happen (rarely beyond 1-2 blocks, but they happen). Any credit given at low confirmation must be reversible if the containing block is orphaned. This is a standard engineering problem with a standard solution (track every deposit to a fixed number of confirmations, maintain a reorg-depth invariant, unwind credits on deep reorg — the same way every custodial BTC service handles this).

**Mitigations that exist and work:**
- Credit at N=1 conf with a conservative haircut and no margin for open positions; full margin at N=6.
- Accept Lightning Network deposits into a Lightning node operated by the orchestrator; Lightning payments are instant and final, and the orchestrator credits margin the moment the HTLC resolves. This adds a Lightning node to the operational surface but it is a well-understood component.
- Accept RBF-disabled, confirmed-in-mempool deposits with conservative limits for very small accounts.

### 4.2 Collateral denomination

XRPL gives us RLUSD — a regulated USD stablecoin on-chain. Bitcoin has no native stablecoin. The perp DEX therefore has to make an explicit choice about how it denominates positions and collateral:

- **Inverse contracts (BTC-denominated).** Contracts are priced in USD but PnL is paid in BTC. This is the original BitMEX XBTUSD model and it works. Users deposit BTC, open a long or short against USD price, and their PnL is settled in BTC at close. Collateral never has to be converted. Maintenance margin is expressed as a percentage of notional in BTC terms. Liquidation uses the mark price in USD and pays out in BTC. This is battle-tested in production at multi-billion-dollar scale on BitMEX, Deribit, and OKX.
- **Linear (USD-denominated) contracts with BTC collateral at mark-to-market.** Users deposit BTC, the enclave continuously revalues the BTC collateral at the current mark price, margin is expressed in USD, PnL is in USD, withdrawals are paid in BTC at the current price. This gives users a more familiar PnL experience but it couples collateral solvency to BTC price volatility and makes liquidations more complex. This is also production-proven (most current retail perp platforms do this).
- **Hybrid: let the user choose.** Standard offering on most modern perp venues.

For a first BTC version the inverse-contract path is the lowest-risk choice — it is mathematically simpler, the collateral side of the system becomes stable in BTC terms, and it avoids having to trust an external USD oracle for solvency decisions.

### 4.3 On-chain custody primitive

On XRPL we use the native escrow object. On Bitcoin, the equivalent is a **Taproot P2TR address whose key-path spend is a FROST 2-of-3 aggregated Schnorr signature**. This is elegant and important for the investor story:

- The vault address on-chain is indistinguishable from a single-signature P2TR address. It is cheap to spend (one Schnorr signature), privacy-preserving (no one can tell from the chain that it's a multisig), and fully native Bitcoin — no smart contract, no sidechain, no wrapper token.
- The 2-of-3 FROST signing ceremony happens inside our three enclaves, reusing the DKG and share-transport machinery the cluster already runs and migrates across MRENCLAVE bumps. **It is not what the XRPL version does** — XRPL settlement is signed by an independent-ECDSA SignerList quorum, so the FROST ceremony itself is new work. On-chain output is a single 64-byte Schnorr signature.
- A script-path spend can be defined as a fallback (e.g. timelocked single-operator recovery after prolonged unavailability of the quorum), using Tapscript.
- This is *more* private than anything available on the XRPL side, because on XRPL a multisig address is visibly a multisig.

### 4.4 Reorg handling

Covered above in §4.1. Small reorgs (1-2 blocks) happen occasionally on Bitcoin mainnet. Deep reorgs (beyond 6 blocks) are extraordinarily rare (measured in years). The orchestrator must maintain a confirmations index for every deposit and withdrawal and unwind state if a credited tx is removed from the best chain. The enclave margin engine must expose an explicit `ecall_perp_revert_deposit(user_id, tx_hash)` that reverses a previously credited balance, protected by the same duplicate-detection machinery as the forward path. This is standard BTC-service engineering and is not novel.

### 4.5 Fee variability and withdrawal batching

Bitcoin transaction fees vary from effectively free to tens of dollars depending on mempool conditions. For user withdrawals:

- The orchestrator should batch withdrawals when possible — a single Bitcoin transaction can pay out to hundreds of recipients, amortizing the fee.
- Users should see the current estimated fee before confirming a withdrawal, and have the option to wait for lower fees.
- The vault must maintain a fee-reserve UTXO so that withdrawals are never blocked on fee availability.

Again, standard. Lightning withdrawals (§4.1) are also an option for small amounts and avoid on-chain fees entirely.

### 4.6 No on-chain programmability

XRPL gives us native escrow, native multi-token balances, soon batch transactions and (possibly) WASM contracts. Bitcoin gives us Taproot scripts, HTLCs, and timelocks — nothing more. This means **the enclave is even more central to the BTC version than to the XRPL version**. Bitcoin is used purely as a settlement rail; all state, all matching, all risk management lives in the enclave. This is actually how BitMEX operates internally (their "Bitcoin account" is a vault, everything else is database state), and it's a clean model. The BTC community is already comfortable with this pattern in the form of Lightning and Fedimint.

---

## 5. Architecture sketch for a BTC version

Putting the above together, a BTC perp DEX using our architecture looks like this.

### 5.1 Deposit flow

1. User generates a deposit address by requesting one from the orchestrator. The orchestrator derives a per-user deposit address via BIP32 derivation under the FROST-aggregated xpub (or, equivalently, uses a single vault address with a unique OP_RETURN tag per user). Enclave stores the mapping `(user_id → deposit_tag)`.
2. User sends BTC to the deposit address. Orchestrator watches the mempool and blockchain via Bitcoin Core (or Electrum RPC / Esplora).
3. At 1 confirmation, orchestrator calls `ecall_perp_deposit_credit_pending(user_id, amount, tx_hash, confirmations)` — the enclave credits a *pending* balance that can back open positions at a conservative haircut but cannot be withdrawn.
4. At N confirmations (configurable, default 6), orchestrator calls `ecall_perp_deposit_confirm(tx_hash)` and the pending balance becomes fully confirmed margin.
5. If at any point the orchestrator detects that the containing block has been orphaned and the tx is no longer in the best chain at some lower confirmation count, it calls `ecall_perp_deposit_revert(tx_hash)` and the enclave reverses the credit. Any open positions funded by the reverted credit are force-closed.

### 5.2 Trading

Identical to the XRPL version. The margin engine, CLOB, liquidation loop, funding loop, and REST API do not know which chain the system is settling to. `PerpState` is the same struct. This is the part of the port that is genuinely free.

### 5.3 Withdrawal

1. User submits a withdraw request specifying amount, destination address, and optional maximum fee.
2. Orchestrator calls `ecall_perp_withdraw_check_and_sign_btc(user_id, amount, dest_addr, fee_rate, tx_template, sig_out)`. The enclave:
   - performs the margin check (solvency after withdrawal),
   - verifies the `tx_template` structure is a valid P2TR key-path spend from the vault to `dest_addr`,
   - produces its FROST share of the Taproot Schnorr signature.
3. Orchestrator aggregates shares from 2-of-3 operators, assembles the final Schnorr signature, broadcasts the tx to the Bitcoin network.
4. The enclave atomically debits the user balance *before* releasing the signature (the same TOCTOU-safe pattern we use on XRPL — check and sign in a single ecall).

### 5.4 Price feed

External oracle (Pyth, Chainlink, or self-run aggregator of CEX prices signed by a trusted feeder). Feeder pushes prices into the enclave via `ecall_perp_update_price(mark_price, index_price, timestamp, feeder_sig)`. The enclave validates the feeder's signature and refuses to act on stale prices. Identical pattern to XRPL version, same code.

### 5.5 Settlement market

Inverse contracts (XBTUSD-style) as the first offering. Collateral and PnL both in BTC, price reference in USD. Linear USD-denominated markets added later if demand exists. This is the same choice every production BTC derivatives venue has made at launch and it has never been a problem.

### 5.6 Chain client

A new `btc_client` Python module alongside the existing `xrpl_client`, providing:
- `get_block(height)` and `get_tip_height()` for block monitoring.
- `watch_address(addr)` for deposit detection.
- `estimate_fee_rate(target_blocks)` for withdrawal sizing.
- `broadcast_tx(raw_tx)` for settlement.
- `get_reorg_depth()` for safety.

Implemented against Bitcoin Core JSON-RPC for self-hosted, or against Electrum / Esplora for lighter setups. This is ~1000-1500 lines of straightforward client code, well below the engineering effort of the XRPL client that already exists.

---

## 6. Competitive landscape

Who is currently offering perpetual futures on BTC, and where are they architecturally?

| Venue | Architecture | BTC settlement | Custody | Decentralized? |
|---|---|---|---|---|
| BitMEX | Centralized exchange | L1 BTC deposits/withdraws | Single operator multisig | No |
| Deribit | Centralized exchange | L1 BTC deposits/withdraws | Single operator custody | No |
| Binance / OKX / Bybit BTC perps | Centralized exchange | L1 BTC deposits/withdraws | Single operator custody | No |
| dYdX v4 | Cosmos app-chain | No — USDC only | Validators | Partially (validator set) |
| GMX | Arbitrum smart contract | Wrapped BTC (WBTC) | Bridge custodians hold real BTC | No (bridge trust) |
| Hyperliquid | Custom L1 | Wrapped BTC via bridge | Validator set custody | Partially (validator set) |
| Synthetix perps | Optimism smart contract | Synthetic sBTC (no real BTC) | None — purely synthetic | Yes but no real BTC exposure |
| Drift, Mango (Solana) | Solana smart contract | Wrapped BTC on Solana | Bridge custodians | No (bridge trust) |
| Lightning-based (experimental) | Various | Lightning channels | Various | Varies, usually small scale |

The gap this reveals is narrow and specific: **nobody is running a truly decentralized BTC perp DEX that settles directly to Bitcoin L1 with minimal custody trust**. Every existing product falls into one of three failure modes from a BTC-purist perspective:

1. Centralized exchange with a single operator holding user BTC (BitMEX, Deribit, Binance).
2. Decentralized matching but wrapped BTC on another chain, meaning users actually hold WBTC issued by a custodian who holds the real BTC (GMX, Hyperliquid, Drift).
3. Synthetic BTC with no real BTC exposure at all (Synthetix).

Our architecture fills the gap: **matching happens in a TEE, custody is 2-of-3 FROST with three independent operators, settlement is native Taproot spends directly on Bitcoin L1, and users never hold a wrapped token** (this describes the Bitcoin leg being proposed here — live XRPL custody today is a SignerList 2-of-3 ECDSA quorum). Deposits are real BTC. Withdrawals are real BTC. The enclaves cannot move user funds unilaterally (FROST 2-of-3 requires quorum). The operators cannot move user funds without enclave consent (because they don't hold full signing shares outside the enclave). No bridge, no L2, no synthetic asset.

This is a defensible position *specifically* in the BTC community, which cares about self-custody, L1 settlement, and avoiding bridge risk far more than the Ethereum community does. It is also, not coincidentally, the same argument we already make for the XRPL version — we are simply pointing it at the chain with the largest and most security-conscious user base.

---

## 7. Our unique wedge

Positioning summary for the investor:

- **L1-native settlement.** Real BTC in, real BTC out, no wrapping, no bridge.
- **Minimal custody trust.** 2-of-3 FROST across three enclaves operated by three independent persons (Bitcoin leg; the XRPL leg reaches the same property through a SignerList 2-of-3 ECDSA quorum), each protected by hardware keys and DCAP attestation. No operator can move funds alone; no enclave can move funds alone; no two colluding parties can move funds unless one of them is an enclave running the audited code.
- **Auditable matching.** The matching and risk engine runs inside SGX enclaves whose binary is reproducibly built and whose `MRENCLAVE` is published and attested. Users can verify cryptographically that the exchange is running the code they think it is — a guarantee no centralized BTC exchange offers.
- **Cutting-edge cryptography shipped in production.** BIP340 Schnorr + FROST 2-of-3 Taproot vaults are state-of-the-art Bitcoin custody that, to date, almost no production venue actually uses. This is technically attractive and press-worthy.
- **Familiar product for the BTC market.** Inverse XBTUSD-style perps are the native product shape Bitcoin users already know from BitMEX. This reduces market-education cost to roughly zero.
- **Symmetry with our XRPL track.** One codebase, one team, one threat model, two chains. Each rail strengthens the credibility of the other.

---

## 8. Engineering scope and realistic timeline

Assuming the XRPL version is shipping and stable, and assuming the current codebase has been refactored to isolate chain-specific concerns behind a thin boundary (see §11), a BTC port has the following approximate shape:

| Workstream | Effort | Notes |
|---|---|---|
| `btc_client` Python module (Bitcoin Core RPC, fee estimation, reorg tracking, address monitoring) | 3-4 weeks | Straightforward, reference implementations exist |
| FROST 2-of-3 Taproot key-path spend construction and signing flow | 2-3 weeks | Signing primitives already in the enclave; this is gluing them to Bitcoin transaction serialization |
| Deposit / withdraw ecalls specialized for BTC (pending/confirmed states, reorg revert) | 2-3 weeks | `ecall_perp_deposit_credit_pending`, `ecall_perp_deposit_confirm`, `ecall_perp_deposit_revert`, `ecall_perp_withdraw_check_and_sign_btc` |
| Orchestrator loops (deposit monitor, reorg handler, withdrawal batcher, fee estimator) | 3-4 weeks | Mirror the XRPL orchestrator |
| Inverse-contract math in the margin engine | 1-2 weeks | Small extension to `PerpState` to denominate positions in BTC rather than assume stablecoin collateral |
| Price feed signer adaptation (external oracle or self-run aggregator) | 1 week | Identical pattern to XRPL version |
| Lightning deposits (optional, can be phase 2) | 3-4 weeks | LND client, HTLC handling, instant-credit integration |
| End-to-end testing on Bitcoin signet and testnet | 2-3 weeks | Standard pre-mainnet validation |
| Security audit (external) | 4-6 weeks | Parallel to development |
| Mainnet launch engineering, monitoring, runbooks | 2-3 weeks | |

**Total, serial, realistic:** approximately **3 months** of a small senior team for a production-quality BTC launch, plus the external audit running in parallel. This is a genuine project, but it is not a research project — it is engineering against well-understood Bitcoin primitives using a signing stack that already exists.

This budget explicitly does not include:
- Rewriting the enclave (it doesn't need to be rewritten).
- Re-doing the SGX deployment pipeline (`deployment-procedure.md` applies unchanged).
- Re-doing the FROST ceremony or operator model.
- Building matching or liquidation logic from scratch.

The 3-month estimate is credible precisely because the large majority of the work is already done.

---

## 9. Real risks and honest open questions

This section exists to prevent the common failure mode of feasibility documents — glossing over the things that could actually bite.

**Technical risks:**

1. **Deposit UX for small, impatient users.** Waiting for 1-6 Bitcoin confirmations is slow compared to XRPL or L2 experiences. Lightning mitigates this but adds operational complexity. Without Lightning, the onboarding experience for "I want to try this right now" users is genuinely worse than what they get on centralized exchanges.

2. **Fee shocks during congestion.** A Bitcoin fee spike (as happened during the 2017, 2021, and 2024 mempool congestion events) can make withdrawals expensive for a day or two. Batching helps but doesn't eliminate this. Users complain.

3. **Reorg engineering has to be correct, not approximately correct.** Getting reorg handling wrong in a financial system is how money disappears. This is mature engineering but it still has to be done carefully, tested with synthetic reorgs on signet, and the failure modes documented. This is the single highest-risk engineering item in the whole plan.

4. **Oracle trust assumption inherits from the XRPL version.** The price feed is still an oracle. A compromised or stale oracle can mis-mark positions and trigger wrong liquidations. This is not worse on BTC than on XRPL, but it is not *better* either — we should not claim BTC solves our oracle problem.

5. **BIP340 + FROST is state-of-the-art but not heavily production-tested yet.** Our FROST implementation inside the enclave is the same code we're running for XRPL, so we have the same confidence in it. But at the Bitcoin scripting layer, Taproot key-path spends from FROST-aggregated public keys are cutting edge. We must validate our signing flow against multiple independent BIP340 verifiers (Bitcoin Core, secp256k1 reference, libbtc) on signet before trusting a dollar of user funds to it.

**Non-technical risks:**

6. **BTC community skepticism of SGX.** There is a historically vocal subset of the Bitcoin community that considers any TEE-based system to be "centralization theater". The FROST 2-of-3 across independent operators argument helps here, and so does the fact that we publish MRENCLAVE and attestation, but this conversation will happen and we need our messaging to be ready. Honest framing: "SGX is a defense layer on top of FROST threshold custody, not a substitute for it — even a full SGX compromise of one enclave does not expose user funds because no single enclave holds enough key material to sign anything".

7. **Regulatory scrutiny of derivatives on BTC.** BTC-denominated derivatives face more regulatory attention in the US and EU than XRPL-denominated derivatives currently do, because Bitcoin itself is the most-traded crypto asset. Legal posture, jurisdiction, KYC requirements, and listing decisions need to be re-examined for a BTC product — the XRPL legal framing does not port automatically.

8. **Competitive response from centralized venues.** BitMEX, Deribit, and Binance will not ignore a credible L1-native decentralized alternative if it gains meaningful TVL. Their response is likely to be fee compression and liquidity mining rather than technical parity, and we need a sustainability story that does not depend on outlasting subsidized competition.

9. **Team focus.** Running two chain products simultaneously from a small team is how small teams ship neither well. This is not a hypothetical — it is the most common failure mode of ambitious feasibility projects. See §10.

**Things we do not yet know and would need to decide:**

- Inverse contracts first and add linear later, or both at launch?
- Self-run oracle vs. external oracle (Pyth/Chainlink)?
- Bitcoin Core direct RPC vs. Esplora/Electrum indexer for address monitoring?
- Lightning at launch, or in phase 2?
- BIP32 per-user deposit derivation vs. single vault with OP_RETURN tagging?
- External audit firm (there are roughly 4-5 firms credible enough for both SGX enclave code *and* Bitcoin transaction logic)?

None of these are showstoppers. All of them deserve explicit decisions before a launch date is committed.

---

## 10. Recommendation

**Do not start the BTC fork yet.** The XRPL version is not yet in production, the failure-mode testing plan is not yet complete, and the operational ceremony is not yet rehearsed across the three operators. Splitting attention now delays the XRPL ship date without proportionally advancing the BTC track.

**Do, starting now, keep the codebase chain-agnostic.** Specifically:

- Introduce a thin `ChainAdapter` interface in the orchestrator that covers deposit monitoring, address derivation, transaction construction, and broadcast. The current XRPL implementation is one backend; a future BTC implementation is another backend. This is the exact same discipline we recommend for `TeeBackend` in Part 9 of `sgx-enclave-capabilities-and-limits.md`, and it is cheap to introduce now but expensive to retrofit later.
- Keep `PerpState`, the margin engine, the liquidation loop, the CLOB logic, and the ecall surface free of any XRPL-specific assumptions. They should accept amounts as `int64_t` fixed-point units denominated in whatever the settlement asset is, with no hardcoded references to RLUSD or XRPL transaction formats.
- Keep chain-specific deposit/withdraw ecalls behind a suffix convention (`ecall_perp_withdraw_check_and_sign_xrpl` today, `ecall_perp_withdraw_check_and_sign_btc` tomorrow), not as an overloaded generic ecall, so each chain's tx-construction logic is explicit and auditable in isolation.

**Do, when the XRPL version is live and stable, fork a BTC track.** The scope is 3 months of engineering plus an external audit, executed by the same team that built the XRPL version, with the same operational model. This is the moment to convert the architecture's latent optionality into a second revenue stream.

**For an investor specifically asking about BTC today:** the correct framing is "the architecture is explicitly designed to port to Bitcoin, the signing and custody layers already ship Bitcoin's exact cryptographic primitives today, and a BTC version is a bounded 3-month project after XRPL launch rather than a new product to build from scratch". This is truthful, it is positive, and it does not overpromise.

---

## 11. Postscript — chain-agnostic architecture as an asset

The argument of this document generalizes beyond Bitcoin. Every chain that uses `secp256k1` for transaction signing can, in principle, host the same perp DEX architecture with approximately the same engineering scope: a new chain client, a new deposit/withdraw ecall surface specialized for that chain's transaction format, and orchestration plumbing. The list of chains this covers is not small:

- Bitcoin, Bitcoin Cash, Litecoin, Dogecoin — direct ports of the BTC version with minor adjustments.
- Zcash transparent addresses — same signing primitives, different address format.
- Ethereum and all EVM chains — different transaction envelope but the signing layer is identical; a port here would additionally benefit from being able to settle against the EVM DeFi ecosystem.
- Stacks, Rootstock, and other BTC sidechains — trivial once the BTC version works.
- Any future chain that adopts Schnorr / BIP340 signing — automatic.

This is a form of product optionality that does not show up on a feature list but is extremely valuable: **the investment required to support chain N+1 is always strictly smaller than the investment required to support chain N**, because the chain-agnostic core (enclave, FROST, margin engine, risk management, deployment model) gets amortized across every rail. The XRPL version is the first and most expensive chain. BTC is the second and cheaper. Every chain after BTC is cheaper still.

Framed for the investor: the XRPL version is not just a product, it is a *platform investment* whose cost is front-loaded. BTC is the first validation of that platform claim. The value of the claim compounds with every additional rail the architecture ships to, and the cost per rail declines monotonically.

---

## Appendix — Cross-references

- `sgx-enclave-capabilities-and-limits.md` — SGX guarantees, sealing, migration, and TDX portability argument. The threat model discussed there applies unchanged to a BTC version.
- `deployment-procedure.md` — operator model and release ceremony. Applies unchanged.
- `xls-survey-for-perp-dex.md` — why XRPL specifically was chosen as the first rail. The "why not BTC first" argument appears there: BTC's latency, fee variability, and lack of native programmability were correctly judged as higher engineering cost for the PoC, not as permanent blockers.
- `feedback_closes_must_route_clob.md` — CLOB routing discipline. Applies unchanged.
- `project_post_hackathon_architecture.md` — Tom's plan for post-hackathon liquidity architecture. The BTC port does not depend on that plan being resolved; it reuses whatever liquidity architecture the XRPL version settles on.
- `feedback_bilingual_docs.md` — bilingual policy. This document has a Russian counterpart.
