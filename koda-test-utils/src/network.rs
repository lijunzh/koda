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
//! # Quick start
//!
//! ```rust,ignore
//! use koda_test_utils::network::FakeLlmServer;
//! use serde_json::json;
//!
//! #[tokio::test]
//! async fn provider_sends_bearer_token() {
//!     let server = FakeLlmServer::spawn().await;
//!     server.mount_chat_ok(json!({
//!         "choices": [{ "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
//!         "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
//!     })).await;
//!
//!     let provider = OpenAIProvider::new(&server.url(), Some("sk-test".into()));
//!     let _ = provider.chat(&[], &[], &settings).await.unwrap();
//!
//!     let reqs = server.received_requests().await;
//!     assert_eq!(reqs.len(), 1);
//!     assert_eq!(reqs[0].headers.get("authorization").unwrap(), "Bearer sk-test");
//! }
//! ```
//!
//! # Design notes
//!
//! * **Path matching is permissive by default.** `mount_chat_ok` matches
//!   any path ending in `/chat/completions` so the same fixture works for
//!   both OpenAI (`/v1/chat/completions`) and a base URL that already
//!   includes `/v1`. Tighten with `mount_with_matchers` when needed.
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

    /// Mount a 200 OK JSON response for `POST */chat/completions`.
    pub async fn mount_chat_ok(&self, body: Value) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Mount a non-2xx response for `POST */chat/completions`.
    ///
    /// Use this to drive provider error-handling tests (4xx auth/rate-limit
    /// errors, 5xx upstream failures).
    pub async fn mount_chat_status(&self, status: u16, body: &str) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&self.server)
            .await;
    }

    /// Mount a 200 OK SSE-streaming response for `POST */chat/completions`.
    ///
    /// `chunks` are framed as `data: <chunk>\n\n` per SSE spec. Pass each
    /// JSON delta as a separate string; do **not** pre-concatenate.
    /// A trailing `data: [DONE]\n\n` is **not** added automatically — pass
    /// `"[DONE]"` as the last chunk if your provider expects it.
    pub async fn mount_chat_sse(&self, chunks: &[&str]) {
        let body = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>();
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&self.server)
            .await;
    }

    /// Mount a 200 OK chat response that takes `delay` to start sending.
    ///
    /// Useful for client-timeout regression tests.
    pub async fn mount_chat_delayed(&self, delay: Duration, body: Value) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .set_delay(delay),
            )
            .mount(&self.server)
            .await;
    }

    /// All requests received across all mounted endpoints, in order.
    ///
    /// Returns an empty vec if request-recording is disabled (it is enabled
    /// by default in wiremock).
    pub async fn received_requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }
}
