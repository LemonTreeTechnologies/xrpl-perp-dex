#!/usr/bin/env bash
# ceremony-parallel.sh — fire the Path-A ceremony on ALL THREE nodes CONCURRENTLY.
#
#   bash orchestrator/scripts/ceremony-parallel.sh preflight  readiness only, fires NOTHING
#   bash orchestrator/scripts/ceremony-parallel.sh dryrun     rehearsal, reversible, default
#   bash orchestrator/scripts/ceremony-parallel.sh real       THE REAL ONE — retires OLD
#
# WHY THIS SCRIPT EXISTS, and it is not convenience.
#
# §11.10 is the one invariant whose violation has no tested recovery: every OLD must stay live
# until every node has passed its export+delegation phase. The ceremony's delegation step needs
# a 2-of-3 operator quorum, each operator signing through their enclave — and a RETIRED OLD
# refuses to sign. So if one node runs to completion first, it stops being able to co-sign; once
# two have retired, the third can never reach quorum. The runbook's own words: "Recovery from a
# violation is in §7.1 — but prevention is the only tested guarantee", and §7.1's last resort is
# re-bootstrapping that operator from scratch, on a path "NOT exercised on hardware".
#
# And yet §3.2 specifies that prevention as: "Each operator, on their own node, at the same
# time: curl ...". Three people trying to be simultaneous. With a single operator holding all
# three nodes through one bastion, doing it by hand IS the sequential violation — there is no
# way to type three curls at once.
#
# The arithmetic, concretely, at 2-of-3: if node-1 retires while node-3 is still collecting,
# node-3 has exactly two signers left and is on the edge. If two retire first, node-3 has one
# and is stuck. Concurrency does not remove the window; it makes the early phases overlap so
# that every node has collected its delegations long before any node reaches the last step.
#
# THE PRE-FLIGHT IS THE OTHER HALF, and arguably the more valuable one: it refuses to fire
# ANYTHING unless all three nodes are ready. A node that is not ready fails late — after the
# others have retired — which is the exact shape §7.1 exists for.
set -uo pipefail

BASTION="andrey@94.130.18.162"
MRENCLAVE_NEW="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
MRENCLAVE_OLD_PREFIX="aead7ecf"
declare -a NODES=(20.71.184.176 20.224.243.60 52.236.130.102)
MODE="${1:-dryrun}"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() { ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=10 azureuser@$1 '$2'"; }

case "$MODE" in
  preflight|dryrun|real) ;;
  *) echo "usage: $0 [preflight|dryrun|real]"; exit 2 ;;
esac

echo "Path-A ceremony — PARALLEL across all three nodes      MODE=$MODE"
echo "target measurement: $MRENCLAVE_NEW"
hr

