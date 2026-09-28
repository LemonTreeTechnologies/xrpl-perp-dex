#!/usr/bin/env python3
"""ci_one_quorum_bundle_producer.py — the quorum-bundle bytes get written in ONE place.

WHY. Four functions in this crate emitted the quorum-bundle wire format, against exactly ONE
consumer: `seal_verify_quorum_bundle_with_set` inside the enclave. They agreed byte for byte —
verified before consolidating them — but only because each was copied carefully, and nothing
maintained that.

This project has already paid for that shape once, the same week: the SPV attested-prefix framing
had three producers against one parser, TWO of the three never framed the section, each tested
against its own idea of the format, everything green, the defect live in the field. The lesson
recorded then was to COUNT THE PRODUCERS. This gate is that count, mechanised, because a comment
asking people to reuse a helper is exactly the kind of convention a hurried patch steps around.

WHAT IT REJECTS: any file other than src/quorum_bundle.rs that writes the format's header. The
header is `version = 1u32` little-endian followed by a count, so `1u32.to_le_bytes()` outside the
canonical module is the signature of a new producer. Assertions in tests are allowed — a test
checking the bytes is a CONSUMER of the format, which is the thing we want more of, not less.

Exit 0 = one producer. Exit 1 = a new one appeared, with the file and line named.
"""
import pathlib
import re
import subprocess
import sys

CANONICAL = "src/quorum_bundle.rs"
HEADER_WRITE = re.compile(r"\b(?:out|buf|bytes|v)\s*\.\s*extend_from_slice\(\s*&\s*1u32\s*\.\s*to_le_bytes\(\)")

root = pathlib.Path(__file__).resolve().parent.parent
tracked = subprocess.run(["git", "ls-files", "src"], cwd=root, capture_output=True, text=True).stdout.split()

offenders = []
for rel in tracked:
    if not rel.endswith(".rs") or rel == CANONICAL:
        continue
    p = root / rel
    if not p.is_file():
        continue
    for n, line in enumerate(p.read_text().splitlines(), 1):
        if "assert" in line:
            continue  # a test reading the bytes is a consumer, not a producer
        if HEADER_WRITE.search(line):
            offenders.append(f"{rel}:{n}: {line.strip()}")

if not (root / CANONICAL).is_file():
    print(f"FAILED — {CANONICAL} is missing. The canonical producer cannot vanish: this gate "
          f"passing while it is gone would be the check silently disarming itself.", file=sys.stderr)
    sys.exit(1)

if offenders:
    print("FAILED — the quorum-bundle header is written outside " + CANONICAL + ":", file=sys.stderr)
    for o in offenders:
        print("  " + o, file=sys.stderr)
    print("\nThere is ONE consumer of this format (seal_verify_quorum_bundle_with_set in the\n"
          "enclave). Call crate::quorum_bundle::build instead; if the format itself must change,\n"
          "change it there and in the enclave parser together.", file=sys.stderr)
    sys.exit(1)

print(f"OK — the quorum-bundle bytes are written only in {CANONICAL}.")
