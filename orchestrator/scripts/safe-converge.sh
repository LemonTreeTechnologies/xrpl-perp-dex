#!/usr/bin/env bash
# safe-converge.sh — the Base Safe owner set as a projection of sealed cluster membership.
#
#   bash orchestrator/scripts/safe-converge.sh plan    READ-ONLY: what the sealed membership
#                                                      implies, and the exact op sequence
#
# WHY THIS EXISTS. safe_projection.rs derives the target owner set verifiably, computes the
# minimal reconciliation plan, and three admin routes are wired for it — and none of it had an
# operable path. The admin surface needs a SIGNED request, and the only signer the CLI offered
# was `sign-request --seed <value>`, i.e. an operator seed in argv, visible to every local user
# through `ps`. So the documented procedure could not be run without breaking the rule about
# secrets on command lines. `--seed-file` now exists; this script is the other half.
#
# THE SEED NEVER LEAVES THE NODE. It is extracted from signers_config.json to a 0600 temp file
# BY A COMMAND RUNNING ON THE NODE, used, and shredded. Nothing prints it, and it does not pass
# through the operator's terminal or any transcript.
#
# NOTE ON WHAT "plan" PROVES: it is a computation, not a chain write. It reads getOwners() and
# getThreshold() from Base-Sepolia and asks THIS node's orchestrator what its own enclave's
# sealed membership implies. No Safe operation is composed, signed or submitted.
set -uo pipefail

BASTION="andrey@94.130.18.162"
NODE="${NODE:-20.71.184.176}"
SAFE="0xa6b6bfbd1c4cf07db46adbb532de0409f3726ef2"
RPC="https://sepolia.base.org"
TARGET_THRESHOLD="${TARGET_THRESHOLD:-1}"
MODE="${1:-plan}"
case "$MODE" in plan) ;; *) echo "usage: $0 plan   (apply is deliberately not implemented yet)"; exit 2 ;; esac

# A MULTI-OWNER SET AT THRESHOLD 1 IS REFUSED BEFORE ANYTHING IS ASKED (audit ruling, Q2,
# 2026-10-08). Today one enclave can act alone; {3 owners}@1 makes EACH of three act alone —
# three unilateral controllers where there was one, for no security gained. It is a t-of-n
# costume: count the independent parties required to move funds and at @1 it is one, which is
# exactly what the cluster's 2-of-3 exists to forbid. Forbidden transiently AND as an end
# state, so add-owners and raise-threshold must land in ONE Safe transaction.
#
# This script DEFAULTED to that posture, which is why the check is here and not in a comment:
# it would have printed a plan for the forbidden thing and called it a plan.
if [ "$TARGET_THRESHOLD" -le 1 ] 2>/dev/null; then
  cat <<'WHY'
REFUSING: target_threshold=1 with a multi-member projection is the forbidden posture.

  project_owner_set maps EVERY sealed member to an owner — there is no selection, so the
  target is always the whole cluster. With threshold 1 that means each of three nodes could
  execute a Safe transaction ALONE, where today only node-1 can.

  The two honest postures (audit ruling):
    (i)  Tier-1 single publisher  = {node-1}@1   — NOT EXPRESSIBLE in this code today;
                                                   project_owner_set has no single-member mode
    (ii) faithful 2-of-3 cluster  = {3}@2        — atomic (one MultiSend), and only meaningful
                                                   once replication lets owners 2/3 approve a
                                                   Safe tx against their own state
  Re-run with TARGET_THRESHOLD=2 to see the (ii) plan. Nothing is submitted either way.
WHY
  exit 3
fi

hr() { printf '%s\n' "------------------------------------------------------------"; }
echo "Safe owner-set projection — MODE=$MODE  node=$NODE  target_threshold=$TARGET_THRESHOLD"
hr

echo "[1/3] the Safe as the CHAIN has it (public read, no key involved)"
call() { curl -s --max-time 25 -X POST "$RPC" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$SAFE\",\"data\":\"$1\"},\"latest\"]}"; }
OWNERS_RAW="$(call 0xa0e67e2b)"   # getOwners()
THRESH_RAW="$(call 0xe75235b8)"   # getThreshold()
read -r OWNERS_JSON CUR_THRESH <<EOF
$(python3 - "$OWNERS_RAW" "$THRESH_RAW" <<'PY'
import sys, json
d = json.loads(sys.argv[1])["result"][2:]
n = int(d[64:128], 16)
# LINKED-LIST ORDER, not sorted: sorting would make every prevOwner wrong on a removal.
owners = ["0x" + d[128 + i*64 + 24: 128 + (i+1)*64] for i in range(n)]
print(json.dumps(owners), int(json.loads(sys.argv[2])["result"], 16))
PY
)
EOF
echo "  owners (linked-list order): $OWNERS_JSON"
echo "  threshold: $CUR_THRESH"
hr

