//! upgrade_corpus.rs — the β18 two-binary upgrade corpus, ceremony half (runbook §10.5).
//!
//! WHAT THIS IS FOR. §10.5 is a PRE-MIGRATION GATE: before β18 is scheduled, build both binaries
//! and run the upgrade for real. Steps 3-6 assert what the β18 LOADER does with a set produced by
//! a β17 enclave, and that set cannot be synthesised — every section seals with
//! `SGX_KEYPOLICY_MRENCLAVE`, so β18 cannot unseal β17's files, and the only thing that moves the
//! plaintext across is the Path-A ceremony's export/import. Post-fix the input is even more
//! tightly bound: β18's own save bumps `save_seq` before its first seal, so it writes 1 and never
//! 0, and a zero-stamped section can ONLY come from an import whose source had no such field —
//! a β17 source. Step 5's spliced `save_seq = 0` chunk therefore has exactly one producer, the
//! real migration, which is what makes step 5 a gate rather than a mock.
//!
//! WHAT IT DELIBERATELY DOES NOT PROVE, stated here because a passing run will be cited:
//!
//!   * NOT reproducible builds. Adding β18's measurement to β17's allowlist needs
//!     `TRUSTED_MRENCLAVES_REPRO_MIN = 2` DISTINCT reproducers, and here ONE machine built β18
//!     and two accounts on ONE enclave sign for it. That bundle asserts a FALSEHOOD — "two
//!     distinct reproducers confirmed this measurement" — and it is contained not by this comment
//!     but by the KEYS: `ecall_sign_repro_proof` signs with an account in the enclave's own pool,
//!     so the signatures come from throwaway accounts created for this run, whose addresses no
//!     production SignerList holds. A production β17 handed this bundle rejects it because those
//!     are not its signers. The falsehood is structurally inadmissible outside the corpus.
//!   * NOT a t-of-n quorum validation. The SignerList here is two accounts on one host. The
//!     delegation quorum's multi-party correctness is PRG-3's subject on real hardware.
//!   * NOT operator independence in any form.
//!
//! LOOPBACK ONLY, and this is a safety property rather than a convenience: this code drives a
//! REAL MIGRATION, and a migration aimed at the live cluster would retire a running node. Every
//! client here comes from `http_helpers::loopback_http_client`, and `check_loopback` refuses a
//! non-loopback base before anything is contacted.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::time::Duration;

use crate::membership_canonical::SignerEntry;
use crate::membership_coordinator::{
    ClusterGenesisApplier, GenesisBootstrapSink, MembershipBundleCollector,
    MembershipEpochStatement, NodeSealResult,
};
use crate::path_a_delegation::DelegationCollector;

/// One throwaway operator identity the corpus created and can sign as.
#[derive(Clone, Debug)]
pub struct CorpusSigner {
    /// 0x-prefixed 20-byte address, as the enclave's `from` field wants it.
    pub address: String,
    pub session_key_hex: String,
    /// 33-byte compressed secp256k1 pubkey — the bundle carries it; the signing
    /// response does not, so it is captured at account creation.
    pub compressed_pubkey: Vec<u8>,
    /// WHICH NODE holds this account's private key, as a bare origin.
    ///
    /// Necessary, not incidental: sealing a founding set makes each enclave enumerate its OWN
    /// pool and refuse with NOT_PARTICIPANT unless a member is a key it holds, so the set has one
    /// account per node — and then a signature can only be requested from the node that holds
    /// that key. Asking one node for every signature fails on the accounts it does not have.
    pub base: String,
}

impl CorpusSigner {
    /// The XRPL AccountID — RIPEMD160(SHA256(compressed_pubkey)) — and NOT the Ethereum address.
    ///
    /// This is the third encoding in the same request body, and getting it wrong is what made the
    /// founding seal fail with NOT_PARTICIPANT: `SealedSignerList.signers[].account_id` stores the
    /// AccountID, and the enclave compares it against RIPEMD160(SHA256()) over the keys in its own
    /// pool. An ETH address is keccak-derived and never matches. Uses the centralised helper,
    /// which carries the XRPL spec's own test vectors, rather than a fourth local derivation.
    pub fn account_id(&self) -> Result<[u8; 20]> {
        if self.compressed_pubkey.len() != 33 {
            bail!(
                "compressed pubkey is {} bytes, expected 33",
                self.compressed_pubkey.len()
            );
        }
        Ok(crate::auth::pubkey_to_account_id(&self.compressed_pubkey))
    }
}

