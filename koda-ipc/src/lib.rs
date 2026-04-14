//! Supervisor ↔ worker IPC protocol for Koda.
//!
//! # Overview
//!
//! When koda runs in supervisor mode the privileged supervisor process spawns
//! a sandboxed worker process.  The worker handles the inference loop and all
//! tools but deliberately has **no outbound network access** — it communicates
//! exclusively via this IPC channel.
//!
//! Any network operation (LLM calls, `WebFetch`) that the worker needs is
//! expressed as an [`IpcRequest`] sent over a Unix domain socket.  The
//! supervisor validates, executes, and returns an [`IpcResponse`].
//!
//! ## Wire format
//!
//! Newline-delimited JSON (one JSON object per line) over a Unix stream
//! socket.  Each [`IpcRequest`] carries a `req_id` UUID; the corresponding
//! [`IpcResponse`] echoes the same `req_id` so pipelined requests are matched
//! without a strict lock-step protocol.
//!
//! ## Environment
//!
//! The supervisor passes `KODA_SUPERVISOR_SOCKET=<path>` to the worker
//! process.  Tools that need network access check for this variable at call
//! time; if it is set they use [`client::fetch`] instead of opening their
//! own sockets.
//!
//! ## Example
//!
//! ```rust,ignore
//! // Worker side — send a fetch request and await the response.
//! let body = koda_ipc::client::fetch(
//!     "/tmp/koda-sup-abc123.sock",
//!     "https://docs.rust-lang.org/",
//!     None,
//! ).await?;
//! println!("{body}");
//! ```

pub mod client;
pub mod message;
pub mod transport;

pub use message::{FetchRequest, FetchResponse, IpcRequest, IpcResponse};
