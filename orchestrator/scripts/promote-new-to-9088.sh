#!/usr/bin/env bash
# promote-new-to-9088.sh — runbook §5, per node, SEQUENTIALLY.
#
#   bash orchestrator/scripts/promote-new-to-9088.sh
#
# WHY A SCRIPT. Between the ceremony and promotion the cluster cannot sign: OLD is retired and
# NEW is not yet in the canonical slot. So §5 runs under outage pressure, and it is five steps
# per node including a `mv` of sealed state. Hand-typing that across three nodes at that moment
# is the shape that has cost this project before.
#
# IT NEVER DELETES ANYTHING. OLD's sealed state is MOVED to accounts.OLD-<mrenclave> and left
# there — §6 decommission removes it later, deliberately, after §4 passed and promotion is
# verified. perp-next/accounts is emptied only AFTER its contents have been copied into place
# and the copy verified by file count.
#
# SEQUENTIAL, one node at a time, verifying before moving on. Promotion has no delegation
# quorum in it, so the §11.10 parallel constraint does not apply here — and doing them one at a
# time means a failure stops with two nodes still in a known state.
set -uo pipefail

BASTION="andrey@94.130.18.162"
MRENCLAVE_NEW="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
MRENCLAVE_OLD_SHORT="aead7ecf"
declare -a NODES=(20.71.184.176 20.224.243.60 52.236.130.102)

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() { ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=20 azureuser@$1 '$2'"; }

echo "runbook §5 — promote NEW into the canonical :9088 slot"
echo "target: $MRENCLAVE_NEW   OLD being set aside: $MRENCLAVE_OLD_SHORT"
hr

for ip in "${NODES[@]}"; do
  echo "=== $ip ==="

  # ── gate: this node must actually be post-ceremony and §4-clean ─────────────
  g="$(on "$ip" '
    printf "new_mre=%s\n" "$(curl -k -s --max-time 8 https://localhost:9089/version 2>/dev/null | sed -n "s/.*\"mrenclave\":\"\([^\"]*\)\".*/\1/p")"
    printf "retired=%s\n" "$(test -f /home/azureuser/perp/accounts/path_a_retired.sealed && echo present || echo ABSENT)"
    printf "new_files=%s\n" "$(ls -1 /home/azureuser/perp-next/accounts/ 2>/dev/null | wc -l)"
    printf "old_files=%s\n" "$(ls -1 /home/azureuser/perp/accounts/ 2>/dev/null | wc -l)"
    printf "aside=%s\n" "$(ls -d /home/azureuser/perp/accounts.OLD-* 2>/dev/null | head -1)"
    printf "src_so=%s\n"  "$(test -s /home/azureuser/perp-next/enclave.signed.so && echo yes || echo MISSING)"
    printf "src_srv=%s\n" "$(test -s /home/azureuser/perp-next/perp-dex-server && echo yes || echo MISSING)"
  ')"
  f() { printf '%s\n' "$g" | sed -n "s/^$1=//p" | head -1; }
  nm="$(f new_mre)"; rt="$(f retired)"; nf="$(f new_files)"; of="$(f old_files)"; as="$(f aside)"
  echo "  NEW=$nm retired-marker=$rt  NEW-files=$nf  OLD-files=$of"
  [ "$nm" = "$MRENCLAVE_NEW" ] || { echo "  STOP: NEW on 9089 is not the target"; exit 1; }
  [ "$rt" = "present" ]        || { echo "  STOP: no retired-marker — this node's ceremony did not complete. §7.2."; exit 1; }
  [ "${nf:-0}" -ge 100 ] 2>/dev/null || { echo "  STOP: NEW holds only ${nf:-?} sealed files"; exit 1; }
  [ -z "$as" ] || { echo "  STOP: $as already exists — this node looks already promoted, or a"; echo "        previous attempt stopped midway. Inspect before re-running."; exit 1; }
  # Checked BEFORE anything moves. If a source binary were missing, the sequence below would
  # fail AFTER setting OLD's state aside, leaving the node stopped with NEW's state and OLD's
  # binary — recoverable, but a mess to be in at this point.
  [ "$(f src_so)"  = "yes" ] || { echo "  STOP: perp-next/enclave.signed.so missing or empty"; exit 1; }
  [ "$(f src_srv)" = "yes" ] || { echo "  STOP: perp-next/perp-dex-server missing or empty"; exit 1; }

  # ── §5.1-5.4 ────────────────────────────────────────────────────────────────
  echo "  promoting (stop both, set OLD aside, install NEW, clear perp-next, start)"
  on "$ip" "
    set -e
    sudo systemctl stop perp-dex-enclave-next perp-dex-enclave
    mv /home/azureuser/perp/accounts /home/azureuser/perp/accounts.OLD-$MRENCLAVE_OLD_SHORT
    cp -r /home/azureuser/perp-next/accounts /home/azureuser/perp/accounts
    n_src=\$(ls -1 /home/azureuser/perp-next/accounts/ | wc -l)
    n_dst=\$(ls -1 /home/azureuser/perp/accounts/ | wc -l)
    [ \"\$n_src\" = \"\$n_dst\" ] || { echo \"    COPY MISMATCH src=\$n_src dst=\$n_dst — NOT clearing perp-next, NOT starting\"; exit 9; }
    # BACK UP BEFORE OVERWRITING. This script did not, on the β18 promotion, and that broke a
    # pattern every one of the thirteen prior cycles had kept — perp/ holds
    # enclave.signed.so.b4b-*, .b5, .b6-*, .b7-*, .bak-b8-*, .b9-*, .b10-*, .b12-*, .b13-*,
    # .pre-bump-20260925.bak and more. The rule is recorded as critical for a reason: the
    # retired generation's sealed state in accounts.OLD-<mre> can only be unsealed by a binary
    # with that MRENCLAVE, so overwriting it without a copy leaves that state readable only
    # after rebuilding the measurement from source. Reproducible, but not at hand -- and
    # not-at-hand is exactly when you need it.
    cp -n /home/azureuser/perp/enclave.signed.so \
          /home/azureuser/perp/enclave.signed.so.$MRENCLAVE_OLD_SHORT.bak
    cp -n /home/azureuser/perp/perp-dex-server \
          /home/azureuser/perp/perp-dex-server.$MRENCLAVE_OLD_SHORT.bak
    test -s /home/azureuser/perp/enclave.signed.so.$MRENCLAVE_OLD_SHORT.bak || {
      echo \"    REFUSING: could not back up the OLD enclave binary\"; exit 8; }
    cp /home/azureuser/perp-next/enclave.signed.so /home/azureuser/perp/enclave.signed.so
    cp /home/azureuser/perp-next/perp-dex-server  /home/azureuser/perp/perp-dex-server
    # DELIBERATELY NOT perp-dex-orchestrator. perp-next holds a stale copy from the
    # side-by-side deploy, while perp/ carries the orchestrator deployed TODAY with the
    # step-5a, inventory-diff, govern-no-op and followups fixes. Copying \"the binaries\" as a
    # set would silently downgrade it — §5 names the enclave and the server, and only those.
    rm -f /home/azureuser/perp-next/accounts/*
    sudo systemctl start perp-dex-enclave
    echo \"    copied \$n_dst sealed files, binaries installed, perp-next cleared\"
  " || { echo "  STOP: promotion failed on this node. OLD's state is at accounts.OLD-$MRENCLAVE_OLD_SHORT"; echo "        and perp-next still holds the migrated set. Do NOT touch the other nodes."; exit 1; }

  sleep 10
  v="$(on "$ip" '
    printf "port8_mre=%s\n" "$(curl -k -s --max-time 10 https://localhost:9088/version 2>/dev/null | sed -n "s/.*\"mrenclave\":\"\([^\"]*\)\".*/\1/p")"
    printf "svc=%s\n" "$(systemctl is-active perp-dex-enclave 2>/dev/null)"
    printf "loaded=%s\n" "$(tail -40 /home/azureuser/perp/enclave.log 2>/dev/null | grep -cE "sealed SignerList loaded|Auto-loaded")"
    printf "fatal=%s\n" "$(tail -40 /home/azureuser/perp/enclave.log 2>/dev/null | grep -ciE "FATAL|perpLoadState failed")"
  ')"
  w() { printf '%s\n' "$v" | sed -n "s/^$1=//p" | head -1; }
  pm="$(w port8_mre)"; sv="$(w svc)"; ld="$(w loaded)"; ft="$(w fatal)"
  echo "  after: :9088 reports ${pm:0:16}  service=$sv  load-lines=$ld  fatal-lines=$ft"
  [ "$pm" = "$MRENCLAVE_NEW" ] || { echo "  STOP: :9088 is not reporting the NEW measurement"; exit 1; }
  [ "$sv" = "active" ]         || { echo "  STOP: perp-dex-enclave is $sv"; exit 1; }
  [ "${ft:-1}" = "0" ]         || { echo "  STOP: the enclave log shows a refusal on this boot"; exit 1; }
  echo "  OK — NEW is serving :9088 on this node"
  hr
done

cat <<'DONE'
All three promoted. :9088 now serves the NEW measurement on every node.

IMMEDIATELY, not later — §5.3, the allowlist:
    bash orchestrator/scripts/govern-b18-and-dryrun.sh   # with MRENCLAVE = the NEW measurement

The migration did not carry trusted_mrenclaves.sealed. Nothing is broken without it, but the
NEXT MRENCLAVE bump's export refuses with -25 until it is governed again, and discovering that
at the next bump means the next bump starts by looking broken.

STILL PENDING, deliberately: §6 OLD decommission. Each node holds
accounts.OLD-aead7ecf — a stale copy of customer state, which §6 says to scrub and which this
script never touches. Do it after promotion is verified in service, not now.
DONE
