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

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::mock::{MockResponse, parse_script};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer};

pub use wiremock::{Request, ResponseTemplate};

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

    /// Mount a response computed from each request.
    ///
    /// For a conversation the fixed `mount_*` responses can't script, where
    /// the reply depends on what the client just sent.
    pub async fn mount_fn(
        &self,
        http_method: &str,
        path_re: &str,
        respond: impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
    ) {
        Mock::given(method(http_method))
            .and(path_regex(path_re))
            .respond_with(respond)
            .mount(&self.server)
            .await;
    }

    /// Play `script` to an OpenAI-compatible client (`--provider lmstudio`
    /// with this server's [`Self::url`]): each `POST …/chat/completions`
    /// gets the next response, as [`crate::MockProvider`] would give it,
    /// and an empty reply once the script is spent. For a test that runs
    /// the `koda` binary, or a sub-agent the engine builds from its
    /// config, where no provider can be handed in.
    ///
    /// An `Error` is a 400 with its message, so Koda doesn't retry it;
    /// a `NetworkError` sends its text and ends the stream without
    /// finishing it.
    ///
    /// **One script, one caller at a time.** Requests take responses in
    /// the order they arrive. Agents that call the model at once, such as
    /// two sub-agents from one batch of tool calls or a background one,
    /// would take each other's: give each its own with
    /// [`Self::mount_script_for`].
    pub async fn mount_script(&self, script: Vec<MockResponse>) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .respond_with(script_responder(script))
            .mount(&self.server)
            .await;
    }

    /// [`Self::mount_script`] with the script in [`parse_script`]'s JSON
    /// form, `[{"text":"hi"}, {"tool":"Read","args":{…}}]`.
    pub async fn mount_script_json(&self, json: &str) {
        self.mount_script(parse_script(json)).await;
    }

    /// A script of its own for the agent whose system prompt begins with
    /// `marker`: its requests take only these responses, in order,
    /// whatever other agents ask at the same time. It wins over
    /// [`Self::mount_script`], which answers everyone else.
    ///
    /// An agent's system prompt starts with its own `system_prompt` text,
    /// so use the start of that. Matching anywhere in the prompt wouldn't
    /// do: every agent's prompt lists the other agents with their
    /// descriptions, which default to their `system_prompt`.
    pub async fn mount_script_for(&self, marker: &str, script: Vec<MockResponse>) {
        Mock::given(method("POST"))
            .and(path_regex(r".*/chat/completions$"))
            .and(SystemPromptStartsWith(marker.to_string()))
            .respond_with(script_responder(script))
            // Before the shared script's mock, which is at the default 5.
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    /// [`Self::mount_script_for`] with the script in [`parse_script`]'s
    /// JSON form.
    pub async fn mount_script_json_for(&self, marker: &str, json: &str) {
        self.mount_script_for(marker, parse_script(json)).await;
    }

    /// The `messages` of each chat request received, in order.
    pub async fn chat_messages(&self) -> Vec<Vec<Value>> {
        self.received_requests()
            .await
            .iter()
            .filter(|r| r.url.path().ends_with("/chat/completions"))
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .map(|body| body["messages"].as_array().cloned().unwrap_or_default())
            .collect()
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

    /// Mount a sequence of two responses for the same SSE endpoint:
    /// 1. The **first** request gets a response that stays silent for
    ///    `silent_for` before any bytes arrive (modelling a stalled SSE
    ///    stream / dead TCP socket).
    /// 2. **Subsequent** requests get an immediate 200 OK SSE stream
    ///    framed from `chunks` (modelling the upstream recovering on
    ///    retry).
    ///
    /// Used by the inference-loop retry regression test (#1123) to
    /// prove that `try_with_rate_limit` actually retries on a transient
    /// network timeout instead of bubbling "connection broken" up to
    /// the user. Pair with a `stream_idle_timeout_ms` well below
    /// `silent_for` so the client gives up on the first attempt.
    ///
    /// SSE framing matches [`Self::mount_chat_sse`]: each chunk is
    /// emitted as `data: <chunk>\n\n`, no automatic `[DONE]` trailer
    /// (callers include it explicitly).
    ///
    /// Wiremock matches mocks in insertion order; `up_to_n_times(1)`
    /// retires the silent mock after one match so the second mock
    /// (the success path) takes over for the retry.
    pub async fn mount_chat_silent_then_resume(&self, silent_for: Duration, chunks: &[&str]) {
        let path_re = r".*/chat/completions$";
        let body = chunks
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect::<String>();

        // First mock: stall before any bytes arrive. The body is
        // irrelevant because the client gives up before the delay
        // elapses, but wiremock still wants something to template.
        Mock::given(method("POST"))
            .and(path_regex(path_re))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body.clone())
                    .set_delay(silent_for),
            )
            .up_to_n_times(1)
            .mount(&self.server)
            .await;

        // Second mock: immediate SSE success.
        Mock::given(method("POST"))
            .and(path_regex(path_re))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
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

/// Answers each request with the next response of `script`, streamed or
/// not as the request asks, and an empty reply once it is spent.
fn script_responder(script: Vec<MockResponse>) -> impl wiremock::Respond {
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    move |request: &Request| {
        let stream =
            serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body["stream"] == true);
        let next = script.lock().unwrap().pop_front();
        openai_reply(next.unwrap_or(MockResponse::Text(String::new())), stream)
    }
}

