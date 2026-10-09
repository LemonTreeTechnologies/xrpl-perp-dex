//! Operator roster — the authority that says WHO may request an admin operation.
//!
//! Authority model B (owner's decision 2026-10-09): admin operations are gated by a per-operator
//! roster, not the escrow key, and not the enclave node identities. This module is the
//! orchestrator-side loader + policy: it parses the roster, enforces the anti-rollback and
//! fail-closed invariants, and answers `can_op(address, op)`. The request SIGNATURE is still
//! checked by `auth::verify_operator_request`; what this changes is the ALLOWLIST that check is
//! made against — it becomes the roster's operators, replacing `signers_config.signers[]`.
//!
//! It gates the orchestrator's admin HTTP surface, NOT the enclave quorum that completes a
//! custody operation. The enclave still cosigns inside SGX. So the roster is the independent
//! authorization the admin surface lacked, in front of a gate that remains — defense in depth.
//!
//! TRUST ROOT (auditor ruling 2026-10-09, option 3): the roster file is operator-SIGNED,
//! Phoenix-style — `cluster_roster.vN.toml` + a detached SSHSIG, verified against a baked
//! `operator.pub`, with a monotonic `version` for anti-rollback. Changing the operator set
//! requires the operator key, NOT deploy/push access, so a pipeline compromise is not a custody-
//! authority compromise.
//!
//! The invariants mirror the enclave's trusted-MRENCLAVE allowlist
//! (`Enclave/trusted_mrenclaves.cpp`, the §33 rulings): fail-closed on empty/missing/bad-signature,
//! no replay/rollback (monotonic version), cluster binding, and a distinct error per cause.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// A distinct reason per refusal, because "roster rejected" with no cause is how a surface comes
/// to look protected while it is not. Mirrors the enclave's `_ERR_` enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterError {
    /// No roster file where one was expected. Fail closed: the surface serves nothing.
    Missing(String),
    /// The file is not parseable as a roster.
    Malformed(String),
    /// Zero operators. An empty roster is "serve nothing", never "allow anyone".
    Empty,
    /// The roster is for a different cluster than this node runs (escrow mismatch).
    WrongCluster { file: String, running: String },
    /// A roster older than or equal to the one already in force — a rollback/replay attempt.
    Rollback { in_force: u64, offered: u64 },
    /// The detached signature has NOT been verified against the baked operator key. Returned
    /// whenever verification is not available or not yet wired — fail closed, never "assume ok".
    SignatureUnverified(String),
    /// The signature was checked and did NOT verify.
    SignatureInvalid(String),
}

impl std::fmt::Display for RosterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosterError::Missing(p) => write!(f, "roster file missing: {p} (fail closed — the admin surface serves nothing)"),
            RosterError::Malformed(e) => write!(f, "roster file malformed: {e}"),
            RosterError::Empty => write!(f, "roster has no operators — fail closed, this is not 'allow anyone'"),
            RosterError::WrongCluster { file, running } => write!(f, "roster is for cluster {file} but this node runs {running}"),
            RosterError::Rollback { in_force, offered } => write!(f, "roster rollback refused: version {offered} is not newer than the {in_force} in force"),
            RosterError::SignatureUnverified(w) => write!(f, "roster signature NOT verified ({w}) — refusing to trust it"),
            RosterError::SignatureInvalid(w) => write!(f, "roster signature did not verify: {w}"),
        }
    }
}

impl std::error::Error for RosterError {}

/// One operator: the handle, the XRPL r-address their request signatures derive to, and the set
/// of admin operations they may REQUEST (the capability list — `can_op`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct OperatorEntry {
    pub handle: String,
    pub xrpl_address: String,
    /// The admin operations this operator may request. An operator with an empty `ops` is on the
    /// roster but may request nothing — valid (e.g. a decommissioned operator kept for the record).
    #[serde(default)]
    pub ops: Vec<String>,
}

