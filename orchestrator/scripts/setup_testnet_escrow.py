#!/usr/bin/env python3
"""Set up fresh XRPL testnet escrow with SignerListSet for P2P signing relay test.

The seed is written to ~/.secrets/perp-dex-xrpl/escrow-testnet.json (0600) — the
canonical home for the testnet escrow seed of THIS project. Without this file
the testnet enclave-bump procedure (docs/testnet-enclave-bump-procedure.{en,ru}.md §7)
cannot run. The path is namespaced by project (perp-dex-xrpl/) because Hetzner
hosts multiple users and projects; an unnamespaced "multisig_escrow_testnet.json"
is ambiguous a year later. The mainnet file should be renamed to the same
pattern (escrow-mainnet.json under the same project dir) at next opportunity.

Pass --signer name=raddress three times (one per node); the script bails if
fewer or more are given so you don't publish a stale address set.
"""
import argparse
import json
import os
import stat
import sys
from pathlib import Path

from xrpl.clients import JsonRpcClient
from xrpl.wallet import generate_faucet_wallet
from xrpl.models.transactions import SignerListSet, AccountSet
from xrpl.models.transactions.signer_list_set import SignerEntry
from xrpl.transaction import submit_and_wait
from xrpl.models.requests import AccountObjects

XRPL_TESTNET = "https://s.altnet.rippletest.net:51234"
SEED_FILE = Path.home() / ".secrets" / "perp-dex-xrpl" / "escrow-testnet.json"

ap = argparse.ArgumentParser()
ap.add_argument("--signer", action="append", metavar="name=raddress", default=[],
                help="Signer entry, e.g. --signer node-1=r4pm... (repeatable, requires 3)")
ap.add_argument("--quorum", type=int, default=2)
ap.add_argument("--seed-file", default=str(SEED_FILE),
                help=f"Where to persist the seed (default: {SEED_FILE})")
args = ap.parse_args()

if len(args.signer) != 3:
    sys.exit("error: pass --signer three times (one per node) — refusing to run with a stale default set")
SIGNERS = []
for s in args.signer:
    if "=" not in s:
        sys.exit(f"error: --signer expected name=raddress, got {s!r}")
    name, addr = s.split("=", 1)
    SIGNERS.append({"name": name.strip(), "xrpl_address": addr.strip()})
QUORUM = args.quorum

seed_path = Path(args.seed_file)
if seed_path.exists():
    sys.exit(f"error: {seed_path} already exists — refusing to overwrite. Move it aside first.")
seed_path.parent.mkdir(parents=True, exist_ok=True)
os.chmod(seed_path.parent, 0o700)

client = JsonRpcClient(XRPL_TESTNET)

print("[1/4] Creating escrow via testnet faucet...")
escrow = generate_faucet_wallet(client, debug=False)
print(f"  Address: {escrow.classic_address}")
print(f"  Seed:    (written to {seed_path}, not echoed)")

print("\n[2/4] Submitting SignerListSet (2-of-3)...")
signer_entries = [SignerEntry(account=s["xrpl_address"], signer_weight=1) for s in SIGNERS]
sls = SignerListSet(
    account=escrow.classic_address,
    signer_quorum=QUORUM,
    signer_entries=signer_entries,
)
resp = submit_and_wait(sls, client, escrow)
status = resp.result.get("meta", {}).get("TransactionResult", "?")
print(f"  Status: {status}")
if status != "tesSUCCESS":
    print(f"  FAIL: {status}")
    exit(1)

print("\n[3/4] Disabling master key (AccountSet asfDisableMaster)...")
acset = AccountSet(
    account=escrow.classic_address,
    set_flag=4,  # asfDisableMaster
)
resp = submit_and_wait(acset, client, escrow)
status = resp.result.get("meta", {}).get("TransactionResult", "?")
print(f"  Status: {status}")

print("\n[4/4] Verifying on-chain...")
objs = client.request(AccountObjects(account=escrow.classic_address, type="signer_list"))
sl = objs.result.get("account_objects", [])
if sl:
    entries = sl[0].get("SignerEntries", [])
    print(f"  SignerList quorum={sl[0].get('SignerQuorum')}, entries={len(entries)}")
    for e in entries:
        se = e.get("SignerEntry", {})
        print(f"    {se.get('Account')} weight={se.get('SignerWeight')}")
else:
    print("  WARNING: no signer list found")

print(f"\n[done] Persisting seed to {seed_path} ...")
seed_path.write_text(json.dumps({
    "escrow_address": escrow.classic_address,
    "escrow_seed": escrow.seed,
    "quorum": QUORUM,
    "signers": SIGNERS,
}, indent=2))
os.chmod(seed_path, stat.S_IRUSR | stat.S_IWUSR)  # 0600
print(f"  written, mode 0600")

print(f"\n{'='*60}")
print(f"ESCROW_ADDRESS={escrow.classic_address}")
print(f"SEED_FILE={seed_path}")
print(f"Explorer: https://testnet.xrpl.org/accounts/{escrow.classic_address}")
