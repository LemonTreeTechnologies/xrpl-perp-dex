//! One place that binds an admin listener, so "how is this surface reached" is answered once.
//!
//! # Why this exists
//!
//! Five ceremony surfaces (migrate-state, signerlist-update, membership, dkg, and the Path-A
//! re-DKG + Safe routes) each bound their own `TcpListener` on loopback with the same hand-rolled
//! check, and each built its app as `let app = router(state)` — **no authentication at all**. On
//! 2026-10-08 four of them were running that way on 127.0.0.1:{7095,9100,9101,9102} across all
//! three nodes, carrying nine routes including state migration, SignerList change and MRENCLAVE
//! governance.
//!
//! # Why a unix socket rather than a credential
//!
//! The audit ruling of that day was that an operator allowlist is a precondition to serving such
//! a surface, and that **loopback is a mitigation, not the control** — a co-located process or an
//! SSRF reaches loopback. A credential answers that by making the caller prove who it is. A unix
//! socket answers it one layer lower: the OS decides who may connect, by file ownership and mode,
//! and nothing on the network can reach it at all.
//!
//! That is the right shape here because these surfaces are **IPC into the running orchestrator**,
//! not a public API. They cannot be CLI subcommands — `migrate-state` needs the live p2p
//! delegation channel, `signerlist-update` needs the live signing relay, `dkg/start` needs the
//! step-publish channel — so a separate process cannot reach them, and the listener is the IPC
//! mechanism rather than an accident. A socket at mode 0600 is the IPC equivalent of "only this
//! user", which is what the operator-only property actually means on a single host.
//!
//! Both modes are supported so the switch is per-surface and reversible: a value starting with
//! `/` is a socket path, anything else is parsed as a loopback TCP address exactly as before.

use std::os::unix::fs::PermissionsExt;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// Mode the socket must end up with. Owner-only: group and other get nothing.
const SOCKET_MODE: u32 = 0o600;

/// Bind and serve an admin router, on a unix socket or a loopback TCP port.
///
/// `flag` and `label` only shape the messages — an operator reading a refusal needs to know
/// which flag they set and which surface it was.
pub async fn serve_admin(
    flag: &str,
    listen: &str,
    router: axum::Router,
    label: &str,
) -> Result<()> {
    if listen.starts_with('/') {
        serve_on_socket(flag, listen, router, label).await
    } else {
        serve_on_loopback_tcp(flag, listen, router, label).await
    }
}

async fn serve_on_socket(flag: &str, path: &str, router: axum::Router, label: &str) -> Result<()> {
    let p = std::path::Path::new(path);
    let parent = p
        .parent()
        .with_context(|| format!("--{flag} {path:?} has no parent directory"))?;
    if !parent.is_dir() {
        bail!(
            "--{flag} {path:?}: the directory {} does not exist",
            parent.display()
        );
    }

    // THE PARENT DIRECTORY MUST BE PRIVATE (audit hardening, 2026-10-08). A 0600 socket under a
    // traversable or group-writable directory still allows a swap or a race: there is a window
    // between `bind` and the chmod, and another between removing a stale socket and binding the
    // new one. Both are same-uid and sub-millisecond, and a 0700 parent closes them by denying
    // anyone else the right to be in that directory at all. CHECKED rather than documented,
    // because a documented precondition is one nobody verifies.
    let pmode = std::fs::metadata(parent)
        .with_context(|| format!("cannot stat {}", parent.display()))?
        .permissions()
        .mode()
        & 0o777;
    if pmode & 0o077 != 0 {
        bail!(
            "--{flag} {path:?}: the directory {} is mode {pmode:o}; it must deny group and other \
             so no one else can race the socket into place. chmod 0700 it and retry.",
            parent.display()
        );
    }

    // A STALE SOCKET IS REPLACED; ANYTHING ELSE IS REFUSED. Unlinking whatever happens to sit at
    // a configured path is how a config typo deletes a file that matters, so the type is checked
    // first and only a socket is removed.
    match std::fs::symlink_metadata(p) {
        Ok(meta) => {
            let ft = meta.file_type();
            #[cfg(unix)]
            let is_sock = std::os::unix::fs::FileTypeExt::is_socket(&ft);
            if !is_sock {
                bail!(
                    "--{flag} {path:?} exists and is NOT a socket — refusing to unlink it. \
                     Point the flag elsewhere or remove that file deliberately."
                );
            }
            std::fs::remove_file(p)
                .with_context(|| format!("could not remove the stale socket {path:?}"))?;
            warn!(path = %path, "replaced a stale admin socket left by a previous run");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot stat {path:?}")),
    }

    let listener = tokio::net::UnixListener::bind(p)
        .with_context(|| format!("failed to bind the {label} admin socket at {path:?}"))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(SOCKET_MODE))
        .with_context(|| format!("could not set mode 0600 on {path:?}"))?;

    // READ THE MODE BACK. Setting a permission and assuming it took is the same shape as a gate
    // that reports success without checking — and a socket left group-readable is exactly the
    // access this change exists to remove.
    let got = std::fs::metadata(p)
        .with_context(|| format!("cannot stat {path:?} after bind"))?
        .permissions()
        .mode()
        & 0o777;
    if got != SOCKET_MODE {
        bail!("{path:?} came back mode {got:o}, expected {SOCKET_MODE:o} — refusing to serve");
    }

    info!(path = %path, mode = "0600", surface = %label,
          "admin listener on a unix socket — the OS decides who may connect");
    axum::serve(listener, router)
        .await
        .map_err(|e| anyhow::anyhow!("{label} admin listener serve error: {e}"))?;
    Ok(())
}