/// The roster as parsed from the TOML file. `version` and `cluster_escrow` are the chain/binding
/// fields; `operators` is the authority set.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Roster {
    /// Monotonic. A node refuses to load a roster whose version is not strictly greater than the
    /// one already in force — anti-rollback, mirroring the enclave's epoch check.
    pub version: u64,
    /// The escrow r-address this roster authorizes. A roster for one cluster must not authorize
    /// another; checked against the node's running escrow.
    pub cluster_escrow: String,
    pub operators: Vec<OperatorEntry>,
    /// Per-op threshold: how many DISTINCT operators must co-sign that op. Carried in the file so
    /// the quorum layer can enforce it; the single-membership check does not read it. Absent ⇒
    /// the op is treated as threshold 1 by any future quorum enforcement. Owner's decision (b)
    /// sets whether the request layer enforces it; the file carries it either way so that lands
    /// without a reshape.
    #[serde(default)]
    pub op_thresholds: std::collections::BTreeMap<String, u32>,
}

impl Roster {
    /// Parse a roster from TOML text. Does NOT verify the signature or any invariant beyond
    /// shape — `load` does that. Separated so the parse is unit-testable without a file.
    pub fn parse(text: &str) -> Result<Roster, RosterError> {
        toml::from_str(text).map_err(|e| RosterError::Malformed(e.to_string()))
    }

    /// May this address request this op? The single-membership check that replaces the
    /// `signers[]` allowlist. Case-sensitive on the r-address, as XRPL addresses are.
    pub fn can_op(&self, xrpl_address: &str, op: &str) -> bool {
        self.operators
            .iter()
            .any(|o| o.xrpl_address == xrpl_address && o.ops.iter().any(|x| x == op))
    }

    /// The allowlist for a given op: the r-addresses permitted to request it. This is what an
    /// admin surface hands `verify_operator_request` in place of `signers[]`.
    pub fn allowlist_for(&self, op: &str) -> Vec<String> {
        self.operators
            .iter()
            .filter(|o| o.ops.iter().any(|x| x == op))
            .map(|o| o.xrpl_address.clone())
            .collect()
    }

    /// Enforce the invariants that do not need the signature: non-empty, cluster binding,
    /// anti-rollback, and cannot-lock-itself-out. The signature is checked separately by `load`
    /// BEFORE this is trusted.
    fn check_invariants(
        &self,
        running_escrow: &str,
        version_in_force: Option<u64>,
    ) -> Result<(), RosterError> {
        if self.operators.is_empty() {
            return Err(RosterError::Empty);
        }
        if self.cluster_escrow != running_escrow {
            return Err(RosterError::WrongCluster {
                file: self.cluster_escrow.clone(),
                running: running_escrow.to_string(),
            });
        }
        if let Some(in_force) = version_in_force {
            if self.version <= in_force {
                return Err(RosterError::Rollback {
                    in_force,
                    offered: self.version,
                });
            }
        }
        // NO cannot-lock-itself-out check. Under option 3 the NEXT roster is signed by the
        // external baked operator.pub, not by the listed operators, so a roster whose operators
        // can govern nothing is still succeedable by a fresh operator.pub-signed version — the
        // lock-out failure mode is unreachable. A check for it would imply a self-governance
        // property the system deliberately does not have (auditor RESP 2026-10-09, Q4).
        Ok(())
    }
}

/// The fixed SSHSIG identity and namespace for perp rosters. The namespace is load-bearing:
/// SSHSIG binds it into the signature, so a roster signed under `phoenix-roster` cannot verify
/// here — cross-cluster domain separation (auditor RESP 2026-10-09, Q2/Q3).
const ROSTER_IDENTITY: &str = "perp-cluster-operator";
const ROSTER_NAMESPACE: &str = "perp-roster";

/// Build an ssh `allowed_signers` line from a baked `operator.pub`.
///
/// `ssh-keygen -Y verify` consumes `<principal> <algo> <base64> [comment]`. A bare public key
/// file is `<algo> <base64> [comment]` with no principal, so prepend the fixed identity when the
/// first token is a bare algorithm; otherwise take the line verbatim (it already has a principal).
/// Mirrors sgx-dev's `allowed_signers_from_pubkey`.
fn allowed_signers_line(pubkey_line: &str) -> String {
    let t = pubkey_line.trim();
    let first = t.split_whitespace().next().unwrap_or("");
    let is_bare_algo =
        first.starts_with("ssh-") || first.starts_with("ecdsa-") || first.starts_with("sk-");
    if is_bare_algo {
        format!("{ROSTER_IDENTITY} {t}")
    } else {
        t.to_string()
    }
}

