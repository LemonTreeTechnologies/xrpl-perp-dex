#!/usr/bin/env bash
#
# enable-attested-clock.sh — switch the attested clock on across the testnet cluster.
#
# Run it from a checkout on the laptop:  ./orchestrator/scripts/enable-attested-clock.sh
# Everything it touches goes through the Hetzner bastion; nothing needs an Azure key here.
#
# WHY A SCRIPT AND NOT A LIST OF COMMANDS: the order matters and the verification
# matters. Followers restart before the sequencer, and each node must be seen to
# ADVANCE before the next one is touched — a clock that reports a nonzero sequence
# once may simply have read one header and then died. So the success criterion here
# is "the sequence went UP between two reads", not "the counter is nonzero".
#
# Safe to re-run: it installs only when the file differs, and skips any node whose
# clock is already advancing.
#
# What it changes:
#   1. installs the systemd drop-in that sets PERP_ATTESTED_CLOCK=1 and the two XRPL
#      URLs, then restarts perp-dex-orchestrator — one node at a time, verifying each;
#   2. removes perp-dex.service, an obsolete unit of ours pointing at a binary in /tmp
#      that no longer exists. It is `Restart=always`, so it has been failing 203/EXEC in
#      a tight loop for weeks: ~4.5 MILLION restarts and ~186k journal lines a day on
#      each of two nodes, with journals at 2.9 GB. A copy is kept under
#      ~/perp/removed-units/ before it is deleted.
#
# A restart costs the p2p mesh 30-60 seconds. Nothing halts: the split halt for a
# stale mark lives behind PRICE_SIGNED_PATH_ENABLED, which is (0 >= 4) = false in the
# deployed enclave, so the freshness comparison is not even compiled in.
set -euo pipefail

DRY=0
DISARM=0
ARM_ANYWAY=0
case "${1:-}" in
  --dry-run)    DRY=1 ;;
  --disarm)     DISARM=1 ;;
  --arm-anyway) ARM_ANYWAY=1 ;;
  "") ;;
  *) echo "usage: $0 [--dry-run|--disarm|--arm-anyway]"; exit 2 ;;
esac

# ── ARMING PRECONDITION ──────────────────────────────────────────────────────
# The clock was DISARMED on 2026-09-27 on an audit ruling, and the reason does not live
# in this file's git history where nobody will look — it lives HERE, because the next
# person to run this with no arguments is re-arming it, and that person may be me in a
# week with the reason gone from my head.
#
# Arming makes the enclave re-seal its state every ~5 seconds instead of a handful of
# times an hour. Nothing serialised those writes, so two could interleave and leave a
# MIXED sealed set — sections from two different saves — which the loader accepts
# SILENTLY, because it checks that sections are PRESENT and not that they are from the
# same VERSION. That is either a node that will not boot or fund accounting assembled
# from two different moments with nothing reporting it.
#
# Two fixes, and BOTH must be DEPLOYED before arming:
#   layer 1 — a recursive lock over COMPUTE->CHECK->COMMIT->SAVE. Merged, enclave main
#             df323f5 (77ph/xrpl-perp-dex-enclave#154). Closes the concurrency cause.
#   layer 2 — each sealed section stamped with state_version so the loader REFUSES a
#             mixed set instead of loading it. A sealed-FORMAT change, so it arrives
#             with a schema bump (β18) and a migration. Design:
#             docs/audit/REQ-section-version-stamp.md in the enclave repo.
#
# Both are enclave changes, so neither is live until a migration has run. Merged is not
# deployed — check the running enclave, never this comment.
#
# Owner's decision, 2026-09-27: ONE migration carrying the SPV cause band, the lock and
# the section stamp together, and the clock returns after it.
#
# TODO when β18 exists: replace this honour-system refusal with a real check — pin β18's
# MRENCLAVE here and compare it against the live enclave, so arming before the fix is
# IMPOSSIBLE rather than merely discouraged. It cannot be written yet because β18 has
# not been built, and writing a placeholder that always passes would be worse than this.
if [ "$DRY" = 0 ] && [ "$DISARM" = 0 ] && [ "$ARM_ANYWAY" = 0 ]; then
  cat <<'WHY'
