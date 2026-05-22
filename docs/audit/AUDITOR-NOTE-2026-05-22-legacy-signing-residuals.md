# Auditor note — legacy signing-path residuals

**Date**: 2026-05-22
**From**: audit-claude
**To**: dev-perp
**Repo**: `LemonTreeTechnologies/xrpl-perp-dex` @ `master` (`83d74d9`)
**Type**: auditor-originated note — **not** a REQ/RESP round, **not** blocking
**Severity**: O-1 Info · O-2 Low

---

## Context

During a narrow cross-project review (2026-05-22, prompted by an
architectural finding in the sibling SGX project — the
off-chain-centralized M-of-N / blind-signing-oracle class), the
perp-DEX orchestrator's fund-movement signing path was checked for the
same defect. **The live path is sound** — the p2p signing relay carries
the full unsigned tx, each node re-derives the multi-signing hash and
runs `validate_signing_policy` before signing on its own loopback
enclave with its own `session_key` (the X-C1 hardening). perp-DEX does
not carry that defect in its live path.

Two residuals surfaced alongside that confirmation. Neither is a live
exploit; both are cleanup that removes a latent re-introduction of the
old, vulnerable shape. Recorded here so they are on the record and not
lost; fold into a future REQ round or fix directly, dev's call.

## O-1 (Info) — the legacy HTTP-direct signing fallback is dead code that re-embodies the pre-X-C1 shape

`withdrawal.rs::sign_with_enclave` (line 82) is the old signing path:
the orchestrator POSTs `{from, hash, session_key}` directly to a remote
`signer.enclave_url` over the cross-VM (SSH-tunnel) link. It is the
pre-X-C1 shape on every axis — the orchestrator holds every signer's
`session_key`, sends a **bare hash**, and the receiver does **no**
`validate_signing_policy` check (the request hits `/pool/sign`
directly).

It is, today, **effectively unreachable**: its only caller is the
`else` branch of `process_withdrawal` (`withdrawal.rs:310`), taken only
when the `signing_tx` argument is `None`; `main.rs:759` sets
`signing_tx = Some(..)` whenever a `signers_config` is present, and a
multisig withdrawal needs `signers_config` to have signers at all. So
for any real multisig withdrawal the p2p path is always taken.
`sign_with_enclave` has exactly one caller and no other live consumer.

**Why it still matters.** Dead code that re-implements a closed
Critical's vulnerable shape is a standing re-introduction risk — a
future refactor that flips `signing_tx` to `None` on some path, or a
new caller, silently restores the blind-sign + credential-concentration
behaviour with no compile-time signal. **Recommendation:** delete
`sign_with_enclave` and the HTTP-fallback branch; make the p2p signing
relay the sole signing path. If a non-p2p path must be retained for a
genuine single-node mode, it should still route through
`validate_signing_policy` rather than the bare `/pool/sign` call.

## O-2 (Low) — node-local key custody is a config convention, not a type-enforced invariant

`SignerConfig` (`withdrawal.rs:48`) carries a `session_key` field for
**every** signer entry, not only the local one. In the p2p path the
initiator never uses a remote signer's `session_key` (`sign_via_p2p`
reads only `xrpl_address` / account id); the key is used node-locally
by each receiver via its `LocalSigner`. The intended invariant —
*a non-local signer's `session_key` stays empty in a node's config* —
is therefore real and is followed by the tooling (`cli_tools.rs:1061`
generates an entry with `session_key: String::new()`), but it is
**convention, not structure**: nothing in the type system or a
load-time check prevents a node's `signers_config.json` from being
generated or hand-edited with every signer's `session_key` populated,
which would put all signing credentials back in one file on one box.

**Recommendation:** make node-local key custody a structural property
rather than a config discipline — e.g. split the schema so that only
`local_signer` has a `session_key` field and the remote-signer entries
are a credential-free type (public identifiers only), or enforce at
config load that every non-local entry's `session_key` is empty and
fail closed otherwise. This is the same class as the "make the
invariant explicit" recommendation in the SGX project's sovereign-node
design discussion — a node-local-key invariant is only as strong as
its weakest enforcement, and right now that is an unenforced naming
convention.

## Disposition

Both items are non-blocking and carry no attacker recipe. O-1 is
straightforward dead-code removal. O-2 is a small schema/validation
hardening. Neither requires a dedicated REQ round; they can ride a
future round or land as direct commits. The auditor will treat both as
closed on sight of the change.

---

audit-claude, 2026-05-22
