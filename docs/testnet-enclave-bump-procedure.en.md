# Testnet Enclave MRENCLAVE-Bump Procedure

**Status:** authoritative for testnet enclave updates.
**Scope:** any change to enclave C++ code (= new MRENCLAVE) on the 3-Azure testnet cluster.
**Mainnet:** see `deployment-procedure.md §11.5 — Path B`. This document does NOT replace that.

## 0. Invariants

1. **MRENCLAVE-bound sealing.** Every sealed blob (`account.sealed`, `frost_share.sealed`, perp state, `ecdh_identity.sealed` once it exists) is bound to MRENCLAVE. A new MRENCLAVE cannot unseal anything from the old one. MRSIGNER-based sealing is permanently rejected — see `deployment-dilemma.en.md §Strategy 2 — REJECTED` and `sgx-enclave-capabilities-and-limits.md`.

2. **Therefore every enclave bump = key rotation.** All XRPL multisig keys are regenerated. The on-chain SignerList becomes invalid the moment we swap binaries, so a SignerListSet is mandatory and non-conditional.

3. **Testnet vs mainnet.** Testnet tolerates a single-wave rip-and-replace because no XRP is at risk. The testnet escrow is **also rotated** each bump — a fresh faucet-funded escrow + fresh SignerListSet, not a re-keying of an existing one. The previous escrow is left orphaned. Mainnet behaves differently: same escrow address forever, key rotation only via `deployment-procedure.md §11.5` Path B (staging port, peer DCAP verify, two-step SignerListSet with quorum→3 buffer, soak, shred, promote). Do **not** export testnet shortcuts to mainnet.

4. **Out-of-cluster services on Hetzner.** The Hetzner dev enclave on `:9089` and dev orchestrator on `:3003` (units `perp-dex-enclave-dev.service` and `perp-dex-orchestrator-dev.service`) are NOT part of the testnet cluster — different escrow, different p2p port. The retired mainnet enclave on `:9088` (PID from last boot, no orchestrator attached) is also outside scope. **Leave all three alone.** Stop/start commands in this procedure target only the 3 Azure VMs.

## 1. Pre-flight

**Self-contained prerequisites.** This procedure does not require any operator-held secrets. The testnet escrow seed lives at the canonical path `~/.secrets/perp-dex-xrpl/escrow-testnet.json` on Hetzner (mode 0600). If that file is missing or stale, step 7 creates a fresh testnet escrow via faucet and writes the new seed there — no human-memory dependency. See `feedback_secrets_canonical_files.md` for the rule.

Run from your local laptop. All of these are read-only.

```bash
# 1.1 Confirm we're on the laptop, not Hetzner. Build will be on Hetzner.
hostname

# 1.2 Both repos at expected tips on Hetzner.
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl && git fetch && git log --oneline -1 origin/master
  cd ~/xrpl-perp-dex-enclave && git fetch && git log --oneline -1 origin/main
"

# 1.3 Inventory the 3 Azure VMs — they must all be reachable through the bastion.
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  ssh andrey@94.130.18.162 "ssh -o ConnectTimeout=5 azureuser@$ip 'hostname'"
done
```

## 2. Build on Hetzner

Per `feedback_enclave_build_gate.md`: the laptop is not a build gate; only Hetzner counts.

```bash
ssh andrey@94.130.18.162 "
  set -e
  # Safety: refuse to wipe uncommitted work in either repo.
  for repo in ~/llm-perp-xrpl ~/xrpl-perp-dex-enclave; do
    cd \$repo
    if [ -n \"\$(git status -uno --porcelain)\" ]; then
      echo \"\$repo working tree is dirty — commit/stash before continuing\" >&2
      exit 1
    fi
  done

  # Sync working trees to origin tips
  cd ~/llm-perp-xrpl && git checkout master && git pull --ff-only
  cd ~/xrpl-perp-dex-enclave && git checkout main && git reset --hard origin/main

  # Orchestrator (~30 s with cache, ~2 min cold)
  cd ~/llm-perp-xrpl/orchestrator
  ~/.cargo/bin/cargo build --release

  # Enclave — ALWAYS --no-cache. BuildKit's COPY cache has misled us
  # before; the cost of an extra 5–10 min is worth the certainty.
  cd ~/xrpl-perp-dex-enclave/EthSignerEnclave
  TAG=phase7-pathA-\$(date +%Y%m%d-%H%M%S)
  docker build --no-cache -f Dockerfile.azure -t perp-dex-enclave:\$TAG .

  # Extract to a fresh dist dir (don't overwrite the previous baseline)
  mv dist-azure dist-azure.prev-\$(date +%Y%m%d-%H%M%S) 2>/dev/null || true
  mkdir -p dist-azure
  cid=\$(docker create perp-dex-enclave:\$TAG)
  docker cp \$cid:/build/out/enclave.signed.so dist-azure/
  docker cp \$cid:/build/out/perp-dex-server   dist-azure/
  docker rm \$cid

  # Pin the build manifest with git_sha + sha256 + timestamp.
  cat > dist-azure/build-manifest.txt <<EOF
git_sha=\$(git rev-parse --short HEAD)
build_date=\$(date -u +%Y-%m-%dT%H:%M:%SZ)
image=perp-dex-enclave:\$TAG
enclave_sha256=\$(sha256sum dist-azure/enclave.signed.so | awk '{print \$1}')
server_sha256=\$(sha256sum  dist-azure/perp-dex-server  | awk '{print \$1}')
EOF
  cat dist-azure/build-manifest.txt
"
```

