//! Length-prefixed JSON IPC for the FS worker (Phase 2a of #934).
//!
//! The host process and `koda-fs-worker` exchange messages over a duplex
//! transport (stdin/stdout in unit tests, Unix domain socket in
//! production — see Phase 2c). The wire format is intentionally boring:
//!
//! ```text
//! ┌──────────────────┬──────────────────────────────────┐
//! │  u32 BE length   │       JSON-serialized payload    │
//! └──────────────────┴──────────────────────────────────┘
//! ```
//!
//! The 4-byte length prefix tells the reader exactly how many JSON bytes
//! to consume — no streaming-parser ambiguity, no newline-delimited
//! foot-guns when payloads embed `\n`. The format was chosen per #934 §8
//! decision 1 ("JSON, length-prefixed. Debuggable, easy, zero new deps.")
//! over msgpack; revisit only if benchmarks show >5% overhead.
//!
//! ## Why not just `serde_json::to_writer` over a pipe?
//!
//! `serde_json::to_writer` doesn't write a separator, so `from_reader`
//! on the receiving end has no idea where one message ends and the next
//! begins. Length prefix is the simplest fix that doesn't constrain
//! payload contents.
//!
//! ## Message envelope
//!
//! - [`Request`] — host → worker. New variants get added in Phase 2c
//!   (Read/Write/Edit/Glob/Grep). Today only `Ping` and `Shutdown` are
//!   implemented; the rest are reserved variants that return
//!   `Response::Error { code: Unimplemented }`.
//! - [`Response`] — worker → host. Always either the variant matching
//!   the request kind or `Response::Error`.
//!
//! ## Versioning
//!
//! Wire compatibility is **not** a goal — host + worker ship in the
//! same binary release and re-spawn together. We don't carry a protocol
//! version field; if the request shape changes between releases, the
//! worker that gets spawned matches the host. Cross-version IPC would
//! be a concrete bug, not a feature.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum size of a single IPC payload (1 MiB).
///
/// Above this we fail the request rather than allocate unbounded —
/// a hostile or buggy peer must not be able to OOM us by sending a
/// length prefix of `u32::MAX`. File reads larger than this should be
/// chunked at a higher layer (the file tools already cap output).
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// Host → worker message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Liveness probe. Worker replies [`Response::Pong`] immediately.
    Ping,

    /// Graceful shutdown. Worker drains its current handler then exits 0.
    /// Used by `SandboxSlot::Drop` to retire workers cleanly without
    /// leaving zombie processes. Phase 2c wires this up.
    Shutdown,

    // ── Phase 2c reserved variants (handlers stubbed today) ──────────
    /// Read file contents. Phase 2c implements; today returns
    /// `Response::Error { Unimplemented }`.
    Read {
        /// Absolute path to read.
        path: PathBuf,
        /// Cap on bytes returned (`None` = read whole file up to
        /// [`MAX_PAYLOAD_BYTES`]). Tools pre-cap at a smaller value.
        max_bytes: Option<usize>,
    },

    /// Write file contents. Phase 2c implements.
    Write {
        /// Absolute path to write.
        path: PathBuf,
        /// Bytes to write (overwrites existing file).
        content: Vec<u8>,
    },

    /// In-place string replacement. Phase 2c implements.
    Edit {
        /// Absolute path of the file to edit.
        path: PathBuf,
        /// Substring to find.
        old_string: String,
        /// Replacement substring.
        new_string: String,
    },

    /// Glob expansion. Phase 2c implements.
    Glob {
        /// Glob pattern, e.g. `**/*.rs`.
        pattern: String,
        /// Directory to anchor the glob expansion.
        root: PathBuf,
    },

    /// Recursive grep. Phase 2c implements.
    Grep {
        /// Regex pattern to search for.
        pattern: String,
        /// Directory to recurse into.
        root: PathBuf,
        /// Optional file glob to filter what gets grepped (e.g. `*.rs`).
        include: Option<String>,
    },

    /// `stat`-like metadata fetch. Phase 2c implements.
    Stat {
        /// Absolute path to stat.
        path: PathBuf,
    },
}

