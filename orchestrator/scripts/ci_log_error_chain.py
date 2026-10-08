#!/usr/bin/env python3
"""ci_log_error_chain.py — a logged error must print its WHOLE chain.

WHY THIS EXISTS. On 2026-10-08 the cluster was found to have refused its own
liabilities-root publication every hour for over a week — 166 times in 7 days — and every
one of those log lines read:

    reserves-commit skipped/failed: enclave reserves_commit (under-custody or signing error)

That text was not what the enclave said. It was a `.context()` string naming two causes
nobody had checked, and `warn!("...: {}", e)` on an `anyhow::Error` prints ONLY the
outermost context while discarding the source chain — which is exactly where the enclave's
rc lives, because `PerpClient::post` deliberately keeps the response body for that purpose.
So the one fact needed to act was constructed rather than read, and then only the
construction was logged. The same defect had been fixed for the attested-clock refusal in
#91 and it simply existed elsewhere.

`{:#}` prints the chain. It is never WORSE than `{}`: for error types whose Display ignores
the alternate flag the output is identical, and for `anyhow::Error` it is the difference
between a guess and an answer.

SCOPE, and it is deliberate. LOG MACROS ONLY — `trace! debug! info! warn! error!`. NOT
`format!`, because several of those compose API response bodies, and handing a client the
internal source chain is a disclosure decision, not a logging one. The boundary is the
property that matters (who reads the string), not the spelling of the macro.

A gate rather than a convention because 79 sites had the defect and 2 did not: at that
ratio the convention plainly does not hold by itself, and the two correct sites were both
written on the day the defect was found. Same reasoning as ci_prefix_framing.py.

    scripts/ci_log_error_chain.py           # check; non-zero on a violation
    scripts/ci_log_error_chain.py --fix     # rewrite the last {} of each violation

MULTI-LINE INVOCATIONS ARE REPORTED, NEVER SILENTLY SKIPPED. A gate that passes over what
it cannot parse is the hollow kind: it would have reported "clean" on a file it never read.
"""
import re
import sys
from pathlib import Path

ORCH = Path(__file__).resolve().parent.parent
SRC = ORCH / "src"

LOG_MACROS = ("trace", "debug", "info", "warn", "error")
# The error-ish final argument. Narrow on purpose: these are the names this codebase uses,
# and a wider net would start rewriting ordinary values.
ERR_ARGS = ("e", "err", "error")

MACRO_RE = re.compile(r'\b(' + "|".join(LOG_MACROS) + r')!\(')
PLACEHOLDER = re.compile(r'\{[^{}]*\}')


def invocation_body(lines: list[str], li: int, open_paren: int):
    """Text between the macro's parens, and the line index where it ENDS.

    Spans lines. The first version of this gate gave up at the end of the first line and
    merely REPORTED what it could not parse — honest, but it left 22 invocations to a hand
    review, and that review found three real defects while a four-line reading window had
    already missed two of them. A check whose coverage depends on how carefully someone
    squints is not a check, so the parser was taught to join instead.
    """
    depth = 0
    in_str = False
    esc = False
    out: list[str] = []
    start = open_paren
    for n in range(li, len(lines)):
        line = lines[n]
        for i in range(start, len(line)):
            c = line[i]
            if in_str:
                out.append(c)
                if esc:
                    esc = False
                elif c == "\\":
                    esc = True
                elif c == '"':
                    in_str = False
                continue
            if c == '"':
                in_str = True
                out.append(c)
            elif c == "(":
                depth += 1
                if depth > 1:
                    out.append(c)
            elif c == ")":
                depth -= 1
                if depth == 0:
                    return "".join(out), n
                out.append(c)
            else:
                out.append(c)
        start = 0
    return None, li


def split_args(body: str):
    """Top-level comma split, respecting strings, parens, brackets and braces."""
    args, depth, in_str, esc, cur = [], 0, False, False, ""
    for c in body:
        if in_str:
            cur += c
            if esc:
                esc = False
            elif c == "\\":
                esc = True
            elif c == '"':
                in_str = False
            continue
        if c == '"':
            in_str = True
            cur += c
        elif c in "([{":
            depth += 1
            cur += c
        elif c in ")]}":
            depth -= 1
            cur += c
        elif c == "," and depth == 0:
            args.append(cur.strip())
            cur = ""
        else:
            cur += c
    if cur.strip():
        args.append(cur.strip())
    return args


def fmt_string_arg(args):
    """Index of the format-string argument: the first arg that is a bare string literal."""
    for i, a in enumerate(args):
        if a.startswith('"') and a.endswith('"') and len(a) >= 2:
            return i
    return None


