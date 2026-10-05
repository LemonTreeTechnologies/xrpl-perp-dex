#!/usr/bin/env bash
# verify-post-migration.sh — runbook §4, on every node, BEFORE promotion.
#
#   bash orchestrator/scripts/verify-post-migration.sh [baseline-dir]
#
# Defaults to the newest ~/path-a-baseline-* that the launcher captured.
#
# WHY IT EXISTS. §4 is the gate between a completed ceremony and promotion, and it had no
# tooling at all — it would be typed by hand, on three nodes, immediately after the one step in
# this whole procedure that cannot be undone. That is the worst possible moment to be composing
# ssh commands, and §4's own instruction for the policy check is "audit every file, not a
# sample", which nobody does by hand across ~180 files times three nodes.
#
# WHAT IT DOES NOT DO: promote anything, decommission anything, or decide anything. It reports,
# and it is explicit about which checks are decisive and which are merely informative — a
# verification script that presents a weak check as a strong one is worse than no script.
set -uo pipefail

BASTION="andrey@94.130.18.162"
MRENCLAVE_NEW="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
declare -a NODES=(20.71.184.176 20.224.243.60 52.236.130.102)
BASE="${1:-$(ls -dt "$HOME"/path-a-baseline-* 2>/dev/null | head -1)}"

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() { ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=15 azureuser@$1 '$2'"; }

[ -n "$BASE" ] && [ -d "$BASE" ] || {
  echo "FAILED: no baseline directory. §4 check 3 compares against pre-migration values, and"
  echo "without them it cannot be performed at all. Pass one, or find the directory the"
  echo "launcher printed as 'saved:' before the ceremony."
  exit 2
}
echo "runbook §4 — post-migration verification, BEFORE promotion"
echo "baseline: $BASE"
hr

fails=0
for ip in "${NODES[@]}"; do
  echo "=== $ip ==="
  bl="$BASE/$ip.baseline"
  [ -f "$bl" ] || { echo "  FAILED: no baseline for this node at $bl"; fails=$((fails+1)); continue; }
  want_files="$(sed -n 's/^files=//p' "$bl" | head -1)"

  # ── §4.1 NEW reports the expected measurement ──────────────────────────────
  mre="$(on "$ip" 'curl -k -s --max-time 8 https://localhost:9089/version' \
         | sed -n 's/.*"mrenclave":"\([^"]*\)".*/\1/p')"
  if [ "$mre" = "$MRENCLAVE_NEW" ]; then echo "  [1] MRENCLAVE: ok"
  else echo "  [1] MRENCLAVE: FAILED — reports ${mre:-nothing}"; fails=$((fails+1)); fi

  # ── §4.2 first-boot autoload: restart ONCE and read what it says ───────────
  # The restart is the check. §4 asks for it explicitly, and the four markers below are the
  # enclave telling you it adopted the migrated state rather than starting fresh.
  off="$(on "$ip" 'stat -c %s /home/azureuser/perp-next/enclave.log 2>/dev/null || echo 0')"
  on "$ip" 'sudo systemctl restart perp-dex-enclave-next' >/dev/null 2>&1
  sleep 8
  fresh="$(on "$ip" "tail -c +$((off + 1)) /home/azureuser/perp-next/enclave.log 2>/dev/null")"
  miss=""
  for m in "migration manifest verified" "sealed SignerList loaded" "Auto-loaded"; do
    printf '%s' "$fresh" | grep -qF "$m" || miss="$miss [$m]"
  done
  if [ -z "$miss" ]; then echo "  [2] first-boot autoload: ok (manifest verified, SignerList loaded, auto-load ran)"
  else echo "  [2] first-boot autoload: FAILED — missing:$miss"; fails=$((fails+1)); fi
  if printf '%s' "$fresh" | grep -qiE "FATAL|perpLoadState failed"; then
    echo "      and it printed a refusal: $(printf '%s' "$fresh" | grep -iE 'FATAL|perpLoadState failed' | head -1)"
    fails=$((fails+1))
  fi

  # ── §4.3 state survival, against the BASELINE ──────────────────────────────
  got_files="$(on "$ip" 'ls -1 /home/azureuser/perp-next/accounts/ 2>/dev/null | wc -l')"
  echo "  [3] inventory: baseline $want_files files, NEW now $got_files"
  if [ "${got_files:-0}" -ge $(( ${want_files:-0} - 2 )) ] 2>/dev/null; then
    echo "      ok (within the two names the migration legitimately does not carry:"
    echo "      recent_nonces.sealed, and trusted_mrenclaves.sealed per §5.3)"
  else
    echo "      FAILED — too many files absent; compare against $bl before anything else"
    fails=$((fails+1))
  fi

  # ── §4.4 sealed-file POLICY, EVERY file. The decisive check here. ──────────
  # key_policy is a uint16 LE at byte offset 2 of every sgx_sealed_data_t: 0x0001 MRENCLAVE,
  # 0x0002 MRSIGNER. Getting this wrong is what INCIDENT-2026-05-20 was, and §4 says audit
  # every file rather than a sample — which is precisely why it needs a script.
  pol="$(on "$ip" 'bad=0; n=0; for f in /home/azureuser/perp-next/accounts/*; do [ -f "$f" ] || continue; n=$((n+1)); v=$(od -An -tu2 -j2 -N2 "$f" 2>/dev/null | tr -d " "); [ "$v" = "1" ] || { bad=$((bad+1)); echo "NOT-MRENCLAVE $v $(basename $f)"; }; done; echo "TOTAL $n BAD $bad"')"
  summary="$(printf '%s' "$pol" | sed -n 's/^TOTAL /TOTAL /p')"
  nbad="$(printf '%s' "$summary" | awk '{print $4}')"
  if [ "${nbad:-1}" = "0" ]; then echo "  [4] seal policy: ok — $summary, every file MRENCLAVE-policy"
  else
    echo "  [4] seal policy: FAILED — $summary"
    printf '%s' "$pol" | grep "^NOT-MRENCLAVE" | head -5 | sed 's/^/      /'
    fails=$((fails+1))
  fi

  # ── §4.5 OLD retired ──────────────────────────────────────────────────────
  r="$(on "$ip" '
    printf "marker=%s\n" "$(test -f /home/azureuser/perp/accounts/path_a_retired.sealed && echo present || echo ABSENT)"
    printf "readonly=%s\n" "$(curl -k -s --max-time 8 https://localhost:9088/version >/dev/null 2>&1 && echo responds || echo dead)"
  ')"
  mk="$(printf '%s\n' "$r" | sed -n 's/^marker=//p')"
  ro="$(printf '%s\n' "$r" | sed -n 's/^readonly=//p')"
  if [ "$mk" = "present" ]; then echo "  [5] OLD retired: ok (path_a_retired.sealed present, /version $ro)"
  else echo "  [5] OLD retired: FAILED — retired-marker $mk. The ceremony did not reach its last step."; fails=$((fails+1)); fi
done
hr
echo "§4 RESULT: $fails failed check(s)"
if [ "$fails" -eq 0 ]; then
  cat <<'DONE'
  All §4 checks passed on all three nodes. Promotion (§5) is cleared.

  AND THEN, not later: §5.3 — re-govern the MRENCLAVE allowlist. The migration does not carry
  it, nothing breaks without it today, but the NEXT bump's export refuses with -25 until it is
  governed again. Doing it at the next bump means that bump starts by looking broken.

  §4.6 is not a check but a checkpoint to hold consciously: OLD can no longer unseal NEW's
  re-sealed files. Rollback to the old enclave is impossible, by design, from now on.
DONE
else
  cat <<'STOPP'
  STOP. Per §7.2: do NOT promote and do NOT run the §6 decommission. OLD's accounts/ is the
  only remaining copy of pre-migration state and is forensic evidence. Leave both enclaves as
  they are and escalate with: the failing check, NEW's enclave.log, the ceremony's
  manifest_hash_hex, and both nodes' accounts/ listings.
STOPP
  exit 1
fi
