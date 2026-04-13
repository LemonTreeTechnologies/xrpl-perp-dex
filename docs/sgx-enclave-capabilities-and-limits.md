# SGX Enclaves — Capabilities, Limits, and the Migration Problem

**Status:** Reference document / Q&A. Written to capture what the team should know about SGX before reasoning about anything that touches the enclave — in particular, before changing code that reads or writes sealed state (like the vault).

**Scope:** Practical SGX fundamentals relevant to this project (perp DEX with FROST 2-of-3 signing + sealed state). Not an exhaustive SGX reference — for that, read the Intel SGX Developer Reference and the SDK manuals.

**Audience:** Technical readers (developers, operators, auditors) who haven't worked with a Trusted Execution Environment before. No prior SGX knowledge is assumed. Some familiarity with OS concepts (processes, syscalls, virtual memory) and cryptographic basics (symmetric vs asymmetric, hashes, AES-GCM) is expected.

**Russian version:** `sgx-enclave-capabilities-and-limits-ru.md` (per the bilingual docs policy).

---

## Part 0 — Framing: why a Trusted Execution Environment at all?

### Q: I know how normal code works. What problem does a TEE solve that normal code can't?

Think about where private keys live in a conventional server. A wallet process runs, keeps the key in RAM, signs transactions. Anyone with **root on that machine** — the sysadmin, a malware infection, a compromised SSH, someone who dumps `/proc/<pid>/mem` — can read that key. Normal operating systems were never designed to protect a process from its own administrator. Root can read any memory it wants. That's not a bug, it's the entire point of the kernel.

For code that holds high-value secrets (signing keys, private user data, access to funds), this is a problem. The traditional answer was **physical isolation**: put the keys on a separate box (an HSM, a hardware wallet, an air-gapped signer) and let the untrusted server ask the trusted box to sign things over a narrow wire. This works but it's expensive, slow, and you still have to secure the wire.

A **Trusted Execution Environment (TEE)** is a different answer: put the sensitive code in a region of the *same* machine, but one that even the kernel can't peek into. The CPU itself enforces the isolation in hardware. The administrator sees a process running, can stop it, can delete its files, but cannot read what's inside it while it runs. You get something between "regular process" and "separate physical machine".

**Intel SGX** is one specific TEE implementation. Others exist (AMD SEV, ARM TrustZone, Intel TDX), each with different trade-offs. SGX operates at the *process* level: a normal Linux process can contain a small isolated region called an **enclave**, and only code that's loaded into that region runs in the protected mode.

### Q: Give me the intuition in one picture.

A normal process is a house. The operating system is the city — it can knock down walls, read your mail, see every room. A TEE adds a **vault room** inside the house. Everything outside the vault works normally. Inside the vault, nobody — not the city, not another tenant, not even someone who breaks into the house with physical tools — can see what's happening, as long as you're willing to accept the constraints of working inside a vault (no windows, no phone line to the outside world, you have to pass messages through a slot in the door).

The vault analogy carries further and we'll use it repeatedly:

- Passing messages through the slot → **ECALL** and **OCALL** (the only ways across the boundary).
- "No phone line to the outside world" → the enclave **cannot make syscalls**, including network operations.
- The vault's deed is held by a specific person → **enclave measurement (MRENCLAVE)** and **signing key measurement (MRSIGNER)**.
- Storing a document so that only *you* can read it later → **sealing**.
- Proving to someone outside that the vault exists and contains what you claim → **attestation**.

If you remember the vault, everything in the rest of this document will land.

### Q: Why would a perp DEX use this specifically?

Because it lets the signing key, the margin ledger, and the risk policy live in code that the operator cannot peek into or silently modify. The operator can start and stop the service, can read the on-disk encrypted blobs, can see the network traffic — but the actual private keys and the actual decision-making code are sealed from them. In a FROST 2-of-3 setup this property holds across three independent operators, each running their own enclave on their own machine. No single operator can exfiltrate keys or forge signatures, because the keys don't exist in a form any single operator can read.

This is the core reason the project is built on SGX: **the trust story is that no operator, not even the one running the binary, has to be trusted for the protocol to be safe**.

### Q: What's the catch?

Several. The rest of this document walks through them. The short version:

1. Enclaves can't do I/O. They're a computation box, not a networking box.
2. Changing enclave code breaks its identity, which breaks its ability to read its own saved data.
3. SGX has had real-world vulnerabilities; it's strong but not unbreakable.
4. You trust Intel (for the hardware and for attestation infrastructure) whether you like it or not.
5. Operationally, SGX is finicky. Microcode updates, CPU replacements, attestation plumbing — all of it has sharp edges.

With that framing in place, the rest is detail.

---

## Part 1 — What SGX actually is

### Q: In one paragraph, what is an SGX enclave?

