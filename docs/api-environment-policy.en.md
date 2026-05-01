# API environment policy — Tom integration

**Status:** Accepted (2026-05-01).
**Audience:** Andrey, Tom, Alex, dev-perp, future maintainers.
**Companions:** [`development-operating-model.{en,ru}.md`](development-operating-model.en.md) (operating modes + mainnet sync procedure), [`feedback_api_boundary_with_tom.md`](../.claude/projects/-home-andrey-llm-perp-xrpl/memory/feedback_api_boundary_with_tom.md) in memory (API boundary — Tom owns clients black-box; we own the API).

This document fixes the policy for which **environment** Tom's components target during development, and the conditions under which a component graduates from testnet to the mainnet-sandbox. The companion document defines what is in Tom's scope vs ours; this document defines where each side runs.

---

## 1. The pairing rule

Tom's component connects to **our same-environment instance**, never crosses environments:

- Tom-**testnet** client → our **testnet** API (Azure DCsv3 cluster, escrow `rbqCUxgi…`, faucet-funded, multi-operator FROST 2-of-3, current code at `master` HEAD).
- Tom-**mainnet-sandbox** client → our **mainnet-sandbox** API (Hetzner mainnet stack) — only after the graduation gate in §4.
- Tom-**production** client → out of scope this document; production has its own launch playbook per `development-operating-model.md` §1.3.

Cross-environment connection (Tom-testnet client hitting our mainnet, or vice versa) is forbidden. Both the API surface and the data model are environment-tagged; bridging them risks polluting real funds with test signals or vice versa.

The `single-mode check` rule (sister memory `feedback_single_mode_check.md`) keeps testnet and mainnet on the same code, which means **the API contract is identical** between the two — no testnet-only branches, no mainnet-only quirks. The pairing rule is therefore not a code-level enforcement; it is an operational discipline.

---

## 2. Default — testnet first

For each new Tom component (new bot, new screen, new demo, new synthetic trader), the default and only-allowed development environment is testnet. The graduation gate (§4) is what unblocks the next environment.

Why testnet first:

- **Cost asymmetry.** Testnet is faucet-funded and resets cheaply; mainnet uses Andrey's real XRP. A client-side bug that fires repeatedly costs nothing on testnet and costs real funds on mainnet, even at sandbox scale.
- **Audit cadence.** Per `development-operating-model.md` §3, every mainnet-sandbox sync requires the audit cycle (REQ-N → RESP-N PASS). Fast iteration on Tom's client side surfaces API issues that may require our patches, which then require re-auditing — running this cycle for every iteration of a Tom component is operationally infeasible. Testnet absorbs the iteration without the audit-cycle cost.
- **Bug isolation.** When a problem appears on testnet, the participating sides are: Tom's client + our orchestrator + our enclave. When the same problem appears on mainnet during dev iteration, additional variables enter (real XRPL state, real fees, real network conditions). Testnet is the calibration step that tells us which problems are Tom's vs ours.
- **Reversibility.** Testnet "broken" → redeploy. Mainnet "broken" with real XRP → no redeploy fixes the lost funds.

---

## 3. Calm rebuttal list — when Tom asks for direct mainnet

These are framed as conversation moves, not refusals. The goal is to keep the boundary while honouring Tom's actual need (which is usually "iteration speed" or "realistic flow").

| Tom's ask | Our response | What it actually unblocks |
|---|---|---|
| "Just for now, mainnet is faster" | "Testnet runs the same code; the speed difference is the deploy step, ~30 min, which we'd anyway do for sandbox graduation." | We absorb the deploy delta on our side instead of putting Tom's client into mainnet. |
| "Testnet doesn't have realistic flow" | "Right — that's why the synthetic-trader path + the V1 vault provide the flow. Once V1 is implemented, testnet has continuous quoting and counter-flow." | We commit to making testnet realistic enough for Tom's component, instead of moving Tom to mainnet to find flow. |
| "I want to demo to investor" | "Demos run on mainnet-sandbox AFTER testnet validation. Sandbox is small-XRP and controlled. Investor demos are calm by design — we don't run them on raw mainnet." | We give Tom a path to a demo, just not the one that skips the gate. |
| "Mainnet is fine, I'll be careful" | "Real XRP doesn't grade on careful. The audit cycle is the gate that makes 'careful' enforceable; bypassing it removes the only mechanism that catches the careless case." | We name the gate, not Tom's discipline, as the actual mechanism. |
| "It worked at the hackathon" | "It worked because the audit gate didn't exist yet — and the cost of that was the chaos we agreed never to repeat. Today we have testnet because we built it for exactly this case." | We treat the hackathon as the precedent that motivated the gate, not as a precedent for skipping it. |

