# Accepted Platform Risks

**Status:** living document; reviewed quarterly or whenever a new platform risk is accepted.
**Audience:** auditor, operator, future developer.
**Companion:** `docs/cluster-trust-model-decision.md` (trust model ADR), `SECURITY-REAUDIT-4-FIXPLAN.md` Appendix A (audit findings + post-audit cycle findings).

This file enumerates **risks we know about, have analysed, and have accepted** because the only fixes are out of our control (vendor / hardware / cloud provider) or are economically prohibitive given the project's stage. Each entry states what the risk is, what we'd have to do to remove it, and why we've chosen not to do that today.

**Rule for adding entries:** never accept silently. Every entry here records (a) the technical content of what is being accepted, (b) the decision date and decision-maker, (c) the conditions under which we would re-open the decision.

---

## P-1 — Azure DCsv3 SGX TCB level reports `SW_HARDENING_NEEDED` (INTEL-SA-00615 / CVE-2022-21123, -21125, -21127, -21166 — MMIO Stale Data)

**Decision:** accepted on 2026-04-26 by operator @77ph for testnet AND for the Azure DCsv3 mainnet path, on the understanding that Azure's host kernel applies the FB_CLEAR (VERW) software mitigation.

**Implementation status:** enclave's `dcap_verify.cpp` allowlist is `{OK, SW_HARDENING_NEEDED}` (commit `8934f63`). The verdict policy is **planned to become a config parameter** as part of FIXPLAN APP-PATHA-1 fix — at which point the testnet config and the mainnet config will both explicitly set `OK,SW_HARDENING_NEEDED` (rather than the current hardcoded loosening), making the decision visible in deploy artefacts.

### What is SW_HARDENING_NEEDED

When `sgx_qv_verify_quote` validates a peer's DCAP quote against current Intel TCB info, it returns one of several verdicts. `SGX_QL_QV_RESULT_SW_HARDENING_NEEDED` (= 0xE007) means: **the platform's hardware and microcode are at the latest Intel-published TCB level**, but **the platform is known to be vulnerable to a class of side-channel issues whose fix is partly software-side** (kernel-level or hypervisor-level), and Intel cannot verify from inside the attestation whether the OS/hypervisor has actually applied that software mitigation. The verdict is a "platform appears patched at all levels Intel can attest, but the OS-side patch is operator responsibility — so we report the conservative status."

For Azure DCsv3 (Ice Lake-SP server-class Xeon Scalable), the dominant in-flight advisory currently producing this verdict for our FMSPC `00606a000000` is **INTEL-SA-00615** — the *Processor MMIO Stale Data* family (CVE-2022-21123 SBDS, CVE-2022-21125 SBDR, CVE-2022-21127 SRBDS, CVE-2022-21166 DRPW).

### What the underlying vulnerability is

CVE-2022-21166 (DRPW, "Device Register Partial Write") and the three siblings: stale data left in microarchitectural fill buffers can be inferred by an attacker performing carefully crafted MMIO operations on the same physical CPU. It is a transient-execution side channel in the MMIO subsystem.

**Conditions required to exploit:**
1. Attacker has **code execution on the same physical host** as the SGX enclave.
2. Attacker can issue MMIO ops or observe their timing to specific device registers.
3. Enclave is performing operations that load secrets into fill buffers within the affected window.

The side channel is local-only. It is not a remote vulnerability. It cannot be exercised from the network.

**Mitigation:**
- Microcode update from Intel (provides the FB_CLEAR capability).
- Kernel update that invokes the VERW instruction at kernel/user transitions and VM entry/exit. Mainline Linux from 5.19 onwards has this; backports exist in earlier supported LTS lines.

Disclosed June 2022. Microcode + kernel mitigations have been in stable distribution kernels for ~3.5 years at the time of this writing.

### Why Azure DCsv3 still reports SW_HARDENING_NEEDED

