#!/usr/bin/env python3
"""
XRPL Authentication utility for Perp DEX Trading API.

Generates auth headers (X-XRPL-Address, X-XRPL-PublicKey, X-XRPL-Signature)
from an XRPL wallet seed/secret.

Usage as library:
    from xrpl_auth import XRPLAuth
    auth = XRPLAuth("sEdV...secret...")
    headers = auth.sign_body('{"user_id":"rXXX","side":"buy","size":"100"}')
    requests.post(url, headers=headers, data=body)

Usage as CLI:
    # Sign a POST body
    python3 xrpl_auth.py --secret sEdV... --body '{"user_id":"rXXX","side":"buy"}'

    # Sign a GET path
    python3 xrpl_auth.py --secret sEdV... --path '/v1/orders?user_id=rXXX'

    # Generate new wallet + show headers for a test order
    python3 xrpl_auth.py --generate

    # Full request example with curl
    python3 xrpl_auth.py --secret sEdV... --curl POST http://YOUR_SERVER:3000/v1/orders \
        '{"user_id":"rXXX","side":"buy","type":"limit","price":"0.55","size":"100","leverage":5}'
"""

import argparse
import hashlib
import json
import sys

from ecdsa import SECP256k1, SigningKey
from ecdsa.util import sigencode_der, sigdecode_der


class XRPLAuth:
    """Generate XRPL signature auth headers for Perp DEX API."""

    def __init__(self, secret: str = None, private_key_hex: str = None):
        """
        Initialize from XRPL wallet secret (sEdV...) or raw private key hex.

        Args:
            secret: XRPL wallet secret/seed (sEdV... or s...)
            private_key_hex: 64-char hex private key (alternative to secret)
        """
        if secret:
            from xrpl.core.keypairs import derive_keypair, derive_classic_address
            pub, priv = derive_keypair(secret)
            self._private_key_hex = priv
            self._public_key_hex = pub.upper()  # compressed, uppercase
            self._address = derive_classic_address(pub)
        elif private_key_hex:
            pk = private_key_hex
            if len(pk) == 66:
                pk = pk[2:]
            sk = SigningKey.from_string(bytes.fromhex(pk), curve=SECP256k1)
            vk = sk.get_verifying_key()
            x = vk.pubkey.point.x()
            y = vk.pubkey.point.y()
            prefix = b'\x02' if y % 2 == 0 else b'\x03'
            compressed = prefix + x.to_bytes(32, 'big')
            self._private_key_hex = private_key_hex
            self._public_key_hex = compressed.hex().upper()
            from xrpl.core.keypairs import derive_classic_address
            self._address = derive_classic_address(self._public_key_hex)
        else:
            raise ValueError("Provide either secret or private_key_hex")

        # Build SigningKey from private key hex
        pk_hex = self._private_key_hex
        # XRPL secp256k1 private key: 00 + 32 bytes = 66 hex chars
        if len(pk_hex) == 66 and pk_hex[:2] == "00":
            pk_hex = pk_hex[2:]
        self._sk = SigningKey.from_string(bytes.fromhex(pk_hex), curve=SECP256k1)

    @property
    def address(self) -> str:
        return self._address

    @property
    def public_key(self) -> str:
        """Compressed public key, lowercase hex, 66 chars."""
        return self._public_key_hex.lower()

    def _sign_hash(self, hash_bytes: bytes) -> bytes:
        """Sign hash and normalize to low-S (required by k256/libsecp256k1)."""
        signature = self._sk.sign_digest(hash_bytes, sigencode=sigencode_der)
        # Decode DER to (r, s)
        r, s = sigdecode_der(signature, SECP256k1.order)
        # Normalize S to low-S
        half_order = SECP256k1.order // 2
        if s > half_order:
            s = SECP256k1.order - s
        # Re-encode as DER
        return sigencode_der(r, s, SECP256k1.order)

    def sign_body(self, body: str) -> dict:
        """
        Sign a request body (for POST/DELETE).
        Returns dict with auth headers.
        """
        body_bytes = body.encode('utf-8') if isinstance(body, str) else body
        hash_bytes = hashlib.sha256(body_bytes).digest()
        signature = self._sign_hash(hash_bytes)

        return {
            "X-XRPL-Address": self._address,
            "X-XRPL-PublicKey": self.public_key,
            "X-XRPL-Signature": signature.hex(),
        }

    def sign_path(self, path: str) -> dict:
        """
        Sign a URI path (for GET requests without body).
        Returns dict with auth headers.
        """
        hash_bytes = hashlib.sha256(path.encode('utf-8')).digest()
        signature = self._sign_hash(hash_bytes)

        return {
            "X-XRPL-Address": self._address,
            "X-XRPL-PublicKey": self.public_key,
            "X-XRPL-Signature": signature.hex(),
        }

    def make_request(self, method: str, url: str, body: str = None) -> dict:
        """
        Make an authenticated HTTP request.
        Returns response JSON.
        """
        import requests as req

        if method.upper() in ("POST", "DELETE", "PUT", "PATCH") and body:
            headers = self.sign_body(body)
            headers["Content-Type"] = "application/json"
            resp = req.request(method, url, headers=headers, data=body, timeout=10)
        else:
            from urllib.parse import urlparse
            parsed = urlparse(url)
            path = parsed.path
            if parsed.query:
                path += "?" + parsed.query
            headers = self.sign_path(path)
            resp = req.request(method, url, headers=headers, timeout=10)

        return resp.json()