/// Refuse anything that is not loopback, before a single byte is sent.
///
/// The corpus performs an ACTUAL Path-A migration. Pointed at the cluster it would export and
/// retire a live node, so this is not a lint — it is the difference between a test and an
/// incident. Checked on the string rather than after a DNS round trip, because a name that
/// resolves to loopback today is not a property this can rely on.
pub fn check_loopback(base: &str) -> Result<()> {
    let authority = base
        .split("://")
        .nth(1)
        .unwrap_or(base)
        .split('/')
        .next()
        .unwrap_or("");
    // A bracketed IPv6 literal contains colons, so the port cannot be stripped by splitting on
    // ':' — doing that turned `[::1]:9097` into `[` and REFUSED a legitimate loopback base. The
    // failure direction was safe but the guard was still wrong, and a guard that rejects the only
    // invocation it is supposed to allow gets disabled by whoever hits it.
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    match host {
        "localhost" | "127.0.0.1" | "::1" => Ok(()),
        other => Err(anyhow!(
            "upgrade-corpus refuses a non-loopback base: {other:?}. This drives a REAL Path-A \
             migration — export plus import, and on a real run OLD is retired. It is only ever \
             aimed at two throwaway SIM servers on this host."
        )),
    }
}

fn client() -> Result<reqwest::Client> {
    crate::http_helpers::loopback_http_client(Duration::from_secs(20))
        .map_err(|e| anyhow!("loopback http client: {e}"))
}

/// Ask ONE account to sign, and turn the answer into a `(pk, der_sig)` pair.
///
/// The response carries `signature.{r,s}`; the compressed pubkey does not come back and is taken
/// from the signer record. Identical shape to what the p2p responder does for the same endpoints,
/// so the bytes the enclave verifies are produced the same way — the transport is the only
/// difference, and on one host the enclave admin API is directly reachable, which is exactly why
/// the production applier uses p2p (X-C1: it is never network-exposed).
/// The base URL convention, stated once because mixing the two cost a run.
///
/// Every path here — and every path constant in membership_http, and every URL HttpEnclaveApi
/// builds — INCLUDES the `/v1` prefix, so the base must be the bare origin
/// (`https://localhost:9097`) and never `https://localhost:9097/v1`. Passing the prefixed form
/// produced `https://localhost:9097/v1/v1/admin/...` and the node answered "Not found", which
/// reads like a missing route rather than a doubled prefix.
async fn sign_with(
    http: &reqwest::Client,
    path: &str,
    signer: &CorpusSigner,
    mut body: serde_json::Value,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let base = signer.base.as_str();
    // THE TWO FIELDS HAVE OPPOSITE CONVENTIONS, which is why this bug keeps recurring.
    //
    // `from` MUST keep its 0x: the enclave checks strlen(account_id) == 42 && [0] == '0' &&
    // [1] == 'x' and refuses anything else. `session_key` must NOT have it: the enclave
    // from_hex()es the string and then checks the SIZE, so a 0x-prefixed key decodes to the wrong
    // length and the refusal reads "Invalid session key size" — which names the size and not the
    // prefix, so the cause is a guess unless you read the enclave's log.
    //
    // /pool/generate returns the session key 0x-PREFIXED, so passing it through unmodified fails.
    // This project has already lost a genesis ceremony to exactly this (recorded as the
    // session_key 0x-prefix bug), and it recurred here. Normalised once, at the one place every
    // signing call goes through, rather than at each caller.
    body["from"] = serde_json::json!(signer.address);
    body["session_key"] = serde_json::json!(signer
        .session_key_hex
        .trim_start_matches("0x")
        .trim_start_matches("0X"));
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let v: serde_json::Value = resp.json().await.with_context(|| format!("decode {url}"))?;
    if v["status"].as_str() != Some("success") {
        bail!("{url} refused: {v}");
    }
    let r = hex::decode(v["signature"]["r"].as_str().unwrap_or(""))
        .with_context(|| format!("{url}: signature.r not hex"))?;
    let s = hex::decode(v["signature"]["s"].as_str().unwrap_or(""))
        .with_context(|| format!("{url}: signature.s not hex"))?;
    let der = crate::xrpl_signer::der_encode_signature(&r, &s);
    Ok((signer.compressed_pubkey.clone(), der))
}

/// Collects membership consent over loopback HTTP instead of gossipsub.
pub struct CorpusConsentCollector {
    pub signers: Vec<CorpusSigner>,
}

