#!/usr/bin/env bash
# deploy-enclave-server.sh — swap perp-dex-server (the HOST binary) without touching the enclave.
#
#   bash orchestrator/scripts/deploy-enclave-server.sh check   verify readiness, change NOTHING
#   bash orchestrator/scripts/deploy-enclave-server.sh go      one node at a time
#
# WHY THIS EXISTS. perp-dex-server had no deployment path of its own: it moved only as part of a
# promotion, copied out of perp-next by the ceremony. So a host-side fix — like the 4 KB
# request-body truncation that made the attested clock refuse every ledger — had no way to the
# cluster short of a full MRENCLAVE bump it does not need.
#
# WHY IT IS SAFE TO SWAP ALONE, and this was PROVEN rather than reasoned. The host and the
# enclave must come from one build, because the generated ecall marshalling has to match. The
# fix was therefore rebased onto d0baab1 — the Enclave/ tree with nothing of it changed — and
# the resulting enclave.signed.so measures
#   367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308
# which is byte for byte the measurement the cluster is running. (Its .so differs in bytes from
# the deployed one: a different signing key, so a different MRSIGNER and signature. MRENCLAVE
# does not depend on the key, and sealing here is MRENCLAVE-policy, so nothing unseals
# differently.) The .so is NOT deployed by this script; only the server is.
#
# THE CHECK THAT PROVES THE FIX, not merely that the binary moved: after the swap it POSTs an
# 8000-hex-character body to the clock route. Before the fix that answered
# {"message":"Invalid JSON format"} — the body truncated at 4 KB. After it, the route reaches
# the enclave and answers with a real return code. A deploy that cannot demonstrate the defect
# is gone has demonstrated nothing.
set -uo pipefail

BASTION="andrey@94.130.18.162"
SRC="/tmp/b18-new-server"          # extracted from the b18-tree build on the bastion
MRENCLAVE_EXPECTED="367cabb24ea4ae60b58075c4ec974b805077b0f6fac293b9e9954f8287baa308"
declare -a NODES=(20.71.184.176 20.224.243.60 52.236.130.102)
MODE="${1:-check}"

hr() { printf '%s\n' "------------------------------------------------------------"; }
on() {
  case "$2" in
    *\'*) echo "on(): refusing — a single quote would break the nested quoting" >&2; return 64 ;;
  esac
  ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=25 azureuser@$1 '$2'"
}
mre() { on "$1" "curl -k -s --max-time 10 https://localhost:9088/version" \
        | sed -n 's/.*"mrenclave":"\([^"]*\)".*/\1/p'; }
# 8000 hex chars: over the OLD 4 KB limit, under any sane new one.
bodylimit() {
  on "$1" "blob=\$(head -c 8000 /dev/zero | tr \\\\0 a); curl -k -s --max-time 15 -X POST https://localhost:9088/v1/perp/attested-clock/advance -H \"Content-Type: application/json\" -d \"{\\\"clock_blob\\\":\\\"\$blob\\\"}\"" \
    | grep -oE "rc=-?[0-9]+|Invalid JSON format" | head -1
}

case "$MODE" in check|go) ;; *) echo "usage: $0 [check|go]"; exit 2 ;; esac
echo "perp-dex-server swap — MODE=$MODE"
hr

echo "[1/3] the new binary must exist on the bastion, and every node must be as expected"
sz="$(ssh -o BatchMode=yes "$BASTION" "stat -c %s $SRC 2>/dev/null || echo 0")"
echo "  source $SRC: ${sz} bytes"
[ "${sz:-0}" -gt 1000000 ] 2>/dev/null || {
  echo "STOP — no usable binary at $SRC on the bastion. Build it first:"
  echo "    the b18-tree branch, docker build, then docker cp out of the image."
  exit 1; }
blocked=0
for ip in "${NODES[@]}"; do
  m="$(mre "$ip")"; lim="$(bodylimit "$ip")"
  printf '  %-16s :9088=%s  8000-char body -> %s\n' "$ip" "${m:0:16}" "${lim:-no answer}"
  [ "$m" = "$MRENCLAVE_EXPECTED" ] || { echo "    BLOCKED: unexpected measurement"; blocked=1; }
done
hr
[ "$blocked" -eq 0 ] || { echo "STOP — nothing swapped."; exit 1; }
if [ "$MODE" = "check" ]; then
  echo "check only. If the body column says Invalid JSON format, the fix is not there yet."
  echo "Next: $0 go"
  exit 0