An SGX enclave is an **isolated execution region** inside an otherwise-normal Linux process, protected by the CPU itself. The enclave's code and data live in a special chunk of RAM called the **Enclave Page Cache (EPC)**. The hardware automatically encrypts every byte written to the EPC and decrypts it only when the CPU is executing code that belongs to that particular enclave; the encryption key is generated inside the CPU at boot and never leaves the chip. The consequence: the operating system, the hypervisor, other processes, other enclaves, and even an attacker who physically taps the memory bus with a probe, **cannot read the enclave's memory in the clear**. The enclave's code is hashed at load time (this hash is called its **measurement**), and that measurement is cryptographically bound to everything the enclave does later — it is the anchor that makes both "saving data so only this exact code can read it" (sealing) and "proving to a remote party what code is running" (attestation) possible.

### Q: What are the key security properties I actually get?

1. **Confidentiality** — the enclave's memory is opaque to everything outside it, including the OS kernel.
2. **Integrity** — the enclave's code cannot be silently tampered with; any change produces a different measurement.
3. **Attestable identity** — the enclave can prove to a remote party that a specific code measurement is running in a genuine Intel SGX CPU.
4. **Bound persistence** — data sealed by an enclave can only be unsealed by an enclave with the matching identity on the same CPU family.

### Q: What SGX does *not* give you.

- **It's not a container or a VM.** It's an isolated compute region and nothing more. No filesystem, no network sockets, no ability to call into the kernel. The only things the enclave can do are what you explicitly wire in as entry and exit points at compile time.
- **It's not tamper-proof against your own bugs.** If the code inside the enclave has a memory-safety bug or a logic flaw that leaks secrets, SGX doesn't save you. The confidentiality guarantee only matters if the enclave code itself doesn't accidentally publish its secrets. This is why enclave code is almost always small, careful, and heavily reviewed.
- **It's not free in performance.** Every time your normal code enters the enclave, the CPU has to switch modes, flush some state, and validate the transition — a few microseconds per crossing. If you design badly, you can spend more time crossing the boundary than doing useful work inside. The EPC is also finite (see Part 8), and you can't use a full standard library inside the enclave.
- **It has a product roadmap like any platform.** Intel deprecated SGX on 11th/12th-gen consumer Core CPUs in 2021, but **server SGX on Xeon Scalable processors is a first-class, actively supported platform**. Intel's official support position (see support article 000089326) is that there are currently *no plans to deprecate Intel SGX on the supported Intel Xeon Scalable processors*, and development continues for cloud and enterprise use — this is what we target. Intel's newer **TDX** (Trust Domain Extensions) is a related technology that protects whole VMs instead of individual processes; it's positioned to *complement*, not replace, server SGX, and is a separate topic.
- **It's not a magical box that can't be broken — but breaking it is a lab exercise, not a remote hack.** SGX raises the attacker's bar enormously but has had real-world breaches. A family of attacks called **side-channels** — where the attacker doesn't read the memory directly but *infers* its contents from timing, cache behaviour, electromagnetic emission, or power draw — has repeatedly been shown to work against SGX. What matters for threat modelling is *how* these attacks actually work in practice:
  - Almost every successful SGX side-channel attack published in academic literature (Plundervolt, VoltPillager, Foreshadow, SGAxe, ÆPIC Leak, and the power-analysis family) requires either **physical possession of the machine** with instrumentation attached (oscilloscopes, power probes, voltage manipulation hardware, FPGA-based memory interposers), or **root-level privileged code running on the same host** with careful tuning over hours or days.
  - None of them is a "click a link and pop the enclave" remote exploit. To mount one, an attacker has to **physically carry our server into their lab, wire it up to measurement gear, and work on it for hours or days** — or first fully compromise the host OS and then run a carefully tuned local attack. Both scenarios are in-scope of our overall threat model (we assume the host OS is hostile; this is the whole point of using SGX), but they're categorically different from the "someone on the internet got our keys" failure that most web systems need to worry about.
  - Each published attack has been mitigated by Intel shipping microcode updates, and each mitigation advances a counter called **CPUSVN** (more on that later) — so a patched server on current microcode is hardened against the *known* families of attacks.
  - Our defence-in-depth answer to the residual risk is **FROST 2-of-3**: even a complete compromise of one enclave on one server does not expose the signing key, because no single enclave ever holds it.
  - The right way to think about SGX is "dramatically better than a normal process, forces any serious attacker into a lab or an insider position, but still requires the same defence-in-depth mindset as any other security boundary".

---

## Part 2 — Architecture and memory model

### Q: Where does the enclave actually live?

Inside a normal Linux process. A single process can contain zero, one, or several enclaves — they're like specially-protected "rooms" inside your process's address space. The physical memory backing an enclave is carved out of a region the BIOS reserves at boot called the EPC (Enclave Page Cache). Every byte the CPU writes to the EPC gets encrypted on the way in and decrypted on the way out of the CPU, automatically, by a dedicated hardware block — so even if you were to freeze the RAM and dump it with a logic analyser, you would get ciphertext, not plaintext. The encryption key is regenerated inside the CPU at every boot and never leaves the chip package.

### Q: How does non-enclave code in the same process talk to enclave code?

Through two kinds of controlled transitions, defined at compile time in a small file called an **EDL** (Enclave Definition Language). Think of the EDL as declaring the "slots in the vault door": every function that the outside world is allowed to ask the enclave to run, and every function that the enclave is allowed to call back into the outside world to have done on its behalf.