# The STRUCTURED form of the same defect. `error = %e` renders with Display, so it loses the
# chain exactly as `{}` does, and the codebase already contains the idiom that fixes it
# (`error = %format!("{e:#}")`) in two places against twenty-two that lack it. The `%` sigil
# is tracing-only syntax, so a line-based match cannot stray outside a log macro — which is
# what makes this class safe to catch without parsing the whole invocation, and it is the
# reason the multi-line invocations are still covered for this half.
STRUCT_RE = re.compile(r'\b(\w+) = %(e|err)\b')

violations: list[tuple[Path, int, str]] = []
unparsed: list[tuple[Path, int, str]] = []
fixed = 0
# A gate that scanned nothing must not report OK. Same failure the enclave coverage ratchet
# has guarded since a file VANISHED and the check stayed green: "no violations found" and
# "nothing was looked at" produce identical output unless the gate counts its own work.
files_scanned = 0
macros_seen = 0
FIX = "--fix" in sys.argv

for path in sorted(SRC.rglob("*.rs")):
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    files_scanned += 1
    changed = False
    for n, line in enumerate(lines, 1):
        for m in MACRO_RE.finditer(line):
            macros_seen += 1
            body, _end = invocation_body(lines, n - 1, m.end() - 1)
            if body is None:
                unparsed.append((path, n, line.strip()[:110]))
                continue
            args = split_args(body)
            if not args:
                continue
            fi = fmt_string_arg(args)
            if fi is None or fi == len(args) - 1:
                continue  # no format string, or no trailing value args
            last = args[-1]
            if last not in ERR_ARGS:
                continue
            fmt = args[fi]
            ph = PLACEHOLDER.findall(fmt)
            if not ph or ph[-1] != "{}":
                continue
            violations.append((path, n, line.strip()[:110]))
            if FIX:
                # Replace only the LAST `{}` of the format string, on whichever line holds
                # it — which is not necessarily the line the macro opens on.
                cut = fmt.rfind("{}")
                new_fmt = fmt[:cut] + "{:#}" + fmt[cut + 2 :]
                for k in range(n - 1, min(n + 40, len(lines))):
                    if fmt in lines[k]:
                        lines[k] = lines[k].replace(fmt, new_fmt, 1)
                        fixed += 1
                        changed = True
                        break
                else:
                    unparsed.append((path, n, "FORMAT STRING SPANS LINES: " + line.strip()[:80]))
    # Second pass: the structured form, line-based for the reason in STRUCT_RE's comment.
    for n, line in enumerate(lines, 1):
        if not MACRO_RE.search(line) and " = %" not in line:
            continue
        if not STRUCT_RE.search(line):
            continue
        violations.append((path, n, line.strip()[:110]))
        if FIX:
            lines[n - 1] = STRUCT_RE.sub(lambda m: f'{m.group(1)} = %format!("{{{m.group(2)}:#}}")', line)
            fixed += 1
            changed = True

    if changed:
        path.write_text("".join(lines), encoding="utf-8")

rel = lambda p: p.relative_to(ORCH)

if FIX:
    print(f"ci_log_error_chain: rewrote {fixed} site(s)")
    for p, n, t in violations:
        print(f"  {rel(p)}:{n}  {t}")
    if unparsed:
        print(f"\n{len(unparsed)} multi-line invocation(s) NOT touched — review by hand:")
        for p, n, t in unparsed:
            print(f"  {rel(p)}:{n}  {t}")
    sys.exit(0)

if violations:
    print("ci_log_error_chain: FAIL — a logged error must print its chain with {:#}, not {}")
    print("  `{}` on an anyhow::Error prints only the outermost context and throws away the")
    print("  source — which is where an enclave rc lives. See this script's docstring.")
    for p, n, t in violations:
        print(f"  {rel(p)}:{n}  {t}")
    print(f"\n  {len(violations)} violation(s). Fix with: scripts/ci_log_error_chain.py --fix")

if unparsed:
    print(f"\nci_log_error_chain: {len(unparsed)} multi-line log invocation(s) this gate")
    print("  cannot parse. REPORTED, not skipped — check each by hand:")
    for p, n, t in unparsed:
        print(f"  {rel(p)}:{n}  {t}")

# FLOOR: refuse to call an empty scan a pass.
if files_scanned < 5 or macros_seen < 50:
    print(
        f"ci_log_error_chain: FAIL — scanned {files_scanned} file(s) and {macros_seen} log "
        f"macro(s), which is too few to be a real run. A gate that looked at nothing prints "
        f"the same thing as a gate that found nothing."
    )
    sys.exit(2)

if not violations:
    print(
        f"ci_log_error_chain: OK — {macros_seen} log macro(s) across {files_scanned} file(s), "
        f"every logged error prints its chain"
    )
sys.exit(1 if violations else 0)
