#!/usr/bin/env python3
"""Operator-level failure tests for 2-of-3 multisig on XRPL testnet.

Run from Hetzner with SSH tunnels:
  localhost:9091 -> node-1 enclave
  localhost:9092 -> node-2 enclave
  localhost:9093 -> node-3 enclave
"""

import json
import hashlib
import urllib.request
import ssl
import time
import sys

XRPL_URL = "https://s.altnet.rippletest.net:51234"
ESCROW_ADDR = "rKHSwKNpaoAN8kFxsrp3ZBhytoPt21hiB2"
TEST_DEST = "rHSLZoUH1b7FW83tbCkL1nkzVtV68s7zDC"

SIGNERS = [
    {
        "name": "node-1",
        "enclave_url": "https://localhost:9091/v1",
        "address": "0xe3d8fd1c7505a446689811be39b4f96c95429a5c",
        "session_key": "0xb69e6802e6bbb59be8dbf5297b25d8fd75ed9c57563c8b0143028e64f6d7f53e",
        "compressed_pubkey": "02317d9b19bf81630024b55bf273f8279a81535f5b67cbc2e66a1fdce9300a7463",
        "xrpl_address": "r4pmNX1b4jHQUtbVKnxGuS6Mozy4abg59J",
    },
    {
        "name": "node-2",
        "enclave_url": "https://localhost:9092/v1",
        "address": "0xb656c3e5720fec6bb1b87e484c0ed7cfca720e78",
        "session_key": "0xc38f4d2f4d84107bc16ed824979ce14121758e98d00292e3d1249e6578e12e51",
        "compressed_pubkey": "0266c1e743846f6d2987108666b2fc31222dab5142f0c770f788e6862782ce2f5a",
        "xrpl_address": "rExSvwKDdVUnMB3wGDsqtjvRLNqU2PZBBd",
    },
    {
        "name": "node-3",
        "enclave_url": "https://localhost:9093/v1",
        "address": "0xb7b7c5f6140f695a7db7be87ffa91bd815cb5547",
        "session_key": "0x4d8e6e0110bada63256885607fc1c01f4d63b50355b1265de37ae01ded8f1b02",
        "compressed_pubkey": "0256e86bb0db9796c0a0ac8ed784f6edea449a95bf00bc12c540002bc7b966433d",
        "xrpl_address": "rBb8KCxQCC1qjaAfJF5PrQs5dJ8kPuqYxT",
    },
]

# XRPL base58 alphabet
ALPHABET = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz"

ssl_ctx = ssl.create_default_context()
ssl_ctx.check_hostname = False
ssl_ctx.verify_mode = ssl.CERT_NONE


