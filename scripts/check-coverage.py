#!/usr/bin/env python3
"""Phase 1.4 coverage gate — parses an LCOV report and ratchets coverage.

Two modes:
  --mode check    — read lcov.info, compare to baseline, exit 1 if regressed
                    beyond `tolerance_pct`.
  --mode update   — read lcov.info, write current numbers as the new baseline
                    (intended to run only on the master branch when check passes).

The baseline file is JSON: { "overall": <float>, "files": {<path>: <float>} }.
Per-file numbers are recorded only for files in `tracked_paths` (bounded set
to avoid noise from auto-generated targets). Overall is over all files in
the lcov, since lcov is the source of truth for "what does the test suite see".

Exit codes:
  0 — pass
  1 — coverage regressed beyond tolerance, OR (in --mode update) no lcov found
  2 — argparse / file errors

Tolerance default 1.0 percentage points: if baseline overall = 31.0%, current
must be >= 30.0% to pass. Per-file rule applies only to tracked paths and the
threshold is the same.
"""

import argparse
import json
import sys
from pathlib import Path
from typing import Dict, Tuple

# Files we care about per-file. Keep this small — the cost of adding a file
# is forever-tracked + forever-monotonic. Files not on this list are still
# counted in "overall" but won't fail per-file regression on their own.
TRACKED_PATHS = [
    "orchestrator/src/auth.rs",
    "orchestrator/src/cli_tools.rs",
    "orchestrator/src/orderbook.rs",
    "orchestrator/src/pool_path_a_client.rs",
    "orchestrator/src/rate_limit.rs",
    "orchestrator/src/shard_router.rs",
]


def parse_lcov(lcov_path: Path) -> Tuple[float, Dict[str, float]]:
    """Return (overall_pct, {repo_relative_path: pct, ...})."""
    total_lf = 0
    total_lh = 0
    per_file: Dict[str, Tuple[int, int]] = {}
    current_sf = None
    cur_lf = 0
    cur_lh = 0

    with lcov_path.open() as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("SF:"):
                current_sf = line[3:]
                cur_lf = 0
                cur_lh = 0
            elif line.startswith("LF:"):
                cur_lf = int(line[3:])
            elif line.startswith("LH:"):
                cur_lh = int(line[3:])
            elif line == "end_of_record":
                if current_sf:
                    rel = relativize(current_sf)
                    if rel:
                        prev_lf, prev_lh = per_file.get(rel, (0, 0))
                        per_file[rel] = (prev_lf + cur_lf, prev_lh + cur_lh)
                    total_lf += cur_lf
                    total_lh += cur_lh
                current_sf = None
                cur_lf = 0
                cur_lh = 0

    overall = (100.0 * total_lh / total_lf) if total_lf > 0 else 0.0
    files_pct = {
        path: (100.0 * lh / lf) if lf > 0 else 0.0
        for path, (lf, lh) in per_file.items()
    }
    return overall, files_pct


def relativize(absolute_path: str) -> str:
    """Convert an lcov absolute path to a repo-relative one we can match.

    Returns the empty string for paths outside the repo (cargo registry,
    /rustc/ stdlib, target/, tests/) — those are noise.
    """
    # Drop everything before /orchestrator/ if present.
    marker = "/orchestrator/"
    idx = absolute_path.find(marker)
    if idx == -1:
        return ""
    rel = "orchestrator/" + absolute_path[idx + len(marker):]
    # Skip generated, test, or dependency code.
    if rel.startswith("orchestrator/target/"):
        return ""
    return rel


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=("check", "update"), required=True)
    ap.add_argument("--lcov", default="orchestrator/lcov.info")
    ap.add_argument("--baseline", default="coverage-baseline.json")
    ap.add_argument(
        "--tolerance-pct",
        type=float,
        default=1.0,
        help="Allowed regression in percentage points (default 1.0).",
    )
    args = ap.parse_args()

    lcov_path = Path(args.lcov)
    baseline_path = Path(args.baseline)

    if not lcov_path.exists():
        print(f"ERROR: {lcov_path} not found", file=sys.stderr)
        return 2

    overall, files_pct = parse_lcov(lcov_path)
    tracked = {p: files_pct.get(p, 0.0) for p in TRACKED_PATHS}

    print(f"Current coverage: overall = {overall:.2f}%")
    for p in TRACKED_PATHS:
        print(f"  {p}: {tracked[p]:.2f}%")

    if args.mode == "update":
        baseline_path.write_text(
            json.dumps(
                {"overall": round(overall, 2), "files": {k: round(v, 2) for k, v in tracked.items()}},
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        print(f"\nBaseline written to {baseline_path}.")
        return 0

    # check mode
    if not baseline_path.exists():
        print(
            f"ERROR: baseline {baseline_path} missing. Run --mode update on master first.",
            file=sys.stderr,
        )
        return 2

    baseline = json.loads(baseline_path.read_text())
    base_overall = float(baseline["overall"])
    base_files = baseline.get("files", {})

    print(f"\nBaseline coverage: overall = {base_overall:.2f}%")
    for p in TRACKED_PATHS:
        print(f"  {p}: {base_files.get(p, 0.0):.2f}%")

    failed = []
    drop = base_overall - overall
    if drop > args.tolerance_pct:
        failed.append(
            f"overall: {overall:.2f}% < baseline {base_overall:.2f}% (drop {drop:.2f}pp > {args.tolerance_pct}pp)"
        )
    for p in TRACKED_PATHS:
        cur = tracked[p]
        base = float(base_files.get(p, 0.0))
        if base - cur > args.tolerance_pct:
            failed.append(
                f"{p}: {cur:.2f}% < baseline {base:.2f}% (drop {base - cur:.2f}pp > {args.tolerance_pct}pp)"
            )

    print()
    if failed:
        print("COVERAGE REGRESSED:")
        for msg in failed:
            print(f"  - {msg}")
        return 1
    print("Coverage OK (within tolerance).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