REFUSING TO ARM.

The attested clock is deliberately off. Arming it makes the enclave re-seal its state
about twelve times a minute on every node, and until the section-version stamp is
DEPLOYED, a crash in the wrong millisecond can leave a mixed sealed set that the loader
accepts silently: inconsistent balances, with nothing reporting it.

Both of these must be live on the cluster first:
  1. the perp-state lock            (enclave PR #154, main df323f5)
  2. the per-section version stamp  (beta-18 — not written yet)

Neither is live until a migration has run. Verify against the deployed MRENCLAVE, not
against this message.

  ./enable-attested-clock.sh --dry-run      see what would change, touch nothing
  ./enable-attested-clock.sh --disarm       turn it off (idempotent)
  ./enable-attested-clock.sh --arm-anyway   arm regardless, having read the above
WHY
  exit 3
fi

BASTION="${BASTION:-andrey@94.130.18.162}"
SSH_OPTS=(-o ConnectTimeout=20)

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONF="$HERE/perp-dex-orchestrator.service.d/10-attested-clock.conf"
[ -f "$CONF" ] || { echo "FATAL: drop-in not found at $CONF"; exit 1; }

echo "bastion : $BASTION"
echo "drop-in : $CONF"
[ "$DRY" = 1 ] && echo "mode    : DRY RUN — reads only, changes nothing"
[ "$DISARM" = 1 ] && echo "mode    : DISARM — remove the drop-in and restart"
echo

CONF_B64="$(base64 -w0 "$CONF")"
CONF_SHA="$(sha256sum "$CONF" | cut -d" " -f1)"

DRIVER="$(mktemp)"
trap 'rm -f "$DRIVER"' EXIT
{
  printf 'CONF_B64=%s\nCONF_SHA=%s\nDRY=%s\nDISARM=%s\n' "$CONF_B64" "$CONF_SHA" "$DRY" "$DISARM"
  cat <<'DRIVER_BODY'
set -uo pipefail

# ip:expected-hostname, followers first and the SEQUENCER LAST. nginx lists
# 20.71.184.176 first and pins reads to it, so it is the node whose absence shows.
NODES="20.224.243.60:sgx-node-2 52.236.130.102:sgx-node-3 20.71.184.176:sgx-node-1"

# NOTE: this driver is piped into `bash -s`, so its source IS stdin. Every ssh
# below that is not fed a heredoc carries -n; without it ssh swallows the rest
# of the script and execution stops silently mid-run.
say() { printf '%s\n' "$*"; }
rule() { say "------------------------------------------------------------"; }

# Read one field out of a node's /v1/system/status attested_clock block.
clock_field() { # ip field
  curl -s -m 8 "http://$1:3000/v1/system/status" 2>/dev/null \
    | python3 -c "import json,sys
try:
    print(json.load(sys.stdin)['attested_clock'].get('$2',''))
except Exception:
    print('')" 2>/dev/null
}

rc_meaning() {
  case "$1" in
    0)   echo "advanced" ;;
    -70|-71) echo "no pinned UNL, or its quorum floor is not met — an operator must pin the validator set; this does NOT fix itself" ;;
    -72) echo "the bundle did not parse" ;;
    -73) echo "the header did not hash to the ledger hash we claimed" ;;
    -74) echo "the validators' quorum did not verify — fewer signatures reached us than the 80% floor needs" ;;
    -89) echo "already at or ahead (normal between ledger closes, not a fault)" ;;
    -90) echo "close_time went backwards" ;;
    *)   echo "unclassified (transport, or an enclave code the driver does not name)" ;;
  esac
}

