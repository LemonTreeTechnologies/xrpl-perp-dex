#!/usr/bin/env bash
# govern-b18-remote.sh — the half that runs ON THE BASTION. Shipped there by
# govern-b18-and-dryrun.sh; not meant to be run from a laptop.
#
# Split into its own file deliberately: the first version embedded this in a heredoc inside a
# function inside the outer script, and three levels of nesting is what has broken every
# hand-written command for this operation. Two plain files have no nesting at all.
#
# Usage: govern-b18-remote.sh <subcommand> <mrenclave-hex> <add|remove>
set -uo pipefail

CMD="${1:?subcommand}"
MRENCLAVE="${2:?mrenclave hex}"
OP="${3:-add}"
NODES="20.71.184.176 20.224.243.60 52.236.130.102"
N1="20.71.184.176"

on() { ssh -o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=no "azureuser@$1" "$2" 2>/dev/null; }

case "$CMD" in
  allowlist)
    for ip in $NODES; do
      printf '  %-16s ' "$ip"
      on "$ip" "curl -sk --max-time 8 https://127.0.0.1:9088/v1/admin/mrenclaves/status" \
        | python3 -c 'import sys,json;d=json.load(sys.stdin);print("entries=%s epoch=%s digest=%s" % (d.get("entry_count"),d.get("allowlist_epoch"),str(d.get("allowlist_digest"))[:16]))' \
        2>/dev/null || echo "unreadable"
    done
    ;;
  new_measurements)
    for ip in $NODES; do
      printf '%s ' "$ip"
      on "$ip" "curl -sk --max-time 8 https://127.0.0.1:9089/version" \
        | python3 -c 'import sys,json;print(json.load(sys.stdin).get("mrenclave",""))' 2>/dev/null \
        || echo "NO-ANSWER-9089"
    done
    ;;
  govern)
    # The payload is written to a FILE on the node and curl reads it with -d @file. An inline
    # JSON string through two ssh hops loses its quotes; that failure produced
    # "Failed to parse the request body as JSON" against a perfectly good endpoint.
    on "$N1" "printf '%s' '{\"op\":\"$OP\",\"mrenclave\":\"$MRENCLAVE\"}' > /tmp/govern.json"
    printf '  payload on node: '
    on "$N1" "cat /tmp/govern.json"
    printf '\n  response: '
    on "$N1" "curl -sS --max-time 280 -X POST http://127.0.0.1:9102/admin/mrenclave-govern -H 'Content-Type: application/json' -d @/tmp/govern.json"
    printf '\n'
    ;;
  dryrun)
    on "$N1" "printf '%s' '{\"expected_mrenclave_new\":\"$MRENCLAVE\",\"old_api_base\":\"https://localhost:9088\",\"new_api_base\":\"https://localhost:9089\",\"delegation_timeout_secs\":120,\"dry_run\":true}' > /tmp/dryrun.json"
    printf '  response: '
    on "$N1" "curl -sS --max-time 280 -X POST http://127.0.0.1:7095/admin/migrate-state -H 'Content-Type: application/json' -d @/tmp/dryrun.json"
    printf '\n'
    ;;
  *) echo "unknown subcommand: $CMD" >&2; exit 2 ;;
esac
