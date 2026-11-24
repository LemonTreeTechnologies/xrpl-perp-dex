//! Startup gate: wait until the local enclave answers before anything reads from it.
//!
//! Several startup steps read from the enclave exactly once (the ECDH identity for Path A, the announcer,
//! the DKG coordinator and the join-brain; the shard id) and treat a failure as "this feature is not
//! available here", which turns it off for the life of the process. When the enclave and the orchestrator
//! start together (an enclave restart restarts the orchestrator through `Requires=`), systemd's `After=`
//! orders the starts but does not wait for the enclave to listen, so those reads met a refused connection
//! and the node ran with them off, silently. This gate makes the order real: the orchestrator does not go
//! past it until the enclave answers HTTP, and refuses to start if it never does.
//!
//! "Answers" means any HTTP response, including an error status: an enclave without one of those routes
//! (an older build) is up, and the step that needs the route decides what that means, as before.

use anyhow::{bail, Result};
use async_trait::async_trait;
use std::time::Duration;

/// Whether the enclave answers at all. Err carries why not (for the refusal message).
#[async_trait]
pub trait Answers {
    async fn answers(&self) -> std::result::Result<(), String>;
}

/// The local enclave over HTTP(S), loopback only.
pub struct HttpEnclave {
    url: String,
    client: reqwest::Client,
}

impl HttpEnclave {
    pub fn new(base_url: &str) -> Result<Self> {
        crate::http_helpers::ensure_loopback_url(base_url)?;
        Ok(Self {
            url: format!("{}/pool/ecdh/pubkey", base_url.trim_end_matches('/')),
            client: crate::http_helpers::loopback_http_client(Duration::from_secs(5))?,
        })
    }
}

#[async_trait]
impl Answers for HttpEnclave {
    async fn answers(&self) -> std::result::Result<(), String> {
        match self.client.get(&self.url).send().await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{e:#}")),
        }
    }
}

/// Wait for the enclave to answer, trying `attempts` times `every` apart. Returns the attempt it answered
/// on (1-based); refuses when it never answers — a node must not start with its enclave-backed features
/// quietly off.
pub async fn await_enclave(e: &dyn Answers, attempts: u32, every: Duration) -> Result<u32> {
    let mut last = String::new();
    for i in 1..=attempts {
        match e.answers().await {
            Ok(()) => return Ok(i),
            Err(why) => last = why,
        }
        if i < attempts {
            tokio::time::sleep(every).await;
        }
    }
    bail!(
        "the local enclave did not answer after {attempts} attempts (last: {last}); refusing to start \
         rather than run with the enclave-backed startup steps off"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct UpAfter {
        refusals: u32,
        calls: AtomicU32,
    }

    #[async_trait]
    impl Answers for UpAfter {
        async fn answers(&self) -> std::result::Result<(), String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.refusals {
                Err("connection refused".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn it_waits_through_refusals_until_the_enclave_answers() {
        let e = UpAfter {
            refusals: 3,
            calls: AtomicU32::new(0),
        };
        assert_eq!(await_enclave(&e, 10, Duration::ZERO).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn it_refuses_to_start_when_the_enclave_never_answers() {
        let e = UpAfter {
            refusals: u32::MAX,
            calls: AtomicU32::new(0),
        };
        let err = await_enclave(&e, 5, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("after 5 attempts") && err.contains("connection refused"),
            "{err}"
        );
        assert_eq!(e.calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_closed_port_does_not_answer_and_any_http_status_does() {
        // A port nobody listens on: bind, read the port, drop the listener.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let closed = HttpEnclave::new(&format!("http://127.0.0.1:{port}/v1")).unwrap();
        assert!(closed.answers().await.is_err());

        // A server that answers 404 on every route is up: the route's absence is not unreadiness.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, axum::Router::new()).await.unwrap() });
        let up = HttpEnclave::new(&format!("http://{a}/v1")).unwrap();
        assert_eq!(up.answers().await, Ok(()));
    }

    #[test]
    fn only_a_loopback_enclave_is_accepted() {
        assert!(HttpEnclave::new("https://10.0.0.5:9089/v1").is_err());
    }
}
