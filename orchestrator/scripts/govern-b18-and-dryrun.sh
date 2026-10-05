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
before="$(run allowlist)"
printf '%s\n' "$before"
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
# SKIP IT IF IT IS ALREADY DONE. The enclave refuses a duplicate add with code -11 on
# purpose — "reject no-ops early so a wasted governance round is visible to operators rather
# than silently consuming an epoch" — so re-running this step on an already-admitted
# measurement reported a 0-of-3 FAILURE on an allowlist that was already correct, and the
# orchestrator then advised a retry that can never succeed.
#
# An earlier version of this comment claimed the step was idempotent. It is not, and the
# enclave is deliberately not: that is the design, and the script was the thing that was
# wrong. So the check moves here — ask the allowlist first, and govern only if there is
# something to govern.
before_entries="$(printf '%s\n' "$before" | grep -c "entries=1" || true)"
if [ "$OP" = "add" ] && [ "$before_entries" -eq 3 ]; then
  echo "  SKIPPED — all three nodes already hold 1 entry at the same digest, so there is"
  echo "  nothing to govern. A duplicate add returns -11 by design and consumes no epoch."
  echo "  (If that one entry is a DIFFERENT measurement, step [2/5] above would have"
  echo "  mismatched and this script would already have stopped.)"
else
  run govern
fi
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

  "status":"dry-run-ok"              FULL PASS: export, import, durability AND step 5a — the
                                     new enclave BOOTED on the migrated state through the
                                     startup load path. Check boot_proof says
                                     post_migration_marker:true and the sealed file count is
                                     the same before and after. Only this clears the ceremony.
  "status":"dry-run-boot-failed"     the 5a boot ran and was judged a FAILURE; boot_failure
                                     says why. For β18 a refusal to boot is the gate working:
                                     the section stamp is checked at startup load. The
                                     migrated set is LEFT on disk for diagnosis. Do not run
                                     the ceremony.
  "status":"dry-run-boot-not-run"    the boot could not be performed. Also NOT a pass.
  code=-25 MRENCLAVE_NOT_ADMITTED    the export refused — the admission did not reach the
                                     enclave that performs the export.
  anything else                      a different failure; send the whole line.

  NOTE: an earlier version of this text promised "status":"ok" for the success case. The
  route returns "dry-run-ok" on a dry run, so the real success read as "anything else".

OLD is still serving on 9088 either way. Nothing has been retired, and the governance can be
undone with OP=remove.
READ
