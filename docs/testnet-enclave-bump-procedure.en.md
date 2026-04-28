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

**Verify Path A + DKG v2 endpoints are present** (catches BuildKit cache lies):

```bash
ssh andrey@94.130.18.162 "
  strings ~/xrpl-perp-dex-enclave/EthSignerEnclave/dist-azure/perp-dex-server \
    | grep -E '/v1/pool/(ecdh|attest|frost|dkg)' | sort -u
"
```

You must see at minimum these v2 endpoints, all required by §9:
- `/v1/pool/ecdh/pubkey`, `/v1/pool/ecdh/report-data`
- `/v1/pool/attestation-quote`, `/v1/pool/attest/verify-peer-quote`
- `/v1/pool/dkg/round1-generate`, `/v1/pool/dkg/round1-export-share-v2`, `/v1/pool/dkg/round2-import-share-v2`, `/v1/pool/dkg/finalize`
- `/v1/pool/frost/share-export-v2`, `/v1/pool/frost/share-import-v2`

If any are missing, the build is stale — delete the dist dir and rebuild with `--no-cache` again. The legacy `/v1/pool/dkg/round1-export-share` and `/v1/pool/dkg/round2-import-share` (without `-v2`) are still in the binary for backwards compat but **must not be used cross-machine** — they fail with `SGX_ERROR_MAC_MISMATCH`. See `feedback_dkg_cross_machine_bug.md`.

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
      ./perp-dex-orchestrator node-bootstrap \\
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

  # Pull the 3 new xrpl_addresses from the node-bootstrap outputs
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

## 9. DKG ceremony (Pedersen, v2 ECDH-over-DCAP transport)

The legacy `/v1/pool/dkg/round1-export-share` endpoint uses `sgx_seal_data` for the share blob, which binds the seal key to the local CPU's TCB — cross-machine `unseal` always returns `SGX_ERROR_MAC_MISMATCH (12289)`. We confirmed this empirically on 2026-04-26 (see `feedback_dkg_cross_machine_bug.md`). The cluster therefore uses the **v2** endpoints — `/v1/pool/dkg/round1-export-share-v2` + `round2-import-share-v2` — which mirror the Path A v2 wire format (ECDH-over-DCAP key agreement + AES-128-GCM, AAD binds `mrenclave_self || group_id || shard_id || ceremony_nonce || sender_pk || recipient_pk`). Since this is the bootstrap, no FROST `group_id` exists yet, so we use `group_id = 32 zero bytes` as the bootstrap sentinel; after §10 the periodic announcer takes over with the real key.

**Participant IDs are 0-indexed**: node-1 → pid 0, node-2 → pid 1, node-3 → pid 2. The enclave validates `my_participant_id < n_participants`; passing pid=3 with n=3 fails. The original `setup_testnet_escrow.py` writes `signers_config.json` with three signers `node-1/2/3`, but those names are labels — the FROST pid mapping is positional in 0-based order.

The orchestrator has no DKG driver yet (Phase 2.1 will codify §9–§10 as a Rust subcommand). For now this is operator-driven curl. **Open ONE interactive bash session on Hetzner and run all §9 blocks in it** — they share state via `IPS`, `PK`, `QUOTE`, `GROUP_ZEROS` arrays:

```bash
ssh andrey@94.130.18.162   # leave this shell open; all §9 blocks run inside it

GROUP_ZEROS='0000000000000000000000000000000000000000000000000000000000000000'
declare -A IPS=([0]=20.71.184.176 [1]=20.224.243.60 [2]=52.236.130.102)
declare -A PK QUOTE
mkdir -p ~/dkg-shares
```

**Round 0 — pre-DKG attestation round (group_id = zeros).** v2 export/import refuses unless the peer is in the local `peer_attest_cache` for the requested `(shard_id, group_id, peer_pk)` tuple. Pre-DKG no `frost_group_id` is configured, so the periodic announcer is dormant. We drive a one-shot attestation round manually with the bootstrap sentinel.

For each node: collect ECDH pubkey + DCAP report_data (bound to `(shard=0, group=zeros)`) + DCAP quote. Then for each (sender, receiver) pair the receiver verifies the sender's quote, populating its cache.