- **ECALL** — untrusted host → trusted enclave. "Host, please ask the enclave to do X." The host hands in arguments and receives a return value, similar to a normal function call, but it's really a controlled entry into the protected region.
- **OCALL** — trusted enclave → untrusted host. "Enclave, please ask the host to do Y on my behalf and hand me the result." Used for anything the enclave can't do directly — which, as we're about to see, is "almost everything that touches the outside world".

Every ECALL or OCALL is a **context switch** — the CPU has to flush certain pipeline state, change execution mode, and re-validate the transition. It's much more expensive than a normal function call, typically a few microseconds. This is slow on a per-call basis but fine on a per-transaction basis. A well-designed enclave is called rarely with "thick" arguments, not frequently with "thin" ones.

### Q: Can an enclave directly issue syscalls?

**No, and this is by design.** A **syscall** is how ordinary user-space code asks the kernel to do privileged things like read a file or send a network packet. Normally you call `read()`, the CPU transitions into kernel mode, the kernel does the work, and you get the bytes back. If an enclave could do this, the kernel — which is explicitly outside the enclave's trust boundary — would see the enclave's request arguments, would control the response, and could easily lie or snoop. So Intel locked it out: the CPU literally refuses to execute `syscall` instructions from inside an enclave.

The consequence is that every syscall-flavoured operation — reading a file, opening a socket, asking for the current time — has to happen *outside* the enclave, and the enclave has to OCALL out and ask the host to do it. The host is untrusted, so the enclave has to treat every OCALL reply as potentially malicious and validate it.

This is the single most important constraint for reasoning about what belongs inside the enclave versus outside it. If your feature needs to talk to the network, it lives in the host. If your feature needs to hold a secret and make decisions about it, it lives in the enclave. Anything in between is a design choice about where to draw the boundary.

---

## Part 3 — The network constraint (and why the enclave cannot send XRPL transactions)

### Q: So the enclave really cannot open a socket?

Correct. There is no TCP/UDP stack inside the enclave, no DNS resolver, no TLS library that talks to anything except via OCALL'd byte streams. If someone tells you "the enclave connects to XRPL", they're being loose with language — what they mean is "the enclave produces signed bytes, the *host* sends them to XRPL, and receives bytes back which it relays into the enclave via ECALL".

### Q: Why does this matter for a signer?

Because it means the signer **cannot autonomously post transactions**. The flow for any signed operation is always:

1. The host (Python, Rust, whatever) constructs a transaction template.
2. Host makes an ECALL into the enclave: "sign this tx with user U's key, subject to policy P".
3. Enclave validates the policy, reads its sealed key material, signs, returns the signed bytes via the ECALL return value.
4. Host takes the signed bytes and submits them to XRPL via whatever RPC/WebSocket client it uses.
5. Host waits for XRPL confirmation, pulls the result back, and ECALLs the enclave again if the enclave needs to know about the result ("tx hash X confirmed at ledger L").

The enclave never sees the wire. The enclave never initiates. The enclave only **reacts** to ECALLs and **delegates** network operations via OCALLs.

### Q: Does this mean the host can withhold transactions the enclave signed?

Yes, and this is an important honest limitation. If the host is compromised or malicious, it can:
- Sign-and-discard (ask the enclave to sign, then throw the signed bytes away).
- Sign-and-delay (hold a signature until the market has moved).
- Sign-and-replay (re-submit an old signature if nonce/sequence allows).

The enclave cannot prevent any of this because it cannot see the network. What it *can* do is make these attacks detectable or useless:

- **Sequence / nonce discipline** — if every signed tx must reference a monotonically increasing counter that the enclave tracks, replay is blocked at the enclave level.
- **Deadlines baked into signed blobs** — include a `LastLedgerSequence` (XRPL-native) or equivalent so stale signatures expire.
- **Cross-operator attestation** — in a FROST 2-of-3 setup, two of three operators are needed to sign, so a single malicious host cannot fabricate a signature at all.
- **Audit trail** — the enclave can log signed tx hashes to its own sealed state, so an auditor can later verify that every sealed-logged tx appears on chain.

This is why our custody is FROST, not a single-enclave signer: the "compromised host withholds signed bytes" attack is mitigated by requiring two independent hosts to collaborate.

### Q: What about time? Can the enclave trust its own clock?

**No.** There is no trusted clock inside an SGX enclave. The CPU does not expose a monotonic secure counter that is freely readable. SGX does have a "trusted time" service (`sgx_get_trusted_time`) but it's been deprecated and was never reliable. In practice, time comes from the untrusted host via OCALL, which means a malicious host can lie about what time it is. Defences: for anything time-sensitive, have the enclave demand a *signed* timestamp from an external source (XRPL ledger close time is a decent choice, since it's bound to a ledger that the host cannot fake without colluding with the whole network).

---

## Part 4 — Sealing: how data is encrypted and decrypted

### Q: If an enclave has no filesystem, how does it persist anything?

At first glance this looks impossible. The enclave can't call `write()`, so it can't save anything to disk. But it can OCALL out to the host and ask the host to save an opaque blob of bytes.

