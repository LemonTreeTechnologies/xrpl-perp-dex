# Reserves proof-of-liabilities — Tier-1 deploy runbook (Base-Sepolia)

> Honest label: this is a **single attested-enclave proof-of-liabilities**, NOT
> "2-of-3" and NOT "reserves". The genuine 2-of-3 (independent per-node recompute)
> is **Tier-2**, gated on full-state replication (AC-R2-3). "Reserves" (assets ≥
> liabilities) needs a Tier-2 XRPL-balance attestation. See
> `docs/audit/RESP-commitment-r2-state-replication-gap.md`.
>
> **Not a solvency proof (AC-E1-6).** Until **AC-R1-5b** (PnL-counterparty
> conservation) and **AC-F8-4** (perp-only ring-fenced reset) land, the published
> `(epoch, root, snapshot)` MUST NOT be represented — in any deck, doc, or on-chain
> consumer — as proof that the book is solvent. It authenticates *who signed* and
> *the structure of the liabilities*, not that assets back them. Only the
> over-stated-liabilities direction fails safe (`custody_ok` false-refuses → no
> publish); an under-statement could publish a root that *looks* solvent but isn't
> — exactly what AC-R1-5b closes. The on-chain contract is named `ReservesRegistry`
> for deployment reasons; the name is not the claim.

## What it does
The **sequencer's enclave** — the sole holder of authoritative perp state —
computes the exhaustive per-asset liabilities merkle root over its sealed state,
refuses if `custody < liabilities` (per asset), and signs the Gnosis-Safe
EIP-712 `SafeTxHash` for `ReservesRegistry.publishReserves(epoch, root,
snapshotHash)` **inside the enclave**. The orchestrator only relays that owner
signature to the Safe and pays gas — it never computes or forges the root
(AC-R2-1).

## Prerequisites
- **Sequencer pool EVM address** — the Safe owner. It is `local_signer.address`
  from the node's `signers_config.json` (the same 0x… address used for governance
  signing).
- **Gas-paying EOA** — a *hot* key that only pays Base-Sepolia gas and is
  `msg.sender` for `execTransaction`. It **cannot forge** a commit (the Safe
  verifies the enclave's owner signature). Generate a fresh key; fund it with
  Base-Sepolia ETH. **Never** reuse the enclave/escrow keys.
- **QuickNode Base-Sepolia RPC** (embeds an API key — a secret).
- Foundry (`forge`) for the registry deploy; the contract + script live in the
  **enclave** repo at `EthSignerEnclave/contracts/reserves_registry/`.

## Steps
1. **Get the sequencer pool EVM address** (Safe owner):
   ```
   jq -r '.local_signer.address' <signers_config.json>   # 0x…
   ```
2. **Deploy a Safe 1-of-1** on Base-Sepolia with that owner + threshold 1
   (via app.safe.global or safe-cli). Record `SAFE=0x…`.
3. **Deploy ReservesRegistry** with `authority = SAFE` (script already exists):
   ```
   cd EthSignerEnclave/contracts/reserves_registry
   RESERVES_AUTHORITY=$SAFE \
   forge script script/DeployReservesRegistry.s.sol \
       --rpc-url "$BASE_SEPOLIA_RPC" --broadcast --private-key "$DEPLOYER_PK"
   ```
   Record `REGISTRY=0x…`. (`$DEPLOYER_PK` = a funded deployer key, env-only.)
4. **Fund the gas EOA** with Base-Sepolia ETH.
5. **Enable the publisher** on the **sequencer** node via its **systemd** unit
   `Environment=` lines (never a shell export, never committed — per the
   no-manual-shell-deploys rule):
   ```
   RESERVES_PUBLISH=1
   RESERVES_RPC_URL=https://…base-sepolia.quiknode.pro/<KEY>/   # secret
   RESERVES_GAS_KEY=0x<gas EOA private key>                      # secret, hot key
   RESERVES_REGISTRY=<REGISTRY>
   RESERVES_SAFE=<SAFE>
   RESERVES_CHAIN_ID=84532
   RESERVES_INTERVAL_SECS=3600
   ```
   `systemctl daemon-reload && systemctl restart <orchestrator unit>`.
6. **Verify** (E-1):
   - The sequencer logs `reserves-commit published to Base-Sepolia tx=0x…`.
   - On-chain `ReservesRegistry.latestReserves()` returns the same `(epoch, root,
     snapshotHash)` the enclave produced; `epoch` increases monotonically.
   - Recompute the root off-chain from the enclave's leaf set and confirm it
     matches (inclusion check for a known account).

## Tier-1 → Tier-2 (later, after full-state replication AC-R2-3)
Add the other nodes' enclave EVM keys as Safe owners and raise the threshold to
2-of-3 — a Safe **owner-add + threshold-change** governance action, **no contract
change and no re-audit** of the registry. The registry stays `onlyAuthority(Safe)`.

## Security notes
- The gas EOA is a **hot key**: least-privilege (only gas), rotatable, isolated
  from enclave/escrow keys. Compromise ⇒ DoS/gas-drain at most, never a forged
  commit.
- `RESERVES_RPC_URL` + `RESERVES_GAS_KEY` are **secrets** — systemd env only,
  never CLI args (ps-visible) and never committed.
- The publisher is **sequencer-only** and **opt-in** (`RESERVES_PUBLISH=1`);
  disabled by default.