Intel's TCB info encodes hardware microcode version, BIOS, and platform firmware levels. It does **not** encode the operating system kernel version. So Intel's conservative position is: even when the platform's hardware/microcode is fully patched (which Azure DCsv3 is — `pcesvn:13`, current sgx-tcb-components on the highest applicable TCB level), the verdict is `SW_HARDENING_NEEDED` to remind the relying party that the host OS must also be patched. There is no cryptographic verdict for "OS has applied VERW mitigation" because there is no in-quote field for it.

In other words: this verdict will remain on Azure DCsv3 essentially indefinitely, regardless of how patched the host is, because the verdict mechanism cannot upgrade itself to `OK` based on OS state alone.

### What we are accepting

By configuring our enclave to accept `SW_HARDENING_NEEDED`, we are accepting **that we trust Microsoft Azure to operate the host kernel at a patch level that includes the FB_CLEAR / VERW mitigation**.

We can verify this informally:
- Azure DCsv3 nodes in our cluster run Ubuntu 22.04 on kernel 6.8.0-1051-azure / 6.8.0-1052-azure (observed 2026-04-26). Both are well past 5.19, both are Azure-curated images with Microsoft's security update train applied.
- Azure publishes [Confidential Computing security baselines](https://learn.microsoft.com/en-us/azure/confidential-computing/) and the company has a contractual obligation to apply CVE-class kernel mitigations within their SLA.

We **cannot** verify this cryptographically from inside the enclave. The acceptance is, fundamentally, a trust statement about Microsoft's operational practice.

### What it would take to NOT accept this

Three options, all currently rejected:

1. **Move off Azure DCsv3 to a platform whose TCB info reports `OK` for our FMSPC.** That would mean a different SGX-capable cloud (e.g. on-prem hardware co-lo, Equinix Metal, OVH bare metal SGX). Cost: re-evaluation of the operational model, new deploy procedure, possibly different DCAP toolchain, possibly different verifier policy decisions. Not a decision for this stage of the project.

2. **Wait for Intel to ship a microcode update that closes INTEL-SA-00615 entirely without OS cooperation.** No public roadmap suggests this is forthcoming; the architectural fix (separate fill buffers per privilege domain) would require new silicon, not microcode. Unrealistic.

3. **Implement custom side-channel mitigation inside our enclave** (e.g. flush sensitive state from registers and re-load on every kernel transition, deliberate noise injection in cache access patterns). Far beyond the scope of this project; would require dedicated cryptographic engineering and likely re-audit. Cost > benefit at our scale.

### Conditions to re-open the decision

We will re-evaluate the acceptance of P-1 if any of the following occur:

- New advisory in INTEL-SA-006XX class extends the side channel to **remote-exploitable** form. (Currently all variants are local-only.)
- Public proof-of-concept exploit demonstrates SGX secret extraction from a fully-patched Azure DCsv3 host. (We're aware of no such PoC as of this writing.)
- Microsoft Azure publicly states they have rolled back or no longer apply the VERW mitigation. (Highly unlikely; would be visible in Azure security bulletins.)
- We move the cluster to a non-Azure platform and the new platform's TCB info reports `OK`. (Then the old accepted-risk entry simply ceases to apply; no migration debt.)

### Why this entry is in this file and not in FIXPLAN open issues

`SECURITY-REAUDIT-4-FIXPLAN.md` Appendix A entry APP-PATHA-1 originally described the verdict-allowlist relaxation as a "fix needed before mainnet." This document re-classifies it: the *hardcoded* nature of the relaxation (no config flag, future operators have to read code) is still a fix the orchestrator should ship — that part stays in FIXPLAN. The *content* of the policy (`{OK, SW_HARDENING_NEEDED}` rather than `{OK}` only) is **the correct policy for any Azure DCsv3 deployment** based on the analysis above, and is documented as accepted-risk here. Future audits should ask "is this the correct policy" against this document, not against an unwritten assumption that strict-OK is automatically right.

---

## P-N — (template for future entries)

Same structure: what is accepted, what the underlying issue is, what we'd have to do to not accept it, conditions to re-open.