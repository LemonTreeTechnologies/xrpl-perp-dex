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
>
> **Operator-capital exclusion is DECLARED, not proven (reserves-input attestation).**
> The v2 `snapshot` commits a canonical hash of the operator-DECLARED operator-capital
> excluded-senders (whose escrow payments count as custody, not liabilities). This is
> **tamper-EVIDENT** — the exclusion is committed on-chain, unchangeable-after-the-fact
> and attributable, so a wrongly-excluded user deposit or a silently-changed exclusion
> policy is **detectable** by the affected user or any watcher. It is **NOT
> tamper-PROOF**: the enclave commits the host's *declared* set at publish time and does
> **NOT** independently verify it against the raw XRPL deposits — that is **AC-BASE-2″
> in-enclave SPV**, which remains the mainnet backing gate. **This disclosure does NOT
> establish backing; it makes the exclusion auditable.** The same excluded_senders_hash is also committed in the one-shot baseline marker (the baseline seeds custody := escrow, which includes operator-capital).
>
> **Baseline observation sources are DECLARED, not proven (source-diversity attestation).**
> The sealed baseline marker records the accepted per-observation XRPL **source
> fingerprints** the quorum spanned, so the "N INDEPENDENT observations" claim is
> auditable from the marker. This attests **source diversity** (a different property from
> the exclusion above), and it is likewise **host-DECLARED, not proven** — the distinctness
> was enforced at ceremony time, but the recorded fingerprints are what the orch reports,
> not an in-enclave proof. An observation quorum is **not** backing; **AC-BASE-2″ SPV
> remains the mainnet gate.**

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

## Enabling SPV-proven deposits (#131 P3) — NOT YET ON

Three steps, in order. The first is a prerequisite, not a flag.

1. **A custody baseline must be proven and sealed** (the ceremony above). The deposit
   boundary is a copy of the sealed reserves floor, so until a baseline exists there is
   nothing to copy and arming returns `-84` forever — a prerequisite, not a transient
   failure.
2. **Arm the boundary**, one command:

   ```
   perp-dex-orchestrator arm-spv-deposit-boundary
   ```

   It takes no value: the enclave copies its own sealed floor, so there is no number for
   the operator to supply and therefore none to get wrong. `-62` means it was already
   armed — a NO-OP, not a failure. **Until this runs, every proven deposit refuses with
   `-85`**, so the flag in step 3 alone produces a log full of refusals and not one
   credited deposit.
3. **Start the scanner** — `PERP_DEPOSIT_SPV=1` on the sequencer, set in the systemd unit
   rather than a shell. The validations collector already runs on every node: the attested
   clock shares it, and two collectors on one stream would double the subscription for no
   gain.

**What switching it on changes, in money terms.** Deposits proven against the pinned UNL
start crediting balances automatically, with sender, amount, ledger and transaction
identity derived in-enclave and never asserted by this side. The enclave refuses any proof
at or below `max(last_credited, boundary)`, so arming cannot replay anything already
settled. The scanner is sequencer-only, because only that node holds the authoritative
state.

**State as of 2026-10-08.** Step 2's route has existed since P3 with **no caller**, which
is why the flag could not be switched on at all; the command above closes that gap. Steps
1 and 3 have not been performed, and `deposit-spv` is OFF on all three nodes.

## Security notes
- The gas EOA is a **hot key**: least-privilege (only gas), rotatable, isolated
  from enclave/escrow keys. Compromise ⇒ DoS/gas-drain at most, never a forged
  commit.
- `RESERVES_RPC_URL` + `RESERVES_GAS_KEY` are **secrets** — systemd env only,
  never CLI args (ps-visible) and never committed.
- The publisher is **sequencer-only** and **opt-in** (`RESERVES_PUBLISH=1`);
  disabled by default.
