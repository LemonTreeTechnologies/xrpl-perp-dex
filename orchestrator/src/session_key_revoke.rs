//! Revoke a session key by rotating it to a value nobody holds.
//!
//! WHY THIS EXISTS. A session key is the credential
//! `ecall_validate_session_key` checks, and that check is a pool lookup plus a
//! `ct_memcmp` — no expiry, no epoch, no counter. The account pool survives
//! Path-A migration. So a session key that leaks stays valid until somebody
//! rotates it, and on 2026-10-08 an inventory found thirteen historically-real
//! ones committed to git, seven of them in a PUBLIC repository.
//!
//! The enclave has always been able to rotate: `POST
//! /v1/pool/regenerate-session-key` reseals `session_key` to
//! `sgx_read_rand` output after checking the OLD value. What was missing was a
//! reviewed way to drive it, so the first attempt was an ad-hoc loop over ssh —
//! which had no guard against hitting a key the cluster is USING, printed
//! whatever the enclave returned, and proved nothing about whether the old
//! credential actually died.
//!
//! THE THREE THINGS THIS GETS RIGHT THAT A LOOP DOES NOT.
//!
//! 1. It cannot be aimed at a live credential. The in-use set is read from the
//!    node's own configs at runtime, and the command REFUSES if the key it was
//!    given is one of them. Not a hardcoded list: a list goes stale, and the
//!    inventory's own first count was wrong — four in-use keys, when there were
//!    nine, because `beta_entry.json` and `betaB_entry.json` each hold one too.
//! 2. It never writes or prints a key. Values arrive in a file, never argv, and
//!    only 16-hex digests are reported.
//! 3. It PROVES the revocation instead of assuming it. The old key is validated
//!    before (is there anything to revoke?) and again after (is it dead?). A
//!    rotation the enclave called successful, whose old key still
//!    authenticates, is reported as a FAILURE — the loud direction.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Normalise a session key the way the enclave sees it: no `0x`, lowercase, no
/// surrounding whitespace. Two files storing the same key in different spellings
/// must collapse to one fingerprint or the in-use guard misses one of them.
fn normalise(raw: &str) -> String {
    let t = raw.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    t.to_ascii_lowercase()
}

/// A short, non-reversible label for a key, for logs and reports.
pub fn digest(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(normalise(raw).as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

/// Is this a plausible session key at all? 32 bytes of hex.
///
/// Checked before anything is sent, so a truncated file or a stray newline
/// fails here with a clear message instead of as an opaque enclave refusal that
/// would then be misread as "the account is gone".
fn validate_shape(raw: &str) -> Result<String> {
    let n = normalise(raw);
    if n.len() != 64 || !n.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!(
            "session key must be 32 bytes of hex (64 hex chars, `0x` optional); \
             got {} characters. Nothing was sent.",
            n.len()
        );
    }
    Ok(n)
}

/// Every session key currently present in the node's config directory.
///
/// Collected by walking the JSON rather than by naming fields in known files:
/// a new config carrying a `session_key` then joins the exclusion set
/// automatically. The safe failure direction here is TOO MANY exclusions, never
/// too few.
pub fn in_use_digests(dir: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read the in-use config directory {}", dir.display()))?;
    for e in entries {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => continue, // unreadable is not fatal; the emptiness check below is the gate
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        collect_session_keys(&v, &mut out);
    }
    Ok(out)
}

fn collect_session_keys(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, val) in m {
                if k == "session_key" {
                    if let Some(s) = val.as_str() {
                        if !normalise(s).is_empty() {
                            out.insert(digest(s));
                        }
                    }
                } else {
                    collect_session_keys(val, out);
                }
            }
        }
        serde_json::Value::Array(a) => {
            for val in a {
                collect_session_keys(val, out);
            }
        }
        _ => {}
    }
}

/// Refuse to touch a key the cluster is using.
///
/// FAIL CLOSED ON AN EMPTY SET. An in-use set that came back empty means the
/// config directory was wrong, not that nothing is in use — and a vacuous guard
/// would wave through the live sequencer credential. The catastrophic direction
/// for this gate is the false ACCEPT, so emptiness is a refusal.
pub fn refuse_if_in_use(key_digest: &str, in_use: &BTreeSet<String>, dir: &Path) -> Result<()> {
    if in_use.is_empty() {
        anyhow::bail!(
            "found NO session keys under {} — refusing to continue. An empty in-use set \
             would make the safety check vacuous, and the key being revoked could be the \
             one this cluster is running on. Point --in-use-dir at the directory holding \
             signers_config.json.",
            dir.display()
        );
    }
    if in_use.contains(key_digest) {
        anyhow::bail!(
            "REFUSING: the key {key_digest} is IN USE by this node ({} in-use key(s) found \
             under {}). Rotating it would break the running cluster. If this key really is \
             meant to be retired, take it out of the config and restart first.",
            in_use.len(),
            dir.display()
        );
    }
    Ok(())
}

