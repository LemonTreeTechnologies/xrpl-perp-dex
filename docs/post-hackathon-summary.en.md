# Post-hackathon summary — Path A close-out (late April – 22 May 2026)

A short summary for the team: what we did after the hackathon, why, what we achieved, the auditor-interaction protocol we introduced, and how this unblocks the path to Tom / vault.

## Context & why

After the hackathon we did not move on to new features — we closed out a foundation: the SGX-enclave upgrade mechanism, "Path A."

The enclave holds customer keys and state in sealed form. When its code is updated the enclave's measurement (MRENCLAVE) changes, and without a dedicated mechanism a new enclave cannot read the old one's state — that would mean losing customer funds and positions. Path A is a migration ceremony that carries state from the old enclave to the new one under cryptographic attestation.

It is foundational: until this mechanism is solid, the system cannot carry real responsibility. So Path A had to be brought to a "closed and proven" state before building further.

*This does not mean we are heading to mainnet — that is far off. It means the foundation is no longer an open risk.*

## What we achieved

- Path A migration proven on real SGX hardware (a 3-node Azure DCsv3 cluster): all 4 state sections, a parallel per-node ceremony.
- Verified that real customer state (balances, positions, vaults) survives a migration.
- Found and fixed a serious latent security bug: sealing used the MRSIGNER key policy instead of MRENCLAVE — "security theater" (the code looked protected, but in fact any enclave signed with the same key could read customer state). Fixed both in code and on the live cluster — all state re-sealed.
- Found and fixed a second bug (A-PA-1): attestation verification did not check the enclave's debug flag — a debug-build puppet would pass the check. Fix landed and is audit-PASS (REQ-18, 22 May 2026).
- Wrote a production-grade operator runbook for the Path A ceremony (bilingual RU/EN).

## The auditor-interaction protocol we introduced

A formal **REQ-N / RESP-N** cycle: the developer writes a verification-request document (REQ), the auditor writes a verdict document (RESP); append-only in git, not in chat — a reproducible, self-documenting audit trail.

The MRSIGNER incident exposed a methodology gap: the auditor and the developer held disjoint context — the auditor had technical facts but not project policy; the developer had policy but a rule that suppressed re-checking facts. In response we introduced:

- a **project-invariants digest** — shipped to the auditor with every REQ, so findings are weighed against policy;
- a mandatory re-rating of findings;
- **falsification tests** for foundational claims;
- the principle of "no unfalsifiable zones."

That the protocol works was shown by bug A-PA-1 — it was found precisely because the auditor refused to rubber-stamp "Path A closed" after the developer and re-checked.

## We unblocked the path to Tom / vault

Path A is closed — the foundational piece is done. Further development — Tom's feedback items and the V1 vault — now builds on a proven foundation that will not have to be redone.