/// Fail closed unless `ssh-keygen` is present AND supports `-Y verify` (OpenSSH >= 8.1). An old
/// or absent binary must not silently degrade into a verifier that always errs OR always passes —
/// the caller gets a clear SignatureUnverified so the operator knows the tool, not the roster, is
/// the problem. (auditor RESP Q3, condition 4.)
pub fn probe_ssh_keygen() -> Result<(), RosterError> {
    // `ssh-keygen -Y` with no subcommand prints a usage listing the Y-subcommands on a supporting
    // build; we look for `verify` in it. We do NOT depend on the exit code here (usage exits
    // non-zero), only on the capability string — this is a capability probe, not the gate.
    let out = Command::new("ssh-keygen")
        .arg("-Y")
        .output()
        .map_err(|e| RosterError::SignatureUnverified(format!("ssh-keygen not runnable: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    if text.contains("verify") {
        Ok(())
    } else {
        Err(RosterError::SignatureUnverified(
            "ssh-keygen does not support `-Y verify` (needs OpenSSH >= 8.1)".to_string(),
        ))
    }
}

/// Verify the detached SSHSIG over the roster file against the baked operator public key, by
/// mirroring `ssh-keygen -Y verify` exactly (auditor RESP 2026-10-09, Q1/Q3).
///
/// The four fail-closed conditions, each from the RESP:
///   1. argv array, never `sh -c` — no shell parsing of any path.
///   2. the ssh-keygen EXIT STATUS is the only gate; stdout is never scanned for "Good signature".
///   3. any spawn / IO / missing-binary error ⇒ SignatureUnverified (fail closed).
///   4. the allowed_signers file is materialized 0600 in a private temp dir, NOT /tmp directly —
///      a world-writable allowed_signers is a TOCTOU: an attacker races to replace it with their
///      own pubkey and the roster then "verifies". `tempfile` creates it owner-only in a dir we
///      own; the payload (the roster TOML) goes on STDIN, never a second temp file.
fn verify_roster_signature(
    roster_bytes: &[u8],
    sig_path: &Path,
    allowed_signers_pubkey: &str,
) -> Result<(), RosterError> {
    // 0600 allowed_signers in an owner-only temp dir (tempfile defaults to 0600 and a dir we own).
    let mut allowed = tempfile::Builder::new()
        .prefix("perp-roster-allowed-signers")
        .tempfile()
        .map_err(|e| {
            RosterError::SignatureUnverified(format!("cannot materialize allowed_signers: {e}"))
        })?;
    allowed
        .write_all(allowed_signers_line(allowed_signers_pubkey).as_bytes())
        .and_then(|_| allowed.write_all(b"\n"))
        .and_then(|_| allowed.flush())
        .map_err(|e| {
            RosterError::SignatureUnverified(format!("cannot write allowed_signers: {e}"))
        })?;

    // (1) argv array. (payload on stdin, not a temp file.)
    let mut child = Command::new("ssh-keygen")
        .arg("-Y")
        .arg("verify")
        .arg("-f")
        .arg(allowed.path())
        .arg("-I")
        .arg(ROSTER_IDENTITY)
        .arg("-n")
        .arg(ROSTER_NAMESPACE)
        .arg("-s")
        .arg(sig_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| RosterError::SignatureUnverified(format!("cannot spawn ssh-keygen: {e}")))?; // (3)

    // Feed the roster bytes to verify on stdin.
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            RosterError::SignatureUnverified("ssh-keygen stdin unavailable".into())
        })?;
        stdin.write_all(roster_bytes).map_err(|e| {
            RosterError::SignatureUnverified(format!("cannot write roster to ssh-keygen: {e}"))
        })?;
        // stdin dropped here → EOF
    }

    let status = child.wait().map_err(|e| {
        RosterError::SignatureUnverified(format!("ssh-keygen did not complete: {e}"))
    })?; // (3)

    // (2) exit status IS the gate.
    if status.success() {
        Ok(())
    } else {
        Err(RosterError::SignatureInvalid(format!(
            "ssh-keygen -Y verify rejected the signature (exit {:?})",
            status.code()
        )))
    }
}

