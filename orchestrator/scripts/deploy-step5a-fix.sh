#!/usr/bin/env bash
# deploy-step5a-fix.sh — put the step-5a dry-run fix (PR #69) on the cluster.
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
ssh -o BatchMode=yes "$BASTION" '
  B=~/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator
  fail=0
  for m in "step 5a" "did not carry" "NO-OP, not a failure to retry" "REQUIRED after promotion"; do
    N=$(strings -a "$B" | grep -cF "$m")
    printf "  %-34s %s\n" "\"$m\"" "$N"
    [ "$N" -gt 0 ] || { echo "    MISSING — this build predates that fix"; fail=1; }
  done
  [ "$fail" -eq 0 ] || exit 3
' || { echo "FAILED: artefact check — do not deploy"; exit 3; }
hr

echo "[3/4] deploying to all three nodes"
# deploy.sh is the current tool and runs FROM Hetzner. (A deploy_one.sh appears in older
# notes of mine; it does not exist in the repo — checked before writing this.)
ssh -o BatchMode=yes "$BASTION" 'cd ~/llm-perp-xrpl/orchestrator && ./scripts/deploy.sh all' \
  || { echo "FAILED: deploy — check which nodes got it before re-running"; exit 4; }
hr

echo "[4/4] PROVING the RUNNING binary on each node carries it"
for ip in 20.71.184.176 20.224.243.60 52.236.130.102; do
  echo -n "  $ip  "
  ssh -o BatchMode=yes "$BASTION" "ssh -o BatchMode=yes -o ConnectTimeout=10 azureuser@$ip '
    B=/home/azureuser/perp/perp-dex-orchestrator
    A=\$(systemctl is-active perp-dex-orchestrator 2>/dev/null)
    N1=\$(strings -a \$B 2>/dev/null | grep -cF \"step 5a\")
    N2=\$(strings -a \$B 2>/dev/null | grep -cF \"did not carry\")
    N3=\$(strings -a \$B 2>/dev/null | grep -cF \"NO-OP, not a failure to retry\")
    N4=\$(strings -a \$B 2>/dev/null | grep -cF \"REQUIRED after promotion\")
    echo \"service=\$A  step5a=\$N1  inventory-diff=\$N2  govern-no-op=\$N3  followups=\$N4\"'"
done
hr
echo "If every node reports service=active and all three markers >0, rehearse on ALL THREE:"
echo "    bash orchestrator/scripts/ceremony-parallel.sh dryrun"
echo
echo "In each node's response read \`status\` AND \`boot_proof\`:"
echo "  \"dry-run-ok\"            the only PASS. It now requires the step-5a boot."
echo "  \"dry-run-boot-failed\"   the boot ran and was judged a failure; boot_failure says why."
echo "  missing_from_new        must hold nothing unexplained — that is the answer to"
echo "                          'OLD has 183 sealed files, the rehearsal wrote 182, which one'."