/// Matches a chat request whose system message begins with the text.
struct SystemPromptStartsWith(String);

impl wiremock::Match for SystemPromptStartsWith {
    fn matches(&self, request: &Request) -> bool {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return false;
        };
        body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|m| {
                m["role"] == "system" && system_text(&m["content"]).starts_with(self.0.as_str())
            })
        })
    }
}

/// A message's text: a string, or the text parts of a content array.
fn system_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// `response` as an OpenAI-compatible chat server sends it: as a stream of
/// chunks when the request asked for one, else as one JSON body.
fn openai_reply(response: MockResponse, stream: bool) -> ResponseTemplate {
    let error = |status: u16, message: &str| {
        ResponseTemplate::new(status)
            .set_body_json(json!({"error": {"message": message, "type": "error"}}))
    };
    let tool_calls = |calls: Vec<koda_providers::ToolCall>| {
        let calls: Vec<Value> = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                json!({"index": index, "id": call.id, "type": "function",
                    "function": {"name": call.function_name, "arguments": call.arguments}})
            })
            .collect();
        vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": null}]}),
            finish("tool_calls"),
        ]
    };
    let text = |text: String, reason: &str| {
        vec![
            json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]}),
            finish(reason),
        ]
    };
    if !stream {
        let (content, calls, reason) = match response {
            MockResponse::Text(t) => (Some(t), vec![], "stop"),
            MockResponse::TextMaxTokens(t) => (Some(t), vec![], "length"),
            MockResponse::ToolCalls(calls) | MockResponse::ToolCallsEager(calls) => {
                (None, calls, "tool_calls")
            }
            MockResponse::NetworkError { .. } => {
                return ResponseTemplate::new(200).set_body_string("{\"choices\": [");
            }
            MockResponse::Error(message) => return error(400, &message),
            MockResponse::Http { status, body } => {
                return ResponseTemplate::new(status).set_body_string(body);
            }
            MockResponse::RateLimit => return error(429, "Too Many Requests"),
            MockResponse::ContextOverflow => {
                return error(400, "prompt is too long, maximum context length exceeded");
            }
        };
        let calls: Vec<Value> = calls
            .into_iter()
            .map(|call| {
                json!({"id": call.id, "type": "function",
                    "function": {"name": call.function_name, "arguments": call.arguments}})
            })
            .collect();
        let mut message = json!({"role": "assistant", "content": content});
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        return ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index": 0, "message": message, "finish_reason": reason}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }));
    }
    let (chunks, done) = match response {
        MockResponse::Text(t) => (text(t, "stop"), true),
        MockResponse::TextMaxTokens(t) => (text(t, "length"), true),
        MockResponse::ToolCalls(calls) | MockResponse::ToolCallsEager(calls) => {
            (tool_calls(calls), true)
        }
        MockResponse::NetworkError { partial_text, .. } => (
            vec![
                json!({"choices": [{"index": 0, "delta": {"content": partial_text}, "finish_reason": null}]}),
            ],
            false,
        ),
        MockResponse::Error(message) => return error(400, &message),
        MockResponse::Http { status, body } => {
            return ResponseTemplate::new(status).set_body_string(body);
        }
        MockResponse::RateLimit => return error(429, "Too Many Requests"),
        MockResponse::ContextOverflow => {
            return error(400, "prompt is too long, maximum context length exceeded");
        }
    };
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// The last chunk of a reply: why it ended, and its usage.
fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})
}

