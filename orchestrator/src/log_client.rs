//! Client for the enclave's replicated-log routes (`/v1/perp/log/*`).
//!
//! The routes are thin relays to one ecall each; the enclave validates everything. A refusal comes back
//! as HTTP 409 (500 when the ecall did not run) with the enclave's own code in `ret`, so every outcome
//! here is TYPED: callers match on [`LogOutcome`] instead of parsing message text.
//!
//! The producer and follower that call this land in the next increments.
#![allow(dead_code)]

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The enclave's log return codes this host acts on. Values mirror `perp_log.h` / `perp_log_view.h`
/// in the enclave repository.
pub mod rc {
    pub const OK: i64 = 0;
    pub const ALREADY_APPLIED: i64 = 1;
    pub const GAP: i64 = -60;
    pub const NOT_YET: i64 = -61;
    pub const DIVERGED: i64 = -62;
    pub const MALFORMED: i64 = -63;
    pub const BAD_INPUT: i64 = -64;
    pub const FULL: i64 = -65;
    pub const BAD_SIG: i64 = -67;
    pub const STALE: i64 = -68;
    pub const FENCED: i64 = -69;
    pub const BAD_BLOB: i64 = -70;
    pub const PROLOGUE_FAILED: i64 = -71;
    /// A sign/persist ecall refused because this node is fenced.
    pub const ERR_FENCED: i64 = -128;
    /// No log before genesis: the log's trust section is not seeded yet.
    pub const OFF: i64 = -134;
    /// The ecall did not run (enclave not initialised, SGX status, malformed account on the host side).
    pub const NOT_RUN: i64 = -1000;
}

/// What a log call came back with, classified for the caller's loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutcome {
    /// Success; the body is in the `Ok` value of the call.
    Ok,
    /// `seq <= head`: an idempotent no-op, move on.
    AlreadyApplied,
    /// Retry later or fetch again: never a reason to stop (GAP, NOT_YET, STALE, BAD_BLOB, BAD_SIG).
    Transient(i64),
    /// This node can no longer follow (DIVERGED, FENCED, a prologue failure, or a sign/persist refusal
    /// while fenced). Stop applying, raise the alarm, mark reads diverged.
    Halted(i64),
    /// The log is not on yet (before genesis).
    Off,
    /// The ecall did not run.
    NotRun,
    /// Any other refusal, with its code (a malformed batch, a full builder, an entry's own code).
    Refused(i64),
}

impl LogOutcome {
    pub fn from_rc(code: i64) -> Self {
        match code {
            rc::OK => LogOutcome::Ok,
            rc::ALREADY_APPLIED => LogOutcome::AlreadyApplied,
            rc::GAP | rc::NOT_YET | rc::STALE | rc::BAD_BLOB | rc::BAD_SIG => {
                LogOutcome::Transient(code)
            }
            rc::DIVERGED | rc::FENCED | rc::PROLOGUE_FAILED | rc::ERR_FENCED => {
                LogOutcome::Halted(code)
            }
            rc::OFF => LogOutcome::Off,
            rc::NOT_RUN => LogOutcome::NotRun,
            other => LogOutcome::Refused(other),
        }
    }
}

/// A non-success answer from a log route.
#[derive(Debug)]
pub enum LogError {
    /// The enclave answered with a code.
    Enclave(LogOutcome),
    /// The server answered without a code (bad request, unknown route, unreadable body).
    Http { status: u16, body: String },
    /// The request never completed.
    Transport(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Enclave(o) => write!(f, "log refused: {o:?}"),
            LogError::Http { status, body } => write!(f, "log route HTTP {status}: {body}"),
            LogError::Transport(e) => write!(f, "log route unreachable: {e}"),
        }
    }
}

impl std::error::Error for LogError {}

/// Classify a non-200 answer. 409/500 carrying an integer `ret` are the enclave's; anything else is HTTP.
pub fn classify_failure(status: u16, body: &str) -> LogError {
    if status == 409 || status == 500 {
        if let Some(code) = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("ret").and_then(Value::as_i64))
        {
            return LogError::Enclave(LogOutcome::from_rc(code));
        }
    }
    LogError::Http {
        status,
        body: body.trim().to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub seq: u64,
    pub t_batch: u64,
    pub chain_hash: [u8; 32],
    pub fenced: bool,
    /// Log mode: the enclave's log trust is seeded (before genesis the head reads seq 0 either way).
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub seq: u64,
    pub chain_hash: [u8; 32],
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub batch: Vec<u8>,
    /// This node's member signature over the batch.
    pub sig: Vec<u8>,
    /// This node's APPLIED share for the batch.
    pub cert: Vec<u8>,
}