fi

echo "[2/3] swapping, one node at a time, verifying before moving on"
for ip in "${NODES[@]}"; do
  echo "  === $ip ==="
  # BACK UP FIRST. The b18 promotion overwrote a deployed binary without a copy and left the
  # retired generation's sealed state readable only after rebuilding its measurement.
  on "$ip" "cp -n /home/azureuser/perp/perp-dex-server /home/azureuser/perp/perp-dex-server.pre-bodyfix.bak && test -s /home/azureuser/perp/perp-dex-server.pre-bodyfix.bak && echo backed-up" \
    || { echo "    STOP: could not back up the current server binary"; exit 1; }
  ssh -o BatchMode=yes "$BASTION" "scp -q -o BatchMode=yes $SRC azureuser@$ip:/tmp/new-perp-dex-server" \
    || { echo "    STOP: could not copy the new binary to the node"; exit 1; }
  on "$ip" "sudo systemctl stop perp-dex-enclave && install -m 755 /tmp/new-perp-dex-server /home/azureuser/perp/perp-dex-server && rm -f /tmp/new-perp-dex-server && sudo systemctl start perp-dex-enclave && echo swapped" \
    || { echo "    STOP: the swap failed. The previous binary is at perp-dex-server.pre-bodyfix.bak"; exit 1; }
  ssh -o BatchMode=yes "$BASTION" "sleep 12"
  m="$(mre "$ip")"
  sv="$(on "$ip" "systemctl is-active perp-dex-enclave")"
  # NO awk. The awk program was passed UNQUOTED through on(), which forbids single quotes,
  # so bash saw its braces and parens as syntax and the command died — the check then
  # returned empty, read as a refusal, and halted a deploy whose three other checks had
  # already passed. A false stop is still a defect. grep and tail need no quoting gymnastics,
  # and this exact command was run against the already-swapped node before being committed:
  # last-start line 123113, refusals since 0.
  ft="$(on "$ip" "N=\$(grep -n \"Server started on port 9088\" /home/azureuser/perp/enclave.log | tail -1 | cut -d: -f1); tail -n +\$N /home/azureuser/perp/enclave.log | grep -ciE \"FATAL|perpLoadState failed\"")"
  lim="$(bodylimit "$ip")"
  # AND A MARKER FOR THE CHANGE BEING DEPLOYED NOW. The 8000-char body check above proves the
  # 2026-10-07 body fix, which is ALREADY on the cluster — so on any later swap it passes
  # whether or not the new binary landed. A check that cannot distinguish the build you want
  # from the one you have is not a check; this is the same lesson the orchestrator deploy
  # script learned when "step 5a" stopped discriminating. Add a line per change.
  mk="$(on "$ip" "strings -a /home/azureuser/perp/perp-dex-server | grep -cF \"Reserves commit refused (rc=\"")"
  printf '    after: :9088=%s  svc=%s  refusals-in-log=%s  8000-char body -> %s  rc-marker=%s\n' \
    "${m:0:16}" "$sv" "${ft:-?}" "${lim:-no answer}" "${mk:-?}"
  [ "${mk:-0}" -gt 0 ] 2>/dev/null || { echo "    STOP: the running server does not carry the reserves-rc change — the swap did not take"; exit 1; }
  [ "$m" = "$MRENCLAVE_EXPECTED" ] || { echo "    STOP: the measurement CHANGED — this should be impossible when only the server is swapped"; exit 1; }
  [ "$sv" = "active" ] || { echo "    STOP: perp-dex-enclave is $sv"; exit 1; }
  [ "${ft:-1}" = "0" ] || { echo "    STOP: the enclave log shows a refusal on this boot"; exit 1; }
  case "${lim:-}" in
    rc=*) echo "    OK — an 8000-character body now reaches the enclave and gets a real code" ;;
    *) echo "    STOP: the body limit is still biting (${lim:-no answer}). The swap did not take."; exit 1 ;;
  esac
  hr
done

echo "[3/3] all three swapped. The clock can be re-checked now:"
echo "    bash orchestrator/scripts/arm-attested-clock.sh check"
echo "Previous binaries kept per node at perp/perp-dex-server.pre-bodyfix.bak — they are the"
echo "rollback, and nothing removes them automatically."