```bash
for pid in 0 1 2 ; do
  ip=${IPS[$pid]}
  PK[$pid]=$(ssh azureuser@$ip 'curl -k -s https://localhost:9088/v1/pool/ecdh/pubkey' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["pubkey"].removeprefix("0x"))')
  rd=$(ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"shard_id\":0,\"group_id\":\"$GROUP_ZEROS\"}' \
    https://localhost:9088/v1/pool/ecdh/report-data" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["report_data"].removeprefix("0x"))')
  QUOTE[$pid]=$(ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"user_data\":\"$rd\"}' \
    https://localhost:9088/v1/pool/attestation-quote" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["quote_hex"].removeprefix("0x").lower())')
done

NOW=$(date +%s)
for tgt in 0 1 2 ; do
  ip=${IPS[$tgt]}
  for src in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 -c "import json; open('/tmp/v_${tgt}_${src}.json','w').write(json.dumps({
      'quote':'${QUOTE[$src]}','peer_pubkey':'${PK[$src]}',
      'shard_id':0,'group_id':'$GROUP_ZEROS','now_ts':$NOW}))"
    scp /tmp/v_${tgt}_${src}.json azureuser@$ip:/tmp/vbody.json >/dev/null
    rc=$(ssh azureuser@$ip 'curl -k -s -o /dev/null -w %{http_code} -X POST \
      -H "Content-Type: application/json" --data-binary @/tmp/vbody.json \
      https://localhost:9088/v1/pool/attest/verify-peer-quote')
    echo "pid=$tgt verifies pid=$src → HTTP $rc"
  done
done
```

You must see all 6 verifications return `HTTP 200`. A `400 quote must be non-empty hex` is the lstrip foot-gun (see Appendix A); a `403` means DCAP collateral is stale or the quote is malformed.

**Round 1 — VSS commitment.** Each node generates its commitment polynomial; the result is public.

```bash
for pid in 0 1 2 ; do
  ip=${IPS[$pid]}
  ssh azureuser@$ip "curl -k -s -X POST -H 'Content-Type: application/json' \
    -d '{\"my_participant_id\":$pid,\"threshold\":2,\"n_participants\":3}' \
    https://localhost:9088/v1/pool/dkg/round1-generate" > /tmp/r1_$pid.json
done
```

Each `vss_commitment` is `threshold × 33 bytes` of compressed-pubkey hex (132 chars + `0x`). It is non-secret and must travel to every other node so they can verify the share you'll send next.

**Round 1.5 — share export (v2).** Each node exports one ECDH-wrapped envelope per peer.

```bash
NOW=$(date +%s)
for src in 0 1 2 ; do
  ip=${IPS[$src]}
  for tgt in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 -c "import json; open('/tmp/exp_${src}_${tgt}.json','w').write(json.dumps({
      'target_participant_id': $tgt, 'peer_pubkey': '${PK[$tgt]}',
      'shard_id': 0, 'group_id': '$GROUP_ZEROS', 'now_ts': $NOW
    }))"
    scp /tmp/exp_${src}_${tgt}.json azureuser@$ip:/tmp/expbody.json >/dev/null
    ssh azureuser@$ip 'curl -k -s -X POST -H "Content-Type: application/json" \
      --data-binary @/tmp/expbody.json \
      https://localhost:9088/v1/pool/dkg/round1-export-share-v2' > ~/dkg-shares/exp_${src}_to_${tgt}.json
  done
done
```

Each response carries `{status, target_participant_id, my_participant_id, envelope: {ceremony_nonce, iv, ct, tag, sender_pubkey}}`. The envelope is intelligible only to the target enclave (its ECDH identity is in the AAD).

**Round 2 — import + verify (v2).** Each node imports the two envelopes destined for it, attaching the sender's public `vss_commitment` so the enclave can verify the share.

```bash
NOW=$(date +%s)
for tgt in 0 1 2 ; do
  ip=${IPS[$tgt]}
  for src in 0 1 2 ; do
    [ $src -eq $tgt ] && continue
    python3 - <<PYEOF > /tmp/imp_body.json
import json
exp = json.load(open('$HOME/dkg-shares/exp_${src}_to_${tgt}.json'))
r1  = json.load(open('/tmp/r1_${src}.json'))
print(json.dumps({
    'from_participant_id': $src,
    'sender_pubkey': '${PK[$src]}',
    'shard_id': 0,
    'group_id': '$GROUP_ZEROS',
    'now_ts': $NOW,
    'envelope': exp['envelope'],
    'vss_commitment': r1['vss_commitment'].removeprefix('0x'),
}))
PYEOF
    scp /tmp/imp_body.json azureuser@$ip:/tmp/impbody.json >/dev/null
    rc=$(ssh azureuser@$ip 'curl -k -s -o /dev/null -w %{http_code} -X POST \
      -H "Content-Type: application/json" --data-binary @/tmp/impbody.json \
      https://localhost:9088/v1/pool/dkg/round2-import-share-v2')
    echo "tgt=$tgt imports from src=$src → HTTP $rc"
  done
done
```

