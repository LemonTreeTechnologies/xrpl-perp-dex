#!/usr/bin/env python3
"""ci_no_committed_enclave_binary.py — no enclave build output is tracked in git.

WHY (F-9, 2026-11-10; this repo's copy removed 2026-11-13). This repo tracked its own
enclave.signed.so, committed once on 2026-04-12 and never updated, behind an explicit
"!enclave.signed.so" .gitignore exception — seven months stale while the enclave source
moved on. Anyone deploying the "published binary" ran an enclave without every later fix,
and the runbook itself calls such a file "stale, a different build from the one being
deployed" (docs/path-a-runbook.en.md). The enclave repo gates the same class
(scripts/ci_no_committed_enclave_binary.py). A binary in the tree cannot stay in step
with the source, so the only safe number of tracked binaries is zero; an enclave's
identity is the MRENCLAVE of a reproducible build of a named commit, and the deploy flow
takes the freshly built artefact as an input path (orchestrator/src/node_deploy.rs).
"""
import subprocess
import sys

FORBIDDEN_SUFFIXES = (".signed.so", "enclave_id.txt")

# Run against the repo ROOT regardless of the job's working-directory — a gate that
# resolves the wrong directory and exits before checking has already let one defect
# class through unnoticed once (see the prefix-framing comment in check.yml).
root = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                      capture_output=True, text=True, check=True).stdout.strip()
tracked = subprocess.run(["git", "ls-files"], cwd=root,
                         capture_output=True, text=True, check=True).stdout.split("\n")
bad = [p for p in tracked if p.endswith(FORBIDDEN_SUFFIXES)]
if bad:
    print("FAILED — enclave build outputs are tracked in git:")
    for p in bad:
        print(f"  - {p}")
    print("Remove them (git rm --cached) — the identity of an enclave is the MRENCLAVE of a "
          "reproducible build, never a committed file.")
    sys.exit(1)
print(f"OK — no enclave build output among {len([p for p in tracked if p])} tracked files.")