/// A [`FakeLlmServer`] playing a script, for a synchronous test that runs
/// the `koda` binary: it keeps its own runtime, so the test needn't be
/// async. [`Self::koda_args`] points `koda` at it.
pub struct ScriptedServer {
    server: Option<FakeLlmServer>,
    url: String,
    runtime: tokio::runtime::Runtime,
}

impl ScriptedServer {
    /// Serve `script`, in [`parse_script`]'s JSON form.
    pub fn start(script: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime for the stub server");
        let server = runtime.block_on(async {
            let server = FakeLlmServer::spawn().await;
            server.mount_script_json(script).await;
            server
        });
        let url = server.url();
        Self {
            server: Some(server),
            url,
            runtime,
        }
    }

    /// A script of its own for the agent whose system prompt begins with
    /// `marker`, as [`FakeLlmServer::mount_script_for`].
    pub fn script_for(&self, marker: &str, script: &str) {
        let server = self.server.as_ref().expect("running");
        self.runtime
            .block_on(server.mount_script_json_for(marker, script));
    }

    /// The flags that send `koda`'s model calls here.
    pub fn koda_args(&self) -> [String; 6] {
        [
            "--provider".into(),
            "lmstudio".into(),
            "--model".into(),
            "stub".into(),
            "--base-url".into(),
            self.url.clone(),
        ]
    }

    /// The `messages` of each chat request received, in order.
    pub fn chat_messages(&self) -> Vec<Vec<Value>> {
        let server = self.server.as_ref().expect("running");
        self.runtime.block_on(server.chat_messages())
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        let _inside = self.runtime.enter();
        self.server.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request as an agent with this system prompt sends it, unstreamed;
    /// the reply's text.
    async fn ask(url: &str, system: &str) -> String {
        let body = json!({"model": "stub", "stream": false, "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": "go"}
        ]});
        let reply: Value = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        reply["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Two agents calling at the same time each get their own script, in
    /// order; a request no script names takes the shared one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_agent_gets_its_own_script_whatever_the_timing() {
        let server = FakeLlmServer::spawn().await;
        server.mount_script_json(r#"[{"text": "shared"}]"#).await;
        server
            .mount_script_json_for(
                "You are ALPHA",
                r#"[{"text": "a1"}, {"text": "a2"}, {"text": "a3"}]"#,
            )
            .await;
        server
            .mount_script_json_for(
                "You are BETA",
                r#"[{"text": "b1"}, {"text": "b2"}, {"text": "b3"}]"#,
            )
            .await;
        let url = server.url();
        let agent = |name: &'static str| {
            let url = url.clone();
            tokio::spawn(async move {
                let mut got = Vec::new();
                for _ in 0..3 {
                    // Each prompt also names the other agent, as Koda's do.
                    let system = format!("You are {name}.\n\nAgents: ALPHA, BETA.");
                    got.push(ask(&url, &system).await);
                }
                got
            })
        };
        let (alpha, beta) = (agent("ALPHA"), agent("BETA"));
        let other = ask(&url, "You are the parent.").await;
        assert_eq!(alpha.await.unwrap(), ["a1", "a2", "a3"]);
        assert_eq!(beta.await.unwrap(), ["b1", "b2", "b3"]);
        assert_eq!(other, "shared");
    }
}
