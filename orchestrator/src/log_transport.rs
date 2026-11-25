//! The replicated-log fetch protocol: a request names a seq, the response carries that record file.
//!
//! Every read is bounded BEFORE it allocates: a request is exactly `FETCH_REQ_LEN` bytes; a response
//! header is 5 bytes and its body length is checked against the record cap before the body is read.
//! Only member peers are served and only member peers' answers are used (the caller enforces both).
#![allow(dead_code)]

use crate::log_store::{Durable, MAX_CERT_FILE, MAX_REC_FILE};
use crate::log_wire::{
    decode_fetch_request, encode_fetch_request, encode_fetch_response, fetch_response_body_len,
    FetchKind, FETCH_REQ_LEN,
};
use libp2p::futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::StreamProtocol;
use std::io;
use std::path::Path;
use tokio::sync::oneshot;

/// A fetch answer: the record file, or `None` when the peer does not have it.
pub type FetchResponse = Option<Vec<u8>>;

/// The protocol name, per cluster: a node of another cluster cannot even negotiate it.
pub fn fetch_protocol(cluster_ns: &str) -> anyhow::Result<StreamProtocol> {
    StreamProtocol::try_from_owned(format!("/perp-dex/{cluster_ns}/log-fetch/1"))
        .map_err(|e| anyhow::anyhow!("log fetch protocol name: {e}"))
}

#[derive(Debug, Clone, Default)]
pub struct LogFetchCodec;

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[async_trait::async_trait]
impl libp2p::request_response::Codec for LogFetchCodec {
    type Protocol = StreamProtocol;
    type Request = (FetchKind, u64);
    type Response = FetchResponse;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<(FetchKind, u64)>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut b = [0u8; FETCH_REQ_LEN];
        io.read_exact(&mut b).await?;
        decode_fetch_request(&b).map_err(invalid)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<FetchResponse>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut head = [0u8; 5];
        io.read_exact(&mut head).await?;
        let Some(n) = fetch_response_body_len(&head).map_err(invalid)? else {
            return Ok(None);
        };
        // `fetch_response_body_len` has already bounded n by the record cap.
        debug_assert!(n <= MAX_REC_FILE);
        let mut body = vec![0u8; n];
        io.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        (kind, seq): (FetchKind, u64),
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&encode_fetch_request(kind, seq)).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: FetchResponse,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let bytes = encode_fetch_response(res.as_deref()).map_err(invalid)?;
        io.write_all(&bytes).await?;
        io.close().await
    }
}

/// A log message cleared for publishing. Outside tests it is only made from a [`Durable`] proof, so
/// nothing is announced or signed as APPLIED before its record is on disk.
#[derive(Debug, Clone)]
pub struct Outgoing(crate::log_wire::LogGossip);

impl Outgoing {
    pub fn announce(d: &Durable) -> Self {
        Self(crate::log_wire::announce(d))
    }
    pub fn applied(d: &Durable, view: &[u8; 32], entry: &[u8]) -> anyhow::Result<Self> {
        Ok(Self(crate::log_wire::applied(d, view, entry)?))
    }
    /// A view report (read from the enclave; it announces no record and signs nothing).
    pub fn view(v: &crate::log_wire::ViewReport) -> Self {
        Self(crate::log_wire::view(v))
    }
    pub fn gossip(&self) -> &crate::log_wire::LogGossip {
        &self.0
    }
    #[cfg(test)]
    pub fn unchecked(g: crate::log_wire::LogGossip) -> Self {
        Self(g)
    }
}

/// What the log loop asks the network to do.
#[derive(Debug)]
pub enum LogOutbound {
    /// Publish an announcement or an APPLIED share.
    Publish(Outgoing),
    /// Ask one member peer for one record file or certificate file.
    Fetch {
        kind: FetchKind,
        seq: u64,
        peer: libp2p::PeerId,
        reply: oneshot::Sender<anyhow::Result<FetchResponse>>,
    },
}

/// What the network hands to the log loop: a parsed log message and its signed member author.
#[derive(Debug, Clone)]
pub struct LogInbound {
    pub author: libp2p::PeerId,
    pub msg: crate::log_wire::Parsed,
}

/// The file this node serves for (`kind`, `seq`), read bounded from the store directory (no shared lock
/// with the writer: files are only ever renamed into place whole).
pub fn served(store_dir: &Path, kind: FetchKind, seq: u64) -> FetchResponse {
    let (ext, cap) = match kind {
        FetchKind::Record => ("rec", MAX_REC_FILE),
        FetchKind::Cert => ("cert", MAX_CERT_FILE),
    };
    let p = store_dir.join(format!("{seq:020}.{ext}"));
    match std::fs::metadata(&p) {
        Ok(m) if m.len() > 0 && m.len() <= cap as u64 => std::fs::read(&p).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::futures::io::Cursor;
    use libp2p::request_response::Codec;

    fn proto() -> StreamProtocol {
        fetch_protocol("rTest").unwrap()
    }

    #[tokio::test]
    async fn requests_and_responses_round_trip() {
        let mut c = LogFetchCodec;
        let mut buf = Cursor::new(Vec::new());
        c.write_request(&proto(), &mut buf, (FetchKind::Cert, 77))
            .await
            .unwrap();
        let mut r = Cursor::new(buf.into_inner());
        assert_eq!(
            c.read_request(&proto(), &mut r).await.unwrap(),
            (FetchKind::Cert, 77)
        );

        for res in [None, Some(vec![1u8, 2, 3])] {
            let mut buf = Cursor::new(Vec::new());
            c.write_response(&proto(), &mut buf, res.clone())
                .await
                .unwrap();
            let mut r = Cursor::new(buf.into_inner());
            assert_eq!(c.read_response(&proto(), &mut r).await.unwrap(), res);
        }
    }

    #[tokio::test]
    async fn an_oversized_response_is_refused_from_its_header_before_the_body_is_read() {
        let mut c = LogFetchCodec;
        let n = u32::try_from(MAX_REC_FILE + 1).unwrap().to_be_bytes();
        // Only the 5-byte header exists: if the reader trusted the length it would wait for the body
        // (or allocate it) and fail differently.
        let mut r = Cursor::new(vec![0, n[0], n[1], n[2], n[3]]);
        let e = c.read_response(&proto(), &mut r).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        // A body shorter than its header says is an error, not a short record.
        let mut short = Cursor::new(vec![0, 0, 0, 0, 9, 1, 2]);
        assert!(c.read_response(&proto(), &mut short).await.is_err());
        // A request of the wrong magic.
        let mut bad = Cursor::new(b"XXXX\0\0\0\0\0\0\0\0\x01".to_vec());
        assert_eq!(
            c.read_request(&proto(), &mut bad).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn the_protocol_is_per_cluster() {
        assert_ne!(fetch_protocol("rA").unwrap(), fetch_protocol("rB").unwrap());
        assert_eq!(
            fetch_protocol("rA").unwrap().as_ref(),
            "/perp-dex/rA/log-fetch/1"
        );
    }

    #[test]
    fn served_records_are_bounded_reads_of_whole_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(served(d.path(), FetchKind::Record, 1), None);
        let p = d.path().join(format!("{:020}.rec", 1));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            served(d.path(), FetchKind::Record, 1),
            Some(b"abc".to_vec())
        );
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            served(d.path(), FetchKind::Record, 1),
            None,
            "an empty file is not served"
        );
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(MAX_REC_FILE as u64 + 1).unwrap();
        assert_eq!(
            served(d.path(), FetchKind::Record, 1),
            None,
            "an oversized file is not served"
        );
    }
}
