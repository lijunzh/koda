//! Provider-neutral HTTP helpers (#1265 item 7).
//!
//! Pre-#1265 the three native providers (`anthropic`, `gemini`,
//! `openai_compat`) each open-coded the same three-line dance for
//! every request:
//!
//! ```ignore
//! let resp = req.send().await.context("Failed to call X API")?;
//! let status = resp.status();
//! if !status.is_success() {
//!     let body = resp.text().await.unwrap_or_default();
//!     anyhow::bail!("X API returned {status}: {body}");
//! }
//! let parsed: SomeType = resp.json().await.context("Failed to parse X response")?;
//! ```
//!
//! Eight sites copy that block with only the provider name varying.
//! That's a textbook DRY violation, *and* a change-amplifier — adding
//! redacted logging, a retry policy, or a richer error type would
//! require touching every site.
//!
//! This module collects the dominant pattern into two helpers:
//!
//! - [`send_or_bail`] — send + status check, return the [`Response`]
//!   for further processing (streaming, custom parsing, etc.).
//! - [`send_and_parse_json`] — full send + status + JSON parse, the
//!   common case for non-streaming endpoints.
//!
//! ## What is *not* in scope
//!
//! Three sites use bespoke status handling and stay inline:
//!
//! 1. **Auth-aware error translation** (`anthropic.rs`, `gemini.rs`):
//!    they translate 401/403 into a user-friendly "Invalid API key"
//!    message before the generic bail. Pulling that into a helper
//!    would just multiply parameters and re-encode the same string
//!    template; YAGNI until a third provider needs it.
//! 2. **Soft-fail to default** (`gemini.rs` cache creation):
//!    intentionally swallows errors and returns `None` because the
//!    cache is best-effort. The pattern is one-off and isn't a
//!    candidate for sharing.
//!
//! ## Provider name parameter
//!
//! Every helper takes `provider_name: &str` purely for error message
//! quality. It's threaded through `with_context` and `bail!` so the
//! resulting error chain reads e.g. `"Failed to call Anthropic API:
//! connection refused"`. Pre-#1265 each provider hard-coded its own
//! name in the `.context(...)` string; this just lifts the literal
//! up by one stack frame.

use anyhow::{Context, Result, anyhow};
use reqwest::{RequestBuilder, Response};
use serde::de::DeserializeOwned;

/// Send a request; if the response status is not 2xx, read the body
/// and bail with `"<provider> API returned <status>: <body>"`.
/// Otherwise return the [`Response`] for further processing.
///
/// Use this when you want to inspect headers, stream the body, or
/// otherwise do non-JSON things with the response. For the common
/// JSON-response case prefer [`send_and_parse_json`].
///
/// ## Errors
///
/// - The underlying transport (`reqwest::Error`) wrapped in a
///   `"Failed to call <provider> API"` context.
/// - HTTP non-2xx status, with the response body included verbatim
///   so the model has something to act on.
pub(crate) async fn send_or_bail(request: RequestBuilder, provider_name: &str) -> Result<Response> {
    let resp = request
        .send()
        .await
        .with_context(|| format!("Failed to call {provider_name} API"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        // `text()` consumes the response, so we read it eagerly here
        // and surface it inside the bail message. `unwrap_or_default`
        // mirrors pre-#1265 behavior — a body-read failure shouldn't
        // mask the underlying status code.
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("{provider_name} API returned {status}: {body}"));
    }
    Ok(resp)
}

/// Send a request, check status, and parse the JSON response into
/// `T`. The non-streaming common case.
///
/// ## Errors
///
/// - Anything [`send_or_bail`] can return.
/// - JSON deserialization failure wrapped in
///   `"Failed to parse <provider> response"` context.
pub(crate) async fn send_and_parse_json<T: DeserializeOwned>(
    request: RequestBuilder,
    provider_name: &str,
) -> Result<T> {
    let resp = send_or_bail(request, provider_name).await?;
    resp.json()
        .await
        .with_context(|| format!("Failed to parse {provider_name} response"))
}