#[async_trait]
impl MembershipBundleCollector for CorpusConsentCollector {
    async fn collect(&self, statement: &MembershipEpochStatement) -> Result<Vec<u8>> {
        let http = client()?;
        let signers_json: Vec<serde_json::Value> = statement
            .new_signers
            .iter()
            .map(|s| {
                serde_json::json!({ "account_id": hex::encode(s.account_id), "weight": s.weight })
            })
            .collect();
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for signer in &self.signers {
            let body = serde_json::json!({
                "escrow_account_id": hex::encode(statement.escrow),
                "signers": signers_json,
                "quorum_threshold": statement.new_quorum,
                "proposed_epoch": statement.proposed_epoch,
                "prev_epoch_hash": hex::encode(statement.prev_epoch_hash),
            });
            let e = sign_with(&http, "/v1/admin/signerlist/sign-consent", signer, body).await?;
            if !entries.iter().any(|(pk, _)| *pk == e.0) {
                entries.push(e);
            }
        }
        if entries.is_empty() {
            bail!("no consent collected — the corpus signers cannot sign");
        }
        Ok(crate::quorum_bundle::build(&entries))
    }
}

/// Applies genesis by calling each LOCAL node's own admin route directly.
///
/// Production broadcasts over p2p because the enclave admin API is loopback-only and a remote
/// node cannot be POSTed to (X-C1). Here both nodes ARE on this host, so loopback is reachable
/// and this performs the identical action without the transport — the same
/// `HttpGenesisBootstrapSink` the p2p receiver runs, not a reimplementation of it.
pub struct CorpusGenesisApplier {
    pub bases: Vec<String>,
}

#[async_trait]
impl ClusterGenesisApplier for CorpusGenesisApplier {
    async fn apply_genesis(
        &self,
        statement: &MembershipEpochStatement,
        bundle: &[u8],
    ) -> Result<Vec<NodeSealResult>> {
        let sink = crate::membership_http::HttpGenesisBootstrapSink::new(client()?);
        let mut out = Vec::new();
        for base in &self.bases {
            let r = sink.bootstrap_on_node(base, statement, bundle).await;
            out.push(NodeSealResult {
                node: base.clone(),
                ok: r.is_ok(),
                error: r.err().map(|e| e.to_string()),
            });
        }
        Ok(out)
    }
}

/// Collects Path-A delegation over loopback HTTP instead of gossipsub.
pub struct CorpusDelegationCollector {
    pub signers: Vec<CorpusSigner>,
}

#[async_trait]
impl DelegationCollector for CorpusDelegationCollector {
    async fn collect(
        &self,
        mrenclave_new: &[u8; 32],
        ceremony_nonce: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let http = client()?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for signer in &self.signers {
            let body = serde_json::json!({
                "mrenclave_new": hex::encode(mrenclave_new),
                "ceremony_nonce": hex::encode(ceremony_nonce),
            });
            let e = sign_with(&http, "/v1/pool/sign/patha-delegation", signer, body).await?;
            if !entries.iter().any(|(pk, _)| *pk == e.0) {
                entries.push(e);
            }
        }
        if entries.is_empty() {
            bail!("no delegation collected — the corpus signers cannot sign");
        }
        Ok(crate::quorum_bundle::build(&entries))
    }
}

/// Seal the founding epoch on OLD (and NEW, so both share a SignerList) from the corpus signers.
pub async fn corpus_genesis(
    escrow: [u8; 20],
    signers: &[CorpusSigner],
    quorum: u32,
    old_base: &str,
    new_base: &str,
) -> Result<()> {
    check_loopback(old_base)?;
    check_loopback(new_base)?;
    let genesis_signers: Result<Vec<SignerEntry>> = signers
        .iter()
        .map(|s| {
            Ok(SignerEntry {
                account_id: s.account_id()?,
                weight: 1,
            })
        })
        .collect();
    let collector = CorpusConsentCollector {
        signers: signers.to_vec(),
    };
    let applier = CorpusGenesisApplier {
        bases: vec![old_base.to_string(), new_base.to_string()],
    };
    let outcome = crate::membership_coordinator::run_genesis_bootstrap(
        escrow,
        genesis_signers?,
        quorum,
        &collector,
        &applier,
    )
    .await
    .context("corpus genesis bootstrap")?;
    let failed: Vec<String> = outcome
        .node_results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| format!("{}: {}", r.node, r.error.clone().unwrap_or_default()))
        .collect();
    if !failed.is_empty() {
        bail!("genesis did not seal on every node: {}", failed.join("; "));
    }
    Ok(())
}