def generate_wallet():
    """Generate a new XRPL wallet (secp256k1) for testing."""
    from xrpl.core.keypairs import generate_seed, derive_keypair, derive_classic_address
    from xrpl.constants import CryptoAlgorithm

    seed = generate_seed(algorithm=CryptoAlgorithm.SECP256K1)
    pub, priv = derive_keypair(seed)
    address = derive_classic_address(pub)

    return {
        "seed": seed,
        "private_key": priv,
        "public_key": pub,
        "address": address,
        "algorithm": "secp256k1",
    }


def main():
    parser = argparse.ArgumentParser(
        description="XRPL Auth utility for Perp DEX API",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Generate new wallet
  python3 xrpl_auth.py --generate

  # Sign a POST body
  python3 xrpl_auth.py --secret sEdV... --body '{"user_id":"rXXX","side":"buy","size":"100"}'

  # Sign a GET path
  python3 xrpl_auth.py --secret sEdV... --path '/v1/orders?user_id=rXXX'

  # Full curl command
  python3 xrpl_auth.py --secret sEdV... --curl POST http://YOUR_SERVER:3000/v1/orders \\
      '{"user_id":"rXXX","side":"buy","type":"limit","price":"0.55","size":"100","leverage":5}'
        """)
    parser.add_argument("--secret", help="XRPL wallet secret (sEdV... or s...)")
    parser.add_argument("--private-key", help="Raw private key hex (64 chars)")
    parser.add_argument("--body", help="JSON body to sign (for POST)")
    parser.add_argument("--path", help="URI path to sign (for GET)")
    parser.add_argument("--generate", action="store_true", help="Generate new XRPL wallet")
    parser.add_argument("--curl", nargs="+", metavar=("METHOD", "URL"),
                        help="Generate curl command: METHOD URL [BODY]")
    parser.add_argument("--request", nargs="+", metavar=("METHOD", "URL"),
                        help="Make actual request: METHOD URL [BODY]")
    args = parser.parse_args()

    if args.generate:
        wallet = generate_wallet()
        print(json.dumps(wallet, indent=2))
        print(f"\nUse: python3 xrpl_auth.py --secret {wallet['seed']} --body '...'")
        return

    if not args.secret and not args.private_key:
        parser.print_help()
        sys.exit(1)

    auth = XRPLAuth(secret=args.secret, private_key_hex=args.private_key)
    print(f"Address:    {auth.address}")
    print(f"Public Key: {auth.public_key}")

    if args.body:
        headers = auth.sign_body(args.body)
        print(f"\nHeaders for POST body:")
        for k, v in headers.items():
            print(f"  {k}: {v}")

    if args.path:
        headers = auth.sign_path(args.path)
        print(f"\nHeaders for GET {args.path}:")
        for k, v in headers.items():
            print(f"  {k}: {v}")

    if args.curl:
        method = args.curl[0]
        url = args.curl[1]
        body = args.curl[2] if len(args.curl) > 2 else None

        if body:
            # Ensure user_id matches
            try:
                body_json = json.loads(body)
                if "user_id" in body_json and body_json["user_id"] != auth.address:
                    body_json["user_id"] = auth.address
                    body = json.dumps(body_json)
                    print(f"\nNote: user_id replaced with {auth.address}")
            except json.JSONDecodeError:
                pass

            headers = auth.sign_body(body)
            print(f"\ncurl -X {method} {url} \\")
            print(f"  -H 'Content-Type: application/json' \\")
            for k, v in headers.items():
                print(f"  -H '{k}: {v}' \\")
            print(f"  -d '{body}'")
        else:
            from urllib.parse import urlparse
            parsed = urlparse(url)
            path = parsed.path
            if parsed.query:
                path += "?" + parsed.query
            headers = auth.sign_path(path)
            print(f"\ncurl -X {method} {url} \\")
            for k, v in headers.items():
                print(f"  -H '{k}: {v}' \\")
            print(f"  # (no body)")

    if args.request:
        method = args.request[0]
        url = args.request[1]
        body = args.request[2] if len(args.request) > 2 else None

        if body:
            try:
                body_json = json.loads(body)
                if "user_id" in body_json:
                    body_json["user_id"] = auth.address
                    body = json.dumps(body_json)
            except json.JSONDecodeError:
                pass

        print(f"\nMaking {method} {url}...")
        result = auth.make_request(method, url, body)
        print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