All 6 imports must return `HTTP 200`. A `403 sender not attested` means §9.0 didn't populate the attest cache for that pair (or the cache TTL of 5 min has elapsed — re-run §9.0). A `403 AEAD failed` means the envelope was tampered with in transit; abort, do not retry. A `403 VSS verification failed` means the peer constructed a share inconsistent with their commitment — that is the malicious-peer signal, abort and investigate.

**Finalize.** Each node aggregates the shares it received and emits the group pubkey.

```bash
for pid in 0 1 2 ; do
  out=$(ssh azureuser@${IPS[$pid]} 'curl -k -s -X POST https://localhost:9088/v1/pool/dkg/finalize')
  echo "pid=$pid → $out"
done
```

All three must report the **byte-identical** `group_pubkey` (32-byte BIP340 x-only, 64 hex chars + `0x`). If they diverge, the DKG transcript was tampered with — abort.

Reference run (2026-04-27): `group_pubkey = 0x847151fe514df4c5e43914bbc0fcc560c70e91c2550198b1a97aa13a368a2293` on all three nodes.

## 10. Configure Path A group + restart orchestrators

Add the 32-byte hex to `shards.toml` on each Azure node:

```toml
[[shards]]
shard_id = 0
enclave_url = "https://localhost:9088/v1"
frost_group_id = "<GROUP_ID_HEX from step 9>"
```

Restart each orchestrator. The Path A peer-quote announcer will wake up (it stays dormant when `frost_group_id` is unset; see `path_a_redkg.rs`).

## 11. Path A wire test (optional regression handle)

§9 already exercises the full ECDH-over-DCAP transport: the v2 export/import code path is **the same** for DKG-bootstrap and post-DKG share rotation, only the source data differs (`dkg_session.my_shares[]` vs `frost_group.shares[signer_id]`). If §9 finalized cleanly, the wire format is verified.

Skip this section unless you want a dedicated regression handle for the post-DKG share-rotation flow specifically (e.g., when adding a fourth operator or doing a share refresh without an MRENCLAVE bump). The post-DKG path uses `/v1/pool/frost/share-export-v2` + `/v1/pool/frost/share-import-v2` (note: `/frost/`, not `/dkg/`) and is driven by the orchestrator's loopback admin route at `/admin/path-a/share-export`. The drop-in pattern for `--admin-listen 127.0.0.1:9099` and the `systemctl revert` to remove it after the test are documented in `path_a_redkg.rs` and the original Phase 6b commit message.

## 12. Multisig signing smoke test

End-to-end check that the new escrow + new SignerList + new operator keys all work together. Faucet a fresh secp256k1 user wallet, deposit a few XRP into the new escrow, then withdraw 1 XRP back. The orchestrator collects 2-of-3 multisig signatures via `/v1/pool/sign` on each operator's enclave and submits via `submit_multisigned`. The success signal is a `tesSUCCESS` validated tx on testnet.

**Step A — deposit 5 XRP from a fresh secp256k1 wallet.** Run on Hetzner:

```bash
ssh andrey@94.130.18.162 "
ESCROW_ADDR=\$(jq -r .escrow_address ~/.secrets/perp-dex-xrpl/escrow-testnet.json)
python3 - <<PYEOF
import json, time
from xrpl.clients import JsonRpcClient
from xrpl.wallet import Wallet, generate_faucet_wallet
from xrpl.constants import CryptoAlgorithm
from xrpl.models.transactions import Payment
from xrpl.transaction import submit_and_wait
from xrpl.utils import xrp_to_drops

client = JsonRpcClient('https://s.altnet.rippletest.net:51234')
fresh = Wallet.create(algorithm=CryptoAlgorithm.SECP256K1)
funded = generate_faucet_wallet(client, wallet=fresh, debug=False)
print('user_id =', funded.classic_address)
pay = Payment(account=funded.classic_address, destination='\$ESCROW_ADDR', amount=xrp_to_drops(5))
resp = submit_and_wait(pay, client, funded)
print('deposit_tx_hash =', resp.result.get('hash'))
print('seed =', funded.seed)
time.sleep(20)  # let the deposit scanner credit the user
PYEOF
"
```

xrpl-py defaults to `ED25519` for `Wallet.create()` — you **must** pass `algorithm=CryptoAlgorithm.SECP256K1` or the orchestrator's auth path (which expects a secp256k1 family generator) will fail to verify. See Appendix A.

