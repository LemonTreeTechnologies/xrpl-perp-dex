#!/usr/bin/env bash
# arm-attested-clock.sh — switch the attested clock ON, one node first, then the rest.
#
#   bash orchestrator/scripts/arm-attested-clock.sh check   readiness only, changes NOTHING
#   bash orchestrator/scripts/arm-attested-clock.sh one     arm node-1 only, then watch
#   bash orchestrator/scripts/arm-attested-clock.sh rest    arm the other two
#   bash orchestrator/scripts/arm-attested-clock.sh disarm  remove the drop-in everywhere
#
# WHY ONE NODE FIRST, and this is the opposite of the ceremony on purpose. The ceremony had to
# fire on all three at once because its delegation step needs a live quorum. The clock has no
# quorum in it: each node advances its OWN clock from a header it verified for itself against
# the sealed pinned UNL. So the nodes are independent here, and independence means a failure
# can be learned on one node instead of three. The last time this was switched on it refused
# 100% of ledgers.
#
# WHAT IS AND IS NOT REVERSIBLE. Arming is: `disarm` removes the drop-in and restarts, and the
# driver is gone. The clock MOVING is not — it advances only forward by construction (-89
# refuses a ledger that is not newer) — but moving it to the current ledger is the whole point,
# and a stale attested time is what halts mark-dependent operations today.
#
# WHAT IT REFUSES TO DO. Arm while the trust root is not demonstrably healthy. The binary has
# its own guard for the deliberate case (PERP_UNL_REFRESH=0 blocks the spawn), and this covers
# the other one: a refresh that is enabled but FAILING. The clock is checked against the
# enclave's derived validator set, and with 6 masters at an 80% floor the quorum is 5-of-6 —
# one stale signing key is the whole margin.
set -uo pipefail

BASTION="andrey@94.130.18.162"
NODE1="20.71.184.176"
declare -a REST=(20.224.243.60 52.236.130.102)
DROPIN="/etc/systemd/system/perp-dex-orchestrator.service.d/attested-clock.conf"
MODE="${1:-check}"

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() {
  case "$2" in
    *\'*) echo "on(): refusing — single quote in the command would break the nested quoting" >&2
          return 64 ;;
  esac
  ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=20 azureuser@$1 '$2'"
}

status_of() {
  on "$1" "curl -s --max-time 10 http://localhost:3000/v1/system/status" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    print('unreadable'); raise SystemExit
c = d.get('attested_clock', {}); u = d.get('unl_refresh', {})
print('%s %s %s %s %s %s %s' % (c.get('driver_enabled'), c.get('advances'), c.get('refusals'),
      c.get('last_refusal_rc'), u.get('enabled'), u.get('submissions'), u.get('failures')))
" 2>/dev/null
}

ready() {   # $1=ip ; echoes a reason on failure, nothing on success
  local ip="$1" s multi guard
  s="$(status_of "$ip")"
  set -- $s
  [ "${1:-}" = "False" ] || [ "${1:-}" = "True" ] || { echo "status unreadable"; return; }
  [ "${5:-}" = "True" ]  || { echo "unl_refresh is not enabled"; return; }
  [ "${6:-0}" -gt 0 ] 2>/dev/null || { echo "unl_refresh has made no submissions yet"; return; }
  [ "${7:-1}" = "0" ]    || { echo "unl_refresh has $7 failure(s) — fix the trust root first"; return; }
  multi="$(on "$ip" "strings -a /home/azureuser/perp/perp-dex-orchestrator | grep -cF clio.altnet.rippletest.net")"
  guard="$(on "$ip" "strings -a /home/azureuser/perp/perp-dex-orchestrator | grep -cF attested_clock_refused_unl_off")"
  [ "${multi:-0}" -gt 0 ] 2>/dev/null || { echo "running binary predates the multi-source fix"; return; }
  [ "${guard:-0}" -gt 0 ] 2>/dev/null || { echo "running binary predates the arming guard"; return; }
}

arm_one() {
  local ip="$1"
  printf '  %-16s ' "$ip"
  # QUOTED, because [Service] unquoted is a glob pattern: bash leaves it literal only
  # when nothing in the cwd matches one of those characters, which is luck and not a
  # guarantee. Escaped double quotes, since on() refuses single ones.
  on "$ip" "printf \"%s\\n\" \"[Service]\" \"Environment=PERP_ATTESTED_CLOCK=1\" | sudo tee $DROPIN > /dev/null && sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator && echo armed" \
    || { echo "FAILED to arm"; return 1; }
}

case "$MODE" in
  check|one|rest|disarm) ;;
  *) echo "usage: $0 [check|one|rest|disarm]"; exit 2 ;;
esac

echo "attested clock — MODE=$MODE"
hr

if [ "$MODE" = "disarm" ]; then
  for ip in "$NODE1" "${REST[@]}"; do
    printf '  %-16s ' "$ip"
    on "$ip" "sudo rm -f $DROPIN && sudo systemctl daemon-reload && sudo systemctl restart perp-dex-orchestrator && echo disarmed"
  done
  hr
  echo "Drop-in removed everywhere. The clock keeps whatever time it had reached — it only ever"
  echo "moves forward, and nothing rolls it back. Re-arm with: $0 one"
  exit 0
fi