/// Worker → host message. Always matches the kind of the originating
/// request, or `Error` on any failure (policy denial, IO error,
/// unimplemented variant, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Ping`] (and the ack for [`Request::Shutdown`]).
    Pong,

    /// Reply to [`Request::Read`].
    Read {
        /// File contents (possibly truncated to `max_bytes`).
        content: Vec<u8>,
    },

    /// Reply to [`Request::Write`].
    Write {
        /// Bytes successfully written to disk.
        bytes_written: usize,
    },

    /// Reply to [`Request::Edit`].
    Edit {
        /// How many occurrences of `old_string` were replaced.
        replacements: usize,
    },

    /// Reply to [`Request::Glob`].
    Glob {
        /// Matching paths in deterministic order (sorted).
        paths: Vec<PathBuf>,
    },

    /// Reply to [`Request::Grep`].
    Grep {
        /// Hits in document order.
        matches: Vec<GrepMatch>,
    },

    /// Reply to [`Request::Stat`].
    Stat {
        /// File size in bytes (0 for directories).
        size: u64,
        /// True if the path is a directory.
        is_dir: bool,
        /// True if the path itself is a symlink (not its target).
        is_symlink: bool,
    },

    /// Anything that didn't go right — policy denial, IO error,
    /// unimplemented request kind, malformed input, etc. The host turns
    /// this into a Rust `Err` and the calling tool decides what to do.
    Error {
        /// Coarse classification (see [`ErrorCode`]).
        code: ErrorCode,
        /// Human-readable detail (logged + surfaced to the LLM).
        message: String,
    },
}

/// Error taxonomy for `Response::Error`. Coarse on purpose — fine-grained
/// classification belongs at the calling tool's level, not the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Worker doesn't implement this variant yet (Phase 2c will).
    Unimplemented,
    /// Sandbox policy refused the operation.
    PolicyDenied,
    /// `std::io::Error` flavor (file not found, permission denied, …).
    Io,
    /// Payload exceeded [`MAX_PAYLOAD_BYTES`] or otherwise malformed.
    Protocol,
    /// Internal worker bug / panic guard. Should never appear in prod.
    Internal,
}

/// One ripgrep-style hit in a `Grep` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    /// File the hit lives in.
    pub path: PathBuf,
    /// 1-based line number (matches `rg`/`grep` convention).
    pub line: usize,
    /// The full matching line, with no trailing newline.
    pub text: String,
}

// ── Framing ──────────────────────────────────────────────────────────────

/// Read a single length-prefixed JSON message from `reader`.
///
/// Returns `Ok(None)` on clean EOF before any bytes are read — the
/// peer closed the transport without sending anything, which the host
/// uses to detect worker death and the worker uses to detect parent
/// hangup.
///
/// # Errors
///
/// - `io::ErrorKind::UnexpectedEof` if EOF arrives mid-frame
///   (length-prefix partial, or payload shorter than declared).
/// - `io::ErrorKind::InvalidData` if the declared length exceeds
///   [`MAX_PAYLOAD_BYTES`] or the payload is not valid JSON-of-`T`.
pub async fn read_message<R, T>(reader: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    // Read the 4-byte length prefix manually so we can distinguish the
    // two flavors of EOF: clean (0 bytes read — peer hung up between
    // messages) versus mid-prefix truncation (1–3 bytes — protocol error).
    let mut len_buf = [0u8; 4];
    let mut read_so_far = 0usize;
    while read_so_far < 4 {
        let n = reader.read(&mut len_buf[read_so_far..]).await?;
        if n == 0 {
            return if read_so_far == 0 {
                Ok(None) // clean peer close — not an error
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("EOF after {read_so_far} of 4 length-prefix bytes"),
                ))
            };
        }
        read_so_far += n;
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload size {len} exceeds max {MAX_PAYLOAD_BYTES}"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let parsed = serde_json::from_slice::<T>(&payload).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed JSON payload: {e}"),
        )
    })?;
    Ok(Some(parsed))
}

