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

use std::collections::BTreeSet;
use std::path::Path;

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
    /// Loading this roster would leave no operator able to govern the roster itself — refused,
    /// mirroring trusted_mrenclaves' "cannot lock itself out".
    WouldLockOut,
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
            RosterError::WouldLockOut => write!(f, "roster would leave no operator able to govern it — refused"),
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
    /// the op is treated as threshold 1 by any future quorum enforcement.
    #[serde(default)]
    pub op_thresholds: std::collections::BTreeMap<String, u32>,
    /// The op that governs the roster itself (adding/removing operators). Its operators are the
    /// ones who may sign the NEXT roster version. Used by the cannot-lock-itself-out check.
    #[serde(default = "default_govern_op")]
    pub roster_govern_op: String,
}

fn default_govern_op() -> String {
    "roster-govern".to_string()
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
        // Cannot lock itself out: at least one operator must be able to govern the roster, or no
        // future version could ever be authorised. Distinct r-addresses only, so a duplicate does
        // not fake a governing operator.
        let governors: BTreeSet<&str> = self
            .operators
            .iter()
            .filter(|o| o.ops.iter().any(|x| *x == self.roster_govern_op))
            .map(|o| o.xrpl_address.as_str())
            .collect();
        if governors.is_empty() {
            return Err(RosterError::WouldLockOut);
        }
        Ok(())
    }
}

/// Verify the detached SSHSIG over the roster file against the baked operator public key.
///
/// NOT YET IMPLEMENTED, and it FAILS CLOSED until it is. The auditor ruled the Phoenix-style
/// SSHSIG shape (`ssh-keygen -Y verify` against a baked `operator.pub`, namespace-scoped); the
/// exact allowed_signers format, namespace and invocation must MIRROR sgx-dev's roster.rs rather
/// than be invented here, because a subtly-wrong signature check is a silent authority bypass.
/// Until that shape is in hand this returns `SignatureUnverified`, so `load` cannot trust any
/// roster — there is no code path that treats an unverified roster as valid.
fn verify_roster_signature(
    _roster_path: &Path,
    _sig_path: &Path,
    _allowed_signers_path: &Path,
) -> Result<(), RosterError> {
    Err(RosterError::SignatureUnverified(
        "SSHSIG verification not yet wired — awaiting the mirrored ssh-keygen -Y verify shape"
            .to_string(),
    ))
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
    allowed_signers_path: &Path,
    running_escrow: &str,
    version_in_force: Option<u64>,
) -> Result<Roster, RosterError> {
    let text = std::fs::read_to_string(roster_path)
        .map_err(|e| RosterError::Missing(format!("{}: {e}", roster_path.display())))?;
    // Signature FIRST: an unverified file's contents are not to be parsed into authority.
    verify_roster_signature(roster_path, sig_path, allowed_signers_path)?;
    let roster = Roster::parse(&text)?;
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
ops = ["{ops_a}", "roster-govern"]

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
            roster_govern_op: "roster-govern".into(),
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
            Err(RosterError::Rollback { in_force: 5, offered: 5 })
        );
        // and an older one
        assert_eq!(
            parse_ok(4).check_invariants(ESCROW, Some(5)),
            Err(RosterError::Rollback { in_force: 5, offered: 4 })
        );
        // strictly newer passes
        parse_ok(6).check_invariants(ESCROW, Some(5)).expect("a newer version must load");
        // first load (nothing in force) passes
        parse_ok(1).check_invariants(ESCROW, None).expect("first load must pass");
    }

    #[test]
    fn a_roster_no_one_can_govern_is_refused() {
        // nobody has roster-govern -> no future version could be authorised -> locked out
        let text = format!(
            r#"
version = 1
cluster_escrow = "{ESCROW}"
[[operators]]
handle = "alice"
xrpl_address = "rAlice00000000000000000000000000000"
ops = ["membership-change"]
"#
        );
        let r = Roster::parse(&text).unwrap();
        assert_eq!(r.check_invariants(ESCROW, None), Err(RosterError::WouldLockOut));
    }

    /// THE SIGNATURE GATE FAILS CLOSED. Until the mirrored SSHSIG verify is wired, `load` must
    /// refuse every roster — there must be no path that trusts an unverified file. This is the
    /// assertion that keeps the half-built loader from being an authority bypass.
    #[test]
    fn load_fails_closed_while_signature_verification_is_unwired() {
        let dir = tempfile::tempdir().unwrap();
        let rp = dir.path().join("cluster_roster.v1.toml");
        std::fs::write(&rp, roster_toml(1, "mrenclave-govern")).unwrap();
        let sp = dir.path().join("cluster_roster.v1.toml.sig");
        std::fs::write(&sp, "not-a-real-sig").unwrap();
        let ap = dir.path().join("operator.pub");
        std::fs::write(&ap, "ssh-ed25519 AAAA...").unwrap();
        match load(&rp, &sp, &ap, ESCROW, None) {
            Err(RosterError::SignatureUnverified(_)) => {}
            other => panic!("load must fail closed on the unwired signature, got {other:?}"),
        }
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
}