If after this conversation Tom still has a real reason that none of these address, the path is to surface it as a spec gap — Tom names what testnet is missing, dev-perp fixes the gap, Tom continues on testnet.

---

## 4. Graduation plan — testnet → mainnet-sandbox

Each Tom component graduates separately. The sequence:

1. **Iteration on testnet** until "demonstrably stable" — definition: Tom's component passes its happy-path repeatedly + Andrey's spot review confirms the surface area is what it claims to be.
2. **API contract check** — dev-perp confirms the API surface Tom's component depends on matches the documented spec (`docs/frontend-api-guide.md`, OpenAPI). Any deviation gets fixed in our docs/code before graduation; we do not ship a graduation that requires Tom to depend on undocumented behaviour.
3. **Mainnet-sandbox sync gate** — per `development-operating-model.md` §3, the orchestrator + enclave on mainnet are at the same code as testnet via Mode S sync. Audit verdict for that sync is PASS. Without this, no graduation.
4. **Sandbox connection** — Tom switches his component's API endpoint to the mainnet-sandbox URL. Volume is small XRP only (definition: amounts that, if lost entirely, are an acceptable cost-of-learning per the `product-sandbox-single-operator` mode in `development-operating-model.md` §1.1).
5. **Sandbox observation period** — dev-perp + Tom + Andrey watch for behaviour deltas (any difference between testnet and sandbox is a signal that the code-paths or environments are not actually identical, and graduation rolls back). Period length: at least one full sandbox session before considering the component "graduated."

There is no automatic promotion to production. Production is a separate launch event with its own playbook; sandbox graduation is not the same step.

---

## 5. What dev-perp does to enforce this

- Maintains the API contract docs (`docs/frontend-api-guide.md`, OpenAPI spec) so testnet behaviour and mainnet-sandbox behaviour are documented identically. The single-mode-check rule guarantees this is also true at runtime.
- Maintains API-level tests in `orchestrator/tests/` that exercise the contract. A failing test means the API is broken on the affected environment regardless of which client (Tom's or ours) reports it.
- The mainnet-sandbox endpoint is gated by the audit verdict per the operating model. dev-perp does not enable Tom's mainnet-sandbox path until the gate is open.
- When Tom reports a testnet API issue, dev-perp's response is to inspect/fix the API and its docs — not to suggest restructuring Tom's client.

---

## 6. What this document is NOT

- Not a rule against demos. Demos run on the appropriate environment (testnet for early, sandbox for later). The policy is "match the environment", not "no demos".
- Not a discipline imposed on Tom. The policy is symmetric — dev-perp also doesn't connect our test infra to mainnet during iteration.
- Not a permanent block on mainnet. The graduation gate exists precisely so components can move; the gate is `audit-verdict + Mode S sync`, not "Andrey's mood".
- Not negotiable per-iteration. The conditions in §4 are the gate; if they're met, graduation proceeds. If they're not, dev-perp fixes the gap.

---

## 7. References

- [`feedback_api_boundary_with_tom.md`](../.claude/projects/-home-andrey-llm-perp-xrpl/memory/feedback_api_boundary_with_tom.md) — Tom owns clients black-box; dev-perp owns the API.
- [`feedback_division_of_work_tom.md`](../.claude/projects/-home-andrey-llm-perp-xrpl/memory/feedback_division_of_work_tom.md) — Tom = spec; dev-perp = code.
- [`feedback_single_mode_check.md`](../.claude/projects/-home-andrey-llm-perp-xrpl/memory/feedback_single_mode_check.md) — same code on testnet + mainnet.
- [`development-operating-model.{en,ru}.md`](development-operating-model.en.md) §1 (operating modes), §3 (mainnet sync procedure).
- Hackathon precedent: Paris 2026-04-12 demo predates the audit gate; "the chaos we agreed never to repeat" is the operational reference for §3 row 5.
