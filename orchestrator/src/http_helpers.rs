//! O-L4: centralized reqwest client factory for loopback-only calls.
//!
//! Before this module every caller reaching the local enclave built its
//! own `reqwest::Client` with `danger_accept_invalid_certs(true)`. That
//! flag is correct for 127.0.0.1 (the enclave's self-signed cert will
//! never chain to a public CA) but leaves a foot-gun: a future caller
//! might reuse the same builder pattern against a non-loopback URL and
//! silently trust an attacker-controlled cert on the wire.
//!
//! `loopback_http_client` is the one-line way to get a client with the
//! invalid-certs relaxation. `ensure_loopback_url` is called at setup
//! time by the loopback-only clients (`PerpClient`, `PoolPathAClient`)
//! so misconfiguration fails fast rather than silently sending enclave
//! traffic over an untrusted transport.
//!
//! Cross-VM signer calls (`withdrawal::collect_signatures`, the p2p
//! relay fallback) remain on their inline builders pending enclave-side
//! E-M2 (CA-signed or pinned-pubkey cert verification).

use std::time::Duration;

use anyhow::{bail, Result};

/// Build a `reqwest::Client` that accepts self-signed certs. Intended
/// for calls whose URL has already been verified loopback — use
/// `ensure_loopback_url` at setup to make that invariant machine-checked.
pub fn loopback_http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(timeout)
        .build()
        .map_err(Into::into)
}

/// Reject URLs that aren't directed at a loopback interface. Parses the
/// host component and compares against `127.0.0.0/8`, `::1`, and the
/// `localhost` hostname. Anything else is rejected so accidental
/// cross-VM use of `loopback_http_client` fails at startup.
pub fn ensure_loopback_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid url: {e}"))?;
    let host = match parsed.host_str() {
        Some(h) => h,
        None => bail!("url has no host: {url}"),
    };
    if host == "localhost" {
        return Ok(());
    }
    // IPv6 literals come back wrapped in square brackets from `host_str`.
    let bare_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(v4) = bare_host.parse::<std::net::Ipv4Addr>() {
        if v4.is_loopback() {
            return Ok(());
        }
    }
    if let Ok(v6) = bare_host.parse::<std::net::Ipv6Addr>() {
        if v6.is_loopback() {
            return Ok(());
        }
    }
    bail!("expected loopback host, got '{host}' in {url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_localhost_hostname() {
        ensure_loopback_url("https://localhost:9088/v1").unwrap();
    }

    #[test]
    fn accepts_ipv4_loopback() {
        ensure_loopback_url("https://127.0.0.1:9088/v1").unwrap();
        ensure_loopback_url("http://127.1.2.3/").unwrap();
    }

    #[test]
    fn accepts_ipv6_loopback() {
        ensure_loopback_url("https://[::1]:9088/v1").unwrap();
    }

    #[test]
    fn rejects_public_host() {
        assert!(ensure_loopback_url("https://enclave.example.com:9088/v1").is_err());
    }

    #[test]
    fn rejects_non_loopback_ip() {
        assert!(ensure_loopback_url("https://10.0.0.5:9088/v1").is_err());
        assert!(ensure_loopback_url("https://192.168.1.1:9088/v1").is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert!(ensure_loopback_url("not a url").is_err());
    }
}
