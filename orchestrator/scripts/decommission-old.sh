#!/usr/bin/env bash
# decommission-old.sh — runbook §6 + §6.1. It DELETES sealed state. Read the gates.
#
#   bash orchestrator/scripts/decommission-old.sh survey     list what would go, delete NOTHING
#   bash orchestrator/scripts/decommission-old.sh historical scrub every copy EXCEPT the newest
#   bash orchestrator/scripts/decommission-old.sh all        scrub every stale copy
#
# WHY IT EXISTS, and the finding that prompted it. §6 says decommissioning OLD is "not optional
# and not 'keep a backup for a few weeks'", because a stale sealed copy of customer state is an
# exposure surface: the binary for its MRENCLAVE is reproducible from source, so the blob is not
# cryptographically inert. That is the direct lesson of INCIDENT-2026-05-20.
#
# It has never been run. As of 2026-10-06 each node carried FIFTEEN stale accounts.* directories
# spanning 2026-07-06 to 2026-10-05 — 141 MB per node, 45 directories across the cluster, one
# full copy of customer state per MRENCLAVE generation.
#
# TWO MODES BECAUSE THEY ARE DIFFERENT DECISIONS. The historical copies have no forensic value
# left: their generations are long superseded and any problem with them would have surfaced
# months ago. The NEWEST one is the immediate pre-migration state, which §7.2 calls forensic
# evidence — worth most in the first days after a migration. `historical` scrubs the first group
# and keeps the newest; `all` scrubs everything.
#
# THE LIVE SET IS VERIFIED BEFORE ANY BACKUP IS TOUCHED. Deleting the fallback while the live
# state is in doubt is the one way this operation can be catastrophic, so: :9088 serves the
# expected measurement, the service is active, no refusal since its last start, the live
# accounts dir holds a plausible set, EVERY live sealed file is MRENCLAVE-policy, and the live
# directory is a different inode from each candidate. Any failure and nothing is deleted.
set -uo pipefail

BASTION="andrey@94.130.18.162"
MRENCLAVE_LIVE="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
NEWEST="accounts.OLD-aead7ecf"
declare -a NODES=(20.71.184.176 20.224.243.60 52.236.130.102)
MODE="${1:-survey}"

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() { ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=20 azureuser@$1 '$2'"; }

case "$MODE" in survey|historical|all) ;; *) echo "usage: $0 [survey|historical|all]"; exit 2 ;; esac
echo "runbook §6 — decommission OLD sealed state      MODE=$MODE"
hr

