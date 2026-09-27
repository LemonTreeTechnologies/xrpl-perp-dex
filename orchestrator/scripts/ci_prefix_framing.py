#!/usr/bin/env python3
"""ci_prefix_framing.py — every producer of the shared attested-ledger prefix must route
through the framing helper.

WHY THIS EXISTS (RESP-wire-format-seam-class-ruling-2026-09-27, Q2).

XSPV, XDEP and XCLK share one prefix, and the enclave's `parse_attested_prefix` reads the
validations section as `pubkey33 | sig_len u8 | sig | vbody_len u16 | vbody`. The collector
buffers RAW STValidations. The conversion lives in `build_validations_section`, and on
2026-09-27 two of the three producers had never called it: the attested clock refused 100%
of ledgers live with -72, and the deposit path carried the identical defect unexercised.

The fix changed the builders' signature so the mistake is unrepresentable for today's three.
That is necessary and not sufficient: a future producer can assemble the prefix by hand and
never touch the helper. A grep-count of today's callers is point-in-time; this gate is the
durable form of it. Same reasoning as `-Werror=switch` on the enclave loader (a permissive
form makes an incomplete set look complete) and as ci_ecall_callers.py — BOTH, not either.

Runs anywhere for the producer-side checks. The cross-repo check needs the enclave tree and
SKIPS without it, like check-meta-sizes-vs-enclave.sh:
    ENCLAVE_REPO=/path/to/xrpl-perp-dex-enclave ./ci_prefix_framing.py
"""
import os
import re
import sys
from pathlib import Path

ORCH = Path(__file__).resolve().parent.parent
SRC = ORCH / "src"
SPV = SRC / "spv_proof.rs"
MAGIC_RE = re.compile(r'(?:b"(X[A-Z]{3})"|&?([A-Z]{4})_MAGIC)')

failures: list[str] = []
notes: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


def top_level_fn_body(text: str, header: str) -> str:
    """Body of a top-level fn, from its header to the closing brace in column 0."""
    i = text.index(header)
    j = text.index("\n}\n", i)
    return text[i:j]


src = SPV.read_text()

# ── 1. EMISSION LOCALITY ─────────────────────────────────────────────────────
# Writing the magic into a buffer is assembling the prefix. Asserting about a magic
# (`assert_eq!(&blob[..4], b"XDEP")`) is reading one, which is fine anywhere — so this
# keys on the emission, not on the mention.
for rs in sorted(SRC.rglob("*.rs")):
    if rs == SPV:
        continue
    for n, line in enumerate(rs.read_text().splitlines(), 1):
        stripped = line.strip()
        if stripped.startswith("//"):
            continue  # a commented-out emission emits nothing
        if "extend_from_slice" in line and MAGIC_RE.search(line):
            fail(
                f"{rs.relative_to(ORCH)}:{n} emits a prefix magic outside spv_proof.rs — "
                "the prefix is assembled in one place so that one place can frame the "
                "validations:\n      " + line.strip()
            )

# ── 2. every public builder frames, and takes the RAW blobs ──────────────────
public = sorted(set(re.findall(r"pub fn (build_x\w+_blob)\(", src)))
# Matches `pub fn` too, deliberately: keyed on `fn` alone, making the unframed form
# public dropped it from this list and the "is public" check below could never fire —
# the 1:1 check caught the mutation instead. A check that cannot fire is a check
# nobody can audit, so it either fires or it goes.
framed = sorted(set(re.findall(r"\n(?:pub )?fn (build_x\w+_blob_framed)\(", src)))
if not public:
    fail("no `pub fn build_x*_blob` found at all — has spv_proof.rs been restructured?")

for fn in public:
    body = top_level_fn_body(src, f"pub fn {fn}(")
    if "validations: &[Vec<u8>]" not in body:
        fail(
            f"{fn} does not take `validations: &[Vec<u8>]`. Taking pre-framed bytes plus a "
            "separate count is the signature that caused the live defect: either argument "
            "can be wrong independently of the other."
        )
    if "build_validations_section(validations)" not in body:
        fail(
            f"{fn} never calls build_validations_section(validations) — it is a producer "
            "that does not frame, which is exactly what refused every ledger on 2026-09-27."
        )

