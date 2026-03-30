# xrpl-perp-dex

Perpetual futures DEX on XRPL mainnet with TEE (Intel SGX) computation layer and RLUSD settlement.

## Architecture

XRPL mainnet has no smart contracts. We use a TEE coprocessor to bridge that gap:

```
Users ──[encrypted orders]──> SGX Enclave (TEE)
                                 ├── Order matching (CLOB)
                                 ├── Margin engine
                                 ├── Funding rate
                                 ├── Liquidation
                                 └── Attestation
                                        │
                              [signed settlements]
                                        │
                                        ▼
                               XRPL Mainnet
                                 ├── RLUSD collateral custody
                                 ├── P&L settlement
                                 └── Deposit / withdrawal
```

- **Computation**: Intel SGX enclave — order matching, margin, funding, liquidation
- **Settlement**: XRPL mainnet — RLUSD transfers, collateral custody via omnibus account
- **Trust model**: users trust attested enclave code + XRPL validators; operator is trusted only for liveness

## Project Structure

```
├── research/              # Research documents (RU + EN versions)
├── grant/                 # Grant application materials
├── enclave/               # SGX enclave — perp engine (C/C++)
├── server/                # REST API server (C++)
├── client/                # Trading client / CLI
├── xrpl/                  # XRPL integration (tx construction, monitoring)
└── docs/                  # Additional documentation
```

## Key Properties

- **Anti-MEV**: orders encrypted with enclave's attested public key; operator sees only ciphertext
- **RLUSD-native**: all collateral, settlement, and fees denominated in RLUSD
- **Verifiable**: remote attestation proves enclave code integrity; signed execution reports for every trade
- **No sidechain**: settles directly on XRPL L1

## Research

| Document | Description |
|---|---|
| [Feasibility Analysis (RU)](research/01-feasibility-analysis.md) | XRPL capabilities, architecture options, recommendation |
| [Feasibility Analysis (EN)](research/01-feasibility-analysis.en.md) | English version |
| [TEE Perp Mechanics (RU)](research/02-tee-perp-mechanics-design.md) | Margin, funding, liquidation, vaults, settlement design |
| [TEE Perp Mechanics (EN)](research/02-tee-perp-mechanics-design.en.md) | English version |
| [Production Architecture (RU)](research/03-production-architecture.md) | HAProxy, public/internal API split, network rules |
| [Production Architecture (EN)](research/03-production-architecture.en.md) | English version |
| [Multi-Operator (RU)](research/04-multi-operator-architecture.md) | FROST 2-of-3, sequencer/validator, failover |
| [Multi-Operator (EN)](research/04-multi-operator-architecture.en.md) | English version |
| [TEE Rationale & API (RU)](research/05-tee-rationale-and-api-design.md) | Why TEE, trust model comparison, production API design |
| [TEE Rationale & API (EN)](research/05-tee-rationale-and-api-design.en.md) | English version |

## Timeline

| Milestone | Date |
|---|---|
| Research complete | 2026-04-07 |
| PoC decision | 2026-04-15 |

## References

- [SGX_project](https://github.com/77ph/SGX_project/blob/feature/bitcoin_dlc/EthSignerEnclave) — TEE signing enclave, prediction market MVP on XRPL (reusable SGX infrastructure)
- [data_collection](https://github.com/8ball030/data_collection) — perp DEX reference

## License

TBD