**Verify Path A endpoints are present** (catches BuildKit cache lies):

```bash
ssh andrey@94.130.18.162 "
  strings ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server \
    | grep -E '/v1/pool/(ecdh|attest|frost)' | sort -u
"
```

You must see all 16 endpoints, including `/v1/pool/ecdh/pubkey`, `/v1/pool/attest/verify-peer-quote`, `/v1/pool/frost/share-export-v2`, `/v1/pool/frost/share-import-v2`. If any are missing, the build is stale — delete the dist dir and rebuild with `--no-cache` again.

## 3. Stop the cluster (one wave)

There is no rolling-upgrade window on testnet. Stop everything before swapping anything — a node that comes up new while peers are still old will publish on `perp-dex/path-a/peer-quote` to confused listeners and noise up the logs.

This stops **only the 3 Azure VMs**. Do not touch the Hetzner-side units (`perp-dex-enclave-dev`, `perp-dex-orchestrator-dev`) or the retired mainnet enclave on `:9088` — see §0 invariant 4.

While stopping, also save each Azure node's current `signers_config.json` so step 13 can roll back cleanly if the new procedure fails.

```bash
ssh andrey@94.130.18.162 "
  TS=\$(date +%Y%m%d-%H%M%S)
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    echo == \$ip ==
    ssh azureuser@\$ip \"
      sudo systemctl stop perp-dex-orchestrator perp-dex-enclave
      cp -a /home/azureuser/perp/signers_config.json /home/azureuser/perp/signers_config.json.prev-\$TS
    \"
  done
"
```

## 4. Swap binaries

Keep the previous artefact next to the new one. The `accounts/` dir on each node also gets a timestamped copy — even though the new MRENCLAVE cannot unseal it, we keep it as forensic evidence rather than deleting blind.

To avoid three-level shell escaping (laptop → Hetzner → Azure), build the per-Azure swap script once on Hetzner via heredoc, then `scp` and `bash` it on each VM. All variables expand on Hetzner before transmission.

```bash
ssh andrey@94.130.18.162 'bash -s' <<'OUTER'
set -e
TS=$(date +%Y%m%d-%H%M%S)
cat > /tmp/swap.sh <<SCRIPT
#!/bin/bash
set -e
cd /home/azureuser/perp
mv enclave.signed.so       enclave.signed.so.prev-${TS}
mv perp-dex-server         perp-dex-server.prev-${TS}
mv perp-dex-orchestrator   perp-dex-orchestrator.prev-${TS}
cp -a accounts             accounts.prev-${TS}
SCRIPT
chmod +x /tmp/swap.sh

for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  echo "== $ip =="
  scp /tmp/swap.sh azureuser@$ip:/tmp/swap.sh
  ssh azureuser@$ip 'bash /tmp/swap.sh'
  scp ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/enclave.signed.so azureuser@$ip:/home/azureuser/perp/
  scp ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server   azureuser@$ip:/home/azureuser/perp/
  scp ~/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator    azureuser@$ip:/home/azureuser/perp/
  ssh azureuser@$ip 'rm -rf ~/perp/accounts && mkdir ~/perp/accounts && rm -f /tmp/swap.sh'
done
rm -f /tmp/swap.sh
OUTER
```

## 5. Start enclaves only

