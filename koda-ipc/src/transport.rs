//! JSON-lines framing over an async byte stream.
//!
//! Each message is a single JSON object followed by a newline (`\n`).
//! The framing is intentionally simple — no length prefix, no binary
//! encoding — so it can be inspected with `cat`, `nc`, or logged verbatim.
//!
//! # Usage
//!
//! ```rust,ignore
//! use tokio::net::UnixStream;
//! use koda_ipc::transport::{recv, send};
//! use koda_ipc::message::{IpcRequest, IpcRequestBody, FetchRequest};
//!
//! let stream = UnixStream::connect("/tmp/koda-sup.sock").await?;
//! let (reader, writer) = stream.into_split();
//!
//! send(&mut writer, &req).await?;
//! let resp: IpcResponse = recv(&mut reader).await?;
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Send one JSON-encoded message followed by `\n`.
///
/// `W` can be any [`tokio::io::AsyncWrite`] — a `UnixStream`, an
/// `OwnedWriteHalf`, or a test `Vec<u8>` via a cursor.
pub async fn send<W, M>(writer: &mut W, msg: &M) -> Result<()>
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    let mut line = serde_json::to_string(msg).context("IPC serialize")?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .context("IPC write")?;
    Ok(())
}

/// Read one newline-terminated JSON message from `reader`.
///
/// Returns an error if the stream closes before a complete line is read or
/// if the JSON cannot be deserialized as `M`.
pub async fn recv<R, M>(reader: &mut BufReader<R>) -> Result<M>
where
    R: tokio::io::AsyncRead + Unpin,
    M: for<'de> Deserialize<'de>,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.context("IPC read")?;
    if n == 0 {
        anyhow::bail!("IPC connection closed unexpectedly");
    }
    serde_json::from_str(line.trim_end()).context("IPC deserialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{FetchRequest, IpcRequest, IpcRequestBody};
    use tokio::io::BufReader;

    /// Round-trip: serialize then deserialize recovers the original message.
    #[tokio::test]
    async fn roundtrip_fetch_request() {
        let req = IpcRequest {
            req_id: "test-id-1".into(),
            body: IpcRequestBody::Fetch(FetchRequest {
                url: "https://example.com/".into(),
                max_body_chars: Some(4096),
            }),
        };

        let mut buf: Vec<u8> = Vec::new();
        send(&mut buf, &req).await.unwrap();

        // The serialized form must end with exactly one newline.
        assert_eq!(buf.last(), Some(&b'\n'));
        // No embedded newlines (valid JSON-lines invariant).
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);

        let mut reader = BufReader::new(buf.as_slice());
        let decoded: IpcRequest = recv(&mut reader).await.unwrap();
        assert_eq!(decoded.req_id, req.req_id);
        match decoded.body {
            IpcRequestBody::Fetch(f) => {
                assert_eq!(f.url, "https://example.com/");
                assert_eq!(f.max_body_chars, Some(4096));
            }
            _ => panic!("wrong body variant"),
        }
    }

    /// `recv` returns an error when the stream is empty.
    #[tokio::test]
    async fn recv_on_empty_stream_errors() {
        let empty: &[u8] = &[];
        let mut reader = BufReader::new(empty);
        let result: anyhow::Result<IpcRequest> = recv(&mut reader).await;
        assert!(result.is_err(), "expected error on empty stream");
    }
}
