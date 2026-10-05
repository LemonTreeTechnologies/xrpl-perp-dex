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
WANT_COMMIT="7246ea7"

hr() { printf '%s\n' "------------------------------------------------------------"; }

echo "deploying the step-5a dry-run fix (master @ ${WANT_COMMIT})"
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

echo "[2/4] PROVING the built artefact carries the fix — never trust the build's success line"
ssh -o BatchMode=yes "$BASTION" '
  B=~/llm-perp-xrpl/orchestrator/target/release/perp-dex-orchestrator
  N=$(strings -a "$B" | grep -c "step 5a")
  echo "  \"step 5a\" strings in the binary: $N"
  [ "$N" -gt 0 ] || { echo "  FAILED: the binary does NOT contain the fix"; exit 3; }
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
    N=\$(strings -a /home/azureuser/perp/perp-dex-orchestrator 2>/dev/null | grep -c \"step 5a\")
    A=\$(systemctl is-active perp-dex-orchestrator 2>/dev/null)
    echo \"service=\$A  step-5a-strings=\$N\"'"
done
hr
echo "If every node reports service=active and step-5a-strings>0, re-run the dry run:"
echo "    bash orchestrator/scripts/govern-b18-and-dryrun.sh"
echo "The allowlist step will report entries=1 already and is idempotent. Read BOTH \`status\`"
echo "and \`boot_proof\` in the [5/5] line: \"dry-run-ok\" is now the only PASS and it requires"
echo "the 5a boot; \"dry-run-boot-failed\" means the boot was attempted and judged a failure."
