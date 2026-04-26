# ADR — Cluster trust model: DCAP cross-attestation

**Status:** Accepted (in production on testnet, 2026-04-26).
**Audience:** future auditors, future developers, the Phoenix PM team (sibling project), Tom's team (downstream functionality).
**Companion:** Phoenix PM has its own ADR for the same problem space; the two should be read in parallel.

## 1. Decision

The orchestrator + enclave establish cross-cluster peer trust by exchanging Intel SGX **DCAP attestation quotes**, verified inside the receiving enclave (`ecall_verify_peer_dcap_quote`), and binding the verified `MRENCLAVE` to the peer's per-instance ECDH identity in a per-ceremony attest cache.

We did **not** adopt an operator-signed cluster roster.

## 2. Context

We had two parallel forcing functions:

1. **Re-audit #4 critical E-C1 (private repo):** cross-machine FROST share transport. Existing `ecall_frost_share_export` / `ecall_frost_share_import` use `sgx_seal_data` with the default MRENCLAVE-bound key policy — the resulting blob can only be unsealed by an enclave with the same MRENCLAVE on the same CPU. There is no possible cross-machine path for those endpoints. The audit prescribed a proper ECDH-bound transport, which we shipped as Path A (Phases 1–6b in `xrpl-perp-dex-enclave`, 5d–6b on the orchestrator side).

2. **Re-audit #4 high O-H3:** validator locks on first-observed `sequencer_id` (TOFU). The audit's fix direction was "Cross-check `batch.sequencer_id` against the elected leader from the election module." We shipped that as commit `aafe84e fix(validator): O-H3 gate batches on elected leader (drop TOFU)`.

The audit prescription for O-H3 did **not** mention a roster, an operator key, or DCAP. It said: *use the existing election module's elected leader.* That is the fix we landed.

The audit prescription for E-C1 effectively required us to introduce ECDH identities, DCAP quote generation, and DCAP verification inside the enclave — because the only secure cross-machine envelope that survives MRENCLAVE-bound sealing is one bound to a verified peer enclave's identity.

Once the DCAP infrastructure exists for share transport, using it ALSO for peer trust in P2P signing (X-C1) and the Path A peer-quote announcer is a natural reuse. The orchestrator does not run an additional, parallel trust mechanism.

## 3. Mechanism (current implementation)

```
┌──────────────────────────────────────────────────────────────┐
│ Each enclave at first start:                                 │
│   - Lazily initialises a per-instance ECDH keypair (P-256-   │
│     style, secp256k1) in `ecdh_identity.sealed`.             │
│   - On `attestation-quote` request, generates a DCAP quote   │
│     whose `report_data` is bound to                          │
│     SHA-256("xperp/ecdh-identity/v1‖" ‖ pk ‖ shard_id ‖      │
│               group_id) ‖ zero_pad(32).                       │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ Each orchestrator (per shard with `frost_group_id` set):     │
│   - Periodically (240 s) fetches local ECDH pubkey + report- │
│     data + quote, publishes `PeerQuoteMessage::Announce`     │
│     over the `perp-dex/path-a/peer-quote` gossipsub topic.   │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ Each receiving orchestrator:                                 │
│   - Forwards the quote to its enclave via                    │
│     `POST /v1/pool/attest/verify-peer-quote`.                │
│   - Enclave: sgx_qv_verify_quote → strict allowlist on       │
│     verdict → recompute report_data formula and compare →    │
│     compare peer.MRENCLAVE to self.MRENCLAVE                 │
│     (same-binary policy) → on success, store                 │
│     (shard_id, group_id, peer_pk) → MRENCLAVE in a 32-slot   │
│     LRU peer-attest cache, TTL 5 min.                        │
└──────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────┐
│ Cross-machine FROST share transport:                         │
│   - Sender: lookup target in attest cache; if present, AEAD- │
│     wrap the share with ECDH+AES-128-GCM and ceremony nonce. │
│   - Receiver: lookup sender in cache; AEAD-unwrap; install   │
│     into `frost_group[signer_id]`.                           │
└──────────────────────────────────────────────────────────────┘
```

