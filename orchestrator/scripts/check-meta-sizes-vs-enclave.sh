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
LIMITS_HDR="$ENCLAVE_REPO/EthSignerEnclave/Enclave/EnclaveLimits.h"
PATHA_HDR="$ENCLAVE_REPO/EthSignerEnclave/Enclave/PathA.h"
PATHA_LIMITS_HDR="$ENCLAVE_REPO/EthSignerEnclave/Enclave/PathALimits.h"
ENCLAVE_CFG="$ENCLAVE_REPO/EthSignerEnclave/Enclave/Enclave.config.xml"

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
checked=0
check() { # label  enclave-type  orch-const
  local e o
  checked=$((checked + 1))
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
check "β12" "PerpMetaLegacyB12" "PERP_META_B12_LEN"
check "β14" "PerpMetaLegacyB14" "PERP_META_B14_LEN"
check "β15" "PerpMetaLegacyB15" "PERP_META_B15_LEN"
check "β16" "PerpMetaLegacyB16" "PERP_META_B16_LEN"
check "β17" "PerpMeta"          "PERP_META_B17_LEN"


# ── The CAPACITY limits, same reasoning one axis over ────────────────────────
#
# The meta sizes were mirrored and drifted, so this gate exists. The four capacity
# limits in the same `limits` module are mirrored the SAME way and nothing checked
# them: they happen to agree today, which is not a property anything maintains. A
# drift here is worse than the meta one — the preflight would clear a migration the
# enclave then refuses, or clear one that overflows a fixed-size array.
echo
echo "capacity-limit drift gate — orch path_a_capacity.rs vs the enclave's own definitions"

enc_define() { grep -oP "define\s+$1\s+\K[0-9]+" "$2" | head -1; }
# The orch mirrors are written as EXPRESSIONS (`16 * 1024 * 1024`), so take the whole
# right-hand side and evaluate it. Reading only the first integer compared 16 against
# 16777216 and reported a drift that was not there — the extractor lying, not the code.
orch_expr() {
  local rhs
  rhs="$(grep -oP "$1: u64 = \K[^;]+" "$RS" | head -1 | tr -d "_")"
  [ -n "$rhs" ] && echo $(( rhs ))
}

check_val() { # label  expected-from-enclave  orch-const
  local o
  checked=$((checked + 1))
  o="$(orch_expr "$3")"
  if [ -z "$2" ] || [ -z "$o" ]; then
    echo "  MISSING $1: enclave='$2' orch($3)='$o'"; fail=1; return
  fi
  if [ "$2" != "$o" ]; then
    echo "  DRIFT   $1: enclave=$2  !=  orch($3)=$o"; fail=1; return
  fi
  echo "  OK  $1: $2"
}

check_val "perp files/shard" \
  "$(enc_define ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD "$LIMITS_HDR")" \
  "PERP_STATE_MAX_FILES_PER_SHARD"

check_val "manifest max files" \
  "$(enc_define PATH_A_MANIFEST_MAX_FILES "$PATHA_HDR")" \
  "MANIFEST_MAX_FILES"

# Written as `(16u * 1024u * 1024u)`, so evaluate rather than pattern-match a literal.
cipher_expr="$(grep -oP "define\s+PATH_A_EXPORT_CIPHER_BUF_SIZE\s+\K.*" "$PATHA_LIMITS_HDR" | head -1 | tr -d "u()" )"
check_val "export cipher buffer" \
  "$( [ -n "$cipher_expr" ] && echo $(( cipher_expr )) )" \
  "EXPORT_CIPHER_BUF_BYTES"

# HeapMaxSize is hex in the enclave XML; the orch mirror is decimal bytes.
heap_hex="$(grep -oP "<HeapMaxSize>\K[^<]+" "$ENCLAVE_CFG" | head -1)"
check_val "enclave heap max" \
  "$( [ -n "$heap_hex" ] && printf "%d" "$heap_hex" )" \
  "ENCLAVE_HEAP_MAX_BYTES"

if [ "$fail" -ne 0 ]; then
  echo "============================================================"
  echo "DRIFT — orch preflight constants disagree with the enclave."
  echo "Update orchestrator/src/path_a_capacity.rs limits to match the enclave headers."
  echo "A preflight computed from stale limits clears migrations the enclave refuses."
  echo "============================================================"
  exit 1
fi
echo
echo "drift gate OK — all $checked mirrored constants match the enclave."
