#!/usr/bin/env bash
# check-meta-sizes-vs-enclave.sh — AC-E1-3 drift gate (RESP-migrate-preflight-goldensizes Q-PF3).
#
# The orch migrate-preflight (path_a_capacity.rs) MIRRORS the frozen perp-meta plaintext
# sizes from the enclave `perp_meta_schema.h` static_asserts. That mirror silently lagged
# the enclave across TWO schema bumps (β9, β10) and surfaced as a false-STOP under
# live-migration pressure. A lockstep comment did not prevent it — so this script asserts,
# mechanically, that the four orch constants EQUAL the enclave static_asserts, and FAILS on
# any drift BEFORE it can reach a live migration.
#
# The enclave header lives in a DIFFERENT repo, so this runs where both trees are checked
# out: run it before a Path-A migration (and it can be wired into any CI that has both).
#   ENCLAVE_REPO=/path/to/xrpl-perp-dex-enclave ./check-meta-sizes-vs-enclave.sh
# Default enclave path: $ENCLAVE_REPO, else ~/xrpl-perp-dex-enclave.
set -euo pipefail

ORCH_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RS="$ORCH_DIR/src/path_a_capacity.rs"
ENCLAVE_REPO="${ENCLAVE_REPO:-$HOME/xrpl-perp-dex-enclave}"
HDR="$ENCLAVE_REPO/EthSignerEnclave/Enclave/perp_meta_schema.h"

if [ ! -f "$HDR" ]; then
  echo "SKIP: enclave header not found at $HDR (set ENCLAVE_REPO to the enclave checkout)."
  echo "      This drift gate needs both repos; run it before a migration / where both are present."
  exit 0
fi

# Extract `static_assert(sizeof(<TYPE>) == <N>, ...)` from the enclave header.
enc_size() { grep -oP "static_assert\(sizeof\($1\) == \K[0-9]+" "$HDR" | head -1; }
# Extract `<CONST>: u64 = <N>;` from the orch mirror.
orch_size() { grep -oP "$1: u64 = \K[0-9]+" "$RS" | head -1; }

fail=0
check() { # label  enclave-type  orch-const
  local e o
  e="$(enc_size "$2")"; o="$(orch_size "$3")"
  if [ -z "$e" ] || [ -z "$o" ]; then
    echo "  MISSING $1: enclave($2)='$e' orch($3)='$o'"; fail=1; return
  fi
  if [ "$e" != "$o" ]; then
    echo "  DRIFT   $1: enclave($2)=$e  !=  orch($3)=$o"; fail=1; return
  fi
  echo "  OK  $1: $e"
}

echo "AC-E1-3 meta-size drift gate — orch path_a_capacity.rs vs enclave perp_meta_schema.h"
check "β7"  "PerpMetaLegacyB7"  "PERP_META_LEGACY_B7_LEN"
check "β8"  "PerpMetaLegacyB8"  "PERP_META_B8_LEN"
check "β9"  "PerpMetaLegacyB9"  "PERP_META_B9_LEN"
check "β10" "PerpMetaLegacyB10" "PERP_META_B10_LEN"
check "β12" "PerpMeta"          "PERP_META_B12_LEN"

if [ "$fail" -ne 0 ]; then
  echo "============================================================"
  echo "META-SIZE DRIFT — orch preflight golden sizes disagree with the enclave freeze."
  echo "Update orchestrator/src/path_a_capacity.rs limits to match perp_meta_schema.h."
  echo "============================================================"
  exit 1
fi
echo "meta-size drift gate OK — all four golden sizes match the enclave static_asserts."
