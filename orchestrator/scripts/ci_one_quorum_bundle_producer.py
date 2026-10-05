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
canonical module is the signature of a new producer.

`#[cfg(test)]` modules are EXEMPT, and that is exact rather than lenient: test code does not
compile into the binary, so it cannot be a production producer, while a golden test must spell
the expected bytes out literally or it is only asserting the builder equals itself.

That exemption used to be "a line containing `assert`", which this gate's first actual run showed
to be unreachable in practice — a golden test builds the bytes on one line and compares on the
next, so the building line carried no `assert` and was flagged. It also skipped any production
line that merely mentioned `debug_assert`. Both directions wrong, and invisible, because the gate
had never executed: it was merged with a path relative to the wrong directory, exited 127, and
did so during the Actions-quota outage when nothing ran to report it.

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
exempt_regions = []
exempt_lines = [0]
for rel in tracked:
    if not rel.endswith(".rs") or rel == CANONICAL:
        continue
    p = root / rel
    if not p.is_file():
        continue
    # Track `#[cfg(test)]` module extent by brace depth. Deliberately conservative: the
    # exemption only begins at a `mod ... {` that a `#[cfg(test)]` attribute immediately
    # precedes, and ends when depth returns to where it started. Anything the tracker fails
    # to recognise stays NON-exempt, so a tracking slip over-reports rather than under-.
    depth = 0
    pending_cfg_test = False
    test_depth = None
    for n, line in enumerate(p.read_text().splitlines(), 1):
        stripped = line.strip()
        if test_depth is None:
            if stripped.startswith("#[cfg(test)]"):
                pending_cfg_test = True
            elif pending_cfg_test and stripped:
                if re.match(r"^(pub\s+)?mod\s+\w+\s*\{", stripped):
                    test_depth = depth
                    exempt_regions.append(f"{rel}:{n}")
                    exempt_lines[0] += 1
                pending_cfg_test = False
        opened = line.count("{") - line.count("}")
        in_test = test_depth is not None
        if in_test:
            exempt_lines[0] += 1
        if not in_test and HEADER_WRITE.search(line):
            offenders.append(f"{rel}:{n}: {stripped}")
        depth += opened
        if test_depth is not None and depth <= test_depth:
            test_depth = None

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
# Printed so the exemption's reach is visible. An exemption nobody can see is one that grows,
# and this one was wrong for a week without anybody being able to tell.
if exempt_regions:
    # The COUNT, not the list: fifty file:line pairs is noise nobody reads, and an
    # exemption nobody reads is the thing this line exists to prevent. The line total is
    # the diagnostic — a tracking slip that ran a test module to end-of-file would show up
    # here as a number far larger than the test code actually present.
    print(f"  ({exempt_regions and len(exempt_regions)} #[cfg(test)] modules exempt, "
          f"{exempt_lines[0]} lines not scanned; `--list-exempt` to see them)")
if "--list-exempt" in sys.argv:
    for r in exempt_regions:
        print(f"    {r}")
