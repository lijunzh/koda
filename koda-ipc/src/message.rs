//! IPC message types — requests from the worker and responses from the supervisor.
//!
//! All messages are serialized as newline-delimited JSON and are versioned via
//! the `PROTOCOL_VERSION` constant so the supervisor can reject workers built
//! against an incompatible schema.

use serde::{Deserialize, Serialize};

/// Protocol version — bump when the message schema changes in a breaking way.
/// Both the supervisor and worker embed this in their handshake.
pub const PROTOCOL_VERSION: u32 = 1;

// ── Handshake ─────────────────────────────────────────────────────────────────

/// First message the worker sends after connecting.  The supervisor replies
/// with [`HandshakeAck`] (or closes the connection if the version is unsupported).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeHello {
    pub protocol_version: u32,
    /// koda build version string — recorded in supervisor logs.
    pub koda_version: String,
}

/// Supervisor's reply to [`HandshakeHello`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeAck {
    pub protocol_version: u32,
    /// `true` = handshake accepted, worker may proceed.
    /// `false` = version mismatch; worker should exit cleanly.
    pub accepted: bool,
    /// Human-readable reason when `accepted = false`.
    pub message: String,
}

// ── Request ───────────────────────────────────────────────────────────────────

/// A request from the worker to the supervisor.
///
/// The `req_id` is echoed in [`IpcResponse::req_id`] so the worker can match
/// concurrent pipelined requests without a lock-step protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Unique request identifier (UUIDv4).
    pub req_id: String,
    /// The operation the worker wants the supervisor to perform.
    pub body: IpcRequestBody,
}

/// The actual operation requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequestBody {
    /// HTTP GET request — worker asks the supervisor to fetch a URL.
    /// The supervisor applies `is_safe_url()` validation before fetching.
    Fetch(FetchRequest),

    /// LLM chat completion — worker delegates the API call to the supervisor
    /// which holds the API keys and network access.
    LlmChat(Box<crate::llm::IpcLlmRequest>),

    /// Graceful shutdown — worker is done and wants the supervisor to
    /// tear down the socket and exit cleanly.
    Shutdown,
}

/// Parameters for an HTTP fetch routed through the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    /// Target URL (must pass supervisor's `is_safe_url()` check).
    pub url: String,
    /// Optional maximum body size in characters; supervisor enforces a cap.
    pub max_body_chars: Option<usize>,
}

// ── Response ──────────────────────────────────────────────────────────────────

/// A response from the supervisor to the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Echoed from the matching [`IpcRequest::req_id`].
    pub req_id: String,
    /// The result of the requested operation.
    pub body: IpcResponseBody,
}

/// The result body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponseBody {
    /// Fetch succeeded.
    FetchOk(FetchResponse),
    /// LLM chat completion succeeded.
    LlmChatOk(crate::llm::IpcLlmResponse),
    /// Operation failed.  `message` is a human-readable error string safe to
    /// surface to the user (no secrets, no internal paths).
    Error { message: String },
    /// Supervisor acknowledged the shutdown request.
    ShutdownAck,
}

/// A successful HTTP fetch result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    /// Response body (possibly truncated at `max_body_chars`).
    pub body: String,
    /// HTTP status code.
    pub status: u16,
}