# ── 0. PREFLIGHT ─────────────────────────────────────────────────────────────
# Do not restart anything until all three nodes are known healthy: restarting
# into an already-broken cluster makes cause and effect impossible to separate.
rule; say "PREFLIGHT"; rule
bad=0
for entry in $NODES; do
  ip="${entry%%:*}"; want="${entry##*:}"
  got="$(ssh -n -o ConnectTimeout=10 -o BatchMode=yes azureuser@"$ip" hostname 2>/dev/null)"
  act="$(ssh -n -o ConnectTimeout=10 -o BatchMode=yes azureuser@"$ip" systemctl is-active perp-dex-orchestrator 2>/dev/null)"
  api="$(curl -s -m 8 -o /dev/null -w '%{http_code}' "http://$ip:3000/v1/system/status" 2>/dev/null)"
  en="$(clock_field "$ip" driver_enabled)"; adv="$(clock_field "$ip" advances)"
  say "  $ip  host=$got  unit=$act  api=$api  clock_enabled=$en  advances=$adv"
  [ "$got" = "$want" ] || { say "    ^ HOSTNAME MISMATCH (expected $want)"; bad=1; }
  [ "$act" = "active" ] || { say "    ^ unit not active"; bad=1; }
  [ "$api" = "200" ]    || { say "    ^ API did not answer 200"; bad=1; }
done
if [ "$bad" -ne 0 ]; then
  rule
  say "STOPPING: the cluster is not in a known-good state, so nothing was changed."
  say "Fix the above first — a restart now would hide the real cause."
  exit 1
fi
say "  all three healthy."

# ── DISARM ───────────────────────────────────────────────────────────────────
# The reverse of the cutover, scripted for the same reason the cutover is: it has
# to be verifiable, and on 2026-09-27 it had to be done in minutes on an auditor's
# ruling. Removing the drop-in restores the node to exactly its prior state; the
# driver is opt-in, so absence of the file IS off.
if [ "${DISARM:-0}" = 1 ]; then
  rule; say "DISARMING"; rule
  for entry in $NODES; do
    ip="${entry%%:*}"; want="${entry##*:}"
    # NO -n HERE. The heredoc IS this call's stdin; -n points stdin at /dev/null, so
    # `bash -s` read nothing, ran nothing, exited 0 and the step reported success while
    # doing absolutely nothing. I wrote that bug into the disarm path minutes after
    # adding -n everywhere else to stop ssh swallowing the driver script. The -n belongs
    # on calls with NO heredoc; here it silently disables the work.
    out="$(ssh -o ConnectTimeout=15 -o BatchMode=yes azureuser@"$ip" bash -s -- "$want" <<'NODE'
set -uo pipefail
want="$1"
[ "$(hostname)" = "$want" ] || { echo "WRONG-HOST $(hostname) != $want"; exit 1; }
f=/etc/systemd/system/perp-dex-orchestrator.service.d/10-attested-clock.conf
if [ -f "$f" ]; then sudo rm -f "$f"; echo -n "drop-in removed; "; else echo -n "drop-in already absent; "; fi
sudo systemctl daemon-reload
sudo systemctl restart perp-dex-orchestrator
echo -n "restarted; "
# `grep -c` exits 1 when the count is 0, and with pipefail that became the whole block's
# status: the disarm SUCCEEDED and reported FAILED. Count into a variable so the status
# belongs to the work, not to whether a grep matched.
n="$(systemctl show perp-dex-orchestrator -p Environment --value | tr ' ' '\n' | grep -c ATTESTED || true)"
echo "ATTESTED vars now: $n"
NODE
)" || { say "  $want ($ip): FAILED"; say "$out"; exit 1; }
    say "  $want ($ip): $(echo "$out" | tr '\n' ' ')"
  done
  # The point of the exercise: the driver must report itself OFF.
  say "  waiting for the APIs..."
  for _ in $(seq 1 15); do sleep 4; done
  bad=0
  for entry in $NODES; do
    ip="${entry%%:*}"; want="${entry##*:}"
    en="$(clock_field "$ip" driver_enabled)"
    say "  $want ($ip): driver_enabled=$en"
    [ "$en" = "False" ] || bad=1
  done
  rule
  if [ "$bad" -ne 0 ]; then
    say "DISARM INCOMPLETE — at least one node still reports the driver enabled."
    exit 1
  fi
  say "DISARMED on all nodes. Re-enable by running this script with no arguments."
  exit 0
fi

# ── DRY RUN ──────────────────────────────────────────────────────────────────
# Rehearse the read half. Everything below this point writes, so a dry run stops
# here — after having exercised the ssh plumbing, the hostname guard and the
# status parser that the real run depends on.
if [ "${DRY:-0}" = 1 ]; then
  rule; say "WOULD CHANGE"; rule
  for entry in $NODES; do
    ip="${entry%%:*}"; want="${entry##*:}"
    r="$(ssh -o ConnectTimeout=10 -o BatchMode=yes azureuser@"$ip" bash -s -- "$CONF_SHA" <<'NODE'
