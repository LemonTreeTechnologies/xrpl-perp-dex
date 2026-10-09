#!/usr/bin/env bash
# deploy-orchestrator.sh — build the orchestrator from master and put it on all three nodes.
#
# RENAMED from deploy-step5a-fix.sh on 2026-10-07: it has deployed four unrelated changes
# since, so the name had become a stale claim about its own scope — the same defect this file
# has already had fixed in it three times (a pinned commit it was not deploying, a retracted
# idempotency note, a marker count that went stale). A filename is output too.
#
# RUN THIS FROM YOUR LAPTOP:  bash orchestrator/scripts/deploy-step5a-fix.sh
#
# WHY IT IS NEEDED BEFORE THE NEXT DRY RUN. The β18 dry run that returned "dry-run-ok" never
# booted the new enclave on the migrated state: it wrote 182 sealed files, verified durability
# in-process, then deleted them and restarted NEW on an EMPTY directory. For β18 the change
# under test IS a startup-load check, so the only thing that could refuse to boot was never
# exercised. PR #69 makes the dry run perform that boot BEFORE the cleanup and judge it.
#
# Until this is deployed, re-running the dry run just reproduces the same uninformative pass.
#
# WHAT IT TOUCHES: the orchestrator binary and the perp-dex-orchestrator service on all three
# testnet nodes. It does NOT touch either enclave, so MRENCLAVE is unaffected and β18's
# measurement stays admitted on the allowlist. Nothing is retired.
set -uo pipefail

BASTION="andrey@94.130.18.162"
# NO PINNED COMMIT. It said 7246ea7 while deploying master's head, which by the third run was
# 32b2ef2 — a deploy script printing a commit it is not deploying is the same class of lie as a
# build whose "Finished" line means nothing. The script reports what it ACTUALLY moved to, which
# the build step prints, and the artefact markers are what prove the content.

hr() { printf '%s\n' "------------------------------------------------------------"; }

# ONE MARKER PER CHANGE that must be on the cluster, and ONE LIST — it used to be written out
# twice, by hand, in step [2/4] and again in step [4/4]. Two lists diverge: #107 and #108 were
# merged and the lists would have proved a binary carrying neither, because neither list knew
# the new strings existed. Checked before adding them that each is present in the new build and
# ABSENT in the deployed one — a marker that does not discriminate is not a check.
MARKERS=(
  "step 5a"
  "did not carry"
  "NO-OP, not a failure to retry"
  "REQUIRED after promotion"
  "clio.altnet.rippletest.net"
  "attested_clock_refused_unl_off"
  "validations stream went SILENT"
  "SPV-deposit boundary ARMED"
  "enclave reserves_commit refused"
  "xperp/v1/admin|"          # #107 the admin canonical binds method+route
  "REFUSING to seal"         # #107 seal-initial requires --expect-escrow
  "is IN USE by this node"   # #108 revoke-session-key's in-use guard
)
# Passed to the remote shells base64-encoded: the markers contain spaces and a pipe, and
# threading those through two levels of ssh quoting is how a check silently tests the wrong
# string. This session already lost a flag that way.
MARKERS_B64=$(printf '%s\n' "${MARKERS[@]}" | base64 -w0)



echo "deploying the orchestrator from master (the build step prints the exact commit)"
hr

echo "[1/4] building on Hetzner (the build clone is on an older branch — moving it to master)"
# TWO defects lived in this block on its first run, both in this file's own stated subject:
#
#   1. no `cargo` on PATH. A non-interactive ssh runs neither the login profile nor .bashrc,
#      so cargo — installed by rustup under ~/.cargo/bin — simply is not there. Hence the
#      explicit `. ~/.cargo/env`, not a login shell, so it fails loudly if that file moves.
#   2. `cargo build ... | tail -3` returns TAIL's exit code, which is always 0. So `set -e`
#      could not fire and step [1/4] reported nothing while the build had not happened. That
#      is precisely the false-green shape this script's step [2/4] exists to catch — and it
#      is what caught it. Fixed with `set -o pipefail` AND an explicit status check, because
#      a pipeline's exit code is not the thing you think it is.
ssh -o BatchMode=yes "$BASTION" '
  set -eo pipefail
  . "$HOME/.cargo/env" 2>/dev/null || { echo "  FAILED: no ~/.cargo/env — where is cargo?"; exit 9; }
  command -v cargo >/dev/null || { echo "  FAILED: cargo still not on PATH"; exit 9; }
  cd ~/llm-perp-xrpl
  echo "  was: $(git branch --show-current) $(git rev-parse --short HEAD)"
  git fetch origin master -q
  git checkout -q master 2>/dev/null || git checkout -q -B master origin/master
  git reset --hard -q origin/master
  echo "  now: $(git branch --show-current) $(git rev-parse --short HEAD)"
  cd orchestrator
  cargo build --release --locked > /tmp/step5a-build.log 2>&1
  rc=$?
  tail -3 /tmp/step5a-build.log
  [ "$rc" -eq 0 ] || { echo "  FAILED: cargo build exited $rc (full log: /tmp/step5a-build.log)"; exit "$rc"; }