/// What the enclave said about a `validate-session-key` call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Authenticates {
    /// HTTP 200 — this key is live for this account on this node.
    Yes,
    /// HTTP 400 — the enclave refused it. Either the account is not in this
    /// node's pool, or the key has already been superseded. The route answers
    /// both with the same status, so the two are NOT distinguishable from
    /// outside, and this enum deliberately does not pretend otherwise.
    No,
}

/// The outcome of one revocation attempt against one node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// The old key did not authenticate to begin with. Nothing to revoke here.
    AlreadyDead,
    /// Rotated, and the old key is now refused. The credential is dead.
    Revoked { new_digest: String },
    /// The enclave accepted the rotation and the OLD KEY STILL WORKS. Loud: the
    /// credential is not dead and must not be recorded as revoked.
    RotatedButStillLive,
    /// The enclave refused the rotation even though the key had just validated.
    RotationRefused { http: u16 },
}

impl Verdict {
    /// Did this attempt leave the old credential unusable?
    pub fn credential_is_dead(&self) -> bool {
        matches!(self, Verdict::AlreadyDead | Verdict::Revoked { .. })
    }

    /// Should the operator be made to look at this one?
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Verdict::RotatedButStillLive | Verdict::RotationRefused { .. }
        )
    }
}

/// Decide the verdict from the three observations.
///
/// Pure, so every branch is reachable in a unit test without an enclave. The
/// ordering matters: `post` is what settles whether the credential died, and it
/// overrides a successful-looking rotation.
pub fn classify(
    pre: Authenticates,
    regen_http: Option<u16>,
    regen_new_digest: Option<String>,
    post: Option<Authenticates>,
) -> Verdict {
    if pre == Authenticates::No {
        return Verdict::AlreadyDead;
    }
    match regen_http {
        Some(200) => match post {
            Some(Authenticates::No) => Verdict::Revoked {
                new_digest: regen_new_digest.unwrap_or_else(|| "unknown".to_string()),
            },
            // Includes the case where the post-check could not be made: an
            // unverified revocation is not a revocation.
            _ => Verdict::RotatedButStillLive,
        },
        Some(code) => Verdict::RotationRefused { http: code },
        None => Verdict::RotationRefused { http: 0 },
    }
}

#[derive(Deserialize)]
struct RegenResponse {
    #[allow(dead_code)]
    status: Option<String>,
    session_key: Option<String>,
}

/// Drive the three calls against one node's enclave API.
async fn attempt(enclave_url: &str, address: &str, key: &str) -> Result<(Verdict, Option<String>)> {
    let http = crate::http_helpers::loopback_http_client(Duration::from_secs(30))?;
    let body = serde_json::json!({ "address": address, "session_key": format!("0x{key}") });

    let pre = http
        .post(format!("{enclave_url}/pool/validate-session-key"))
        .json(&body)
        .send()
        .await
        .context("failed to reach the enclave /pool/validate-session-key")?;
    let pre = if pre.status().as_u16() == 200 {
        Authenticates::Yes
    } else {
        Authenticates::No
    };
    if pre == Authenticates::No {
        return Ok((Verdict::AlreadyDead, None));
    }

    let regen = http
        .post(format!("{enclave_url}/pool/regenerate-session-key"))
        .json(&body)
        .send()
        .await
        .context("failed to reach the enclave /pool/regenerate-session-key")?;
    let code = regen.status().as_u16();
    let new_key = if code == 200 {
        regen
            .json::<RegenResponse>()
            .await
            .ok()
            .and_then(|r| r.session_key)
            .map(|s| normalise(&s))
    } else {
        None
    };
    let new_digest = new_key.as_deref().map(digest);

    if code != 200 {
        return Ok((classify(pre, Some(code), None, None), None));
    }

    let post = http
        .post(format!("{enclave_url}/pool/validate-session-key"))
        .json(&body)
        .send()
        .await
        .context("failed to re-check the old key after rotation")?;
    let post = if post.status().as_u16() == 200 {
        Authenticates::Yes
    } else {
        Authenticates::No
    };

    Ok((classify(pre, Some(code), new_digest, Some(post)), new_key))
}