want_sha="$1"
f=/etc/systemd/system/perp-dex-orchestrator.service.d/10-attested-clock.conf
# Compare by hash, so a dry run writes NOTHING to the node — not even a temp file.
if [ ! -f "$f" ]; then echo -n "drop-in: ABSENT, would install"
elif [ "$(sha256sum "$f" | cut -d" " -f1)" = "$want_sha" ]; then echo -n "drop-in: already identical"
else echo -n "drop-in: DIFFERS, would replace"; fi
if [ -f /etc/systemd/system/perp-dex.service ]; then
  echo -n " | perp-dex.service: PRESENT ($(systemctl show perp-dex.service -p NRestarts --value) restarts), would remove"
else
  echo -n " | perp-dex.service: absent"
fi
echo
NODE
)"
    adv="$(clock_field "$ip" advances)"
    if [ "${adv:-0}" -gt 0 ] 2>/dev/null; then
      say "  $want ($ip): clock ALREADY advancing — restart would be skipped"
    else
      say "  $want ($ip): would restart perp-dex-orchestrator (clock not advancing)"
    fi
    say "      $r"
  done
  rule
  say "DRY RUN complete — nothing was changed. Re-run without --dry-run to apply."
  exit 0
fi

# ── 1. THE CLOCK, NODE BY NODE ───────────────────────────────────────────────
for entry in $NODES; do
  ip="${entry%%:*}"; want="${entry##*:}"
  rule; say "NODE $want ($ip)"; rule

  adv="$(clock_field "$ip" advances)"
  if [ "${adv:-0}" -gt 0 ] 2>/dev/null; then
    say "  clock already advancing (advances=$adv) — leaving this node alone."
    continue
  fi

  say "  installing the drop-in (idempotent)..."
  out="$(ssh -o ConnectTimeout=15 -o BatchMode=yes azureuser@"$ip" bash -s -- "$CONF_B64" "$want" <<'NODE'
set -uo pipefail
b64="$1"; want="$2"
[ "$(hostname)" = "$want" ] || { echo "WRONG-HOST $(hostname) != $want"; exit 1; }
d=/etc/systemd/system/perp-dex-orchestrator.service.d
f="$d/10-attested-clock.conf"
sudo mkdir -p "$d"
printf '%s' "$b64" | base64 -d > /tmp/.clk.conf
if [ -f "$f" ] && cmp -s /tmp/.clk.conf "$f"; then
  echo "unchanged"
else
  sudo cp /tmp/.clk.conf "$f" && echo "written"