# ── 3. the helper stays unreachable from outside ─────────────────────────────
if "pub fn build_validations_section" in src:
    fail(
        "build_validations_section is pub again. Public, a caller can frame by hand and pass "
        "the result to a *_framed builder, which reopens the count/bytes disagreement."
    )
if "fn build_validations_section" not in src:
    fail("build_validations_section is gone — the framing step must live somewhere.")

# ── 4. the unframed form is private and reachable only from its wrapper ──────
for fn in framed:
    if f"pub fn {fn}(" in src:
        fail(f"{fn} is public. The unframed form exists only for the cross-language vectors.")
    wrapper = fn[: -len("_framed")]
    callers = {
        rs.relative_to(ORCH)
        for rs in SRC.rglob("*.rs")
        if rs != SPV and re.search(rf"\b{fn}\(", rs.read_text())
    }
    if callers:
        fail(f"{fn} is called from outside spv_proof.rs: {sorted(map(str, callers))}")
    if f"pub fn {wrapper}(" not in src:
        fail(f"{fn} has no public framing wrapper `{wrapper}` — a producer with no framing.")

# ── 5. public builders and unframed forms correspond 1:1 ─────────────────────
expected = sorted(f"{fn}_framed" for fn in public)
if expected != framed:
    fail(
        "public builders and unframed forms are not in 1:1 correspondence — a new producer "
        f"has appeared without one.\n      public: {public}\n      framed: {framed}"
    )

# ── 6. CROSS-REPO: every magic the enclave PARSES has a framing-routed producer ──
# The durable form of "is my enumeration complete?": the answer stops being a grep I ran
# once and becomes a thing the build asserts.
enclave = Path(os.environ.get("ENCLAVE_REPO", Path.home() / "xrpl-perp-dex-enclave"))
parser = enclave / "EthSignerEnclave" / "Enclave" / "xrpl_spv.cpp"
if not parser.is_file():
    notes.append(
        f"SKIP cross-repo check: {parser} not found. Set ENCLAVE_REPO to the enclave "
        "checkout to assert every magic the enclave parses has a framing-routed producer."
    )
else:
    parsed = sorted(
        set(re.findall(r'parse_attested_prefix\([^;]*?"(X[A-Z]{3})"', parser.read_text()))
    )
    if not parsed:
        fail(f"found no parse_attested_prefix magics in {parser} — has the parser moved?")
    # A public builder is a thin wrapper; the magic is written by the *_framed form it
    # calls. Follow the call rather than reading only the wrapper — reading the wrapper
    # found nothing and reported every magic as unproduced, i.e. a gate that cannot
    # discriminate. It failed closed, which is the right direction, but a gate whose
    # verdict does not track its subject is not a gate.
    produced: set[str] = set()
    for fn in public:
        body = top_level_fn_body(src, f"pub fn {fn}(")
        for decl in (f"fn {fn}_framed(", f"pub fn {fn}_framed("):
            if f"{fn}_framed(" in body and f"\n{decl}" in src:
                body += top_level_fn_body(src, decl)
                break
        for a, b in MAGIC_RE.findall(body):
            produced.add(a or b)
    missing = [m for m in parsed if m not in produced]
    for magic in missing:
        fail(
            f"the enclave parses {magic} but no framing-routed public builder emits it. "
            "A prefix the enclave accepts with no framing producer is a path that will "
            "refuse everything the day it is switched on — that was XDEP."
        )
    if not missing:
        notes.append(f"cross-repo OK: enclave parses {parsed}, all framing-routed here.")

for n in notes:
    print(f"  {n}")
if failures:
    print("\nPREFIX FRAMING GATE FAILED\n")
    for f in failures:
        print(f"  - {f}")
    print(
        "\nThe shared prefix has one framing step and every producer must go through it.\n"
        "See docs/audit/REQ-wire-format-seam.md in the enclave repo."
    )
    sys.exit(1)
print(f"prefix framing gate OK — {len(public)} producers, all framing-routed: {public}")
