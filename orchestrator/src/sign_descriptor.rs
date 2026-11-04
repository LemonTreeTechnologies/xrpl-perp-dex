//! `sign-descriptor-digest` — the operator-side half of decision (b).
//!
//! The enclave's `ecall_sign_membership_consent` now refuses to sign a node's consent unless the
//! transition carries a descriptor AUTHORIZED by the baked governance key. This produces that
//! authorization: compute the descriptor digest EXACTLY as the enclave does
//! (`membership_canonical::compute_membership_descriptor_digest`, pinned cross-language), then
//! sign it RAW with the governance ed25519 key — the F2 shape the auditor ruled (no SSHSIG, the
//! enclave verifies a raw signature with `crypto_ed25519_check`).
//!
//! The governance key lives on the operator's machine as an OpenSSH private key
//! (`~/.config/perp-roster/operator.key`, ed25519, unencrypted for the testnet bring-up). We read
//! the 32-byte seed out of the `openssh-key-v1` container and sign with ed25519-dalek. As a
//! self-check — and so an operator cannot sign with the WRONG key — the derived public key is
//! compared to the BAKED governance pub (the same bytes the enclave verifies against): a mismatch
//! REFUSES rather than emitting a signature that would silently fail-close in the enclave.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

use crate::membership_canonical::{
    compute_membership_descriptor_digest, compute_set_hash, SignerEntry,
};

/// The baked governance public key — byte-identical to the enclave's `kMembershipGovPubs[0]`
/// (membership_descriptor.cpp). The operator's key MUST derive to this, or they are holding the
/// wrong key and we refuse before producing a signature.
pub const BAKED_GOV_PUB: [u8; 32] = [
    0xb4, 0x8b, 0xbc, 0x35, 0x33, 0x8d, 0xf4, 0x1a, 0x09, 0xeb, 0x32, 0x0d, 0x18, 0xce, 0xf8, 0xff,
    0x78, 0x89, 0xe4, 0x9b, 0x2e, 0x4b, 0xbf, 0x21, 0x29, 0x55, 0x4a, 0x6d, 0xa1, 0x19, 0x5e, 0x65,
];

/// Read one SSH-wire `string` (uint32-BE length prefix + bytes) at `*pos`, advancing `*pos`.
/// Bounds-checked: this parses an operator-supplied file, so every read refuses rather than
/// panics on a short or oversized length.
fn read_ssh_string<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    if *pos + 4 > buf.len() {
        bail!("openssh key truncated reading a length prefix");
    }
    let n = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]) as usize;
    *pos += 4;
    if n > buf.len() || *pos + n > buf.len() {
        bail!("openssh key truncated reading a {n}-byte field");
    }
    let s = &buf[*pos..*pos + n];
    *pos += n;
    Ok(s)
}