async fn serve_on_loopback_tcp(
    flag: &str,
    addr: &str,
    router: axum::Router,
    label: &str,
) -> Result<()> {
    let parsed: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --{flag} address {addr:?}: {e}"))?;
    if !parsed.ip().is_loopback() {
        bail!(
            "--{flag} must resolve to a loopback address; got {}",
            parsed.ip()
        );
    }
    let listener = tokio::net::TcpListener::bind(parsed).await.map_err(|e| {
        anyhow::anyhow!("failed to bind the {label} admin listener on {parsed}: {e}")
    })?;
    // SAID AT EVERY START, not written in a doc. Loopback is reachable by every process on the
    // box and by an SSRF; the socket form is not.
    warn!(listen = %parsed, surface = %label,
          "admin listener on a loopback TCP port — reachable by ANY local process. \
           Prefer a unix socket: pass an absolute path to --{flag}");
    axum::serve(listener, router)
        .await
        .map_err(|e| anyhow::anyhow!("{label} admin listener serve error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A temp dir at 0700, because the socket path's parent must deny group and other.
    ///
    /// `tempfile::tempdir()` honours the umask, and on this machine that yields 0775 — so the
    /// guard refused every fixture the moment it landed. That is the guard working: a default
    /// directory is NOT private, which is precisely why the check exists rather than a note
    /// saying "put it somewhere private".
    fn private_tempdir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn probe_router() -> axum::Router {
        axum::Router::new().route(
            "/probe",
            axum::routing::post(|| async { "reached the router" }),
        )
    }

    /// Speak HTTP/1.1 over the socket by hand.
    ///
    /// No client crate does unix-socket HTTP without a feature we do not carry, and the point
    /// here is to prove the socket really serves the router — so the bytes are written straight
    /// in, the way the WS split-body corpus drives a raw TCP segment.
    async fn post_over_socket(path: &std::path::Path, uri: &str) -> String {
        let mut s = tokio::net::UnixStream::connect(path)
            .await
            .expect("connect");
        let req = format!(
            "POST {uri} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.expect("write");
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("read");
        String::from_utf8_lossy(&out).to_string()
    }

    /// A socket is bound at mode 0600 AND it really serves the router.
    ///
    /// Both halves matter: a socket nobody can reach is not a surface, and a surface at mode
    /// 0666 is not the control this change exists to install.
    #[tokio::test]
    async fn a_socket_is_owner_only_and_serves_the_router() {
        let dir = private_tempdir();
        let path = dir.path().join("admin.sock");
        let p = path.clone();
        tokio::spawn(async move {
            let _ = serve_admin("admin-listen", p.to_str().unwrap(), probe_router(), "probe").await;
        });
        // wait for the bind rather than sleeping blind
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let meta = std::fs::symlink_metadata(&path).expect("the socket must exist");
        assert!(
            std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()),
            "not a socket"
        );
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "the socket must be owner-only"
        );
        let resp = post_over_socket(&path, "/probe").await;
        assert!(resp.contains("200 OK"), "{resp}");
        assert!(resp.contains("reached the router"), "{resp}");
    }

    /// A REGULAR FILE at the configured path is refused, and NOT unlinked.
    ///
    /// Blindly removing whatever sits at a configured path is how a typo deletes something that
    /// matters. The file must still be there afterwards — asserted, because "we refuse" and "we
    /// refuse without touching it" are different promises.
    #[tokio::test]
    async fn a_regular_file_at_the_path_is_refused_and_left_alone() {
        let dir = private_tempdir();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"precious").unwrap();
        // BOUNDED, and this is the probe teaching the test. Under the mutation that unlinks
        // ANYTHING at the path, serve_admin binds and then serves forever — so an unbounded
        // `.await` here HANGS instead of failing, and my probe sat on it for 928 seconds
        // before I looked. A refusal is supposed to be immediate; the timeout is what makes
        // "it served instead" a RED result rather than a stuck one.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_admin(
                "admin-listen",
                path.to_str().unwrap(),
                probe_router(),
                "probe",
            ),
        )
        .await
        .expect("a regular file must be refused IMMEDIATELY, not served")
        .expect_err("a regular file must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("NOT a socket"), "{msg}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"precious",
            "the file was touched"
        );
    }

    /// A STALE socket from a previous run is replaced, so a restart is not blocked by its own
    /// leftovers — the common operational case, and the only kind of path this will unlink.
    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let dir = private_tempdir();
        let path = dir.path().join("stale.sock");
        // leave a real but unserved socket behind, as a killed process would
        let orphan = tokio::net::UnixListener::bind(&path).unwrap();
        drop(orphan);
        assert!(path.exists());

        let p = path.clone();
        tokio::spawn(async move {
            let _ = serve_admin("admin-listen", p.to_str().unwrap(), probe_router(), "probe").await;
        });
        // Retry the CONNECT, not just the request: before the rebind the path still holds the
        // dead socket and connecting to it fails. The first version of this loop called a
        // helper that `expect`ed the connect, so the probe panicked instead of waiting — the
        // test harness failing, not the subject.
        for _ in 0..300 {
            if tokio::net::UnixStream::connect(&path).await.is_ok()
                && post_over_socket(&path, "/probe").await.contains("200 OK")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the stale socket was not replaced by a serving one");
    }

    /// A socket under a GROUP- OR WORLD-ACCESSIBLE directory is refused.
    ///
    /// A 0600 socket in a traversable directory still allows a swap or a race in the window
    /// between removing a stale socket and binding the new one. The mode of the socket is not
    /// the whole control; the directory is half of it.
    #[tokio::test]
    async fn a_loose_parent_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("admin.sock");
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_admin(
                "admin-listen",
                path.to_str().unwrap(),
                probe_router(),
                "probe",
            ),
        )
        .await
        .expect("a loose parent directory must be refused IMMEDIATELY, not served")
        .expect_err("a loose parent directory must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("755"), "{msg}");
        assert!(
            !path.exists(),
            "no socket may be created under a loose directory"
        );
    }

    /// The TCP mode keeps its loopback-only refusal — the switch must not have widened it.
    #[tokio::test]
    async fn a_non_loopback_tcp_address_is_still_refused() {
        // Bounded for the same reason as the regular-file test: with the loopback check
        // removed this BINDS and serves forever, so an unbounded await hangs instead of
        // failing. My probe reported HUNG rather than RED, and a stuck probe is not a pass —
        // a refusal must be immediate, and the timeout is what says so.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_admin("admin-listen", "0.0.0.0:59999", probe_router(), "probe"),
        )
        .await
        .expect("a non-loopback address must be refused IMMEDIATELY, not served")
        .expect_err("a non-loopback address must be refused");
        assert!(format!("{err:#}").contains("loopback"), "{err:#}");
    }
}
