//! Worker-side IPC client.
//!
//! Provides a simple `fetch` function that sends a [`FetchRequest`] to the
//! supervisor over the Unix socket path stored in `KODA_SUPERVISOR_SOCKET`
//! and returns the response body on success.
//!
//! If `KODA_SUPERVISOR_SOCKET` is not set the caller falls back to making its
//! own HTTP request (the non-sandboxed code path).

use anyhow::{Context, Result, bail};
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::llm::{IpcLlmRequest, IpcLlmResponse};
use crate::message::{
    FetchRequest, HandshakeAck, HandshakeHello, IpcRequest, IpcRequestBody, IpcResponse,
    IpcResponseBody, PROTOCOL_VERSION,
};
use crate::transport::{recv, send};

/// Environment variable the supervisor sets when it spawns a worker.
/// Value is the path to the supervisor's Unix domain socket.
pub const SUPERVISOR_SOCKET_ENV: &str = "KODA_SUPERVISOR_SOCKET";

/// Return the supervisor socket path from the environment, or `None` if
/// this process is not running as a supervised worker.
pub fn supervisor_socket_path() -> Option<String> {
    normalize_socket_path(std::env::var(SUPERVISOR_SOCKET_ENV).ok())
}

/// Pure helper — filters out missing/empty values.
/// Tested separately so the env-var tests don't need global state.
fn normalize_socket_path(raw: Option<String>) -> Option<String> {
    raw.filter(|s| !s.is_empty())
}

/// Ask the supervisor to fetch `url` and return the response body.
///
/// Performs the full connect → handshake → request → response → disconnect
/// cycle in one call.  The supervisor applies its own `is_safe_url()` check;
/// this function does not duplicate that validation.
///
/// # Errors
///
/// Returns an error if:
/// - The socket is unreachable (supervisor crashed or path is wrong).
/// - The handshake is rejected (version mismatch).
/// - The supervisor returns an `Error` response.
/// - The network or JSON framing fails.
pub async fn fetch(socket_path: &str, url: &str, max_body_chars: Option<usize>) -> Result<String> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to supervisor socket {socket_path}"))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // ── Handshake ──────────────────────────────────────────────────────────
    let hello = HandshakeHello {
        protocol_version: PROTOCOL_VERSION,
        koda_version: env!("CARGO_PKG_VERSION").into(),
    };
    send(&mut write_half, &hello)
        .await
        .context("send handshake hello")?;

    let ack: HandshakeAck = recv(&mut reader).await.context("recv handshake ack")?;
    if !ack.accepted {
        bail!(
            "supervisor rejected handshake (protocol version {}): {}",
            PROTOCOL_VERSION,
            ack.message
        );
    }

    // ── Fetch request ──────────────────────────────────────────────────────
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = IpcRequest {
        req_id: req_id.clone(),
        body: IpcRequestBody::Fetch(FetchRequest {
            url: url.to_string(),
            max_body_chars,
        }),
    };
    send(&mut write_half, &req)
        .await
        .context("send fetch request")?;

    let resp: IpcResponse = recv(&mut reader).await.context("recv fetch response")?;
    if resp.req_id != req_id {
        bail!("req_id mismatch: sent {req_id}, got {}", resp.req_id);
    }

    match resp.body {
        IpcResponseBody::FetchOk(f) => Ok(f.body),
        IpcResponseBody::Error { message } => bail!("supervisor fetch error: {message}"),
        IpcResponseBody::ShutdownAck => bail!("unexpected ShutdownAck for fetch request"),
        IpcResponseBody::LlmChatOk(_) => bail!("unexpected LlmChatOk for fetch request"),
    }
}

/// Ask the supervisor to execute an LLM chat completion and return the result.
///
/// Like [`fetch`], this handles the full connect → handshake → request → response
/// cycle. The supervisor uses its own API keys and network access; the worker
/// never needs to hold credentials or open outbound TCP connections.
///
/// # Errors
///
/// Returns an error if the socket is unreachable, the handshake is rejected,
/// or the supervisor returns an `Error` response.
pub async fn llm_chat(socket_path: &str, req: IpcLlmRequest) -> Result<IpcLlmResponse> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to supervisor socket {socket_path}"))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // ── Handshake ──────────────────────────────────────────────────────────
    let hello = HandshakeHello {
        protocol_version: PROTOCOL_VERSION,
        koda_version: env!("CARGO_PKG_VERSION").into(),
    };
    send(&mut write_half, &hello)
        .await
        .context("send handshake hello")?;

    let ack: HandshakeAck = recv(&mut reader).await.context("recv handshake ack")?;
    if !ack.accepted {
        anyhow::bail!(
            "supervisor rejected handshake (protocol version {}): {}",
            PROTOCOL_VERSION,
            ack.message
        );
    }

    // ── LLM chat request ───────────────────────────────────────────────────
    let req_id = uuid::Uuid::new_v4().to_string();
    let ipc_req = IpcRequest {
        req_id: req_id.clone(),
        body: IpcRequestBody::LlmChat(Box::new(req)),
    };
    send(&mut write_half, &ipc_req)
        .await
        .context("send llm_chat request")?;

    let resp: IpcResponse = recv(&mut reader).await.context("recv llm_chat response")?;
    if resp.req_id != req_id {
        anyhow::bail!("req_id mismatch: sent {req_id}, got {}", resp.req_id);
    }

    match resp.body {
        IpcResponseBody::LlmChatOk(r) => Ok(r),
        IpcResponseBody::Error { message } => anyhow::bail!("supervisor llm_chat error: {message}"),
        _ => anyhow::bail!("unexpected response body for llm_chat request"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test the pure `normalize_socket_path` helper — no global env state
    // so these are safe to run in parallel.

    #[test]
    fn normalize_absent() {
        assert!(normalize_socket_path(None).is_none());
    }

    #[test]
    fn normalize_present() {
        assert_eq!(
            normalize_socket_path(Some("/tmp/test.sock".into())).as_deref(),
            Some("/tmp/test.sock")
        );
    }

    #[test]
    fn normalize_empty_string_treated_as_absent() {
        assert!(normalize_socket_path(Some(String::new())).is_none());
    }
}
