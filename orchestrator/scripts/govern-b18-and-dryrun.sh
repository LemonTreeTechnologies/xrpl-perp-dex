#!/usr/bin/env bash
# govern-b18-and-dryrun.sh — runbook §11.3: admit β18 onto the cluster's governed allowlist,
# then re-run the dry run and report the export's return code.
#
# RUN THIS FROM YOUR LAPTOP:   bash orchestrator/scripts/govern-b18-and-dryrun.sh
# TO UNDO the admission:       OP=remove bash orchestrator/scripts/govern-b18-and-dryrun.sh
#
# It ships govern-b18-remote.sh (next to this file) to the bastion and calls it. Two plain files
# rather than quoting inside quoting inside quoting — that nesting is what broke every
# hand-written command for this operation.
#
# WHAT HAPPENS, in order:
#   1. prints the allowlist on all three nodes BEFORE anything is touched
#   2. REFUSES unless every node's NEW enclave on 9089 reports EXACTLY the measurement being
#      admitted. Admitting one that nothing runs would put a stranger on the allowlist.
#   3. governs it — 2-of-3 operator quorum, collected over the p2p relay
#   4. prints the allowlist AFTER and REFUSES to go on unless all three moved to exactly 1 entry
#   5. runs the DRY RUN on node-1 and prints the export's return code
#
# IT NEVER RETIRES OLD. The dry run stops before §3 step 6 by construction, so the point of no
# return is not reached and OLD keeps serving on 9088 throughout. The governance itself is
# additive and reversible (OP=remove).
set -uo pipefail

BASTION="andrey@94.130.18.162"
MRENCLAVE="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
OP="${OP:-add}"
HERE="$(cd "$(dirname "$0")" && pwd)"

hr() { printf '%s\n' "------------------------------------------------------------"; }
run() { ssh -o BatchMode=yes "$BASTION" "bash /tmp/govern-b18-remote.sh $1 $MRENCLAVE $OP"; }

echo "β18 allowlist governance + dry run    OP=$OP"
echo "measurement: $MRENCLAVE"
hr

[ -f "$HERE/govern-b18-remote.sh" ] || { echo "FAILED: govern-b18-remote.sh not next to this script"; exit 2; }
scp -q -o BatchMode=yes "$HERE/govern-b18-remote.sh" "$BASTION:/tmp/govern-b18-remote.sh" \
  || { echo "FAILED: cannot reach the bastion $BASTION"; exit 2; }

echo "[1/5] allowlist BEFORE"
run allowlist
hr

echo "[2/5] what each node's NEW enclave on 9089 actually reports"
meas="$(run new_measurements)"
printf '%s\n' "$meas" | sed 's/^/  /'
mismatch=0
count=0
while read -r ip got; do
  [ -z "${ip:-}" ] && continue
  count=$((count + 1))
  [ "${got:-}" = "$MRENCLAVE" ] || { echo "  MISMATCH $ip reports ${got:-nothing}"; mismatch=1; }
done <<< "$meas"
if [ "$count" -ne 3 ] || { [ "$mismatch" -ne 0 ] && [ "$OP" = "add" ]; }; then
  hr
  echo "STOP — expected all three nodes to report the measurement being admitted; $count answered."
  echo "Admitting a measurement no node runs puts a stranger on the allowlist, and the export"
  echo "would still refuse anyway: the identity pin compares against what NEW really reports."
  echo "Re-run the side-by-side deploy (runbook §3.1) first, then this script."
  exit 1
fi
echo "  all three report exactly the measurement being admitted"
hr

echo "[3/5] governing — 2-of-3 operator quorum over the p2p relay, up to ~2 min"
run govern
hr

echo "[4/5] allowlist AFTER"
after="$(run allowlist)"
printf '%s\n' "$after"
want=1; [ "$OP" = "remove" ] && want=0
landed="$(printf '%s\n' "$after" | grep -c "entries=$want" || true)"
if [ "$landed" -ne 3 ]; then
  hr
  echo "STOP — expected entries=$want on all three, got $landed of 3."
  echo "Do NOT run the ceremony on a split allowlist: the nodes that did not admit the"
  echo "measurement refuse their export with -25 while the others proceed, and §3's parallel"
  echo "requirement then breaks on exactly the stragglers it exists to protect."
  exit 1
fi
echo "  all three nodes at entries=$want"
hr

if [ "$OP" = "remove" ]; then
  echo "OP=remove complete. Stopping here; the dry run would refuse by design."
  exit 0
fi

echo "[5/5] DRY RUN on node-1 — export, import, durability. STOPS before OLD retires."
run dryrun
hr
cat <<'READ'
How to read the line above:

  "status":"ok"                      the export PROCEEDED. The gate's ADMIT path has fired for
                                     the first time in this cluster's life.
  code=-25 MRENCLAVE_NOT_ADMITTED    still refused — the admission did not reach the enclave
                                     that performs the export.
  anything else                      a different failure; send the whole line.

OLD is still serving on 9088 either way. Nothing has been retired, and the governance can be
undone with OP=remove.
READ