echo "[2/3] asking the node what its OWN enclave's sealed membership implies"
# The request body is built ON the node from its own signers_config.json — public fields only
# (name, compressed_pubkey, address). The sealed membership is read by the orchestrator from
# its own enclave, NOT taken from this body: a caller that supplied both the set and the keys
# could otherwise converge the Safe onto any membership it liked.
REMOTE=$(cat <<'RSCRIPT'
set -uo pipefail
cd /home/azureuser/perp || exit 2
OWNERS_JSON='__OWNERS__'; CUR_THRESH='__THRESH__'; TGT='__TGT__'
python3 - "$OWNERS_JSON" "$CUR_THRESH" "$TGT" > /tmp/safe-proj-req.json <<'PY'
import json, sys
owners, thresh, tgt = json.loads(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
cfg = json.load(open("/home/azureuser/perp/signers_config.json"))
known = [{"name": s["name"], "compressed_pubkey": s["compressed_pubkey"], "address": s["address"]}
         for s in cfg["signers"]]
print(json.dumps({"current_owners": owners, "current_threshold": thresh,
                  "enclave_url": "https://localhost:9088/v1",
                  "known_members": known, "target_threshold": tgt}))
PY
# The seed goes file-to-file on THIS host. Never echoed, never in argv, shredded on exit.
umask 077
trap 'shred -u /tmp/.sc-seed 2>/dev/null || rm -f /tmp/.sc-seed' EXIT
# The canonical path (feedback_secrets_canonical_files): signers_config.json carries
# escrow_seed as an EMPTY string on purpose — the value was moved to the secrets file, which
# is the right shape. Reading the config instead produced a 1-byte file and a refusal that
# said "seed file first line is empty", which is the correct refusal and a useless place to
# look. The keypair's on-chain authority is revoked (disable_master_tx_hash is recorded
# beside it), so signing an admin request with it proves identity without conferring any
# power to move value — which is what an auth credential should be.
SECRET_FILE=/home/azureuser/.secrets/perp-dex-xrpl/escrow-testnet.json
[ -s "$SECRET_FILE" ] || { echo "NO-CREDENTIAL: $SECRET_FILE is missing on this node"; exit 4; }
python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["escrow_seed"])' "$SECRET_FILE" > /tmp/.sc-seed
chmod 600 /tmp/.sc-seed
URL="http://localhost:3000/v1/admin/safe/projection"
BODY="$(cat /tmp/safe-proj-req.json)"
OUT="$(./perp-dex-orchestrator sign-request --seed-file /tmp/.sc-seed --method POST --url "$URL" --body "$BODY" 2>&1)"
# sign-request prints THREE header lines (address, pubkey, blank) before the command, so the
# command starts at line 4. `tail -n +2` kept the comment lines too — harmless in bash, but
# it also meant a failure message was eval'd as a command instead of being reported.
case "$OUT" in Error:*) echo "SIGN-FAILED: $OUT"; exit 3 ;; esac
CMD="$(printf '%s' "$OUT" | tail -n +4)"
[ -n "$CMD" ] || { echo "SIGN-FAILED: no curl command emitted"; exit 3; }
eval "$CMD" -s --max-time 25
RSCRIPT
)
REMOTE="${REMOTE//__OWNERS__/$OWNERS_JSON}"
REMOTE="${REMOTE//__THRESH__/$CUR_THRESH}"
REMOTE="${REMOTE//__TGT__/$TARGET_THRESHOLD}"
printf '%s' "$REMOTE" > /tmp/safe-converge-remote.sh
scp -q -o BatchMode=yes /tmp/safe-converge-remote.sh "$BASTION:/tmp/sc.sh" || { echo "FAILED: reach bastion"; exit 2; }
ssh -o BatchMode=yes "$BASTION" "scp -q /tmp/sc.sh azureuser@$NODE:/tmp/sc.sh" || { echo "FAILED: reach node"; exit 2; }
RESP="$(ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=30 azureuser@$NODE 'bash /tmp/sc.sh; rm -f /tmp/sc.sh /tmp/safe-proj-req.json'")"
printf '%s\n' "$RESP" | head -c 1400
hr

echo "[3/3] reading it"
python3 - "$RESP" <<'PY'
import sys, json
try:
    d = json.loads(sys.argv[1])
except Exception:
    print("  could not parse the response — printed verbatim above"); raise SystemExit(0)
if d.get("status") == "error":
    print("  the node REFUSED:", d.get("message")); raise SystemExit(0)
# Post-check too, because the node derives the target and this script only asked for it.
owners, th = d.get("target_owners", []), d.get("target_threshold")
if len(owners) > 1 and th == 1:
    print("  REFUSING TO PRINT A PLAN: the node derived a multi-owner set at threshold 1,")
    print("  which is the forbidden posture (three unilateral controllers). Not a plan.")
    raise SystemExit(3)
print("  in_sync         :", d.get("in_sync"))
print("  target_threshold:", d.get("target_threshold"))
for o in d.get("target_owners", []): print("  target owner    :", o)
plan = d.get("plan", [])
print(f"  plan            : {len(plan)} operation(s)")
for i, s in enumerate(plan, 1):
    print(f"     {i}. {s.get('op')}  calldata {len(s.get('data',''))//2-1} bytes")
if plan:
    print()
    print("  NOTHING WAS SUBMITTED. Each step still needs: derive-step on every node")
    print("  (so each derives the calldata from its OWN enclave and chain read), an owner")
    print("  signature over the SafeTxHash, then /admin/safe/exec to relay it.")
PY