/// One live order of the enclave book (the binding hash stays inside the enclave).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookOrder {
    pub order_id: u64,
    pub user_id: String,
    pub side: u8,
    pub kind: u8,
    pub tif: u8,
    pub reduce_only: u8,
    pub target_position_id: u32,
    pub limit: i64,
    pub size: i64,
    pub filled: i64,
    pub leverage: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookPage {
    pub orders: Vec<BookOrder>,
    pub total: u32,
    pub last_order_id: u64,
}

/// Width of the user id field in a book record (`USER_ID_SIZE` in the enclave).
pub const BOOK_USER_ID_LEN: usize = 36;
/// One book record: order_id 8 | user_id 36 | side kind tif reduce_only 4 | target 4 | 4 × i64.
pub const BOOK_REC_LEN: usize = 8 + BOOK_USER_ID_LEN + 4 + 4 + 32;

/// Decode a page of book records. Strict: the length must be exactly `n` whole records, and a user id
/// must be printable ASCII up to its first NUL, with only zeros after it.
pub fn decode_book_page(page: &[u8], n: u32) -> Result<Vec<BookOrder>> {
    let n = n as usize;
    anyhow::ensure!(
        page.len()
            == n.checked_mul(BOOK_REC_LEN)
                .context("record count overflows")?,
        "book page is {} bytes, expected {} records of {}",
        page.len(),
        n,
        BOOK_REC_LEN
    );
    let be64 = |b: &[u8]| u64::from_be_bytes(b.try_into().expect("8 bytes"));
    let mut out = Vec::with_capacity(n);
    for r in page.chunks_exact(BOOK_REC_LEN) {
        let uid = &r[8..8 + BOOK_USER_ID_LEN];
        let end = uid.iter().position(|&c| c == 0).unwrap_or(BOOK_USER_ID_LEN);
        anyhow::ensure!(
            uid[end..].iter().all(|&c| c == 0),
            "user id has bytes after its terminator"
        );
        anyhow::ensure!(
            uid[..end].iter().all(|c| c.is_ascii_graphic()),
            "user id is not printable ASCII"
        );
        let p = 8 + BOOK_USER_ID_LEN;
        out.push(BookOrder {
            order_id: be64(&r[0..8]),
            user_id: String::from_utf8(uid[..end].to_vec()).expect("checked ASCII"),
            side: r[p],
            kind: r[p + 1],
            tif: r[p + 2],
            reduce_only: r[p + 3],
            target_position_id: u32::from_be_bytes(r[p + 4..p + 8].try_into().expect("4 bytes")),
            limit: be64(&r[p + 8..p + 16]) as i64,
            size: be64(&r[p + 16..p + 24]) as i64,
            filled: be64(&r[p + 24..p + 32]) as i64,
            leverage: be64(&r[p + 32..p + 40]) as i64,
        });
    }
    Ok(out)
}

fn hex32(v: &Value, key: &str) -> Result<[u8; 32]> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}"))?;
    let b = hex::decode(s).with_context(|| format!("{key} is not hex"))?;
    b.try_into()
        .map_err(|_| anyhow::anyhow!("{key} is not 32 bytes"))
}

fn hexv(v: &Value, key: &str) -> Result<Vec<u8>> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}"))?;
    hex::decode(s).with_context(|| format!("{key} is not hex"))
}

fn u64f(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("missing {key}"))
}

/// The inputs of one apply: what the follower received for a batch.
pub struct ApplyInput<'a> {
    pub batch: &'a [u8],
    pub quote_blob: &'a [u8],
    pub market_id: u32,
    pub side_blobs: &'a [u8],
    pub sig_bundle: &'a [u8],
    pub cert_bundle: &'a [u8],
}

#[derive(Clone)]
pub struct LogClient {
    base_url: String,
    client: reqwest::Client,
}