def base58_decode(s):
    num = 0
    for c in s:
        num = num * 58 + ALPHABET.index(c)
    result = num.to_bytes((num.bit_length() + 7) // 8, 'big')
    pad = 0
    for c in s:
        if c == ALPHABET[0]:
            pad += 1
        else:
            break
    return b'\x00' * pad + result


def xrpl_address_to_account_id(addr):
    decoded = base58_decode(addr)
    return decoded[1:-4]  # strip version byte and 4-byte checksum


def xrpl_rpc(method, params):
    data = json.dumps({"method": method, "params": [params]}).encode()
    req = urllib.request.Request(XRPL_URL, data=data,
                                headers={"Content-Type": "application/json"})
    resp = urllib.request.urlopen(req, timeout=30, context=ssl_ctx)
    return json.loads(resp.read())["result"]


def get_sequence(account):
    result = xrpl_rpc("account_info", {"account": account})
    return result["account_data"]["Sequence"]


def get_balance(account):
    result = xrpl_rpc("account_info", {"account": account})
    return int(result["account_data"]["Balance"]) / 1_000_000


def sign_with_enclave(signer, hash_hex):
    """Call enclave /v1/pool/sign to get ECDSA signature on a hash."""
    url = f"{signer['enclave_url']}/pool/sign"
    data = json.dumps({
        "from": signer["address"],
        "hash": hash_hex,
        "session_key": signer["session_key"],
    }).encode()
    req = urllib.request.Request(url, data=data,
                                headers={"Content-Type": "application/json"})
    resp = urllib.request.urlopen(req, timeout=30, context=ssl_ctx)
    result = json.loads(resp.read())
    if result.get("status") != "success":
        raise Exception(f"Sign failed: {result}")
    r = result["signature"]["r"]
    s = result["signature"]["s"]
    return r, s


def der_encode(r_hex, s_hex):
    """DER encode an (r, s) ECDSA signature."""
    r_bytes = bytes.fromhex(r_hex)
    s_bytes = bytes.fromhex(s_hex)
    # Ensure positive integers (prepend 0x00 if high bit set)
    if r_bytes[0] & 0x80:
        r_bytes = b'\x00' + r_bytes
    if s_bytes[0] & 0x80:
        s_bytes = b'\x00' + s_bytes
    r_tlv = bytes([0x02, len(r_bytes)]) + r_bytes
    s_tlv = bytes([0x02, len(s_bytes)]) + s_bytes
    return bytes([0x30, len(r_tlv) + len(s_tlv)]) + r_tlv + s_tlv


def build_multisig_hash(tx_blob_hex, signer_account_id):
    """
    XRPL multi-signing hash = SHA-512Half(HashPrefix::txMultiSign + tx_blob + signer_account_id)
    HashPrefix::txMultiSign = 0x534D5400
    """
    prefix = bytes.fromhex("534D5400")
    tx_blob = bytes.fromhex(tx_blob_hex)
    data = prefix + tx_blob + signer_account_id
    h = hashlib.sha512(data).digest()
    return h[:32]


def serialize_payment(account, destination, amount_drops, fee, sequence):
    """
    Minimal XRPL binary serialization for a Payment transaction.
    This produces the canonical binary that XRPL expects for multisig hashing.
    We use xrpl-py for proper serialization.
    """
    from xrpl.core.binarycodec import encode
    tx = {
        "TransactionType": "Payment",
        "Account": account,
        "Destination": destination,
        "Amount": str(amount_drops),
        "Fee": str(fee),
        "Sequence": sequence,
        "SigningPubKey": "",
    }
    return encode(tx)


def submit_multisigned(tx_json):
    result = xrpl_rpc("submit_multisigned", {"tx_json": tx_json})
    return result


def collect_signatures(tx_blob_hex, available_signers, quorum=2):
    """Collect signatures from available signers. Returns list of Signer objects."""
    collected = []
    for signer in available_signers:
        if len(collected) >= quorum:
            break
        account_id = xrpl_address_to_account_id(signer["xrpl_address"])
        hash_bytes = build_multisig_hash(tx_blob_hex, account_id)
        hash_hex = "0x" + hash_bytes.hex()

        try:
            r, s = sign_with_enclave(signer, hash_hex)
            der = der_encode(r, s)
            collected.append({
                "Signer": {
                    "Account": signer["xrpl_address"],
                    "SigningPubKey": signer["compressed_pubkey"].upper(),
                    "TxnSignature": der.hex().upper(),
                }
            })
            print(f"  Collected signature from {signer['name']}")
        except Exception as e:
            print(f"  FAILED to sign with {signer['name']}: {e}")

    # Sort by AccountID
    collected.sort(key=lambda x: xrpl_address_to_account_id(x["Signer"]["Account"]))
    return collected


def test_withdrawal(label, available_signers, amount_xrp=1, expect_success=True):
    """Test a multisig withdrawal with given set of available signers."""
    print(f"\n{'='*60}")
    print(f"TEST: {label}")
    print(f"Available signers: {[s['name'] for s in available_signers]}")
    print(f"Amount: {amount_xrp} XRP, Quorum: 2")
    print(f"{'='*60}")

    amount_drops = int(amount_xrp * 1_000_000)
    fee = 36  # 12 * (1 + 2 signers)
    sequence = get_sequence(ESCROW_ADDR)

    print(f"Escrow sequence: {sequence}")

    # Serialize the unsigned tx
    tx_blob_hex = serialize_payment(ESCROW_ADDR, TEST_DEST, amount_drops, fee, sequence)
    print(f"Tx blob: {tx_blob_hex[:40]}...")

    # Collect signatures
    sigs = collect_signatures(tx_blob_hex, available_signers)
    print(f"Signatures collected: {len(sigs)}/2")

    if len(sigs) < 2:
        if not expect_success:
            print(f"RESULT: PASSED — correctly failed to collect quorum ({len(sigs)}/2)")
            return True
        else:
            print(f"RESULT: FAILED — expected success but only got {len(sigs)} signatures")
            return False

    # Build full tx with Signers
    tx_json = {
        "TransactionType": "Payment",
        "Account": ESCROW_ADDR,
        "Destination": TEST_DEST,
        "Amount": str(amount_drops),
        "Fee": str(fee),
        "Sequence": sequence,
        "SigningPubKey": "",
        "Signers": sigs,
    }

    # Submit
    print("Submitting to XRPL...")
    result = submit_multisigned(tx_json)
    engine_result = result.get("engine_result", "unknown")
    tx_hash = result.get("tx_json", {}).get("hash", "unknown")

    if engine_result in ("tesSUCCESS", "terQUEUED"):
        if expect_success:
            print(f"RESULT: PASSED — {engine_result}, hash: {tx_hash}")
            return True
        else:
            print(f"RESULT: FAILED — expected failure but got {engine_result}")
            return False
    else:
        msg = result.get("engine_result_message", "")
        if not expect_success:
            print(f"RESULT: PASSED — correctly rejected: {engine_result} ({msg})")
            return True
        else:
            print(f"RESULT: FAILED — {engine_result}: {msg}")
            print(f"Full result: {json.dumps(result, indent=2)}")
            return False


def test_malicious_signer(available_signers):
    """Test that a bad signature from one signer is rejected by XRPL."""
    print(f"\n{'='*60}")
    print(f"TEST: Malicious signer (corrupted signature)")
    print(f"{'='*60}")

    amount_drops = 1_000_000  # 1 XRP
    fee = 36
    sequence = get_sequence(ESCROW_ADDR)

    tx_blob_hex = serialize_payment(ESCROW_ADDR, TEST_DEST, amount_drops, fee, sequence)

    # Get one good signature from node-1
    signer1 = available_signers[0]
    account_id1 = xrpl_address_to_account_id(signer1["xrpl_address"])
    hash1 = build_multisig_hash(tx_blob_hex, account_id1)
    r1, s1 = sign_with_enclave(signer1, "0x" + hash1.hex())
    der1 = der_encode(r1, s1)
    print(f"  Good signature from {signer1['name']}")

    # Create a BAD signature for node-2 (corrupt the DER)
    signer2 = available_signers[1]
    bad_der = b'\x30\x44\x02\x20' + b'\xDE\xAD' * 16 + b'\x02\x20' + b'\xBE\xEF' * 16
    print(f"  Corrupted signature for {signer2['name']}")

    sigs = sorted([
        {
            "Signer": {
                "Account": signer1["xrpl_address"],
                "SigningPubKey": signer1["compressed_pubkey"].upper(),
                "TxnSignature": der1.hex().upper(),
            }
        },
        {
            "Signer": {
                "Account": signer2["xrpl_address"],
                "SigningPubKey": signer2["compressed_pubkey"].upper(),
                "TxnSignature": bad_der.hex().upper(),
            }
        },
    ], key=lambda x: xrpl_address_to_account_id(x["Signer"]["Account"]))

    tx_json = {
        "TransactionType": "Payment",
        "Account": ESCROW_ADDR,
        "Destination": TEST_DEST,
        "Amount": str(amount_drops),
        "Fee": str(fee),
        "Sequence": sequence,
        "SigningPubKey": "",
        "Signers": sigs,
    }

    print("Submitting with one bad signature...")
    result = submit_multisigned(tx_json)
    engine_result = result.get("engine_result", "unknown")
    msg = result.get("engine_result_message", "")

    # XRPL should reject this (bad signature = invalid)
    if engine_result not in ("tesSUCCESS", "terQUEUED"):
        print(f"RESULT: PASSED — XRPL rejected bad sig: {engine_result} ({msg})")
        return True
    else:
        print(f"RESULT: FAILED — XRPL accepted a corrupted signature?!")
        return False


if __name__ == "__main__":
    print("=" * 60)
    print("OPERATOR-LEVEL FAILURE TESTS")
    print(f"Escrow: {ESCROW_ADDR}")
    print(f"Balance: {get_balance(ESCROW_ADDR)} XRP")
    print("=" * 60)

    results = {}

    # Test 1: Normal 2-of-3 (all signers available)
    results["1. Normal 2-of-3 withdrawal"] = test_withdrawal(
        "Normal 2-of-3 withdrawal (all signers available)",
        SIGNERS, amount_xrp=1, expect_success=True
    )

    # Test 2: One operator offline (node-3 down, node-1+2 sign)
    results["2. One operator offline (2-of-3)"] = test_withdrawal(
        "One operator offline — node-3 unavailable",
        [SIGNERS[0], SIGNERS[1]], amount_xrp=1, expect_success=True
    )

    # Test 3: Two operators offline (only node-1, can't reach quorum)
    results["3. Two operators offline (1-of-3)"] = test_withdrawal(
        "Two operators offline — only node-1 available",
        [SIGNERS[0]], amount_xrp=1, expect_success=False
    )

    # Test 4: Malicious signer (corrupted signature)
    results["4. Malicious signer"] = test_malicious_signer(SIGNERS)

    # Test 5: Different 2-of-3 combination (node-1+3, node-2 down)
    results["5. Alternative 2-of-3 (node-1+3)"] = test_withdrawal(
        "Alternative signing pair — node-2 down, node-1+3 sign",
        [SIGNERS[0], SIGNERS[2]], amount_xrp=1, expect_success=True
    )

    # Test 6: Another 2-of-3 combination (node-2+3, node-1 down)
    results["6. Alternative 2-of-3 (node-2+3)"] = test_withdrawal(
        "Alternative signing pair — node-1 down, node-2+3 sign",
        [SIGNERS[1], SIGNERS[2]], amount_xrp=1, expect_success=True
    )

    # Summary
    print(f"\n{'='*60}")
    print("SUMMARY")
    print(f"{'='*60}")
    for name, passed in results.items():
        status = "PASSED" if passed else "FAILED"
        print(f"  [{status}] {name}")

    total = len(results)
    passed = sum(1 for v in results.values() if v)
    print(f"\n{passed}/{total} tests passed")

    sys.exit(0 if passed == total else 1)