fi
rm -f /tmp/.clk.conf
sudo systemctl daemon-reload
# Prove systemd actually merged it, and that the base unit's RUST_LOG survived
# (Environment= accumulates across drop-ins rather than replacing).
systemctl show perp-dex-orchestrator -p Environment --value | tr ' ' '\n' | grep -E 'ATTESTED|XRPL|RUST_LOG' | sed 's/^/env: /'
NODE
)" || { say "  FAILED to install:"; say "$out"; say "  Remaining nodes were NOT touched."; exit 1; }
  say "$out" | sed 's/^/    /'
  case "$out" in *ATTESTED_CLOCK=1*) ;; *) say "  FAILED: systemd did not merge PERP_ATTESTED_CLOCK"; exit 1 ;; esac

  say "  restarting perp-dex-orchestrator..."
  ssh -n -o ConnectTimeout=15 -o BatchMode=yes azureuser@"$ip" \
    'sudo systemctl restart perp-dex-orchestrator' || { say "  FAILED: restart"; say "  Remaining nodes were NOT touched."; exit 1; }

  # Wait for the API to come back before judging the clock.
  say "  waiting for the API..."
  up=0
  for _ in $(seq 1 30); do
    sleep 2
    if [ "$(curl -s -m 5 -o /dev/null -w '%{http_code}' "http://$ip:3000/v1/system/status" 2>/dev/null)" = "200" ]; then
      up=1; break
    fi
  done
  [ "$up" = 1 ] || { say "  FAILED: API did not return within 60s"; \
    ssh -n -o BatchMode=yes azureuser@"$ip" 'journalctl -u perp-dex-orchestrator -n 25 --no-pager -q' | sed 's/^/    /'; exit 1; }
  say "  API up."

  # THE SUCCESS CRITERION: the sequence must go UP. A single nonzero reading only
  # proves one header was read; it does not prove a clock that keeps running.
  say "  waiting for the clock to advance twice (up to 150s)..."
  first=""; ok=0
  for _ in $(seq 1 30); do
    sleep 5
    seq_now="$(clock_field "$ip" attested_ledger_seq)"
    [ -n "$seq_now" ] || continue
    [ "$seq_now" = "0" ] && continue
    if [ -z "$first" ]; then
      first="$seq_now"
      say "    first attested ledger: $first"
    elif [ "$seq_now" -gt "$first" ] 2>/dev/null; then
      say "    advanced: $first -> $seq_now"
      ok=1; break
    fi
  done
  if [ "$ok" != 1 ]; then
    rcv="$(clock_field "$ip" last_refusal_rc)"
    say "  FAILED: the clock did not advance."
    say "    advances      = $(clock_field "$ip" advances)"
    say "    refusals      = $(clock_field "$ip" refusals)"
    say "    last_refusal  = $rcv  ($(rc_meaning "${rcv:-x}"))"
    ssh -n -o BatchMode=yes azureuser@"$ip" \
      'journalctl -u perp-dex-orchestrator -n 30 --no-pager -q | grep -iE "clock|unl|validat" | tail -15' | sed 's/^/    /'
    say "  Remaining nodes were NOT touched."
    exit 1
  fi
  ct="$(clock_field "$ip" attested_close_time_ripple_epoch)"
  say "  OK — attested close_time (ripple epoch) $ct = $(python3 -c "import datetime as d;print(d.datetime.fromtimestamp(int('${ct:-0}')+946684800,d.timezone.utc).isoformat())" 2>/dev/null)"
done

# ── 2. THE OBSOLETE CRASH-LOOPING UNIT ───────────────────────────────────────
rule; say "OBSOLETE perp-dex.service"; rule
for entry in $NODES; do
  ip="${entry%%:*}"; want="${entry##*:}"
  out="$(ssh -o ConnectTimeout=15 -o BatchMode=yes azureuser@"$ip" bash -s <<'NODE'
set -uo pipefail
f=/etc/systemd/system/perp-dex.service
if [ ! -f "$f" ]; then echo "absent — nothing to do"; exit 0; fi
n="$(systemctl show perp-dex.service -p NRestarts --value 2>/dev/null)"
mkdir -p /home/azureuser/perp/removed-units
sudo cp "$f" /home/azureuser/perp/removed-units/perp-dex.service.removed
sudo systemctl disable --now perp-dex.service >/dev/null 2>&1 || true
sudo rm -f "$f"
sudo systemctl daemon-reload
sudo systemctl reset-failed perp-dex.service >/dev/null 2>&1 || true
echo "removed (had $n restarts); copy kept at ~/perp/removed-units/perp-dex.service.removed"
NODE
)" || out="FAILED: $out"
  say "  $want ($ip): $out"
done

# ── 3. FINAL STATE ───────────────────────────────────────────────────────────
rule; say "FINAL"; rule
for entry in $NODES; do
  ip="${entry%%:*}"; want="${entry##*:}"
  say "  $want ($ip): enabled=$(clock_field "$ip" driver_enabled) advances=$(clock_field "$ip" advances) seq=$(clock_field "$ip" attested_ledger_seq) refusals=$(clock_field "$ip" refusals) last_rc=$(clock_field "$ip" last_refusal_rc)"
done
rule
say "Done. 'refusals' rising slowly alongside a rising 'seq' is normal only if"
say "last_rc is NOT -70/-71; -89 is never counted as a refusal."
DRIVER_BODY
} > "$DRIVER"

ssh "${SSH_OPTS[@]}" "$BASTION" 'bash -s' < "$DRIVER"