Orchestrators stay down — they can't authenticate against the live testnet escrow yet (old SignerList still on chain).

```bash
ssh andrey@94.130.18.162 "
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    ssh azureuser@\$ip 'sudo systemctl start perp-dex-enclave && sleep 2 && curl -k -s https://localhost:9088/v1/health'
  done
"
```

You should see a healthy response from each enclave on `:9088`. The new enclave starts with empty sealed state.

## 6. Generate fresh keypairs per node

The orchestrator binary doubles as an operator CLI. From each Azure node, talk to its local enclave on `:9088`:

```bash
ssh andrey@94.130.18.162 "
  mkdir -p ~/phase7-entries
  for i in 1 2 3; do
    case \$i in
      1) ip=20.71.184.176 ;;
      2) ip=20.224.243.60 ;;
      3) ip=52.236.130.102 ;;
    esac
    ssh azureuser@\$ip \"
      cd ~/perp
      ./perp-dex-orchestrator operator-setup \\
        --enclave-url https://localhost:9088/v1 \\
        --name node-\$i \\
        --output /tmp/node-\$i.json
    \"
    scp azureuser@\$ip:/tmp/node-\$i.json ~/phase7-entries/
  done
"
```

Each `/tmp/node-N.json` contains the new XRPL address, compressed pubkey, session key. The enclave seals the corresponding private key locally.

## 7. Create fresh testnet escrow + register signers (one shot)

On testnet we do **not** preserve the escrow across enclave bumps. The seed for the previous escrow is rarely captured (the original `setup_testnet_escrow.py` printed it to stdout only, see `feedback_secrets_canonical_files.md`), and faucet escrows are free anyway. Each bump = fresh testnet escrow with a fresh seed file.

The patched `setup_testnet_escrow.py` does both in one shot: faucet-fund a new escrow, submit SignerListSet for the 3 new node addresses, disable master key, write seed to `~/.secrets/perp-dex-xrpl/escrow-testnet.json` (0600).

```bash
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl

  # Move aside any prior testnet seed file (from a previous bump)
  if [ -f ~/.secrets/perp-dex-xrpl/escrow-testnet.json ]; then
    mv ~/.secrets/perp-dex-xrpl/escrow-testnet.json \\
       ~/.secrets/perp-dex-xrpl/escrow-testnet.json.prev-\$(date +%Y%m%d-%H%M%S)
  fi

  # Pull the 3 new xrpl_addresses from the operator-setup outputs
  N1=\$(jq -r .xrpl_address ~/phase7-entries/node-1.json)
  N2=\$(jq -r .xrpl_address ~/phase7-entries/node-2.json)
  N3=\$(jq -r .xrpl_address ~/phase7-entries/node-3.json)

  python3 orchestrator/scripts/setup_testnet_escrow.py \\
    --signer node-1=\$N1 \\
    --signer node-2=\$N2 \\
    --signer node-3=\$N3 \\
    --quorum 2
"
```

Output prints `ESCROW_ADDRESS=r…` and `SEED_FILE=…`. Verify on https://testnet.xrpl.org that the new address has quorum 2 with the three node addresses, and master key disabled.

Save the new escrow address as a shell variable for step 8:

```bash
ESCROW_ADDR=$(ssh andrey@94.130.18.162 "jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json")
echo "$ESCROW_ADDR"
```

The unified `signers_config.json` for the cluster is built using the **new** escrow address:

```bash
ssh andrey@94.130.18.162 "
  cd ~/llm-perp-xrpl/orchestrator
  ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)
  ./target/release/perp-dex-orchestrator config-init \\
    --entries ~/phase7-entries/node-1.json \\
              ~/phase7-entries/node-2.json \\
              ~/phase7-entries/node-3.json \\
    --escrow-address \$ESCROW_ADDR \\
    --quorum 2 \\
    --output ~/phase7-entries/signers_config.json
"
```

## 8. Distribute config + start orchestrators

Each node needs:
- A copy of `signers_config.json` with its own `local_signer` field set (shape: `FullSignersConfig` in `cli_tools.rs`).
- An updated `start_orchestrator.sh` (or systemd unit ExecStart) pointing `--escrow-address` at the **new** escrow created in step 7.