impl LogClient {
    /// Same loopback rule as the main enclave client: TLS checks are relaxed only for a loopback URL.
    pub fn new(base_url: &str) -> Result<Self> {
        crate::http_helpers::ensure_loopback_url(base_url)
            .context("LogClient requires a loopback enclave URL")?;
        let client = crate::http_helpers::loopback_http_client(std::time::Duration::from_secs(30))
            .context("failed to build reqwest client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, LogError> {
        let resp = req
            .send()
            .await
            .map_err(|e| LogError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| LogError::Transport(e.to_string()))?;
        if status != 200 {
            return Err(classify_failure(status, &text));
        }
        serde_json::from_str(&text).map_err(|_| LogError::Http { status, body: text })
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, LogError> {
        self.send(
            self.client
                .post(format!("{}{}", self.base_url, path))
                .json(&body),
        )
        .await
    }

    async fn get(&self, path: &str) -> Result<Value, LogError> {
        self.send(self.client.get(format!("{}{}", self.base_url, path)))
            .await
    }

    fn shape<T>(v: Value, f: impl FnOnce(&Value) -> Result<T>) -> Result<T, LogError> {
        f(&v).map_err(|e| LogError::Http {
            status: 200,
            body: format!("{e}: {v}"),
        })
    }

    pub async fn head(&self) -> Result<Head, LogError> {
        let v = self.get("/perp/log/head").await?;
        Self::shape(v, |v| {
            Ok(Head {
                seq: u64f(v, "seq")?,
                t_batch: u64f(v, "t_batch")?,
                chain_hash: hex32(v, "chain_hash")?,
                fenced: v
                    .get("fenced")
                    .and_then(Value::as_bool)
                    .context("missing fenced")?,
                on: v.get("on").and_then(Value::as_bool).context("missing on")?,
            })
        })
    }

    pub async fn view(&self) -> Result<View, LogError> {
        let v = self.get("/perp/log/view").await?;
        Self::shape(v, |v| {
            Ok(View {
                seq: u64f(v, "seq")?,
                chain_hash: hex32(v, "chain_hash")?,
                digest: hex32(v, "digest")?,
            })
        })
    }

    pub async fn book(&self, offset: u32, limit: u32) -> Result<BookPage, LogError> {
        let v = self
            .get(&format!("/perp/log/book?offset={offset}&limit={limit}"))
            .await?;
        Self::shape(v, |v| {
            let n = u32::try_from(u64f(v, "n")?).context("n out of range")?;
            Ok(BookPage {
                orders: decode_book_page(&hexv(v, "page")?, n)?,
                total: u32::try_from(u64f(v, "total")?).context("total out of range")?,
                last_order_id: u64f(v, "last_order_id")?,
            })
        })
    }

    pub async fn begin(&self, quote_blob: &[u8], market_id: u32) -> Result<(), LogError> {
        self.post(
            "/perp/log/begin",
            json!({"quote_blob": hex::encode(quote_blob), "market_id": market_id}),
        )
        .await?;
        Ok(())
    }

    /// Do these APPLIED signatures for (seq, chain_hash) verify inside the enclave against the sealed
    /// signer list — one share (`quorum` false) or a whole certificate (`quorum` true)? `Ok(())` = yes;
    /// a refusal (`BAD_SIG`) = no.
    pub async fn check_applied(
        &self,
        seq: u64,
        chain_hash: &[u8; 32],
        bundle: &[u8],
        quorum: bool,
    ) -> Result<(), LogError> {
        self.post(
            "/perp/log/check_applied",
            json!({"seq": seq, "chain_hash": hex::encode(chain_hash), "bundle": hex::encode(bundle), "quorum": quorum}),
        )
        .await?;
        Ok(())
    }

    pub async fn add(&self, entry: &[u8], blob: &[u8]) -> Result<(), LogError> {
        self.post(
            "/perp/log/add",
            json!({"entry": hex::encode(entry), "blob": hex::encode(blob)}),
        )
        .await?;
        Ok(())
    }

    pub async fn commit(
        &self,
        account_id: &str,
        session_key: &[u8; 32],
    ) -> Result<Committed, LogError> {
        let v = self
            .post(
                "/perp/log/commit",
                json!({"account_id": account_id, "session_key": hex::encode(session_key)}),
            )
            .await?;
        Self::shape(v, |v| {
            Ok(Committed {
                batch: hexv(v, "batch")?,
                sig: hexv(v, "sig")?,
                cert: hexv(v, "cert")?,
            })
        })
    }

    /// Apply one batch. `Ok(cert)` is this node's APPLIED share (empty when the apply signs none).
    pub async fn apply(
        &self,
        input: &ApplyInput<'_>,
        account_id: &str,
        session_key: &[u8; 32],
    ) -> Result<Vec<u8>, LogError> {
        let v = self
            .post(
                "/perp/log/apply",
                json!({
                    "batch": hex::encode(input.batch),
                    "quote_blob": hex::encode(input.quote_blob),
                    "market_id": input.market_id,
                    "side_blobs": hex::encode(input.side_blobs),
                    "sig_bundle": hex::encode(input.sig_bundle),
                    "cert_bundle": hex::encode(input.cert_bundle),
                    "account_id": account_id,
                    "session_key": hex::encode(session_key),
                }),
            )
            .await?;
        Self::shape(v, |v| hexv(v, "cert"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Query,
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use std::collections::HashMap;

    #[test]
    fn every_code_lands_in_its_class() {
        assert_eq!(LogOutcome::from_rc(0), LogOutcome::Ok);
        assert_eq!(LogOutcome::from_rc(1), LogOutcome::AlreadyApplied);
        for c in [-60, -61, -67, -68, -70] {
            assert_eq!(LogOutcome::from_rc(c), LogOutcome::Transient(c), "code {c}");
        }
        for c in [-62, -69, -71, -128] {
            assert_eq!(LogOutcome::from_rc(c), LogOutcome::Halted(c), "code {c}");
        }
        assert_eq!(LogOutcome::from_rc(-134), LogOutcome::Off);
        assert_eq!(LogOutcome::from_rc(-1000), LogOutcome::NotRun);
        for c in [-63, -64, -65, -66, -72, -1092, -4135] {
            assert_eq!(LogOutcome::from_rc(c), LogOutcome::Refused(c), "code {c}");
        }
    }

    #[test]
    fn a_refusal_needs_both_the_status_and_an_integer_ret() {
        assert!(matches!(
            classify_failure(409, r#"{"ret":-62}"#),
            LogError::Enclave(LogOutcome::Halted(-62))
        ));
        assert!(matches!(
            classify_failure(500, r#"{"ret":-1000}"#),
            LogError::Enclave(LogOutcome::NotRun)
        ));
        assert!(matches!(
            classify_failure(400, r#"{"ret":-62}"#),
            LogError::Http { status: 400, .. }
        ));
        assert!(matches!(
            classify_failure(409, r#"{"ret":"-62"}"#),
            LogError::Http { status: 409, .. }
        ));
        assert!(matches!(
            classify_failure(409, "rc=-62"),
            LogError::Http { status: 409, .. }
        ));
    }

    fn rec(id: u64, user: &str, limit: i64) -> Vec<u8> {
        let mut r = id.to_be_bytes().to_vec();
        let mut u = user.as_bytes().to_vec();
        u.resize(BOOK_USER_ID_LEN, 0);
        r.extend(u);
        r.extend([1u8, 2, 3, 0]);
        r.extend(7u32.to_be_bytes());
        for v in [limit, 5, 1, 3] {
            r.extend(v.to_be_bytes());
        }
        r
    }

    #[test]
    fn book_records_decode_field_by_field() {
        assert_eq!(BOOK_REC_LEN, 84);
        let mut page = rec(7, "rAlice", 2_0000_0000);
        page.extend(rec(9, "rBob", -1));
        let o = decode_book_page(&page, 2).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(
            o[0],
            BookOrder {
                order_id: 7,
                user_id: "rAlice".into(),
                side: 1,
                kind: 2,
                tif: 3,
                reduce_only: 0,
                target_position_id: 7,
                limit: 2_0000_0000,
                size: 5,
                filled: 1,
                leverage: 3
            }
        );
        assert_eq!(o[1].limit, -1, "a negative limit round-trips");
        assert!(
            decode_book_page(&page, 1).is_err(),
            "length must be exactly n records"
        );
        assert!(
            decode_book_page(&page[..83], 1).is_err(),
            "a partial record is refused"
        );
        let mut garbage = rec(1, "rA", 1);
        garbage[8 + 5] = b'x';
        assert!(
            decode_book_page(&garbage, 1).is_err(),
            "bytes after the terminator are refused"
        );
        let mut ctrl = rec(1, "rA", 1);
        ctrl[8] = 0x07;
        assert!(
            decode_book_page(&ctrl, 1).is_err(),
            "a non-printable user id is refused"
        );
        assert!(decode_book_page(&[], 0).unwrap().is_empty());
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn routes_carry_the_wire_contract_and_typed_refusals() {
        let app = Router::new()
            .route(
                "/v1/perp/log/head",
                get(|| async { Json(json!({"status":"success","seq":3,"t_batch":9,"chain_hash":"ab".repeat(32),"fenced":false,"on":true})) }),
            )
            .route(
                "/v1/perp/log/view",
                get(|| async { Json(json!({"status":"success","seq":3,"chain_hash":"ab".repeat(32),"digest":"cd".repeat(32)})) }),
            )
            .route(
                "/v1/perp/log/book",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert_eq!(q.get("offset").map(String::as_str), Some("2"));
                    assert_eq!(q.get("limit").map(String::as_str), Some("5"));
                    let page = hex::encode(rec(12, "rCarol", 1));
                    Json(json!({"status":"success","page":page,"n":1,"total":3,"last_order_id":12}))
                }),
            )
            .route(
                "/v1/perp/log/begin",
                post(|Json(b): Json<Value>| async move {
                    assert_eq!(b["quote_blob"], "0102");
                    assert_eq!(b["market_id"], 4);
                    (StatusCode::CONFLICT, Json(json!({"status":"error","ret":-134})))
                }),
            )
            .route(
                "/v1/perp/log/add",
                post(|| async { (StatusCode::BAD_REQUEST, Json(json!({"status":"error","message":"entry"}))) }),
            )
            .route(
                "/v1/perp/log/commit",
                post(|Json(b): Json<Value>| async move {
                    assert_eq!(b["session_key"], "11".repeat(32));
                    Json(json!({"status":"success","batch":"aa","sig":"bb","cert":"cc"}))
                }),
            )
            .route(
                "/v1/perp/log/apply",
                post(|Json(b): Json<Value>| async move {
                    for k in ["batch", "quote_blob", "side_blobs", "sig_bundle", "cert_bundle", "account_id", "session_key"] {
                        assert!(b.get(k).and_then(Value::as_str).is_some(), "apply body carries {k}");
                    }
                    assert_eq!(b["market_id"], 0);
                    (StatusCode::CONFLICT, Json(json!({"status":"error","ret":-62})))
                }),
            )
            .route(
                "/v1/perp/log/check_applied",
                post(|Json(b): Json<Value>| async move {
                    assert_eq!((b["seq"].as_u64(), b["chain_hash"].as_str()), (Some(7), Some("ee".repeat(32).as_str())));
                    assert_eq!(b["bundle"], "0102");
                    if b["quorum"] == true {
                        (StatusCode::OK, Json(json!({"status":"success"})))
                    } else {
                        (StatusCode::CONFLICT, Json(json!({"status":"error","ret":-67})))
                    }
                }),
            );
        let c = LogClient::new(&serve(app).await).unwrap();
        let h = c.head().await.unwrap();
        assert_eq!(
            (h.seq, h.t_batch, h.chain_hash, h.fenced, h.on),
            (3, 9, [0xab; 32], false, true)
        );
        assert_eq!(c.view().await.unwrap().digest, [0xcd; 32]);
        assert!(c.check_applied(7, &[0xee; 32], &[1, 2], true).await.is_ok());
        assert!(matches!(
            c.check_applied(7, &[0xee; 32], &[1, 2], false).await,
            Err(LogError::Enclave(LogOutcome::Transient(-67)))
        ));
        let b = c.book(2, 5).await.unwrap();
        assert_eq!(
            (b.total, b.last_order_id, b.orders[0].user_id.as_str()),
            (3, 12, "rCarol")
        );
        assert!(matches!(
            c.begin(&[1, 2], 4).await,
            Err(LogError::Enclave(LogOutcome::Off))
        ));
        assert!(matches!(
            c.add(&[1], &[]).await,
            Err(LogError::Http { status: 400, .. })
        ));
        let k = [0x11u8; 32];
        let cm = c.commit("0x11", &k).await.unwrap();
        assert_eq!(
            (cm.batch, cm.sig, cm.cert),
            (vec![0xaa], vec![0xbb], vec![0xcc])
        );
        let input = ApplyInput {
            batch: &[1],
            quote_blob: &[2],
            market_id: 0,
            side_blobs: &[],
            sig_bundle: &[3],
            cert_bundle: &[],
        };
        assert!(matches!(
            c.apply(&input, "0x11", &k).await,
            Err(LogError::Enclave(LogOutcome::Halted(-62)))
        ));
    }

    #[tokio::test]
    async fn a_malformed_success_body_is_not_success() {
        let app = Router::new().route(
            "/v1/perp/log/head",
            get(|| async { Json(json!({"status":"success","seq":1})) }),
        );
        let c = LogClient::new(&serve(app).await).unwrap();
        assert!(matches!(
            c.head().await,
            Err(LogError::Http { status: 200, .. })
        ));
    }

    #[test]
    fn non_loopback_url_is_refused() {
        assert!(LogClient::new("https://10.0.0.5:9088/v1").is_err());
    }
}