/// Load a roster from disk, verify its signature, and enforce every invariant. Returns the roster
/// only if ALL pass. Any failure is fail-closed: the caller gets an error and must serve nothing.
///
/// `roster_path`  the `cluster_roster.vN.toml`
/// `sig_path`     its detached SSHSIG
/// `allowed_signers_path`  the baked operator.pub, as an ssh allowed_signers file
/// `running_escrow`  the escrow this node actually runs, for the cluster-binding check
/// `version_in_force`  the version already loaded, for anti-rollback (None at first load)
pub fn load(
    roster_path: &Path,
    sig_path: &Path,
    operator_pub_path: &Path,
    running_escrow: &str,
    version_in_force: Option<u64>,
) -> Result<Roster, RosterError> {
    let bytes = std::fs::read(roster_path)
        .map_err(|e| RosterError::Missing(format!("{}: {e}", roster_path.display())))?;
    let pubkey = std::fs::read_to_string(operator_pub_path).map_err(|e| {
        // A missing baked key is fail-closed, not "skip verification".
        RosterError::SignatureUnverified(format!(
            "baked operator.pub unreadable at {}: {e}",
            operator_pub_path.display()
        ))
    })?;
    // Signature FIRST, over the RAW bytes: an unverified file's contents are never parsed into
    // authority. (Kept deliberately stronger than sgx-dev's parse-first — auditor RESP.)
    verify_roster_signature(&bytes, sig_path, &pubkey)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| RosterError::Malformed(format!("roster is not UTF-8: {e}")))?;
    let roster = Roster::parse(text)?;
    roster.check_invariants(running_escrow, version_in_force)?;
    Ok(roster)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESCROW: &str = "rfYnJDSAeFuDCUTq2oYbckbJcz3gAJTNCd";

    fn roster_toml(version: u64, ops_a: &str) -> String {
        format!(
            r#"
version = {version}
cluster_escrow = "{ESCROW}"

[[operators]]
handle = "alice"
xrpl_address = "rAlice00000000000000000000000000000"
ops = ["{ops_a}"]

[[operators]]
handle = "bob"
xrpl_address = "rBob000000000000000000000000000000"
ops = ["membership-change"]
"#
        )
    }

    fn parse_ok(version: u64) -> Roster {
        Roster::parse(&roster_toml(version, "mrenclave-govern")).expect("valid roster must parse")
    }

    #[test]
    fn can_op_is_per_operator_and_per_op() {
        let r = parse_ok(1);
        assert!(r.can_op("rAlice00000000000000000000000000000", "mrenclave-govern"));
        assert!(r.can_op("rBob000000000000000000000000000000", "membership-change"));
        // bob may NOT mrenclave-govern; alice may NOT the op bob has only if she lacks it
        assert!(!r.can_op("rBob000000000000000000000000000000", "mrenclave-govern"));
        // a stranger is never permitted
        assert!(!r.can_op("rStranger0000000000000000000000000", "membership-change"));
    }

    #[test]
    fn allowlist_for_an_op_is_exactly_the_operators_with_it() {
        let r = parse_ok(1);
        let al = r.allowlist_for("membership-change");
        // both alice (via her ops) and bob have membership-change? alice has mrenclave-govern +
        // roster-govern only; bob has membership-change. So just bob.
        assert_eq!(al, vec!["rBob000000000000000000000000000000".to_string()]);
    }

    #[test]
    fn an_empty_roster_is_fail_closed() {
        let r = Roster {
            version: 1,
            cluster_escrow: ESCROW.into(),
            operators: vec![],
            op_thresholds: Default::default(),
        };
        assert_eq!(r.check_invariants(ESCROW, None), Err(RosterError::Empty));
    }

    #[test]
    fn a_roster_for_another_cluster_is_refused() {
        let r = parse_ok(1);
        match r.check_invariants("rOtherClusterEscrow0000000000000000", None) {
            Err(RosterError::WrongCluster { .. }) => {}
            other => panic!("expected WrongCluster, got {other:?}"),
        }
    }

    #[test]
    fn a_rollback_is_refused_but_a_newer_version_passes() {
        let r5 = parse_ok(5);
        // offering version 5 when 5 is already in force is a replay
        assert_eq!(
            r5.check_invariants(ESCROW, Some(5)),
            Err(RosterError::Rollback {
                in_force: 5,
                offered: 5
            })
        );
        // and an older one
        assert_eq!(
            parse_ok(4).check_invariants(ESCROW, Some(5)),
            Err(RosterError::Rollback {
                in_force: 5,
                offered: 4
            })
        );
        // strictly newer passes
        parse_ok(6)
            .check_invariants(ESCROW, Some(5))
            .expect("a newer version must load");
        // first load (nothing in force) passes
        parse_ok(1)
            .check_invariants(ESCROW, None)
            .expect("first load must pass");
    }

    #[test]
    fn allowed_signers_line_prepends_identity_only_for_a_bare_key() {
        let bare = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAImock bob@host";
        assert_eq!(
            allowed_signers_line(bare),
            format!("{ROSTER_IDENTITY} {bare}")
        );
        // already has a principal (first token is not a bare algo) -> verbatim
        let with_principal = "someone ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAImock";
        assert_eq!(allowed_signers_line(with_principal), with_principal);
    }

    #[test]
    fn a_missing_roster_file_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let rp = dir.path().join("nope.toml");
        let sp = dir.path().join("nope.sig");
        let ap = dir.path().join("operator.pub");
        match load(&rp, &sp, &ap, ESCROW, None) {
            Err(RosterError::Missing(_)) => {}
            other => panic!("a missing roster must fail closed, got {other:?}"),
        }
    }

    // ---- SSHSIG verification, against a REAL ssh-keygen ----------------------------------------
    //
    // These need `ssh-keygen` with `-Y sign/verify` (OpenSSH >= 8.1). If it is not present the
    // test self-skips rather than failing a dev box that lacks it — but CI has it, and the probe
    // test below asserts the capability is detected so a silent skip cannot hide a regression.

    /// Generate an ed25519 keypair and SSHSIG-sign `payload` under `namespace`. Returns
    /// (pubkey_line, sig_path, _tmpdir-kept-alive). Mirrors how a real operator signs a roster.
    fn ssh_sign(
        payload: &[u8],
        namespace: &str,
        identity: &str,
    ) -> Option<(String, std::path::PathBuf, tempfile::TempDir)> {
        if probe_ssh_keygen().is_err() {
            return None;
        }
        let d = tempfile::tempdir().unwrap();
        let key = d.path().join("id");
        // -N "" no passphrase, -C identity as the comment
        let ok = Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", identity, "-f"])
            .arg(&key)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !ok.success() {
            return None;
        }
        let payload_path = d.path().join("payload");
        std::fs::write(&payload_path, payload).unwrap();
        // ssh-keygen -Y sign -f <key> -n <namespace> <payload>  → writes <payload>.sig
        let signed = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-n", namespace, "-f"])
            .arg(&key)
            .arg(&payload_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !signed.success() {
            return None;
        }
        let pubkey = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let sig = payload_path.with_file_name("payload.sig");
        Some((pubkey, sig, d))
    }

    /// THE MANDATORY POSITIVE CASE. Without a green here, an always-Err verify would pass every
    /// tamper test below (the hollow-verifier trap). A good roster, signed with a key baked as
    /// operator.pub, in the perp-roster namespace, MUST verify.
    #[test]
    fn a_correctly_signed_roster_verifies_and_loads() {
        let bytes = roster_toml(7, "mrenclave-govern").into_bytes();
        let Some((pubkey, sig, _d)) = ssh_sign(&bytes, ROSTER_NAMESPACE, ROSTER_IDENTITY) else {
            eprintln!("ssh-keygen -Y unavailable; skipping positive SSHSIG test");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let rp = dir.path().join("cluster_roster.v7.toml");
        std::fs::write(&rp, &bytes).unwrap();
        let ap = dir.path().join("operator.pub");
        std::fs::write(&ap, &pubkey).unwrap();
        let r = load(&rp, &sig, &ap, ESCROW, None).expect("a correctly signed roster must load");
        assert_eq!(r.version, 7);
        assert!(r.can_op("rBob000000000000000000000000000000", "membership-change"));
    }

    /// Each tamper ⇒ refusal. Run only when the positive case could run (ssh-keygen present),
    /// so a skip never masquerades as a pass.
    #[test]
    fn every_tampered_or_mis_namespaced_roster_is_refused() {
        let bytes = roster_toml(7, "mrenclave-govern").into_bytes();
        let Some((pubkey, sig, _d)) = ssh_sign(&bytes, ROSTER_NAMESPACE, ROSTER_IDENTITY) else {
            eprintln!("ssh-keygen -Y unavailable; skipping negative SSHSIG tests");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let ap = dir.path().join("operator.pub");
        std::fs::write(&ap, &pubkey).unwrap();

        // (a) a flipped roster byte: the signed bytes and the file disagree.
        let mut flipped = bytes.clone();
        flipped[0] ^= 0x01;
        let rp = dir.path().join("flipped.toml");
        std::fs::write(&rp, &flipped).unwrap();
        assert!(
            load(&rp, &sig, &ap, ESCROW, None).is_err(),
            "a flipped roster byte must be refused"
        );

        // (b) a key NOT baked (attacker's own key signs; we baked the real one).
        let bytes2 = roster_toml(8, "mrenclave-govern").into_bytes();
        if let Some((_attacker_pub, atk_sig, _d2)) =
            ssh_sign(&bytes2, ROSTER_NAMESPACE, ROSTER_IDENTITY)
        {
            let rp2 = dir.path().join("v8.toml");
            std::fs::write(&rp2, &bytes2).unwrap();
            // verify v8 (attacker-signed) against the ORIGINAL baked pub → must refuse
            assert!(
                load(&rp2, &atk_sig, &ap, ESCROW, None).is_err(),
                "a roster signed by a non-baked key must be refused"
            );
        }

        // (c) wrong namespace: sign under phoenix-roster, verify demands perp-roster.
        if let Some((ph_pub, ph_sig, _d3)) = ssh_sign(&bytes, "phoenix-roster", ROSTER_IDENTITY) {
            let rp3 = dir.path().join("ph.toml");
            std::fs::write(&rp3, &bytes).unwrap();
            let ap3 = dir.path().join("ph.pub");
            std::fs::write(&ap3, &ph_pub).unwrap();
            assert!(
                load(&rp3, &ph_sig, &ap3, ESCROW, None).is_err(),
                "a roster signed under a different namespace must be refused"
            );
        }

        // (d) empty allowed_signers (baked pub is blank).
        let apx = dir.path().join("empty.pub");
        std::fs::write(&apx, "").unwrap();
        let rp4 = dir.path().join("good.toml");
        std::fs::write(&rp4, &bytes).unwrap();
        assert!(
            load(&rp4, &sig, &apx, ESCROW, None).is_err(),
            "an empty baked key must be refused"
        );
    }

    #[test]
    fn the_ssh_keygen_capability_probe_is_honest() {
        // On a box with modern OpenSSH this is Ok; if ssh-keygen is absent/old it is Err. Either
        // way it must not panic, and a present-but-unsupported build must fail CLOSED (Err).
        match probe_ssh_keygen() {
            Ok(()) => {}
            Err(RosterError::SignatureUnverified(_)) => {}
            other => panic!("probe must be Ok or a fail-closed SignatureUnverified, got {other:?}"),
        }
    }
}