The trick is that before the enclave hands the bytes to the host, it **encrypts them inside the enclave using a key that only an enclave with the right identity can derive**. The host receives ciphertext, stores it however it likes, and has no way to read it. Later, the same enclave (or another enclave the CPU considers equivalent) asks the host for the ciphertext back, re-derives the same key, and decrypts it.

This whole dance — encrypt inside, store outside, decrypt inside later — is called **sealing**. Think of it as the enclave putting something into a locked box that only the enclave's specific hardware-derived key can open, and then letting the untrusted world carry the locked box around for safekeeping.

The magic is entirely in *where the key comes from*. The key is not stored anywhere. It is derived, every time, from:
1. A master secret that Intel burned into the CPU at the factory.
2. The identity of the enclave asking for the key.
3. A few version numbers.

No enclave on any other CPU can derive the same key, because the master secret is different. No other enclave on the same CPU can derive the same key, because the enclave identity is different. This is why sealed data is tied to **one specific enclave binary, on one specific physical CPU**.

### Q: Walk me through a seal operation end-to-end.

1. Enclave has some bytes it wants to persist — e.g. a private key share, or a sealed vault state blob.
2. Enclave calls `sgx_seal_data()` (or its equivalent in whatever SGX SDK you're using).
3. The CPU derives a **sealing key** from a secret fused into the CPU at manufacturing time (the root sealing key), combined with the enclave's measurement (more on this in a moment) and some CPU security-version info (CPUSVN, ISV_SVN).
4. The enclave uses this derived key with AES-GCM (or AES-CCM) to produce a ciphertext blob with an integrity tag.
5. Enclave OCALLs out: "host, please write this blob to file `/path/to/sealed.bin`".
6. The host writes the blob. The host cannot read the plaintext — it never had the key and the key was never outside the CPU.

### Q: And unsealing?

The reverse:

1. Host reads the blob from disk, ECALLs: "here's the sealed blob".
2. Enclave calls `sgx_unseal_data()`.
3. The CPU re-derives the sealing key from the same inputs (root key + measurement + CPUSVN).
4. If the derived key matches, AES-GCM decrypts and integrity-verifies, and the enclave gets plaintext.
5. If any of the inputs differ — different enclave binary, different CPU, different CPUSVN — the key derivation produces a different key, and decryption fails.

The last sentence is the entire migration problem in one line. Keep it in mind.

### Q: What's the difference between MRENCLAVE-sealing and MRSIGNER-sealing?

The sealing-key derivation can be bound to either of two enclave identities:

- **MRENCLAVE** — a SHA-256 measurement of the enclave's code and initial data. Change one byte of your enclave binary and MRENCLAVE changes. Sealing to MRENCLAVE means: *only this exact binary, rebuilt bit-for-bit identically, can ever unseal this blob*.

- **MRSIGNER** — the SHA-256 hash of the Intel-format public key used to sign the enclave. Every enclave signed by the same key has the same MRSIGNER, regardless of code differences. Sealing to MRSIGNER means: *any enclave signed by us, including future versions, can unseal this blob*.

The SDK exposes this choice as the `policy` parameter to the seal call.

### Q: What's the trade-off?

| Property | MRENCLAVE sealing | MRSIGNER sealing |
|---|---|---|
| Tamper resistance | Very high — any code change invalidates the data | Lower — any enclave signed by us can read it, including a buggy new version |
| Upgrade path | **You cannot upgrade without explicit migration** | Upgrades "just work" as long as the signing key is unchanged |
| Worst case if signing key leaks | Data is safe — attacker needs the exact binary | Attacker can build a malicious enclave signed with the leaked key and read everything |
| Typical use | Ephemeral secrets, test keys, per-version state | Long-lived keys, user databases, anything that must survive upgrades |

In practice, long-lived persistent state (like the vault's position database) uses **MRSIGNER** sealing. Short-lived or version-specific state uses **MRENCLAVE** sealing. The choice is made per-blob, not globally.

### Q: Is there anything else that affects the sealing key?

Yes, and this is where people get bitten:

- **CPUSVN** — the CPU's security version number, which bumps whenever microcode adds mitigations for new side-channel attacks. The sealing key derivation uses the *current* CPUSVN, not the one from when the data was sealed. There's a "rollback resistance" mode where you can derive a key for an older CPUSVN (so data sealed under CPUSVN=5 can still be read after a bump to CPUSVN=6), and a strict mode where you can't. Strict mode is what you want for defence against microcode downgrades; rollback mode is what you want for survivability across microcode updates.

- **ISV_SVN** — the application-level security version number you embed in your enclave metadata. Like CPUSVN but under your control. Useful for "patches 1–3 can read each other's data but version 4 cannot be read by versions 1–3".

- **The CPU itself.** Sealing keys are per-CPU. Move your disk to a different machine and the sealing fails, even if you rebuild the exact same enclave. This is intentional — it prevents disk cloning. It also means **you cannot back up sealed blobs by copying them to another server and expect them to work there**. Backup requires an explicit export mechanism (more on this in Part 7).

### Q: So what is the actual lifecycle of "save the vault's positions"?

Assuming MRSIGNER sealing, and assuming we haven't rebuilt the enclave yet:

1. Enclave has vault state in RAM.
2. ECALL `save_state` fires.
3. Enclave serialises state to bytes.
4. Enclave calls seal with MRSIGNER policy → ciphertext blob.
5. Enclave OCALLs `write_file(blob)`.
6. Host writes.

Later:

7. Host reads file.
8. ECALL `load_state(blob)`.
9. Enclave calls unseal — succeeds because same CPU + same MRSIGNER + compatible CPUSVN.
10. Enclave has vault state back in RAM.

That's the happy path. Everything that follows in Part 7 is about what happens when one of the inputs changes.

---

## Part 5 — Attestation: proving what's running to a remote party

### Q: Quick summary — what problem does attestation solve, and what is it?

**The problem:** imagine you're a user who wants to deposit funds into our protocol. You open a connection to `api-perp.ph18.io`. A server answers. How do you know the code answering you is actually the enclave we claim it is, and not a malicious orchestrator impersonating it? You can't just look at the code — it's running on somebody else's machine. You need the machine itself to vouch for what it's running, in a way you can cryptographically check.

**The solution:** **attestation** is a mechanism by which an enclave, with help from the CPU it runs on, produces a small signed document (called a **quote**) that says:

- "A specific piece of code (identified by its measurement) is running."
- "It is running inside a genuine Intel SGX CPU, not an emulator or a lie."
- "The enclave has attached this 64 bytes of arbitrary data to the proof" (this is called **report data**, and is usually a hash of a freshly-generated public key, so the user can then set up an encrypted channel to the enclave).

The third party, if they trust Intel as the root of the chain, can verify the quote's signature and then trust that the public key in the report data genuinely came from the measured enclave on a real SGX CPU. From that point on, they can encrypt messages to the enclave that only the enclave can read.

In short: attestation is how an enclave introduces itself over an untrusted network in a way that can't be impersonated.

### Q: Local vs remote attestation?

- **Local attestation** — enclave A proves to enclave B on the same CPU that A is the measurement it claims. Fast, no Intel services involved. Used for intra-machine enclave-to-enclave key exchange.
- **Remote attestation** — enclave proves to a party on a different machine. There are two generations: **EPID** (older, Intel's privacy-preserving group signature scheme, needs Intel attestation service) and **DCAP** (newer, standard PKI-based, can be verified offline after bootstrap, used by modern Azure Confidential Computing and our setup).

### Q: What goes into a DCAP quote?

Simplified: the quote is a structure signed by a per-CPU attestation key (provisioned by Intel), containing MRENCLAVE, MRSIGNER, CPUSVN, ISV_SVN, a flag set, and 64 bytes of report data. To verify, you chain the signature to Intel's root certificate and then apply your own policy (accept this MRENCLAVE? accept this MRSIGNER? require CPUSVN ≥ N?).

### Q: What do we use attestation for in this project?

Two things:

1. **DCAP attestation endpoint** — the orchestrator exposes a REST endpoint that returns a fresh quote proving the enclave is running. A client can verify this before trusting the orchestrator with deposits.
2. **Cross-node attestation for FROST** — two enclaves that are about to perform distributed signing verify each other's quotes first. If a peer comes up with an unexpected MRENCLAVE (say, because someone deployed a backdoored build to that node), the other peers refuse to sign with it. See `deployment-procedure.md` section 3.5 for how this integrates with the deploy pipeline.

---

## Part 6 — The version migration problem (the hard one)

This is the section the user specifically asked for. It's the part that bites every SGX project eventually. If you only read one part of this document, read this one.

### Q: Restate the problem in one sentence.

If you change code inside the enclave — even one byte — MRENCLAVE changes, and any state that was sealed to MRENCLAVE can no longer be unsealed by the new enclave. Even if you're using MRSIGNER sealing, bugs in schema compatibility, ISV_SVN choices, or CPUSVN bumps can still lock out your old data.

### Q: What actually changes when I edit vault code?

Assume you edit `PerpEngine.cpp` to change how liquidation threshold is computed. Rebuild the enclave. The new `.so` file has:

- A new MRENCLAVE (because the code bytes changed).
- Same MRSIGNER (because you used the same signing key).
- Possibly a new ISV_SVN if you bumped it in metadata.

Now what happens to the sealed state on disk (say, the vault's positions file)?

- If positions were **sealed to MRENCLAVE**: the new enclave **cannot read them at all**. Unseal fails. You have a brick.
- If positions were **sealed to MRSIGNER** with compatible ISV_SVN: the new enclave *can* unseal the blob. But:
  - The **schema** inside the plaintext might not match what the new code expects. If you added a field to `PerpPosition`, the serialiser inside the new enclave will read the old blob as a malformed version-N-1 structure, silently garbling data. This is a separate problem from sealing, and it's usually what kills you in practice.
- If you also bumped CPUSVN (by updating microcode): depending on the seal policy, you may also lose access.

So the migration problem has **three layers** stacked on top of each other:

1. **Sealing key derivation must still succeed** → MRSIGNER policy + CPUSVN rollback tolerance.
2. **Schema must be versioned and backwards-readable** → every persisted struct carries a version byte, and the new enclave has explicit code to read old versions and upgrade them in memory.
3. **Invariant changes must be explicitly migrated** → if your new code assumes "every position has a `funding_index` field" but old positions don't, you need a migration step that computes or defaults the field during load.

Missing any one of the three loses you data.

### Q: What's the default advice people get, and why is it insufficient?

The naive advice is "use MRSIGNER sealing and version your schemas". This handles layers 1 and 2 but ignores layer 3 (invariant migrations) and also ignores the more subtle problem that MRSIGNER sealing weakens your tamper-resistance story — anyone who compromises the signing key can read all historical data.

For our project specifically, the signing key for the enclave lives with us (77ph personal account for the enclave repo, and a proper HSM-backed key for production). That key's security becomes the floor on every sealed blob's security, for the lifetime of the project.

### Q: Walk me through a concrete vault migration — v1.4.2 of the vault wants to add a new field `last_funding_ts` to every open position. How do we actually migrate?

Let's assume we're using MRSIGNER sealing with a versioned on-disk format. The on-disk blob for the vault looks like:

```
[4-byte magic "VALT"]
[2-byte version, e.g. 0x0001]
[payload bytes, format depends on version]
[AES-GCM tag — added by the SDK seal operation]
```

When the new enclave loads, it does this:

1. OCALL out: read the blob from disk.
2. `sgx_unseal_data()` with MRSIGNER policy.
   - If this fails because CPUSVN moved forward → log, bail out, require manual recovery (section further below).
   - If it succeeds → we have plaintext bytes.
3. Parse the magic, check it matches `"VALT"`. If not, refuse to load — this is either corruption or a blob from a different enclave.
4. Parse the version field.
5. If `version == 0x0002` (current) → deserialise directly.
6. If `version == 0x0001` (prior) → call the **v1→v2 migrator**. The migrator:
   a. Reads the old layout (no `last_funding_ts` field).
   b. Constructs the new layout in memory.
   c. Fills `last_funding_ts` with a sensible default — probably the ledger time at which the new binary started, or zero with an explicit "treat as never-updated" semantic.
   d. Returns the new struct.
7. New enclave operates normally on the upgraded in-memory state.
8. First time the new enclave calls `save_state`, it writes version `0x0002`. From this point on, the disk format has been permanently upgraded.

**Rollback is asymmetric.** If you ever need to roll back to v1.4.1, that version has never seen the new format and cannot read version `0x0002` blobs. You either:
- Keep the old blob as a backup before the first upgrade save (mandatory operational step), or
- Write a *downgrader* in v1.4.2 that can re-serialise to v1 format on request, for rollback emergencies.

The downgrader is rarely worth the engineering cost unless the schema change is dangerous enough that you want a fast rollback path.

### Q: What if the new enclave needs to change the sealing policy itself (say, move from MRENCLAVE to MRSIGNER)?

This is the **dual-mode transition** pattern:

- Phase 0 — everything sealed with MRENCLAVE. You realise you need upgrades and decide to switch to MRSIGNER.
- Phase 1 — ship a **transition enclave** (call it v1.9) that, on startup:
  1. Reads the old MRENCLAVE-sealed blob.
  2. Unseals it using MRENCLAVE policy.
  3. Re-seals the same plaintext using MRSIGNER policy.
  4. Writes the re-sealed blob to a new file name (`state.mrsigner.sealed`) while leaving the old file intact as a safety net.
  5. From then on, writes only MRSIGNER-sealed blobs.
- Phase 2 — v2.0 and later enclaves only know about MRSIGNER sealing. The old MRENCLAVE blob is deleted after enough operational time has passed that you're confident the new format is working.

The critical property: **the transition enclave runs exactly once on each host to do the re-seal**. Crucially, the transition enclave must itself be built from the same source commit as v1 of the enclave (so its MRENCLAVE matches what sealed the old blob) and must also know the v2 sealing policy (so it can re-seal forward). This is why version transitions are sometimes implemented as a *ceremony* — you build and run a one-off binary just for the transition, then throw it away.

### Q: What if I completely rebuild the enclave signing key? (Say the old signing key was compromised and we rotate to a new one.)

This is the nuclear case. New signing key → new MRSIGNER → **none of your existing MRSIGNER-sealed blobs can be read by the new enclave, ever**.

The only mitigation path is to export the plaintext through the old enclave before rotating:

1. Build a **key-rotation ceremony enclave** using the old signing key. This enclave has one job: unseal everything and re-export the plaintext in a form the new enclave can consume.
2. The ceremony enclave must re-export the plaintext *without* exposing it to the untrusted host — that defeats the whole point. The standard way to do this is:
   a. Old ceremony enclave and new enclave both run on the same machine simultaneously.
   b. They perform **local attestation** to verify each other's identities.
   c. They establish an authenticated ephemeral key exchange (ECDH inside local attestation).
   d. Old enclave unseals, sends plaintext over the attested channel to new enclave.
   e. New enclave re-seals under the new MRSIGNER.
3. After the ceremony, old sealed files are deleted.

For a FROST 2-of-3 setup, this ceremony has to be performed on each of the three nodes, ideally simultaneously or at least before the signing quorum resumes, to avoid the cluster getting stuck with one node on old-key-sealed state and two on new-key-sealed state.

### Q: What happens if I just YOLO a rebuild and the new enclave can't read the old blob?

Depends what was in the blob:

- **FROST key shares** — catastrophic. You cannot sign anything anymore. You also cannot participate in a FROST quorum because you don't have your share. The only recovery is DKG from scratch, which means a new account (new multisig address), which means any funds in the old account are stuck unless the threshold of surviving operators can still sign a recovery tx on the old account (which requires *their* enclaves to still work, i.e., they didn't YOLO at the same time).
- **Vault state** — bad but not terminal. You can reconstruct position state from the XRPL transaction history (every position was the result of a signed tx on chain) with significant manual effort. You lose any in-enclave state that wasn't on-chain (e.g., unsubmitted orders, volatile internal counters).
- **User margin ledger** — very bad. Unlike the vault, margin is mutated by internal logic between on-chain events, so you cannot reconstruct it purely from chain history without replaying every deposit, withdrawal, funding payment, and fee in order. Possible but brittle.

**The operational rule is:** before changing anything inside the enclave, understand which category each sealed blob falls into, and have a migration plan for each one *before* bumping the version.

### Q: Is there any way to make sealed data portable between CPUs?

Not directly. The sealing key is derived from a per-CPU root. If you want a sealed blob to survive a CPU swap (or to replicate the enclave to a second host), you have to build an **export/import** pathway:

- Export: enclave on CPU A unseals its data, encrypts it under a key provided via a remote-attested channel from the enclave on CPU B, and the host pipes the ciphertext from A to B.
- Import: enclave on CPU B decrypts, verifies, re-seals under CPU B's sealing key.

The security of this hinges on correct remote attestation on both ends. It is not a straightforward "scp the file" operation.

### Q: And microcode updates — do they break sealing?

Sometimes. When Intel issues a mitigation for a new side-channel attack, CPUSVN can advance. If your sealing policy requires strict CPUSVN equality, the new enclave cannot unseal data that was sealed before the bump. If your sealing policy allows rollback to lower CPUSVN (the `SGX_KEYPOLICY_NOISVPRODID` flag and related), the new enclave can still read the old data.

There's a trade-off: rollback tolerance means a sealed blob from a vulnerable CPUSVN can still be read by a patched enclave, which might be exactly what you *don't* want if the vulnerability leaked your sealing key. In practice, for long-lived operational state (like a vault), rollback tolerance is usually the right call because the alternative — losing state on every microcode update — is operationally catastrophic.

---

## Part 7 — Our specific architecture

### Q: How is this actually laid out in the perp DEX?

From `project_fork_and_deploy.md` and the current codebase:

- **Enclave (C/C++) — `77ph/xrpl-perp-dex-enclave`**
  Holds FROST key shares, signs XRPL transactions, manages the per-user margin ledger (per the plan in `plans/jiggly-mapping-starfish.md`), maintains the vault state, runs the liquidation and funding logic. Sealed state for shares + vault + margin.

- **Orchestrator (Rust) — `LemonTreeTechnologies/xrpl-perp-dex`**
  Runs on the host outside the enclave. Owns the matching engine, user sessions, WebSocket + REST API, XRPL client (monitors ledger close events, submits signed txs returned from the enclave, relays results back via ECALL). Holds no key material.

- **Host OS (Ubuntu/Debian on Hetzner + Azure Confidential Computing)**
  Provides SGX driver, AESM daemon, DCAP infrastructure. Untrusted from the enclave's perspective.

### Q: Where is the boundary between "inside enclave" and "outside enclave" for the vault?

- **Inside enclave:** the vault's balance sheet, per-user positions, the signing keys, the logic that decides whether a withdrawal is permitted, the logic that computes funding payments, the logic that decides whether a position is liquidatable, the fixed-point math.
- **Outside enclave:** the CLOB order book, the matching engine, the price feeds, the XRPL client, the persistence layer (the host holds the ciphertext files), the REST/WebSocket server, the metrics/logs.

The orchestrator is the "nervous system" — it perceives the outside world and drives the enclave via ECALLs. The enclave is the "policy core" — it decides what is allowed, signs what it approves, and never touches the wire.

### Q: Which sealed blobs exist and what's their sealing policy (target state)?

*Target state, to be implemented by the enclave ecalls plan; current state depends on what has been wired.*

| Blob | Contains | Target policy | Rationale |
|---|---|---|---|
| `frost_share.sealed` | FROST key share for this node | MRSIGNER + versioned | Must survive enclave upgrades; recovery is impossible |
| `vault_state.sealed` | Vault balance sheet + open positions | MRSIGNER + versioned | Must survive upgrades; reconstructable from chain with effort |
| `margin_ledger.sealed` | Per-user margin accounts | MRSIGNER + versioned | Must survive upgrades; hard to reconstruct |
| `tx_dedup.sealed` | Processed tx hash table (deposit replay guard) | MRSIGNER + versioned | Must survive upgrades to keep replay guarantee |
| `nonce_ctr.sealed` | Monotonic signing counter | MRENCLAVE | Fresh on each binary; old counters are irrelevant and keeping them bound to the old binary closes a minor replay surface |

### Q: When is the "migration ceremony" pattern warranted for this project?

Three cases:

1. **Signing-key rotation.** If we ever rotate the enclave's signing key (i.e., the key used to sign the enclave binary itself, which determines MRSIGNER) — ceremony required. No shortcuts.
2. **Schema change that crosses an invariant.** If a new enclave version wants to interpret old data in a way that requires computing new fields from chain history — build an explicit upgrade path, do not rely on the seal-and-reload round-trip.
3. **Switching sealing policy itself** (e.g., adding AEAD KDF parameters, changing the SDK major version) — dual-mode transition enclave.

For routine changes — bug fixes, new ecalls, minor logic tweaks — the normal MRSIGNER sealing + schema versioning path is sufficient and no ceremony is needed.

---

## Part 8 — Known limitations and operational gotchas

### Q: Summarise the ones that have actually bitten this project or a close neighbour.

1. **DCAP attestation only works on Azure, not Hetzner.** Hetzner's SGX hardware does not expose the DCAP provisioning service. Remote attestation is Azure-only in practice for our deployment. Documented in `feedback_dcap_subprocess_pattern.md`. Workaround on Hetzner: local attestation only, no remote attestation endpoint.
2. **DCAP subprocess pattern.** Never split the DCAP `target_info` / `get_quote` flow across subprocesses, and never link the DCAP QL (quote library) into the enclave host directly — it corrupts state. See `feedback_dcap_subprocess_pattern.md`.
3. **Azure + DCAP two bugs.** `AZDCAP_DEBUG_LOG_LEVEL` must be set or stdout gets corrupted, and new VMs sometimes need deallocate+start to trigger PCK provisioning. See `feedback_dcap_azure_two_bugs.md`.
4. **EPC is not infinite.** Older SGX CPUs cap EPC at ~128MB. Newer server CPUs (Ice Lake Xeon and later) support up to 512GB, but you pay in page eviction if you exceed the hardware EPC. Eviction is transparent but slow — the CPU re-encrypts pages as they move in and out. For our vault with a few thousand positions this is not a concern, but for a CLOB with millions of resting orders it would be.
5. **Target platform is server Xeon only.** SGX was deprecated on consumer Core CPUs (11th/12th gen) in 2021 and isn't a viable development target. Server SGX on Xeon Scalable remains the supported platform; Intel's public position is that there are no plans to deprecate it (Intel support article 000089326). Don't try to run this on a laptop. Note also that Intel's legacy **EPID** attestation service was EOL'd on 2 April 2025 — DCAP is the only path for remote attestation now, and our design already assumes this.
6. **Every SGX vulnerability bumps CPUSVN.** If your sealing policy doesn't allow rollback, every microcode update is a mini-migration event. We've chosen rollback tolerance for long-lived state for this reason.
7. **Attack surface is not zero.** Side-channel attacks on SGX keep being discovered. SGX raises the bar substantially against the OS and physical attacker, but it is not a magical "unhackable" box. The FROST 2-of-3 design is our insurance against any single-enclave compromise.

### Q: How do we currently build and deploy the enclave?

Build happens on the SGX server in HW mode (`make SGX_MODE=HW SGX_DEBUG=1`), because the build toolchain expects access to a real SGX device for the signing step. Deployment is `scp` + restart today. The post-hackathon plan in `deployment-procedure.md` formalises this into a 2-of-3 signed, reproducibly-built, attested pipeline.

---

## Appendix A — One-line glossary

- **EPC** — Enclave Page Cache, the physically reserved region of RAM where enclave pages live.
- **MEE** — Memory Encryption Engine, the CPU block that transparently encrypts EPC memory.
- **ECALL** — Entry from untrusted host code into enclave code.
- **OCALL** — Exit from enclave code to untrusted host code (for syscalls, network, file I/O).
- **MRENCLAVE** — SHA-256 measurement of the enclave's code and initial data.
- **MRSIGNER** — SHA-256 hash of the enclave signer's public key.
- **CPUSVN** — CPU Security Version Number; advances when Intel ships microcode mitigations.
- **ISV_SVN** — Independent Software Vendor Security Version Number; app-level counter under your control.
- **EPID** — Older remote attestation scheme using group signatures; Intel-run attestation service required.
- **DCAP** — Datacenter Attestation Primitives; newer PKI-based remote attestation usable offline.
- **AESM** — Application Enclave Services Manager, the Intel-shipped daemon on the host that brokers attestation-related operations.
- **FROST** — Flexible Round-Optimized Schnorr Threshold signatures; our 2-of-3 scheme.
- **DKG** — Distributed Key Generation; how the FROST shares are created without anyone holding the full key.

## Appendix B — Cross-references

- `project_fork_and_deploy.md` — current deployment state and fork strategy.
- `deployment-procedure.md` — target post-PoC deployment with reproducible builds and 2-of-3 release signing.
- `feedback_dcap_subprocess_pattern.md` — DCAP quote/target_info pitfalls.
- `feedback_dcap_azure_two_bugs.md` — Azure-specific DCAP bugs.
- `plans/jiggly-mapping-starfish.md` — the 11-ecall perp engine plan; source of the sealed-blob taxonomy in §7.
- `xls-survey-for-perp-dex.md` — why XRPL on-chain programmability doesn't replace the enclave.