```bash
ssh andrey@94.130.18.162 "
  cd ~/phase7-entries
  ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)

  for i in 1 2 3; do
    case \$i in
      1) ip=20.71.184.176 ;;
      2) ip=20.224.243.60 ;;
      3) ip=52.236.130.102 ;;
    esac

    # Build per-node signers_config with the right local_signer pointer
    jq --argjson local \"\$(cat node-\$i.json)\" '. + {local_signer: \$local}' \\
      signers_config.json > /tmp/signers_config_node-\$i.json
    scp /tmp/signers_config_node-\$i.json azureuser@\$ip:/home/azureuser/perp/signers_config.json

    # Update start_orchestrator.sh: replace the old --escrow-address rUjzn... with new one.
    # Use sed against the regex 'r[1-9A-HJ-NP-Za-km-z]{24,34}' (XRPL r-address shape).
    ssh azureuser@\$ip \"
      cp -a ~/perp/start_orchestrator.sh ~/perp/start_orchestrator.sh.prev-\$(date +%Y%m%d-%H%M%S)
      sed -i -E 's|--escrow-address +r[1-9A-HJ-NP-Za-km-z]{24,34}|--escrow-address \$ESCROW_ADDR|' ~/perp/start_orchestrator.sh
      grep -- '--escrow-address' ~/perp/start_orchestrator.sh
      sudo systemctl start perp-dex-orchestrator
    \"
  done
"
```

Wait ~30 s, then verify the p2p mesh:

```bash
ssh andrey@94.130.18.162 "
  for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
    ssh azureuser@\$ip 'curl -s http://localhost:3000/v1/health'
    echo
  done
"
```

## 9. DKG ceremony (4-stage Pedersen)

The orchestrator has no DKG driver — it's operator-driven curl. The 4 stages run on each Azure node against its local enclave on `:9088`. Participant IDs are 1–3, threshold 2, n 3.

**Round 1 — VSS commitment.** Each node generates its commitment polynomial; this is public.

```bash
# Run on each Azure node, substituting MY_ID = 1, 2, 3
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round1-generate \
  -H 'Content-Type: application/json' \
  -d '{"my_participant_id": MY_ID, "threshold": 2, "n_participants": 3}' \
  > /tmp/round1-MY_ID.json
```

The response has `vss_commitment` (hex). It is non-secret and must be broadcast to the other two nodes.

**Round 1.5 — pairwise sealed share export.** Each node generates one sealed share per peer.

```bash
# On node MY_ID, for each peer TARGET_ID ∈ {1,2,3} \ {MY_ID}:
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round1-export-share \
  -H 'Content-Type: application/json' \
  -d '{"target_participant_id": TARGET_ID}' \
  > /tmp/share-from-MY_ID-to-TARGET_ID.json
```

Now the operator manually shuffles `(sealed_share, vss_commitment)` pairs between Azure VMs via the Hetzner bastion (Azure-to-Azure SSH is closed to Hetzner-key only). The sealed share is encrypted to the target enclave's identity; the vss_commitment is public.

**Round 2 — import + verify.** Each node imports the two shares it received from peers; the enclave verifies each share against the matching VSS commitment.

```bash
# On node TARGET_ID, for each FROM_ID ∈ {1,2,3} \ {TARGET_ID}:
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/round2-import-share \
  -H 'Content-Type: application/json' \
  -d "{
    \"from_participant_id\": FROM_ID,
    \"sealed_share\":      \"$(jq -r .sealed_share share-from-FROM_ID-to-TARGET_ID.json)\",
    \"vss_commitment\":    \"$(jq -r .vss_commitment round1-FROM_ID.json)\"
  }"
```

A 500 here means VSS verification failed — the peer either misbehaved or the share was corrupted in transit. **Abort and restart from Round 1**. Do not silently retry; investigate first.

**Finalize.** Each node finalizes; all three must produce the same `group_pubkey` (32-byte BIP340 x-only).

```bash
curl -k -s -X POST https://localhost:9088/v1/pool/dkg/finalize > /tmp/finalize.json
GROUP_ID_HEX=$(jq -r .group_pubkey /tmp/finalize.json)
echo "$GROUP_ID_HEX"  # 64 hex chars
```

Cross-check: the `group_pubkey` value must be byte-identical on all three nodes. If they diverge, the DKG transcript was tampered with — abort.

## 10. Configure Path A group + restart orchestrators

Add the 32-byte hex to `shards.toml` on each Azure node:

```toml
[[shards]]
shard_id = 0
enclave_url = "https://localhost:9088/v1"
frost_group_id = "<GROUP_ID_HEX from step 9>"
```