# ── verify the LIVE set on every node first ──────────────────────────────────
echo "[1/3] the LIVE state must be healthy on every node before any backup is touched"
blocked=0
for ip in "${NODES[@]}"; do
  q="$(on "$ip" '
    printf "mre=%s\n"  "$(curl -k -s --max-time 8 https://localhost:9088/version 2>/dev/null | sed -n "s/.*\"mrenclave\":\"\([^\"]*\)\".*/\1/p")"
    printf "svc=%s\n"  "$(systemctl is-active perp-dex-enclave 2>/dev/null)"
    printf "live=%s\n" "$(ls -1 /home/azureuser/perp/accounts/ 2>/dev/null | wc -l)"
    printf "sym=%s\n"  "$(test -L /home/azureuser/perp/accounts && echo YES || echo no)"
    printf "fatal=%s\n" "$(awk "/Server started on port 9088/{n=NR} {a[NR]=\$0} END{c=0; for(i=n;i<=NR;i++) if (a[i] ~ /FATAL|perpLoadState failed/) c++; print c}" /home/azureuser/perp/enclave.log 2>/dev/null)"
    bad=0; for f in /home/azureuser/perp/accounts/*; do [ -f "$f" ] || continue; v=$(od -An -tu2 -j2 -N2 "$f" 2>/dev/null | tr -d " "); [ "$v" = "1" ] || bad=$((bad+1)); done
    printf "badpol=%s\n" "$bad"
  ')"
  f() { printf '%s\n' "$q" | sed -n "s/^$1=//p" | head -1; }
  m="$(f mre)"; s="$(f svc)"; lv="$(f live)"; sy="$(f sym)"; ft="$(f fatal)"; bp="$(f badpol)"
  printf '  %-16s :9088=%s svc=%s live=%s files badpolicy=%s refusals=%s symlink=%s\n' \
    "$ip" "${m:0:16}" "$s" "$lv" "$bp" "$ft" "$sy"
  [ "$m" = "$MRENCLAVE_LIVE" ] || { echo "    BLOCKED: :9088 is not the expected measurement"; blocked=1; }
  [ "$s" = "active" ]          || { echo "    BLOCKED: perp-dex-enclave is $s"; blocked=1; }
  [ "${lv:-0}" -ge 100 ] 2>/dev/null || { echo "    BLOCKED: live set holds only ${lv:-?} files"; blocked=1; }
  [ "$sy" = "no" ]             || { echo "    BLOCKED: perp/accounts is a symlink"; blocked=1; }
  [ "${ft:-1}" = "0" ]         || { echo "    BLOCKED: ${ft} refusal(s) in the log since the last start"; blocked=1; }
  [ "${bp:-1}" = "0" ]         || { echo "    BLOCKED: ${bp} live sealed file(s) are NOT MRENCLAVE-policy"; blocked=1; }
done
hr
[ "$blocked" -eq 0 ] || { echo "STOP — nothing deleted. The live state must be beyond doubt first."; exit 1; }
echo "  live state healthy everywhere"
hr

# ── survey what would go ─────────────────────────────────────────────────────
echo "[2/3] stale copies per node"
total=0
for ip in "${NODES[@]}"; do
  echo "  $ip"
  # NO SINGLE QUOTES in anything handed to on(): it embeds its argument inside single quotes,
  # so an inner one closes the outer quoting. The first version put the printf FORMAT in single
  # quotes and the format arrived word-split, printing %4s and MB as separate columns.
  on "$ip" "for d in /home/azureuser/perp/accounts.*; do [ -d \"\$d\" ] || continue; printf \"    %-48s %4s MB  %s\\n\" \"\$(basename \$d)\" \"\$((\$(du -sb \$d | cut -f1)/1048576))\" \"\$(stat -c %y \$d | cut -c1-10)\"; done"
  n="$(on "$ip" 'ls -d /home/azureuser/perp/accounts.* 2>/dev/null | wc -l')"
  total=$((total + ${n:-0}))
done
hr
echo "  $total stale directories across the cluster"
if [ "$MODE" = "survey" ]; then
  echo "  survey only — nothing deleted. Next: $0 historical   (keeps $NEWEST)"
  echo "                           or: $0 all          (scrubs it too)"
  exit 0
fi
hr

# ── consent ──────────────────────────────────────────────────────────────────
if [ "$MODE" = "historical" ]; then
  echo "About to DELETE every stale copy EXCEPT $NEWEST, on all three nodes."
  echo "That keeps the immediate pre-migration state (§7.2 forensic evidence) and removes the"
  echo "generations whose forensic value is gone."
  phrase="scrub the historical copies"
else
  echo "About to DELETE EVERY stale copy, INCLUDING $NEWEST, on all three nodes."
  echo "After this there is no on-disk copy of any pre-β18 state. Note that no binary for the"
  echo "retired measurement was preserved on these nodes, so that copy is already unreadable"
  echo "without rebuilding aead7ecf from source — but deletion is still final."
  phrase="scrub every copy including the newest"
fi
printf 'Type exactly: %s\n> ' "$phrase"
read -r ans
[ "$ans" = "$phrase" ] || { echo "NOT CONFIRMED — nothing deleted."; exit 1; }
hr

# ── scrub, then re-scan ──────────────────────────────────────────────────────
echo "[3/3] scrubbing"
for ip in "${NODES[@]}"; do
  printf '  %-16s ' "$ip"
  if [ "$MODE" = "historical" ]; then
    on "$ip" "n=0; for d in /home/azureuser/perp/accounts.*; do [ -d \"\$d\" ] || continue; [ \"\$(basename \$d)\" = \"$NEWEST\" ] && continue; rm -rf \"\$d\" && n=\$((n+1)); done; printf 'removed %s, kept %s\n' \"\$n\" \"$NEWEST\""
  else
    on "$ip" "n=0; for d in /home/azureuser/perp/accounts.*; do [ -d \"\$d\" ] || continue; rm -rf \"\$d\" && n=\$((n+1)); done; printf 'removed %s\n' \"\$n\""
  fi
  # §6.1 — leave perp-next down to the one-time machine setup
  on "$ip" 'c=$(ls -1 /home/azureuser/perp-next/accounts/ 2>/dev/null | wc -l); if [ "$c" = "0" ]; then rmdir /home/azureuser/perp-next/accounts 2>/dev/null; rm -f /home/azureuser/perp-next/enclave.signed.so /home/azureuser/perp-next/perp-dex-server /home/azureuser/perp-next/perp-dex-orchestrator /home/azureuser/perp-next/civetweb_access.log /home/azureuser/perp-next/enclave.log; printf "    §6.1: perp-next cleared, left: %s\n" "$(ls -1 /home/azureuser/perp-next/ | tr "\n" " ")"; else printf "    §6.1 SKIPPED: perp-next/accounts holds %s files — a promoted set was left behind, investigate\n" "$c"; fi'
  printf '    re-scan: %s\n' "$(on "$ip" 'printf "live=%s files, stale dirs=%s" "$(ls -1 /home/azureuser/perp/accounts/ | wc -l)" "$(ls -d /home/azureuser/perp/accounts.* 2>/dev/null | wc -l)"')"
done
hr
echo "Done. The audit record lives in docs/ and memory, not as sealed blobs on disk (§6)."
echo "NOT touched, deliberately: enclave.signed.so.*.bak — those are BINARIES, not customer"
echo "state, and they are what makes a prior generation's state readable at all."
