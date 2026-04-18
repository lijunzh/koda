//! In-process fake network services for HTTP-level provider tests.
//!
//! # Why this module exists
//!
//! Provider unit tests in `koda-core::providers::*` test serialization and
//! parsing against static strings. They do **not** exercise the actual
//! `reqwest::Client` round-trip: bearer-auth header injection, base-URL
//! routing, status-code error handling, SSE chunk framing over a real TCP
//! socket, retry/timeout behavior, or proxy interaction.
//!
//! [`FakeLlmServer`] closes that gap. It binds an ephemeral port on
//! `127.0.0.1`, lets you stage canned responses (JSON, SSE, error status,
//! delays), and lets you assert on what the provider actually sent.
//!
//! Built on [`wiremock`] for the matcher DSL — `koda-test-utils` already
//! ships a `tokio` runtime so the cost is just one extra dev-dep.
//!
//! # Quick start (generic API — works for any provider)
//!
//! ```rust,ignore
//! use koda_test_utils::network::FakeLlmServer;
//! use serde_json::json;
//!
//! let server = FakeLlmServer::spawn().await;
//!
//! // OpenAI-compat:
//! server.mount_ok("POST", r".*/chat/completions$", json!({...})).await;
//!
//! // Anthropic:
//! server.mount_ok("POST", r".*/v1/messages$", json!({...})).await;
//!
//! // Gemini:
//! server.mount_ok("POST", r".*:generateContent.*", json!({...})).await;
//! ```
//!
//! Convenience wrappers like [`FakeLlmServer::mount_chat_ok`] exist for the
//! OpenAI-compat path; they are 3-line shims around the generic primitives.
//!
//! # Design notes
//!
//! * **Path matching uses regex.** Matchers are *substring-anchored* via
//!   `path_regex` — i.e. they match anywhere in the path. Anchor with
//!   `$` for "ends with" or `^` for "starts with" as needed.
//!
//! * **One mount per response.** Each `mount_*` call adds a separate Mock
//!   to the server. Multiple mounts on the same path are matched in
//!   insertion order — see wiremock docs for the exact resolution rules.
//!
//! * **Lifetime = test function.** The server shuts down when `Self` is
//!   dropped. Don't share across tests; spawn fresh per test for isolation.

use std::time::Duration;

use serde_json::Value;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// In-process fake LLM server for HTTP-level provider tests.
///
/// See module docs for usage and design notes.
pub struct FakeLlmServer {
    server: MockServer,
}

impl FakeLlmServer {
    /// Bind an ephemeral port on `127.0.0.1` and start serving.
    pub async fn spawn() -> Self {
        Self {
            server: MockServer::start().await,
        }
    }

    /// Base URL with no trailing slash, e.g. `http://127.0.0.1:54321`.
    ///
    /// Pass this directly to provider constructors that take a `base_url`.
    pub fn url(&self) -> String {
        self.server.uri()
    }

    /// All requests received across all mounted endpoints, in order.
    ///
    /// Returns an empty vec if request-recording is disabled (it is enabled
    /// by default in wiremock).
    pub async fn received_requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }

    // ── Generic primitives ────────────────────────────────────────────────
    //
    // These take an HTTP method + path regex so any provider's endpoint
    // shape can be mocked. The provider-specific wrappers below are 3-line
    // shims that call these.

    /// Mount a 200 OK JSON response for `<method> <path matching regex>`.
    pub async fn mount_ok(&self, http_method: &str, path_re: &str, body: Value) {
        Mock::given(method(http_method))
            .and(path_regex(path_re))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Mount a non-2xx response. For driving provider error-path tests.
    pub async fn mount_status(&self, http_method: &str, path_re: &str, status: u16, body: &str) {
        Mock::given(method(http_method))
            .and(path_regex(path_re))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&self.server)
            .await;
    }

    /// Mount a 200 OK SSE-streaming response.
    ///
    /// `chunks` are framed as `data: <chunk>\n\n` per SSE spec. Pass each
    /// JSON delta as a separate string; do **not** pre-concatenate. A
    /// trailing `data: [DONE]\n\n` is **not** added automatically — pass
    /// `"[DONE]"` as the last chunk if your provider expects it.
    pub async fn mount_sse(&self, http_method: &str, path_re: &str, chunks: &[&str]) {
        let body = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>();
        Mock::given(method(http_method))
            .and(path_regex(path_re))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&self.server)
            .await;
    }

    /// Mount a 200 OK JSON response that takes `delay` to start sending.
    ///
    /// Useful for client-timeout regression tests.
    pub async fn mount_delayed(
        &self,
        http_method: &str,
        path_re: &str,
        delay: Duration,
        body: Value,
    ) {
        Mock::given(method(http_method))
            .and(path_regex(path_re))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(&self.server)
            .await;
    }

    // ── OpenAI-compat convenience wrappers ────────────────────────────────
    //
    // These hard-code `POST .*/chat/completions$`. They predate the generic
    // primitives above and are kept for the openai_compat_http_test.rs
    // call sites. New per-provider tests should call the generics directly.

    /// 200 OK JSON response for `POST .*/chat/completions$`.
    pub async fn mount_chat_ok(&self, body: Value) {
        self.mount_ok("POST", r".*/chat/completions$", body).await
    }

    /// Non-2xx response for `POST .*/chat/completions$`.
    pub async fn mount_chat_status(&self, status: u16, body: &str) {
        self.mount_status("POST", r".*/chat/completions$", status, body)
            .await
    }

    /// 200 OK SSE-streaming response for `POST .*/chat/completions$`.
    pub async fn mount_chat_sse(&self, chunks: &[&str]) {
        self.mount_sse("POST", r".*/chat/completions$", chunks)
            .await
    }

    /// 200 OK JSON response with delay for `POST .*/chat/completions$`.
    pub async fn mount_chat_delayed(&self, delay: Duration, body: Value) {
        self.mount_delayed("POST", r".*/chat/completions$", delay, body)
            .await
    }
}