Restart each orchestrator. The Path A peer-quote announcer will wake up (it stays dormant when `frost_group_id` is unset; see `path_a_redkg.rs`).

## 11. Path A wire test

Pick one node as ceremony sender — say node-1. Restart its orchestrator with `--admin-listen 127.0.0.1:9099`. The other two stay unchanged. Per the security design, admin-listen is off by default and binds loopback-only.

Add the flag via a systemd drop-in. First inspect the current ExecStart on node-1 so you know what to replicate:

```bash
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'systemctl cat perp-dex-orchestrator | grep ExecStart'"
```

Create a drop-in that overrides ExecStart to the existing line + the new flag:

```bash
ssh andrey@94.130.18.162 'ssh azureuser@20.71.184.176 "sudo systemctl edit perp-dex-orchestrator"'
# In the editor, paste:
#   [Service]
#   ExecStart=
#   ExecStart=<paste current ExecStart verbatim, append> --admin-listen 127.0.0.1:9099
# Save, exit. Then:
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator'"
```

The empty `ExecStart=` line is required: it clears the inherited value before you set the new one.

Wait ~5 minutes for the periodic peer-quote announcer (240 s interval) to make all three peers visible in each other's attest cache. Verify via `/v1/pool/attest/peer-lookup` if needed.

Then trigger share-export on node-1:

```bash
ssh azureuser@20.71.184.176 "
  curl -s -X POST http://127.0.0.1:9099/admin/path-a/share-export \\
    -H 'Content-Type: application/json' \\
    -d '{
      \"shard_id\": 0,
      \"group_id\": \"$GROUP_ID_HEX\",
      \"signer_id\": 1,
      \"targets\": [
        \"<node-2 ECDH pubkey from /v1/pool/ecdh/pubkey>\",
        \"<node-3 ECDH pubkey from /v1/pool/ecdh/pubkey>\"
      ]
    }'
"
```

On nodes 2 and 3, watch the orchestrator logs for `verified peer quote` followed by `imported v2 FROST share`. On node-1, the response body has `published: 2, refused: 0, errored: 0`.

After the test, drop the override and restart so admin-listen goes back to off:

```bash
ssh andrey@94.130.18.162 "ssh azureuser@20.71.184.176 'sudo systemctl revert perp-dex-orchestrator && sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator'"
```

`systemctl revert` removes all drop-ins and returns to the base unit. The admin surface should not stay live.

## 12. Multisig signing smoke test

End-to-end check that the new SignerList works. Submit any small testnet withdrawal through the orchestrator's API; it triggers the multisig flow that hits `/pool/sign` on each enclave and submits via `submit_multisigned`. A signed-and-confirmed tx hash is the success signal.

## 13. Rollback

If anything fails between steps 5 and 12 and the cluster is unrecoverable:

1. `systemctl stop perp-dex-enclave perp-dex-orchestrator` on all 3 Azure nodes.
2. Restore the `*.prev-<timestamp>` artefacts saved aside earlier:
   - binaries from step 4 (`enclave.signed.so.prev-…`, `perp-dex-server.prev-…`, `perp-dex-orchestrator.prev-…`)
   - sealed state from step 4 (`accounts.prev-<TS>` → `accounts/`)
   - `signers_config.json.prev-<TS>` saved in step 3
   - `start_orchestrator.sh.prev-<TS>` saved in step 8 (restores the old `--escrow-address`)
3. If §11 was reached, drop any admin-listen override: `sudo systemctl revert perp-dex-orchestrator` on the sender node.
4. Restart enclaves + orchestrators. The previous testnet escrow (from before this bump) is still on chain with its old SignerList — restored binaries + restored config will work against it as before.
5. **If §7 already created a new escrow:** no on-chain rollback is needed for the new escrow — it's faucet-funded testnet, abandon it. The seed file `~/.secrets/perp-dex-xrpl/escrow-testnet.json` should be moved aside (it was already moved before §7 started, see `escrow-testnet.json.prev-<TS>` in `~/.secrets/perp-dex-xrpl/`).

Document the failure mode in this file under a new section. Future-you will thank you.

## What this procedure does NOT cover

- **Mainnet** updates — see `deployment-procedure.md §11.5 — Path B`.
- **DKG without enclave bump** (e.g., adding a fourth operator to an existing group) — that's a follow-up doc; this one assumes a full reset.
- **Recovery from lost shares** — the recovery flow (`ecall_generate_account_with_recovery`) is out of scope.