The trust root for the entire chain is **Intel SGX Root CA**, accessed via the configured PCCS (`/etc/sgx_default_qcnl.conf`).

## 4. What this gives us

- **Compromised peer (operator runs a different binary on a participant node):** rejected. The peer's quote will report a different `MRENCLAVE`; the receiving enclave's same-binary check (`compare to self`) fails; nothing further proceeds.
- **Compromised orchestrator (operator runs a malicious orchestrator process on a participant node):** the orchestrator can publish `PeerQuoteMessage::Announce`, but the contained quote is still genuinely produced by the SGX hardware on that machine and bound to the real enclave's ECDH identity. The orchestrator cannot forge. Worst it can do is *not* deliver — a liveness, not safety, concern.
- **Operator compromise (the human running the cluster):** their effect is limited to the binary they can replace on a node. If they replace it with a different `MRENCLAVE`, the quote check rejects it. They cannot extract a FROST share, because shares are sealed to MRENCLAVE on each node and Path A only releases them to a verified peer.

## 5. What this does NOT give us

- **Roster-style "is this peer an authorised member of the cluster?"** Path A's same-binary check is "any peer running the same MRENCLAVE on a DCAP-validated SGX platform is trusted." That's a hardware-rooted membership, not an explicit `{peer_id → allowed_ops, priority}` table. We rely on the configured `--p2p-peers` list at orchestrator startup for the network-layer ACL; that list is not signed.
- **Defense-in-depth against a peer running the same binary on different SGX hardware:** nothing in the mechanism distinguishes "the three Azure VMs the operator chose" from "any other SGX machine running the same binary somewhere on the internet." If an attacker obtains the same binary AND an SGX machine, they could in theory attempt to join the gossipsub topic and announce a valid peer-quote. They would still need to be on the operator's `--p2p-peers` list for the gossipsub mesh to route to them, but that ACL is not cryptographically enforced.
- **Strong operator key compromise resistance:** there is no operator key in our trust path. There is no compromise to resist. (Trade-off vs Phoenix PM, who DO have an operator key, see below.)

## 6. Sibling-project comparison: Phoenix PM (`77ph/SGX_project`)

Phoenix PM solves the same problem space with an **operator-signed roster**, per their audit's S-H6 fix direction. We confirmed this by direct read of their public repo (`grep -rn 'sgx_qv_verify_quote' EthSignerEnclave/` returns zero matches; their commit `17ecc37 feat(orchestrator): Batch B PR 1 — roster loader + ssh-keygen verify` is the canonical implementation).