/// `revoke-session-key` — rotate one leaked session key out of existence.
#[allow(clippy::too_many_arguments)]
pub async fn revoke_session_key(
    enclave_url: &str,
    address: &str,
    key_file: &Path,
    in_use_dir: &Path,
    save_new: Option<&PathBuf>,
) -> Result<()> {
    crate::http_helpers::ensure_loopback_url(enclave_url)
        .context("the enclave API is loopback-only (O-L4)")?;
    if !(address.len() == 42 && address.starts_with("0x")) {
        anyhow::bail!(
            "--address must be 0x followed by 40 hex characters, as the enclave pool keys accounts"
        );
    }

    let raw = std::fs::read_to_string(key_file)
        .with_context(|| format!("cannot read the key file {}", key_file.display()))?;
    let key = validate_shape(&raw)?;
    let d = digest(&key);

    let in_use = in_use_digests(in_use_dir)?;
    refuse_if_in_use(&d, &in_use, in_use_dir)?;

    println!("revoke-session-key");
    println!("==================");
    println!("enclave : {enclave_url}");
    println!("account : {address}");
    println!("key     : {d}  (digest only; the value is never printed)");
    println!(
        "in use  : {} key(s) found under {}",
        in_use.len(),
        in_use_dir.display()
    );
    println!();

    let (verdict, new_key) = attempt(enclave_url, address, &key).await?;

    // One grep-able line first, so a transcript of a batch run can be audited
    // without reading the prose. `dead=` is the only thing that matters for the
    // question "is this leaked credential still usable".
    println!(
        "RESULT key={d} account={address} dead={} attention={}",
        verdict.credential_is_dead(),
        verdict.needs_attention()
    );

    match &verdict {
        Verdict::AlreadyDead => {
            println!("ALREADY DEAD — the enclave refused this key before anything was changed.");
            println!("  Either the account is not in THIS node's pool, or the key was already");
            println!("  superseded. The route answers both the same way, so which one it is");
            println!("  cannot be told from outside. Nothing was rotated.");
        }
        Verdict::Revoked { new_digest } => {
            println!("REVOKED — rotated, and the old key is now refused.");
            println!("  new value digest: {new_digest}");
            match save_new {
                Some(p) => {
                    let v = new_key.clone().unwrap_or_default();
                    std::fs::write(p, format!("0x{v}\n"))
                        .with_context(|| format!("cannot write {}", p.display()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
                            .with_context(|| format!("cannot chmod 0600 {}", p.display()))?;
                    }
                    println!(
                        "  saved to {} (mode 0600) — this is a SECRET from now on",
                        p.display()
                    );
                }
                None => {
                    println!("  DISCARDED, not stored. The account can no longer be driven by");
                    println!("  anyone, including us: the signing key is intact but there is no");
                    println!("  credential left to present for it. That is the intended effect.");
                }
            }
        }
        Verdict::RotatedButStillLive => {
            anyhow::bail!(
                "the enclave reported a successful rotation for {d} BUT THE OLD KEY STILL \
                 AUTHENTICATES. The credential is NOT revoked. Do not record this as done — \
                 re-check the enclave and this node's pool state."
            );
        }
        Verdict::RotationRefused { http } => {
            anyhow::bail!(
                "the key {d} authenticated, so the account IS in this node's pool, but the \
                 rotation was refused with HTTP {http}. The credential is still live."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// THE GUARD THAT MATTERS MOST: it must not be possible to aim this at the
    /// credential the cluster is running on.
    #[test]
    fn a_key_that_is_in_use_is_refused_by_its_digest() {
        let d = digest("0xAB".to_string().repeat(32).as_str());
        let e = refuse_if_in_use(&d, &set(&[&d, "deadbeefdeadbeef"]), Path::new("/perp"))
            .expect_err("an in-use key must never be rotated");
        let m = format!("{e}");
        assert!(m.contains(&d), "the refusal must name the key: {m}");
        assert!(m.contains("IN USE"), "and say why: {m}");
    }

    /// An EMPTY in-use set is a wrong directory, not an all-clear. A vacuous
    /// guard is worse than no guard, because it reads as a guard.
    #[test]
    fn an_empty_in_use_set_is_a_refusal_not_an_all_clear() {
        let e = refuse_if_in_use("aaaabbbbccccdddd", &BTreeSet::new(), Path::new("/wrong"))
            .expect_err("an empty in-use set must fail closed");
        assert!(
            format!("{e}").contains("/wrong"),
            "name the directory it looked in"
        );
    }

    #[test]
    fn a_key_that_is_not_in_use_passes_the_guard() {
        refuse_if_in_use(
            "aaaabbbbccccdddd",
            &set(&["0000111122223333"]),
            Path::new("/perp"),
        )
        .expect("a non-live key must pass, or the gate is just a wall");
    }

    /// Two spellings of one key must collapse to one fingerprint, or the guard
    /// misses the copy that is spelled differently from the one in the config.
    #[test]
    fn the_digest_ignores_0x_case_and_whitespace() {
        let a = digest("0xAABB00112233445566778899aabbccddeeff00112233445566778899aabbccdd");
        let b = digest("  aabb00112233445566778899AABBCCDDEEFF00112233445566778899aabbccdd\n");
        assert_eq!(a, b, "one key stored two ways must give one digest");
    }

    #[test]
    fn a_truncated_key_is_rejected_before_anything_is_sent() {
        let e = validate_shape("0xabcd").expect_err("a short key must not be sent");
        assert!(
            format!("{e}").contains("Nothing was sent"),
            "the message must make clear no request went out"
        );
        validate_shape(&"ab".repeat(32)).expect("a well-formed key must pass");
        validate_shape("0xzz").expect_err("non-hex must not be sent");
    }

    /// Every in-use key in the directory is found, whatever shape the config
    /// holds it in — nested under an array, or at the top level. This is the
    /// count I got wrong by hand: four, when there were nine.
    #[test]
    fn in_use_collection_finds_nested_and_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("signers_config.json"),
            r#"{"escrow_address":"rX","signers":[
                 {"name":"n1","session_key":"0x11"},
                 {"name":"n2","session_key":"0x22"}],
               "local_signer":{"name":"n1","session_key":"0x11"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("beta_entry.json"),
            r#"{"session_key":"0x33"}"#,
        )
        .unwrap();
        // Not JSON, and a non-.json file: neither may break the walk.
        std::fs::write(dir.path().join("notes.txt"), "session_key: 0x44").unwrap();
        std::fs::write(dir.path().join("broken.json"), "{not json").unwrap();

        let got = in_use_digests(dir.path()).unwrap();
        assert_eq!(
            got.len(),
            3,
            "0x11 twice collapses; 0x22 and 0x33 join; got {got:?}"
        );
        assert!(
            got.contains(&digest("0x33")),
            "the top-level beta_entry key must be found"
        );
        assert!(
            !got.contains(&digest("0x44")),
            "a .txt file is not a config"
        );
    }

    /// An empty `session_key` is the right shape in signers_config (the value
    /// moved to .secrets) and must not become a bogus exclusion.
    #[test]
    fn an_empty_session_key_field_is_not_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("c.json"),
            r#"{"escrow_seed":"","session_key":""}"#,
        )
        .unwrap();
        assert!(in_use_digests(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn a_key_that_never_authenticated_is_already_dead_and_nothing_is_rotated() {
        assert_eq!(
            classify(Authenticates::No, None, None, None),
            Verdict::AlreadyDead
        );
        assert!(Verdict::AlreadyDead.credential_is_dead());
        assert!(!Verdict::AlreadyDead.needs_attention());
    }

    #[test]
    fn a_rotation_whose_old_key_is_then_refused_is_a_revocation() {
        let v = classify(
            Authenticates::Yes,
            Some(200),
            Some("abcd1234abcd1234".into()),
            Some(Authenticates::No),
        );
        assert_eq!(
            v,
            Verdict::Revoked {
                new_digest: "abcd1234abcd1234".into()
            }
        );
        assert!(v.credential_is_dead());
    }

    /// THE FALSIFICATION THAT THE AD-HOC LOOP DID NOT HAVE. A 200 from the
    /// enclave is a claim, not a result. If the old key still authenticates the
    /// credential is alive, and this must never read as success.
    #[test]
    fn a_successful_looking_rotation_whose_old_key_still_works_is_a_failure() {
        let v = classify(
            Authenticates::Yes,
            Some(200),
            Some("x".into()),
            Some(Authenticates::Yes),
        );
        assert_eq!(v, Verdict::RotatedButStillLive);
        assert!(!v.credential_is_dead(), "this must NOT count as revoked");
        assert!(v.needs_attention());
    }

    /// And an UNVERIFIED revocation is not a revocation either: no post-check
    /// means no claim.
    #[test]
    fn a_rotation_with_no_post_check_is_not_reported_as_revoked() {
        let v = classify(Authenticates::Yes, Some(200), Some("x".into()), None);
        assert_eq!(v, Verdict::RotatedButStillLive);
        assert!(!v.credential_is_dead());
    }

    #[test]
    fn a_live_key_the_enclave_refuses_to_rotate_is_still_live() {
        let v = classify(Authenticates::Yes, Some(500), None, None);
        assert_eq!(v, Verdict::RotationRefused { http: 500 });
        assert!(!v.credential_is_dead());
        assert!(v.needs_attention());
    }
}