' || { echo "FAILED: build — nothing deployed"; exit 2; }
hr

echo "[2/4] PROVING the built artefact carries EVERY fix it is supposed to"
# ONE MARKER PER CHANGE, and that matters. "step 5a" alone stopped proving anything the moment
# a second fix landed on top of it: the string is in both builds, so the check would pass on a
# binary missing the newer change entirely. A marker that cannot distinguish the version you
# want from the one you have is not a check. Add a line here with every change that must be on
# the cluster before the next ceremony step.
ssh -o BatchMode=yes "$BASTION" "MARKERS_B64=$MARKERS_B64 bash -s" <<'REMOTE' \
  || { echo "FAILED: artefact check — do not deploy"; exit 3; }
  B=~/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator
  fail=0
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    N=$(strings -a "$B" | grep -cF -- "$m")
    printf "  %-34s %s\n" "\"$m\"" "$N"
    [ "$N" -gt 0 ] || { echo "    MISSING — this build predates that fix"; fail=1; }
  done <<< "$(printf '%s' "$MARKERS_B64" | base64 -d)"
  [ "$fail" -eq 0 ] || exit 3
REMOTE
hr

echo "[3/4] deploying to all three nodes"
# deploy.sh is the current tool and runs FROM Hetzner. (A deploy_one.sh appears in older
# notes of mine; it does not exist in the repo — checked before writing this.)
ssh -o BatchMode=yes "$BASTION" 'cd ~/llm-perp-xrpl/orchestrator && ./scripts/deploy.sh all' \
  || { echo "FAILED: deploy — check which nodes got it before re-running"; exit 4; }
hr

echo "[4/4] PROVING the RUNNING binary on each node carries it"
# THE SAME LIST as step [2/4]. This block used to spell out N1..N9 by hand through two levels
# of ssh quoting, which is both unreadable and a second list to forget: when #107 and #108
# landed, neither list knew their strings existed, so both would have "proved" a binary
# carrying neither change.
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  echo "  $ip"
  ssh -o BatchMode=yes "$BASTION" "IP=$ip MARKERS_B64=$MARKERS_B64 bash -s" <<'REMOTE'
    ssh -o BatchMode=yes -o ConnectTimeout=15 "azureuser@$IP" \
        "MARKERS_B64=$MARKERS_B64 bash -s" <<'INNER'
      B=/home/azureuser/perp/perp-dex-orchestrator
      echo "    service=$(systemctl is-active perp-dex-orchestrator 2>/dev/null)"
      fail=0
      while IFS= read -r m; do
        [ -n "$m" ] || continue
        N=$(strings -a "$B" 2>/dev/null | grep -cF -- "$m")
        printf "    %-34s %s\n" "\"$m\"" "$N"
        [ "$N" -gt 0 ] || { echo "      MISSING on the RUNNING binary"; fail=1; }
      done <<< "$(printf '%s' "$MARKERS_B64" | base64 -d)"
      [ "$fail" -eq 0 ] || echo "    ^^ THIS NODE IS NOT CURRENT"
INNER
REMOTE
done
hr
# Count-agnostic on purpose: it said "all three markers" the moment there were four, which
# is the same stale-claim shape as the pinned commit this script used to print.
echo "If every node reports service=active and EVERY marker above is >0, the cluster is current."
echo "To arm the attested clock:  bash orchestrator/scripts/arm-attested-clock.sh"