echo "[1/3] readiness — the trust root must be demonstrably healthy before the clock is armed"
targets=("$NODE1"); [ "$MODE" = "rest" ] && targets=("${REST[@]}")
blocked=0
for ip in "$NODE1" "${REST[@]}"; do
  why="$(ready "$ip")"
  s="$(status_of "$ip")"; set -- $s
  printf '  %-16s clock=%s adv=%s ref=%s | unl enabled=%s subs=%s fail=%s  %s\n' \
    "$ip" "${1:-?}" "${2:-?}" "${3:-?}" "${5:-?}" "${6:-?}" "${7:-?}" \
    "$([ -z "$why" ] && echo READY || echo "NOT READY: $why")"
  [ -z "$why" ] || blocked=1
done
hr
[ "$blocked" -eq 0 ] || { echo "STOP — nothing armed."; exit 1; }
if [ "$MODE" = "check" ]; then
  # The hint has to reflect what is actually armed. It said "Next: one" even when all three
  # were already running, which is the stale-output shape this session has corrected in four
  # other scripts: the state moved and the prose beside it did not.
  armed=0
  for ip in "$NODE1" "${REST[@]}"; do
    s="$(status_of "$ip")"; set -- $s
    [ "${1:-}" = "True" ] && armed=$((armed + 1))
  done
  echo "readiness only — nothing changed. $armed of 3 node(s) have the clock armed."
  case "$armed" in
    0) echo "Next: $0 one   (node-1 first; the nodes are independent here, so a failure is"
       echo "               learned on one node rather than three)" ;;
    3) echo "All three are armed. Nothing to do — to switch them off: $0 disarm" ;;
    *) echo "Next: $0 rest  (arms the remaining node(s))" ;;
  esac
  exit 0
fi

echo "[2/3] arming ${#targets[@]} node(s)"
for ip in "${targets[@]}"; do arm_one "$ip" || exit 1; done
hr

echo "[3/3] watching for ~3 minutes — advances must climb, refusals must stop climbing"
# JUDGED ON THE DELTA, not the lifetime total. A freshly armed node takes one -208
# (under quorum) on its first tick, because the collector has only just subscribed and has not
# yet buffered a full set of validations for the ledger it is asked about. That is expected and
# never repeats — but a verdict keyed on "refusals == 0" called it INCONCLUSIVE on a node that
# had advanced 29 times and refused once, three minutes earlier. Second false verdict of the
# day from the same habit: I diagnosed node-1 by hand using exactly this delta ("529 -> 529,
# unchanged") and then left the script judging the total.
declare -A ref0 adv0
for ip in "${targets[@]}"; do
  s="$(status_of "$ip")"; set -- $s
  adv0[$ip]="${2:-0}"; ref0[$ip]="${3:-0}"
done
for round in 1 2 3 4 5 6; do
  ssh -o BatchMode=yes "$BASTION" "sleep 30"
  for ip in "${targets[@]}"; do
    s="$(status_of "$ip")"; set -- $s
    printf '  t+%-3s %-16s enabled=%s advances=%s refusals=%s last_rc=%s\n' \
      "$((round * 30))s" "$ip" "${1:-?}" "${2:-?}" "${3:-?}" "${4:-?}"
  done
done
hr
fail=0
for ip in "${targets[@]}"; do
  s="$(status_of "$ip")"; set -- $s
  d_adv=$(( ${2:-0} - ${adv0[$ip]:-0} ))
  d_ref=$(( ${3:-0} - ${ref0[$ip]:-0} ))
  printf '  %-16s over the window: advances +%s, refusals +%s\n' "$ip" "$d_adv" "$d_ref"
  if [ "$d_adv" -gt 0 ] && [ "$d_ref" -eq 0 ]; then
    echo "       ADVANCING and refusals have stopped — this is the shape we want."
    [ "${3:-0}" -gt 0 ] 2>/dev/null && echo "       (${3} lifetime refusal(s), none during the window: a startup -208 before the"
    [ "${3:-0}" -gt 0 ] 2>/dev/null && echo "        validation buffer filled is expected and does not repeat.)"
  elif [ "$d_adv" -eq 0 ] && [ "$d_ref" -gt 0 ]; then
    echo "       REFUSING every ledger (advances +0, refusals +$d_ref, last_rc=${4:-?}) — this"
    echo "       is what happened the last time it was switched on. Disarm and read the rc:"
    echo "       -70/-71 pinned UNL missing or below its floor; -7x the quorum did not verify;"
    echo "       -89/-90 not newer / time went backwards (benign if advances are also climbing)."
    fail=1
  elif [ "$d_adv" -gt 0 ] && [ "$d_ref" -gt 0 ]; then
    echo "       ADVANCING but still refusing (+$d_adv / +$d_ref). Not a failure and not clean:"
    echo "       some ledgers are being asked about before their validations are buffered."
    echo "       Watch whether the refusal rate falls; read the journal for the rc."
    fail=1
  else
    echo "       STALLED (+0 advances) — no tick completed in three minutes. Read the"
    echo "       orchestrator journal; a tick needs the validations buffered for that ledger."
    fail=1
  fi
done
hr
if [ "$fail" -eq 0 ] && [ "$MODE" = "one" ]; then
  echo "node-1 is healthy. Arm the other two:  $0 rest"
elif [ "$fail" -ne 0 ]; then
  echo "Disarm while you investigate:  $0 disarm"
  exit 1
fi