/// Write a length-prefixed JSON message to `writer`.
///
/// # Errors
///
/// - `io::ErrorKind::InvalidData` if the serialized payload exceeds
///   [`MAX_PAYLOAD_BYTES`] (host-side bug — refuse to send so the peer
///   doesn't have to defend against it).
/// - Any IO error from the underlying transport.
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let payload = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {e}")))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "outgoing payload size {} exceeds max {}",
                payload.len(),
                MAX_PAYLOAD_BYTES
            ),
        ));
    }
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::duplex;

    // ── Roundtrip: every message kind must serialize and parse back ──

    #[tokio::test]
    async fn ping_roundtrips_through_framing() {
        let (mut a, mut b) = duplex(64);
        write_message(&mut a, &Request::Ping).await.unwrap();
        let got: Request = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(got, Request::Ping);
    }

    #[tokio::test]
    async fn pong_roundtrips_through_framing() {
        let (mut a, mut b) = duplex(64);
        write_message(&mut a, &Response::Pong).await.unwrap();
        let got: Response = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(got, Response::Pong);
    }

    #[tokio::test]
    async fn error_response_roundtrips() {
        let (mut a, mut b) = duplex(256);
        let msg = Response::Error {
            code: ErrorCode::PolicyDenied,
            message: "deny file-write* /etc/passwd".into(),
        };
        write_message(&mut a, &msg).await.unwrap();
        let got: Response = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn complex_request_roundtrips() {
        // Edit carries a path + two strings — common worst case for
        // serde-tagging bugs. If this works, simpler variants will too.
        let (mut a, mut b) = duplex(512);
        let req = Request::Edit {
            path: PathBuf::from("/work/src/main.rs"),
            old_string: "let x = 1;\nlet y = 2;".into(),
            new_string: "let x = 42;".into(),
        };
        write_message(&mut a, &req).await.unwrap();
        let got: Request = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(got, req);
    }

    #[tokio::test]
    async fn multiple_messages_back_to_back_parse_correctly() {
        // Length-prefix correctness: two messages on the wire must not
        // bleed into each other. Streaming JSON parsers fail this test.
        let (mut a, mut b) = duplex(1024);
        write_message(&mut a, &Request::Ping).await.unwrap();
        write_message(
            &mut a,
            &Request::Read {
                path: "/a".into(),
                max_bytes: Some(1024),
            },
        )
        .await
        .unwrap();
        let m1: Request = read_message(&mut b).await.unwrap().unwrap();
        let m2: Request = read_message(&mut b).await.unwrap().unwrap();
        assert_eq!(m1, Request::Ping);
        assert_eq!(
            m2,
            Request::Read {
                path: "/a".into(),
                max_bytes: Some(1024)
            }
        );
    }

    // ── EOF / error paths ────────────────────────────────────────────

    #[tokio::test]
    async fn clean_eof_before_any_bytes_returns_none() {
        // The peer dropped the transport without sending. Host uses
        // this to detect worker exit; worker uses it to detect parent
        // hangup. Must NOT be an error.
        let mut empty = Cursor::new(Vec::<u8>::new());
        let got: Option<Request> = read_message(&mut empty).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn eof_mid_length_prefix_is_unexpected_eof() {
        // 2 bytes of length, then EOF. Must error, not silently truncate.
        let mut partial = Cursor::new(vec![0u8, 0u8]);
        let err = read_message::<_, Request>(&mut partial).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn eof_mid_payload_is_unexpected_eof() {
        // Length says 100 bytes, transport has 4. Must error.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(b"abcd");
        let mut cur = Cursor::new(buf);
        let err = read_message::<_, Request>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn payload_size_above_cap_is_rejected() {
        // Hostile peer claiming a huge length must not let us allocate
        // unbounded. We reject *before* the malloc.
        let oversize = (MAX_PAYLOAD_BYTES as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(oversize.to_vec());
        let err = read_message::<_, Request>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds max"));
    }

    #[tokio::test]
    async fn malformed_json_payload_is_invalid_data() {
        let mut buf = Vec::new();
        let body = b"this is not json";
        buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        buf.extend_from_slice(body);
        let mut cur = Cursor::new(buf);
        let err = read_message::<_, Request>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("malformed JSON"));
    }

    #[tokio::test]
    async fn write_rejects_oversize_payload_locally() {
        // We refuse to *send* something the peer would refuse to *read*
        // — symmetric defense, fail fast in this process so the bug
        // doesn't show up as a confusing remote error.
        let huge = "x".repeat(MAX_PAYLOAD_BYTES + 100);
        let req = Request::Write {
            path: "/a".into(),
            content: huge.into_bytes(),
        };
        let (mut a, _b) = duplex(MAX_PAYLOAD_BYTES * 2);
        let err = write_message(&mut a, &req).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