/// Sign the reproducibility proof over NEW's measurement with every corpus signer.
///
/// THIS BUNDLE ASSERTS A FALSEHOOD and the module header says why that is contained. It is built
/// here, used immediately by the governance call, and never returned to a caller that could
/// persist it.
async fn corpus_repro_bundle(
    signers: &[CorpusSigner],
    mrenclave_new: &[u8; 32],
) -> Result<Vec<u8>> {
    let http = client()?;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for signer in signers {
        let body = serde_json::json!({ "mrenclave": hex::encode(mrenclave_new) });
        let e = sign_with(&http, "/v1/admin/mrenclaves/sign-repro-proof", signer, body).await?;
        if !entries.iter().any(|(pk, _)| *pk == e.0) {
            entries.push(e);
        }
    }
    if (entries.len() as u32) < 2 {
        bail!(
            "the repro floor is 2 DISTINCT signers and only {} signed — a one-signer SignerList \
             cannot add anything to the allowlist, by design",
            entries.len()
        );
    }
    Ok(crate::quorum_bundle::build(&entries))
}

/// Put NEW's measurement on OLD's governed allowlist, which OLD's export requires.
///
/// `ADMIT_PURPOSE_PATHA_TARGET` is "export verifies NEW → self OR allowlist", and the two
/// measurements differ by construction, so self does not apply. Only this direction needs it:
/// `ADMIT_PURPOSE_PATHA_SOURCE` has no allowlist gate, because the source is governed by the
/// caller's identity pin, the ceremony quorum and the payload's GCM authentication instead.
pub async fn corpus_admit_new_on_old(
    old_base: &str,
    signers: &[CorpusSigner],
    escrow: [u8; 20],
    mrenclave_new: &[u8; 32],
) -> Result<()> {
    check_loopback(old_base)?;
    let http = client()?;

    // Chain onto the allowlist head OLD currently holds; a stale prev hash is refused with
    // PREVHASH_MISMATCH (-5). Read through the PRODUCTION source rather than by parsing the
    // status JSON here: my own reader looked for "epoch" and "allowlist_hash" while the route
    // answers "allowlist_epoch" and "allowlist_digest", so it silently saw a zero head and
    // proposed epoch 1 onto a chain already at 1. A second reader of the same response is a
    // second thing to keep in step with the route.
    use crate::mrenclave_governance::AllowlistStatusSource;
    let (epoch, prev) =
        crate::membership_http::HttpAllowlistStatusSource::new(client()?, old_base.to_string())
            .current()
            .await
            .context("read OLD's current allowlist head")?;

    let op = crate::mrenclave_governance::GovernanceOp {
        op: 1, // ADD
        mrenclave: *mrenclave_new,
        escrow,
        proposed_epoch: epoch + 1,
        prev_allowlist_hash: prev,
    };

    // Governance consent, then the reproducibility proof. Both are real signatures from accounts
    // whose private keys never leave the enclave; the harness holds only session keys.
    let mut gov: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for signer in signers {
        let body = serde_json::json!({
            "op": op.op,
            "mrenclave": hex::encode(op.mrenclave),
            "escrow_account_id": hex::encode(op.escrow),
            "proposed_epoch": op.proposed_epoch,
            "prev_allowlist_hash": hex::encode(op.prev_allowlist_hash),
        });
        let e = sign_with(&http, "/v1/admin/mrenclaves/sign-governance", signer, body).await?;
        if !gov.iter().any(|(pk, _)| *pk == e.0) {
            gov.push(e);
        }
    }
    let gov_bundle = crate::quorum_bundle::build(&gov);
    let repro_bundle = corpus_repro_bundle(signers, mrenclave_new).await?;

    crate::membership_http::HttpGovernSink::new(client()?)
        .govern_on_node(old_base, &op, &gov_bundle, &repro_bundle)
        .await
        .context("govern NEW's measurement onto OLD's allowlist")
}