**Step B — withdraw 1 XRP via the orchestrator CLI on node-1.**

```bash
ssh andrey@94.130.18.162 "
  ssh azureuser@20.71.184.176 \
    \"~/perp/perp-dex-orchestrator withdraw \
      --api http://localhost:3000 \
      --seed '<seed from step A>' \
      --amount 1.00000000 \
      --destination '<user_id from step A>'\"
"
```

The success response carries `xrpl_tx_hash`. Verify on chain:

```python
from xrpl.clients import JsonRpcClient
from xrpl.models.requests import Tx
client = JsonRpcClient('https://s.altnet.rippletest.net:51234')
r = client.request(Tx(transaction='<xrpl_tx_hash>')).result
assert r['meta']['TransactionResult'] == 'tesSUCCESS'
assert r['validated'] is True
assert len(r['tx_json']['Signers']) >= 2  # quorum=2 reached
```

The `Signers[]` array should contain **2 of the 3** operator addresses — the orchestrator stops collecting sigs once quorum is met (the unused operator's signer slot is omitted, not zero-padded).

Reference run (2026-04-27): user `rJWZfQuNvAqLDBBFR5eNGrdztbXSqpbipU`, withdrawal `0AD9913799EC94078CC36463B491B0CF1A7FD4AC8D951246958B6226289A856F` (`tesSUCCESS`, validated, 2 signers — node-1 + node-2; node-3 not needed).

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

## Appendix A — common foot-guns

These are the specific traps we hit while running this procedure on 2026-04-26/27. None of them are "bugs in the procedure" — they're places where Python/JS string semantics or XRPL defaults silently produce wrong values that look right.

**`str.lstrip("0x")` is NOT `str.removeprefix("0x")`.** `lstrip` takes a *set* of chars and strips any combination from the left. On `"0x030002…"` it strips `0x03` (matching the chars `{0, x, 0, 3?}` — actually `{0, x}` repeated) leaving `"3002…"`, which is both wrong content and odd-length (so subsequent `hex_to_bytes` fails with "non-empty hex" or similar misleading errors). Always use `removeprefix("0x")`. We were bitten by this in the §9.0 attestation round; symptom is `HTTP 400 quote must be non-empty hex` despite the quote string clearly being non-empty.

**FROST participant_id is 0-indexed.** The enclave validates `my_participant_id < n_participants`. With `n_participants=3` only pids `{0, 1, 2}` are valid; `pid=3` returns `HTTP 500 DKG round1 generate failed`. Earlier versions of this doc said "Participant IDs are 1–3" which was wrong — the convention got cleaned up after §9.0 of the 2026-04-27 bump exposed it.

**`Wallet.create()` and `Wallet.from_seed()` default to ED25519 even for secp256k1 seeds.** xrpl-py's API silently picks ed25519 unless you pass `algorithm=CryptoAlgorithm.SECP256K1`. The orchestrator's auth flow uses an XRPL secp256k1 family generator (`derive_keypair_from_seed`); ed25519 keys won't decode. Symptom is `HTTP 401` from `/v1/withdraw` even though the seed/address pair looks valid. Same trap exists in `generate_faucet_wallet(client, wallet=…)` — pass an explicitly-secp256k1 `wallet` argument; do NOT call `generate_faucet_wallet(client)` with no `wallet` and expect to swap algorithms later.

**`peer_attest_cache` TTL is 5 minutes.** v2 export/import refuses if the peer's verified DCAP quote has aged out. If §9.4 finalize fails with `403 sender not attested` on a re-run, the cause is usually that §9.0 ran more than 5 minutes ago — re-run it.

**SSH-shell-quoting clobbers large hex strings.** Passing a 9.5 KB DCAP quote through three levels of shell escaping (laptop → bash → ssh → bash → curl `-d`) loses bytes silently — the receiving side sees a JSON body where the `quote` field is truncated or empty. Always write request bodies to a file with Python's `json.dumps`, `scp` the file, then `curl --data-binary @/tmp/body.json`. Patterns in §9 use this.

**`unix2dos -q` files where the original was CRLF.** Some files in the enclave repo (`server/server.cpp`, `server/api/v1/pool_handler.hpp`) are committed with CRLF line endings. Any tool that rewrites them with LF (Python's `Path.write_text`, etc.) makes `git diff --stat` show every line as changed, drowning the actual edit. Restore CRLF with `unix2dos -q <file>` after edit; the diff collapses back to just your real change.