#[cfg(test)]
mod tests {
    //! Behavioral parity with pre-#1265 inline sites. Tests use a
    //! locally-bound `axum` server on `127.0.0.1:0` to keep the
    //! assertions transport-real without depending on a live
    //! provider. This matches the `web_fetch.rs` test pattern
    //! already used elsewhere in this crate — no new dev-deps.
    //!
    //! Threading model: the bare current_thread tokio test runtime is
    //! **not** safe here because the axum server runs on a
    //! `tokio::spawn`'d task; current_thread silently hides
    //! spawn-related bugs. See #1109 F2 /
    //! `scripts/check_tokio_test_flavor.py`.

    use super::*;
    use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
    use serde::Deserialize;
    use tokio_util::sync::CancellationToken;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Echo {
        ok: bool,
        msg: String,
    }

    /// Spin up an axum server on `127.0.0.1:0` serving `/ok`,
    /// `/boom`, `/echo`, and `/bad`. Returns the base URL and a
    /// cancel token the test must trigger before dropping. Same
    /// shape as `web_fetch.rs::spawn_test_server` so this stays
    /// idiomatic with the existing crate-local convention.
    async fn spawn_server() -> (String, CancellationToken) {
        async fn ok() -> impl IntoResponse {
            (StatusCode::OK, "hello")
        }
        async fn boom() -> impl IntoResponse {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal kaboom")
        }
        async fn echo() -> impl IntoResponse {
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"ok":true,"msg":"hi"}"#,
            )
        }
        async fn bad() -> impl IntoResponse {
            // 200 OK but non-JSON body — forces parse failure path.
            (StatusCode::OK, "not json")
        }

        let app = Router::new()
            .route("/ok", get(ok))
            .route("/boom", get(boom))
            .route("/echo", get(echo))
            .route("/bad", get(bad));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let ct = CancellationToken::new();
        let ct_server = ct.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct_server.cancelled_owned().await })
                .await
                .ok();
        });
        (url, ct)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_or_bail_returns_response_on_2xx() {
        let (url, ct) = spawn_server().await;
        let client = reqwest::Client::new();
        let req = client.get(format!("{url}/ok"));
        let resp = send_or_bail(req, "TestProvider").await.expect("ok");
        let body = resp.text().await.expect("body");
        assert_eq!(body, "hello");
        ct.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_or_bail_includes_status_and_body_on_error() {
        let (url, ct) = spawn_server().await;
        let client = reqwest::Client::new();
        let req = client.get(format!("{url}/boom"));
        let err = send_or_bail(req, "TestProvider").await.unwrap_err();
        let msg = format!("{err}");
        // Verbatim format: pre-#1265 inline sites used the same
        // shape, and the model's error-recovery path matches
        // against it. Don't change without bumping consumers.
        assert!(msg.contains("TestProvider API returned"), "got: {msg}");
        assert!(msg.contains("500"), "got: {msg}");
        assert!(msg.contains("internal kaboom"), "got: {msg}");
        ct.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_and_parse_json_round_trips_typed_body() {
        let (url, ct) = spawn_server().await;
        let client = reqwest::Client::new();
        let req = client.get(format!("{url}/echo"));
        let parsed: Echo = send_and_parse_json(req, "TestProvider").await.expect("ok");
        assert_eq!(
            parsed,
            Echo {
                ok: true,
                msg: "hi".into()
            }
        );
        ct.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_and_parse_json_errors_on_malformed_body() {
        let (url, ct) = spawn_server().await;
        let client = reqwest::Client::new();
        let req = client.get(format!("{url}/bad"));
        let err = send_and_parse_json::<Echo>(req, "TestProvider")
            .await
            .unwrap_err();
        // The parse-failure context is what pre-#1265 inline sites
        // surfaced verbatim — keep the model's recovery path stable.
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Failed to parse TestProvider response"),
            "got: {chain}"
        );
        ct.cancel();
    }
}