# ── PRE-FLIGHT. Every node, every condition, BEFORE anything fires. ───────────
echo "[1/3] pre-flight — all three nodes must be ready before ANY node fires"
ready=0
digests=""
for ip in "${NODES[@]}"; do
  echo "  $ip"
  # EVERY field through printf with an explicit \n, and the value via command substitution.
  #
  # The first version streamed `curl | sed` straight into `echo -n "field="`, and GNU sed
  # PRESERVES a missing final newline from its input — which these JSON bodies have. So the
  # fields ran together: `new_mre=367cabb2...next_files=0`, and the parse then reported a
  # mismatch on a node that was in fact correct. It failed SAFE, which is the right direction,
  # but a pre-flight that always refuses is as useless as one that always passes.
  p="$(on "$ip" '
    V=$(curl -k -s --max-time 8 https://localhost:9088/version 2>/dev/null | sed -n "s/.*\"mrenclave\":\"\([^\"]*\)\".*/\1/p")
    printf "old_mre=%s\n" "$(printf "%s" "$V" | cut -c1-8)"
    V=$(curl -k -s --max-time 8 https://localhost:9089/version 2>/dev/null | sed -n "s/.*\"mrenclave\":\"\([^\"]*\)\".*/\1/p")
    printf "new_mre=%s\n" "$V"
    printf "next_files=%s\n" "$(ls -1 /home/azureuser/perp-next/accounts/ 2>/dev/null | wc -l)"
    printf "svc_old=%s\n"  "$(systemctl is-active perp-dex-enclave 2>/dev/null)"
    printf "svc_new=%s\n"  "$(systemctl is-active perp-dex-enclave-next 2>/dev/null)"
    printf "svc_orch=%s\n" "$(systemctl is-active perp-dex-orchestrator 2>/dev/null)"
    printf "step5a=%s\n"   "$(strings -a /home/azureuser/perp/perp-dex-orchestrator 2>/dev/null | grep -c "step 5a")"
    # The allowlist status is served by the ENCLAVE on 9088, NOT by the orchestrator admin
    # listener on 7095. An earlier version asked 7095 and got an EMPTY body with exit code 0 —
    # a check that read as "no entries" whatever the truth was.
    S=$(curl -sk --max-time 8 https://127.0.0.1:9088/v1/admin/mrenclaves/status 2>/dev/null)
    printf "entries=%s\n" "$(printf "%s" "$S" | sed -n "s/.*\"entry_count\":\([0-9]*\).*/\1/p")"
    printf "digest=%s\n"  "$(printf "%s" "$S" | sed -n "s/.*\"allowlist_digest\":\"\([^\"]*\)\".*/\1/p" | cut -c1-16)"
  ')"
  g() { printf '%s\n' "$p" | sed -n "s/^$1=//p" | head -1; }
  old_mre="$(g old_mre)"; new_mre="$(g new_mre)"; nf="$(g next_files)"
  so="$(g svc_old)"; sn="$(g svc_new)"; sor="$(g svc_orch)"; s5="$(g step5a)"
  ent="$(g entries)"; dig="$(g digest)"
  bad=""
  [ "$old_mre" = "$MRENCLAVE_OLD_PREFIX" ] || bad="$bad old-mrenclave($old_mre)"
  [ "$new_mre" = "$MRENCLAVE_NEW" ]        || bad="$bad new-mrenclave($new_mre)"
  [ "${nf:-x}" = "0" ]                     || bad="$bad perp-next/accounts-not-empty($nf)"
  [ "$so" = "active" ]                     || bad="$bad OLD-service($so)"
  [ "$sn" = "active" ]                     || bad="$bad NEW-service($sn)"
  [ "$sor" = "active" ]                    || bad="$bad orchestrator($sor)"
  [ "${s5:-0}" -gt 0 ] 2>/dev/null         || bad="$bad orchestrator-missing-step5a-fix"
  # The target must be ADMITTED here, or this node's export refuses with -25 while the others
  # proceed — a split that leaves the refusing node needing the §7.1 path.
  [ "${ent:-x}" = "1" ]                    || bad="$bad allowlist-entries($ent)"
  [ -n "$dig" ]                            || bad="$bad allowlist-unreadable"
  digests="$digests $dig"
  if [ -n "$bad" ]; then
    echo "    NOT READY:$bad"
  else
    echo "    ready (OLD $old_mre serving, NEW is the target, allowlist 1 entry @ $dig,"
    echo "           perp-next/accounts empty, orchestrator carries the step-5a fix)"
    ready=$((ready + 1))
  fi
done
# One digest for the whole cluster, or the nodes do not agree on WHAT is admitted — and a
# per-node entry count of 1 says nothing about whether it is the same 1.
uniq_digests="$(printf '%s\n' $digests | sort -u | wc -l)"
if [ "$ready" -eq 3 ] && [ "$uniq_digests" -ne 1 ]; then
  echo "  SPLIT ALLOWLIST — the three nodes report DIFFERENT digests:$digests"
  ready=0
fi
hr
if [ "$ready" -ne 3 ]; then
  echo "STOP — $ready of 3 nodes ready. NOTHING was fired."
  echo
  echo "This refusal is the point of the script. A node that is not ready fails LATE — after"
  echo "the others have already retired their OLD — and that is the one failure with no tested"
  echo "recovery (§7.1: re-bootstrap the operator, on a path never exercised on hardware)."
  echo "Fix the nodes listed above, then re-run. A non-empty perp-next/accounts means a"
  echo "half-finished prior attempt: clear it (§7 table) before retrying."
  exit 1
fi
echo "  all three ready, and they agree on what is admitted"
hr

# ── BASELINE. Captured BEFORE anything fires, because §4 cannot run without it. ──
#
# §4 check 3 is "state survival — compare against pre-migration values", and NOTHING in §3
# says to capture those values. The ceremony is irreversible, so a baseline not taken before
# it is a baseline that can never be taken: the check would be unperformable at precisely the
# moment it matters. A verification step you cannot execute is not a safety net.
#
# WHAT DISCRIMINATES, said plainly. The vault API currently reports all zeros and
# active:false, so it would read IDENTICALLY on a migration that carried nothing — as a
# witness it cannot tell success from total loss. The discriminating witness is the sealed
# FILE INVENTORY: 183 files and ~10.5 MB per node today, against ~0 for an empty NEW. So the
# name list and its digest are the load-bearing part here, and the API bodies are recorded
# because they are cheap and because the vault may hold real values by the next cycle.
BASE="$HOME/path-a-baseline-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$BASE"
echo "[1b/3] capturing the PRE-MIGRATION baseline §4 will compare against"
for ip in "${NODES[@]}"; do
  on "$ip" '
    printf "files=%s\n"  "$(ls -1 /home/azureuser/perp/accounts/ 2>/dev/null | wc -l)"
    printf "bytes=%s\n"  "$(du -sb /home/azureuser/perp/accounts/ 2>/dev/null | cut -f1)"
    printf "names_sha256=%s\n" "$(ls -1 /home/azureuser/perp/accounts/ 2>/dev/null | sort | sha256sum | cut -d" " -f1)"
    echo "--- names ---"; ls -1 /home/azureuser/perp/accounts/ 2>/dev/null | sort
    echo "--- vault type=1 ---"; curl -k -s --max-time 8 "https://localhost:9088/v1/perp/vault/status?type=1" 2>/dev/null; echo
  ' > "$BASE/$ip.baseline" 2>&1
  echo "  $ip  $(sed -n 's/^files=/files /p;s/^bytes=/bytes /p;s/^names_sha256=/sha /p' "$BASE/$ip.baseline" | tr '\n' ' ')"
done
echo "  saved: $BASE"
echo "  §4 check 3 compares NEW's inventory against these. Keep this directory until §4 passes."
hr
if [ "$MODE" = "preflight" ]; then
  echo "preflight only — NOTHING was fired. Next:"
  echo "    bash orchestrator/scripts/ceremony-parallel.sh dryrun"
  exit 0
fi

# ── CONSENT, for the real mode only, in consequences and not in jargon. ───────
if [ "$MODE" = "real" ]; then
  cat <<'CONSENT'
[2/3] WHAT YOU ARE ABOUT TO AUTHORISE — read this, it is not a formality

  WHAT BECOMES IRREVERSIBLE. On each node the last step has the OLD enclave seal a
  retired-marker. After that OLD refuses to sign or mutate state for good, and the state has
  been re-sealed under the NEW measurement, which OLD cannot unseal. There is no rollback to
  the old enclave from that moment — not "hard", not "needs a procedure": impossible.

  WHAT IS NOT AT RISK. Customer funds are in the XRPL escrow, not in the enclave, and the
  operator identities and the SignerList do not change. The migration carries state; it does
  not move money. OLD's sealed state stays on disk as a forensic copy until the §6
  decommission, which is a separate, later, deliberate step.

  WHAT THE REHEARSAL ALREADY PROVED, on real state: export, import, durability, and — since
  2026-10-05 — that the NEW enclave BOOTS on the migrated state through its startup load.
  That last one is what this build's change is about, and until that fix the rehearsal never
  checked it.

  THE ONE THING THAT CAN STILL GO WRONG BADLY. If a node's ceremony fails while the others
  succeed, the cluster tolerates the split transiently and the failed node can be re-run
  WHILE THE OTHER OLDs ARE STILL ALIVE. That window is why this fires all three at once. If
  you lose that window, §7.1 applies, and §7.1 ends in re-bootstrapping an operator through a
  path that has never been exercised on hardware.

  WHAT TO DO IF ONE NODE FAILS: do NOT promote, do NOT decommission, do NOT retry serially.
  Re-run the ceremony on the failed node immediately, while the other OLDs are still live.

CONSENT
  printf 'Type exactly: retire OLD on all three nodes\n> '
  read -r answer
  if [ "$answer" != "retire OLD on all three nodes" ]; then
    echo "NOT CONFIRMED — nothing was fired."
    exit 1
  fi
  hr
else
  echo "[2/3] dry-run mode — reversible. OLD keeps serving; NEW is reset to empty afterwards."
  hr
fi

# ── FIRE. All three at once, not one after another. ───────────────────────────
DRY=true; [ "$MODE" = "real" ] && DRY=false
echo "[3/3] firing on all three CONCURRENTLY (dry_run=$DRY)"
for ip in "${NODES[@]}"; do
  (
    body="{\"expected_mrenclave_new\":\"$MRENCLAVE_NEW\",\"old_api_base\":\"https://localhost:9088\",\"new_api_base\":\"https://localhost:9089\",\"delegation_timeout_secs\":120,\"dry_run\":$DRY}"
    on "$ip" "printf '%s' '$body' > /tmp/ceremony.json && curl -sS --max-time 600 -X POST http://127.0.0.1:7095/admin/migrate-state -H 'Content-Type: application/json' -d @/tmp/ceremony.json" \
      > "$OUT/$ip.out" 2> "$OUT/$ip.err"
    echo "$?" > "$OUT/$ip.rc"
  ) &
done
echo "  launched at $(date -u +%H:%M:%S)Z — waiting for all three"
wait
echo "  all three returned by $(date -u +%H:%M:%S)Z"
hr

ok=0
for ip in "${NODES[@]}"; do
  echo "=== $ip (exit $(cat "$OUT/$ip.rc" 2>/dev/null || echo ?)) ==="
  cat "$OUT/$ip.out" 2>/dev/null; echo
  [ -s "$OUT/$ip.err" ] && { echo "  stderr:"; sed 's/^/    /' "$OUT/$ip.err"; }
  if [ "$MODE" = "real" ]; then
    grep -q '"status":"ok"' "$OUT/$ip.out" 2>/dev/null && ok=$((ok + 1))
  else
    grep -q '"status":"dry-run-ok"' "$OUT/$ip.out" 2>/dev/null && ok=$((ok + 1))
  fi
done
hr
echo "RESULT: $ok of 3 succeeded"
if [ "$ok" -eq 3 ]; then
  if [ "$MODE" = "real" ]; then
    echo "  All three migrated. Next: §4 verification on EVERY node BEFORE promotion."
    echo "  Do not promote on a partial result; do not decommission OLD until §4 passed."
  else
    echo "  All three rehearsed, including the step-5a boot. Check each boot_proof above says"
    echo "  post_migration_marker:true and the sealed file count is unchanged across the boot."
    echo "  Only then run:  bash orchestrator/scripts/ceremony-parallel.sh real"
  fi
else
  echo "  NOT all three. Read each node's line above."
  if [ "$MODE" = "real" ]; then
    echo "  ACT NOW, while the surviving OLDs are still live: re-run the ceremony on the failed"
    echo "  node(s) only. Do NOT promote and do NOT decommission anything first — the live OLDs"
    echo "  are what makes the retry able to reach delegation quorum at all (§7.1)."
  fi
  exit 1
fi