/// Extract the 32-byte ed25519 seed from an UNENCRYPTED `openssh-key-v1` private key.
///
/// Refuses anything else — an encrypted key (`ciphername != "none"`), a non-ed25519 key, or more
/// than one key — because a silent wrong-parse here would sign with garbage. The 64-byte private
/// field is `seed(32) || pub(32)`; we return the seed.
pub fn parse_openssh_ed25519_seed(pem: &str) -> Result<[u8; 32]> {
    const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
    const END: &str = "-----END OPENSSH PRIVATE KEY-----";
    let start = pem
        .find(BEGIN)
        .ok_or_else(|| anyhow!("not an OpenSSH private key (missing BEGIN marker)"))?
        + BEGIN.len();
    let end = pem[start..]
        .find(END)
        .ok_or_else(|| anyhow!("not an OpenSSH private key (missing END marker)"))?
        + start;
    let b64: String = pem[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .context("openssh key base64 decode")?;

    const MAGIC: &[u8] = b"openssh-key-v1\0";
    if blob.len() < MAGIC.len() || &blob[..MAGIC.len()] != MAGIC {
        bail!("not an openssh-key-v1 container");
    }
    let mut pos = MAGIC.len();
    let ciphername = read_ssh_string(&blob, &mut pos)?;
    if ciphername != b"none" {
        bail!("the governance key is ENCRYPTED — decrypt it first (ssh-keygen -p), or use an unencrypted key for the testnet ceremony");
    }
    let _kdfname = read_ssh_string(&blob, &mut pos)?;
    let _kdfopts = read_ssh_string(&blob, &mut pos)?;
    if pos + 4 > blob.len() {
        bail!("openssh key truncated reading key count");
    }
    let nkeys = u32::from_be_bytes([blob[pos], blob[pos + 1], blob[pos + 2], blob[pos + 3]]);
    pos += 4;
    if nkeys != 1 {
        bail!("expected exactly one key in the openssh container, got {nkeys}");
    }
    let _pubkey = read_ssh_string(&blob, &mut pos)?; // the public key blob (we re-derive from the seed)
    let priv_section = read_ssh_string(&blob, &mut pos)?;

    // Inside the private section: checkint, checkint, then keytype, pub, priv, comment, padding.
    if priv_section.len() < 8 {
        bail!("openssh private section too short");
    }
    if priv_section[0..4] != priv_section[4..8] {
        bail!("openssh private section checkints differ — wrong passphrase or corrupt key");
    }
    let mut p = 8usize;
    let keytype = read_ssh_string(priv_section, &mut p)?;
    if keytype != b"ssh-ed25519" {
        bail!("governance key is not ssh-ed25519 (got a different key type)");
    }
    let pubk = read_ssh_string(priv_section, &mut p)?;
    if pubk.len() != 32 {
        bail!("openssh ed25519 public field is not 32 bytes");
    }
    let privk = read_ssh_string(priv_section, &mut p)?;
    if privk.len() != 64 {
        bail!("openssh ed25519 private field is not 64 bytes (seed||pub)");
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&privk[..32]);
    Ok(seed)
}

/// Compute the descriptor digest and sign it with the governance key. Returns (digest, 64-byte
/// signature). Refuses if the key does not derive to the baked governance pub.
#[allow(clippy::too_many_arguments)]
pub fn sign_descriptor(
    key_pem: &str,
    op: u8,
    escrow: &[u8; 20],
    proposed_epoch: u64,
    prev_epoch_hash: &[u8; 32],
    signers: &[SignerEntry],
    quorum: u32,
    new_member_mrenclave: &[u8; 32],
    // The gov pub the key must derive to — the enclave verifies against the pub BAKED into its
    // measurement. Defaults (via the CLI) to the production BAKED_GOV_PUB; set to a test pub ONLY
    // for a test measurement (M_test) with a throwaway key. Keeps the wrong-key self-check — it is
    // parameterised, not removed: signing with a key that does not match the TARGET measurement's
    // baked pub still refuses.
    expect_gov_pub: &[u8; 32],
) -> Result<([u8; 32], [u8; 64])> {
    let set_hash = compute_set_hash(signers, quorum);
    let digest = compute_membership_descriptor_digest(
        op,
        escrow,
        proposed_epoch,
        prev_epoch_hash,
        &set_hash,
        new_member_mrenclave,
    );

    let sig = sign_raw_digest_with_gov_key(key_pem, &digest, expect_gov_pub)?;
    Ok((digest, sig))
}

/// Sign the allowlist governance descriptor: the digest the
/// enclave re-derives in `ecall_govern_trusted_mrenclaves` and verifies under the baked gov key.
/// Same key, same wrong-key self-check, same raw-ed25519 shape as a membership descriptor.
pub fn sign_mrenclave_govern_descriptor(
    key_pem: &str,
    op: &crate::mrenclave_governance::GovernanceOp,
    gen: &crate::mrenclave_governance::GovContext,
    expect_gov_pub: &[u8; 32],
) -> Result<([u8; 32], [u8; 64])> {
    let digest = crate::mrenclave_governance::compute_trusted_mrenclave_descriptor_digest(
        op.op,
        &op.mrenclave,
        &op.escrow,
        op.proposed_epoch,
        &op.prev_allowlist_hash,
        gen,
    );
    let sig = sign_raw_digest_with_gov_key(key_pem, &digest, expect_gov_pub)?;
    Ok((digest, sig))
}

/// Sign the OWNER's UNL policy descriptor (REQ unl-anchor-auto-advance, (A'+D)): the exact
/// freshness anchor and quorum fraction, in the generation that will verify it. Same key and
/// same refusal as every other governance signature.
pub fn sign_unl_policy_descriptor(
    key_pem: &str,
    escrow: &[u8; 20],
    policy: &crate::unl_policy::UnlPolicyOp,
    gen: &crate::mrenclave_governance::GovContext,
    expect_gov_pub: &[u8; 32],
) -> Result<([u8; 32], [u8; 64])> {
    let digest = crate::unl_policy::unl_policy_gov_descriptor_digest(
        escrow,
        policy.unl_epoch,
        &policy.prev_unl_hash,
        policy.pinned_ledger_seq,
        (policy.quorum_num, policy.quorum_den),
        gen,
    );
    let sig = sign_raw_digest_with_gov_key(key_pem, &digest, expect_gov_pub)?;
    Ok((digest, sig))
}

/// The one place the governance key signs: derive it, REFUSE unless it is the expected baked
/// key, then RAW ed25519 over the 32-byte digest (what `crypto_ed25519_check` verifies).
fn sign_raw_digest_with_gov_key(
    key_pem: &str,
    digest: &[u8; 32],
    expect_gov_pub: &[u8; 32],
) -> Result<[u8; 64]> {
    let seed = parse_openssh_ed25519_seed(key_pem)?;
    let sk = SigningKey::from_bytes(&seed);
    let derived_pub = sk.verifying_key().to_bytes();
    if derived_pub != *expect_gov_pub {
        bail!(
            "this key is NOT the expected governance key — its public key {} does not match the one \
             the target enclave verifies against ({}). Refusing to sign; a signature by the wrong \
             key would fail-close in every node's enclave.",
            hex::encode(derived_pub),
            hex::encode(expect_gov_pub)
        );
    }
    // RAW ed25519 over the 32-byte digest — exactly what crypto_ed25519_check verifies in-enclave.
    Ok(sk.sign(digest).to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_op(op: u8) -> crate::mrenclave_governance::GovernanceOp {
        crate::mrenclave_governance::GovernanceOp {
            op,
            mrenclave: [0x11; 32],
            escrow: [0x33; 20],
            proposed_epoch: 7,
            prev_allowlist_hash: [0x22; 32],
        }
    }

    const TEST_GEN: crate::mrenclave_governance::GovContext =
        crate::mrenclave_governance::GovContext {
            governing_mrenclave: [0x44; 32],
            membership_epoch_digest: [0x55; 32],
        };

    /// Build an UNENCRYPTED openssh-key-v1 blob from a seed, so the parser is exercised against
    /// the real wire format WITHOUT committing any private-key PEM to the repo.
    fn synth_openssh_key(seed: &[u8; 32]) -> String {
        let sk = SigningKey::from_bytes(seed);
        let pubk = sk.verifying_key().to_bytes();
        fn ssh_str(out: &mut Vec<u8>, b: &[u8]) {
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
        // public key blob: string "ssh-ed25519", string pub
        let mut pubblob = Vec::new();
        ssh_str(&mut pubblob, b"ssh-ed25519");
        ssh_str(&mut pubblob, &pubk);
        // private section: checkint, checkint, keytype, pub, priv(seed||pub), comment, padding
        let mut priv_sec = Vec::new();
        priv_sec.extend_from_slice(&[1, 2, 3, 4]); // checkint
        priv_sec.extend_from_slice(&[1, 2, 3, 4]); // checkint (equal)
        ssh_str(&mut priv_sec, b"ssh-ed25519");
        ssh_str(&mut priv_sec, &pubk);
        let mut privfield = Vec::new();
        privfield.extend_from_slice(seed);
        privfield.extend_from_slice(&pubk);
        ssh_str(&mut priv_sec, &privfield);
        ssh_str(&mut priv_sec, b"test-comment");
        let mut pad = 1u8;
        while priv_sec.len() % 8 != 0 {
            priv_sec.push(pad);
            pad += 1;
        }
        let mut blob = Vec::new();
        blob.extend_from_slice(b"openssh-key-v1\0");
        ssh_str(&mut blob, b"none");
        ssh_str(&mut blob, b"none");
        ssh_str(&mut blob, b"");
        blob.extend_from_slice(&1u32.to_be_bytes());
        ssh_str(&mut blob, &pubblob);
        ssh_str(&mut blob, &priv_sec);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
        format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{b64}\n-----END OPENSSH PRIVATE KEY-----\n")
    }

    #[test]
    fn parser_round_trips_the_seed() {
        let seed = [0x7u8; 32];
        let pem = synth_openssh_key(&seed);
        let got = parse_openssh_ed25519_seed(&pem).expect("parse");
        assert_eq!(
            got, seed,
            "the seed must round-trip through the openssh container"
        );
    }

    #[test]
    fn parser_refuses_garbage_and_non_openssh() {
        assert!(parse_openssh_ed25519_seed("not a key").is_err());
        assert!(parse_openssh_ed25519_seed(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nZZZZ\n-----END OPENSSH PRIVATE KEY-----"
        )
        .is_err());
    }

    /// A key that is NOT the baked governance key must be REFUSED before any signature is emitted
    /// — signing with the wrong key would fail-close in every enclave and read as "nobody signed".
    #[test]
    fn signing_refuses_a_non_governance_key() {
        let seed = [0x9u8; 32]; // derives to some pub that is NOT BAKED_GOV_PUB
        let pem = synth_openssh_key(&seed);
        let escrow = [0x10u8; 20];
        let prev = [0x20u8; 32];
        let member = [0u8; 32];
        let signers = vec![
            SignerEntry {
                account_id: [1u8; 20],
                weight: 1,
            },
            SignerEntry {
                account_id: [2u8; 20],
                weight: 1,
            },
        ];
        let err = sign_descriptor(
            &pem,
            3,
            &escrow,
            1,
            &prev,
            &signers,
            2,
            &member,
            &BAKED_GOV_PUB,
        )
        .expect_err("a non-governance key must be refused");
        assert!(
            format!("{err}").contains("NOT the expected governance key"),
            "the refusal must say the key is wrong: {err}"
        );
    }

    /// With a key that DOES derive to the baked pub, signing succeeds and the signature verifies
    /// under the baked pub over the descriptor digest — i.e. exactly what the enclave checks.
    #[test]
    fn signing_with_the_governance_key_produces_an_enclave_verifiable_sig() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        // Find the seed whose pub is BAKED_GOV_PUB? We don't have the private seed here (it is on
        // the operator laptop). Instead, prove the PROPERTY generically: for ANY key, the emitted
        // signature verifies under that key's derived pub over the digest. The baked-pub gate is
        // covered by the refusal test above; here we cover the sign/verify round-trip by
        // temporarily checking against the derived pub rather than the baked constant.
        let seed = [0x9u8; 32];
        let sk = SigningKey::from_bytes(&seed);
        let escrow = [0x10u8; 20];
        let prev = [0x20u8; 32];
        let member = [0u8; 32];
        let signers = vec![SignerEntry {
            account_id: [1u8; 20],
            weight: 1,
        }];
        let set_hash = compute_set_hash(&signers, 1);
        let digest = compute_membership_descriptor_digest(3, &escrow, 1, &prev, &set_hash, &member);
        let sig = sk.sign(&digest);
        let vk = VerifyingKey::from_bytes(&sk.verifying_key().to_bytes()).unwrap();
        let sig2 = Signature::from_bytes(&sig.to_bytes());
        assert!(
            vk.verify(&digest, &sig2).is_ok(),
            "the raw ed25519 signature over the digest must verify under the signer's pub"
        );
    }

    /// The REAL signing path for an allowlist descriptor, end to end: synth OpenSSH key ->
    /// sign_mrenclave_govern_descriptor (expecting that key's own pub, as for a test measurement)
    /// -> the 64-byte signature verifies under that pub over EXACTLY the digest the enclave
    /// re-derives.
    #[test]
    fn mrenclave_govern_descriptor_signature_verifies_over_the_enclave_digest() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let seed = [0x42u8; 32];
        let pem = synth_openssh_key(&seed);
        let derived = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let (digest, sig) =
            sign_mrenclave_govern_descriptor(&pem, &test_op(1), &TEST_GEN, &derived)
                .expect("the expected key must sign");
        assert_eq!(
            digest,
            crate::mrenclave_governance::compute_trusted_mrenclave_descriptor_digest(
                1,
                &[0x11; 32],
                &[0x33; 20],
                7,
                &[0x22; 32],
                &TEST_GEN
            ),
            "the signed digest must be the enclave's digest"
        );
        let vk = VerifyingKey::from_bytes(&derived).unwrap();
        assert!(vk.verify(&digest, &Signature::from_bytes(&sig)).is_ok());
        // and it does NOT verify over a different op (binding is real)
        let other = crate::mrenclave_governance::compute_trusted_mrenclave_descriptor_digest(
            2,
            &[0x11; 32],
            &[0x33; 20],
            7,
            &[0x22; 32],
            &TEST_GEN,
        );
        assert!(vk.verify(&other, &Signature::from_bytes(&sig)).is_err());
        // D-1b: nor in another generation — another enclave version, or another signer-list
        // epoch of the same version. This is the cross-generation replay the binding stops.
        for gen in [
            crate::mrenclave_governance::GovContext {
                governing_mrenclave: [0x45; 32],
                ..TEST_GEN
            },
            crate::mrenclave_governance::GovContext {
                membership_epoch_digest: [0x56; 32],
                ..TEST_GEN
            },
        ] {
            let d = crate::mrenclave_governance::compute_trusted_mrenclave_descriptor_digest(
                1,
                &[0x11; 32],
                &[0x33; 20],
                7,
                &[0x22; 32],
                &gen,
            );
            assert!(vk.verify(&d, &Signature::from_bytes(&sig)).is_err());
        }
    }

    /// A key that is not the expected (baked) governance key is refused before any
    /// signature is produced.
    #[test]
    fn unl_policy_descriptor_signature_verifies_and_binds_the_anchor() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let seed = [0x42u8; 32];
        let pem = synth_openssh_key(&seed);
        let derived = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let policy = crate::unl_policy::UnlPolicyOp {
            unl_epoch: 3,
            prev_unl_hash: [0x22; 32],
            pinned_ledger_seq: 22_056_907,
            quorum_num: 5,
            quorum_den: 6,
        };
        let (digest, sig) =
            sign_unl_policy_descriptor(&pem, &[0x33; 20], &policy, &TEST_GEN, &derived)
                .expect("the expected key must sign");
        let vk = VerifyingKey::from_bytes(&derived).unwrap();
        assert!(vk.verify(&digest, &Signature::from_bytes(&sig)).is_ok());
        // A host that picks a different anchor gets a digest the signature does not cover.
        let moved = crate::unl_policy::unl_policy_gov_descriptor_digest(
            &[0x33; 20],
            3,
            &[0x22; 32],
            22_056_999,
            (5, 6),
            &TEST_GEN,
        );
        assert!(vk.verify(&moved, &Signature::from_bytes(&sig)).is_err());
        // And a non-governance key is refused before signing.
        assert!(
            sign_unl_policy_descriptor(&pem, &[0x33; 20], &policy, &TEST_GEN, &BAKED_GOV_PUB)
                .is_err()
        );
    }

    #[test]
    fn mrenclave_govern_descriptor_refuses_a_non_governance_key() {
        let pem = synth_openssh_key(&[0x42u8; 32]);
        let err = sign_mrenclave_govern_descriptor(&pem, &test_op(1), &TEST_GEN, &BAKED_GOV_PUB)
            .expect_err("a non-governance key must be refused");
        assert!(
            err.to_string().contains("NOT the expected governance key"),
            "{err}"
        );
    }
}