| Property | This project (DCAP cross-attest) | Phoenix PM (operator-signed roster) |
|---|---|---|
| Trust root | Intel SGX hardware + Intel Root CA | Operator's long-lived ed25519 / ECDSA key (ssh-keygen format) |
| Auth root for "is this peer real" | Quote signed by SGX QE + chain to Intel Root | Roster signed by operator key |
| Auth root for "is this peer running the right binary" | MRENCLAVE compare in enclave | Implicit (operator's responsibility to put right binary on right host) |
| Compromise path: peer key | Peer's ECDH identity is bound to its MRENCLAVE; key alone is useless to attacker | Peer entry in roster identifies a public key; if peer's private key leaks, attacker can impersonate that peer until roster re-issued |
| Compromise path: operator key | No operator key in trust path | Operator key compromise → cluster pwn (mitigation: M-of-N YubiKey signing, in PM's roadmap, not yet shipped per their text) |
| Compromise path: malicious binary on a node | Detected (different MRENCLAVE → quote rejected) | Not detected by trust layer (roster says "this peer_id is allowed"; says nothing about WHAT binary the peer runs) |
| Dependency on Azure DCAP infrastructure | Hard dependency: PCCS, TCB info chain, QvL versions, SGX Root CA freshness on the verifier side | Zero dependency |
| Operational ergonomics: add/remove peer | Restart orchestrator with new `--p2p-peers` list; enclave re-attests automatically | Re-sign roster, distribute, validators reload |
| Operational ergonomics: rotate enclave binary (new MRENCLAVE) | Full bump procedure: `docs/testnet-enclave-bump-procedure.md` § §3–§12 | Roster unchanged across enclave binary changes; only orchestrator-side update needed (in their model, the binary's MRENCLAVE is not a trust input) |

**Both** approaches are correct against their respective audit recommendations. **Neither** approach is wrong. They are legitimate architectural divergence rooted in:
- Different audit prescriptions (PM's S-H6 prescribes roster; ours O-H3 prescribes the election module — neither prescribes the other).
- Different forcing functions for cross-machine secret transport (E-C1 forced us into DCAP infrastructure for SHARES; PM didn't have an equivalent compelling them down that path).
- Different threat-model emphases (we de-emphasize operator-key compromise risk; they de-emphasize Azure-DCAP-infrastructure-dependency risk).

## 7. Open question for the next auditor

**This is the question we want explicitly raised in the next audit. It is the most important architectural question in the project right now.**

> Two sibling SGX cluster projects with shared C++ enclave lineage have, in response to similar but distinct re-audit findings, converged on **different cluster-trust models**:
>
> - **This project (LemonTreeTechnologies/xrpl-perp-dex):** DCAP-based peer cross-attestation. Trust root = Intel SGX + Intel Root CA. No operator key. Implementation referenced in `docs/cluster-trust-model-decision.md` (this file).
> - **Phoenix PM (77ph/SGX_project):** operator-signed roster. Trust root = a long-lived operator ECDSA / ed25519 key. No DCAP cross-attestation. Implementation per their commit `17ecc37` and surrounding Batch B PRs.
>
> Both projects' code review trails show the chosen path was the audit-prescribed one for their respective findings. Neither team independently chose its model — each followed direct audit guidance.
>
> **Questions we want the next auditor to address explicitly:**
>
> 1. **Is one model strictly stronger than the other for the threat model of these projects** (testnet deploy → mainnet of a perpetuals DEX with live XRP collateral, in our case; prediction markets in PM's case)? If yes, which, and why?
>
> 2. **Is the architecturally-correct end state defense-in-depth** (roster for "who is allowed to ask anything" + DCAP cross-attest for "what binary is answering")? PM Claude proposed this in their analysis; we agree in principle. The auditor should rule on whether this is mandatory, recommended, or out-of-scope.
>
> 3. **For the team that does NOT have it today**, what migration path is reasonable? In particular for us: if the auditor recommends adding an operator-signed roster on top of DCAP cross-attest, what's the minimal change-set that would close the residual "peer running same MRENCLAVE on attacker-controlled SGX hardware" gap (see §5 of this document)?
>
> 4. **For the team that does NOT have DCAP cross-attest**, the converse: what's the minimal change-set to close "peer running a different binary than expected" gap, given that adding DCAP cross-attest from scratch requires the full Path A toolchain (PCCS config, verdict-policy, Azure operational dependencies — see APP-PATHA-1, APP-OPS-1, APP-OPS-2 in our `SECURITY-REAUDIT-4-FIXPLAN.md` Appendix A for the operational realities we hit)?
>
> 5. **Should the two projects share architecture** going forward? They are sibling projects with shared C++ enclave lineage, separately maintained. Architectural divergence at the trust-model layer means parallel security review, parallel deploy procedures, parallel threat-model documents. Is that maintenance burden acceptable, or should one model be normalised across both projects? If normalised — to which one?
>
> Each team has so far closed audit findings independently. Neither has cross-reviewed the other's trust model in depth. We believe this is the right time — before either project ships v1 with significant on-chain value at stake — to settle the architectural question instead of letting it harden by accretion.

## 8. Tracking

- This ADR lives at `docs/cluster-trust-model-decision.md` in the orchestrator repo.
- A pointer to it from `SECURITY-REAUDIT-4-FIXPLAN.md` Appendix A under APP-PATHA-1 — added in the same PR as this file lands.
- Phoenix PM has been asked (by its operator, on the same day this ADR was written) to produce a parallel ADR. When theirs lands, both should be linked from each other.
- The §7 "Open question for the next auditor" block should be lifted verbatim into the auditor's brief at the start of the next audit cycle.
