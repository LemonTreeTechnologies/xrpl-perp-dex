#!/usr/bin/env python3
"""Set up fresh XRPL testnet escrow with SignerListSet for P2P signing relay test."""
import json
from xrpl.clients import JsonRpcClient
from xrpl.wallet import generate_faucet_wallet
from xrpl.models.transactions import SignerListSet, AccountSet
from xrpl.models.transactions.signer_list_set import SignerEntry
from xrpl.transaction import submit_and_wait
from xrpl.models.requests import AccountObjects

XRPL_TESTNET = "https://s.altnet.rippletest.net:51234"

SIGNERS = [
    {"name": "node-1", "xrpl_address": "r4pmNX1b4jHQUtbVKnxGuS6Mozy4abg59J"},
    {"name": "node-2", "xrpl_address": "rExSvwKDdVUnMB3wGDsqtjvRLNqU2PZBBd"},
    {"name": "node-3", "xrpl_address": "rBb8KCxQCC1qjaAfJF5PrQs5dJ8kPuqYxT"},
]
QUORUM = 2

client = JsonRpcClient(XRPL_TESTNET)

print("[1/4] Creating escrow via testnet faucet...")
escrow = generate_faucet_wallet(client, debug=False)
print(f"  Address: {escrow.classic_address}")
print(f"  Seed:    {escrow.seed}")

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

print(f"\n{'='*60}")
print(f"ESCROW_ADDRESS={escrow.classic_address}")
print(f"ESCROW_SEED={escrow.seed}")
print(f"Explorer: https://testnet.xrpl.org/accounts/{escrow.classic_address}")