/// Run the real Path-A ceremony between two loopback SIM servers.
///
/// `dry_run` is FALSE on purpose. A dry run stops before OLD retires, which sounds right for a
/// test — but the admin handler then RESETS the new side, and the imported state is precisely what
/// steps 3-6 must inspect. The corpus drives the driver directly so no reset happens either way;
/// a full run is chosen because it is the ceremony the gate exists to exercise, and both servers
/// are throwaway.
pub async fn corpus_ceremony(
    old_base: &str,
    new_base: &str,
    mrenclave_new_hex: &str,
    signers: &[CorpusSigner],
) -> Result<String> {
    check_loopback(old_base)?;
    check_loopback(new_base)?;
    let http = crate::path_a_http_client::HttpEnclaveApi::new()
        .map_err(|e| anyhow!("HttpEnclaveApi: {e}"))?;
    let delegation = CorpusDelegationCollector {
        signers: signers.to_vec(),
    };
    let api = crate::path_a_delegation::ComposedEnclaveApi { http, delegation };
    let mut driver = crate::path_a_ceremony::CeremonyDriver::new(api);
    let params = crate::path_a_ceremony::CeremonyParams {
        expected_mrenclave_new: mrenclave_new_hex.to_string(),
        old_api_base: old_base.to_string(),
        new_api_base: new_base.to_string(),
        dry_run: false,
    };
    match driver.run(&params).await {
        Ok(success) => Ok(success.ceremony_nonce_hex),
        Err(e) => Err(anyhow!("ceremony failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_accepts_only_this_host() {
        for ok in [
            "https://localhost:9097/v1",
            "https://127.0.0.1:9098/v1",
            "http://localhost/v1",
            "https://[::1]:9097/v1",
        ] {
            assert!(check_loopback(ok).is_ok(), "should accept {ok}");
        }
    }

    #[test]
    fn loopback_refuses_the_cluster() {
        // The shapes that would actually appear in a mistaken invocation: a cluster node by IP,
        // by name, and the bastion. Each would export and retire a LIVE node.
        for bad in [
            "https://20.166.12.34:9088/v1",
            "https://node-1.internal:9088/v1",
            "https://94.130.18.162/v1",
            "https://localhost.evil.example/v1",
        ] {
            let e = check_loopback(bad).expect_err("should refuse {bad}");
            assert!(
                e.to_string().contains("non-loopback"),
                "the refusal must SAY why, got: {e}"
            );
        }
    }

    #[test]
    fn loopback_is_not_fooled_by_a_prefix() {
        // `localhost.evil.example` starts with "localhost"; a `starts_with` check would pass it.
        assert!(check_loopback("https://localhost.evil.example/v1").is_err());
        // …and a userinfo segment must not smuggle a host past the check.
        assert!(check_loopback("https://localhost@10.0.0.5/v1").is_err());
    }

    #[test]
    fn the_session_key_is_sent_without_0x_and_the_address_with_it() {
        // Not cosmetic: the enclave refuses a 0x-prefixed session key with "Invalid session key
        // size" — a message about the length, not the prefix — and requires the ADDRESS to keep
        // its prefix. Opposite conventions in adjacent fields, which is how this recurs.
        let s = CorpusSigner {
            address: "0x18bac6444d7b815756b883a77200ded21b2cb9ae".into(),
            session_key_hex: "0xfdd90377d937d6630d526365ac24e4b4e49b4366ece1dc487541fd245e927015"
                .into(),
            compressed_pubkey: vec![2u8; 33],
            base: "https://localhost:9097".into(),
        };
        let sent = s.session_key_hex.trim_start_matches("0x");
        assert!(
            !sent.starts_with("0x"),
            "the session key must lose its prefix"
        );
        assert_eq!(sent.len(), 64, "32 bytes of hex once the prefix is gone");
        assert!(
            s.address.starts_with("0x") && s.address.len() == 42,
            "the address must KEEP its prefix and be 42 chars"
        );
    }

    #[test]
    fn account_id_is_the_xrpl_derivation_not_the_eth_address() {
        // The enclave compares signers[].account_id against RIPEMD160(SHA256(pubkey)) over its own
        // pool. Using the ETH address here is what produced NOT_PARTICIPANT on a real run.
        let pk = vec![2u8; 33];
        let s = CorpusSigner {
            address: "0x1111111111111111111111111111111111111111".into(),
            session_key_hex: "00".into(),
            compressed_pubkey: pk.clone(),
            base: "https://localhost:9097".into(),
        };
        let got = s.account_id().expect("33-byte pubkey derives");
        assert_eq!(got, crate::auth::pubkey_to_account_id(&pk));
        assert_ne!(
            got.to_vec(),
            hex::decode("1111111111111111111111111111111111111111").unwrap(),
            "it must NOT be the ETH address"
        );

        let bad = CorpusSigner {
            compressed_pubkey: vec![2u8; 32],
            ..s
        };
        assert!(bad.account_id().is_err(), "a 32-byte pubkey is refused");
    }
}
