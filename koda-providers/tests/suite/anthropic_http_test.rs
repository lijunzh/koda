//! HTTP-layer integration tests for [`AnthropicProvider`] using
//! [`koda_test_utils::network::FakeLlmServer`].
//!
//! Companion to `openai_compat_http_test.rs`. Anthropic differs from
//! OpenAI in three ways that motivate dedicated tests:
//!
//! * Endpoint is `POST /v1/messages` (not `/chat/completions`).
//! * Auth is the `x-api-key` header (not `Authorization: Bearer …`).
//! * `anthropic-version` is required on every request.
//!
//! These tests pin all three behaviors against a real reqwest round-trip.

use koda_providers::anthropic::AnthropicProvider;
use koda_providers::{ChatMessage, LlmProvider, StreamChunk};
use koda_providers::{ModelSettings, ProviderType};
use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};

/// Minimal well-formed `/v1/messages` non-streaming response.
fn ok_messages_body() -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5-5",
        "content": [{ "type": "text", "text": "ok" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
}

fn settings() -> ModelSettings {
    ModelSettings::defaults_for("claude-sonnet-5-5", &ProviderType::Anthropic)
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage::text("user", text)
}

// ── chat() ────────────────────────────────────────────────────────────────

/// `input_tokens` leaves out cached tokens, but they still fill the window.
#[test]
fn context_tokens_counts_the_cache() {
    let provider = AnthropicProvider::new("sk-ant-test".into(), None);
    let usage = koda_providers::TokenUsage {
        prompt_tokens: 288,
        cache_read_tokens: 20_000,
        cache_creation_tokens: 3_535,
        ..Default::default()
    };
    assert_eq!(provider.context_tokens(&usage), 23_823);
}

/// Compaction reads this to tell a reply cut off at the cap from a finished one.
#[tokio::test]
async fn chat_reports_the_stop_reason() {
    let server = FakeLlmServer::spawn().await;
    let mut body = ok_messages_body();
    body["content"] = serde_json::json!([]);
    body["stop_reason"] = serde_json::json!("max_tokens");
    server.mount_ok("POST", r".*/v1/messages$", body).await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    let reply = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    assert_eq!(reply.usage.stop_reason, "max_tokens");
}

#[tokio::test]
async fn chat_skips_thinking_blocks_in_the_reply() {
    let server = FakeLlmServer::spawn().await;
    let mut body = ok_messages_body();
    body["content"] = serde_json::json!([
        {"type": "thinking", "thinking": "Let me plan.", "signature": "sig"},
        {"type": "redacted_thinking", "data": "abc"},
        {"type": "text", "text": "The summary."}
    ]);
    server.mount_ok("POST", r".*/v1/messages$", body).await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    let reply = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("a thinking block must not fail the response");

    assert_eq!(reply.content.as_deref(), Some("The summary."));
}

#[tokio::test]
async fn chat_sends_post_to_v1_messages_endpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat must succeed against 200 mock");

    let reqs = server.received_requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert!(
        reqs[0].url.path().ends_with("/v1/messages"),
        "wrong path: {}",
        reqs[0].url.path()
    );
}

#[tokio::test]
async fn chat_sends_x_api_key_header_not_bearer() {
    // Anthropic uses `x-api-key`, NOT `Authorization: Bearer …`.
    // This is the most common provider-confusion bug; pin it explicitly.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-secret".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let api_key = reqs[0]
        .headers
        .get("x-api-key")
        .expect("x-api-key header required");
    assert_eq!(api_key, "sk-ant-secret");
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "Anthropic must NOT send Authorization header"
    );
}

#[tokio::test]
async fn chat_sends_anthropic_version_header() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let version = reqs[0]
        .headers
        .get("anthropic-version")
        .expect("anthropic-version header required on every request");
    // Don't pin the exact date — that's an implementation detail of
    // the constant ANTHROPIC_API_VERSION. Just assert it looks like a date.
    let v = version.to_str().unwrap();
    assert!(
        v.len() >= 10 && v.chars().nth(4) == Some('-'),
        "version must look like YYYY-MM-DD, got: {v}"
    );
}

#[tokio::test]
async fn chat_returns_error_on_5xx_with_status_in_message() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*/v1/messages$",
            503,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"upstream"}}"#,
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("5xx must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("503"), "error must mention status: {msg}");
    assert!(msg.contains("overloaded_error"), "error must include body");
}

#[tokio::test]
async fn chat_returns_error_on_401_invalid_api_key() {
    // Regression guard: pins current 'no-retry-on-4xx' behavior.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*/v1/messages$",
            401,
            r#"{"type":"error","error":{"type":"authentication_error"}}"#,
        )
        .await;

    let provider = AnthropicProvider::new("bad".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("401 must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error must mention status: {msg}");
}

/// The request carries three breakpoints: last tool, system, and the last
/// block of the last message, so the conversation prefix is cached too.
fn assert_conversation_breakpoint(body: &Value) {
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap()["content"].as_array().unwrap();
    assert_eq!(last.last().unwrap()["cache_control"]["type"], "ephemeral");
    assert_eq!(last.last().unwrap()["type"], "tool_result");

    // Nothing else in the messages is marked.
    let marked = serde_json::to_string(messages)
        .unwrap()
        .matches("cache_control")
        .count();
    assert_eq!(marked, 1, "only the last block of the last message");
}

fn cache_test_conversation() -> Vec<ChatMessage> {
    let mut result = ChatMessage::text("tool", "file contents");
    result.tool_call_id = Some("toolu_1".into());
    vec![
        ChatMessage::text("system", "You are Koda."),
        user_msg("read it"),
        result,
    ]
}

fn cache_test_tools() -> Vec<koda_providers::ToolDefinition> {
    vec![koda_providers::ToolDefinition {
        name: "Read".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object"}),
    }]
}

#[tokio::test]
async fn chat_marks_the_last_message_block_as_a_cache_breakpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    provider
        .chat(&cache_test_conversation(), &cache_test_tools(), &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_conversation_breakpoint(&body);
}

#[tokio::test]
async fn chat_stream_marks_the_last_message_block_as_a_cache_breakpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let mut collector = provider
        .chat_stream(&cache_test_conversation(), &cache_test_tools(), &settings())
        .await
        .unwrap();
    while collector.rx.recv().await.is_some() {}

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_conversation_breakpoint(&body);
}

/// A request that replays thinking says what to do if its history changed,
/// because compaction edits history and the default is a 400.
#[tokio::test]
async fn chat_stream_asks_to_drop_thinking_that_no_longer_matches() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse("POST", r".*/v1/messages$", &[r#"{"type":"message_stop"}"#])
        .await;
    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    let model = "claude-haiku-5-5";
    let settings = ModelSettings::defaults_for(model, &ProviderType::Anthropic);

    let mut turn = ChatMessage::text("assistant", "done");
    turn.thinking_blocks = Some(
        json!({"model": model, "blocks": [{"type": "thinking", "thinking": "", "signature": "s"}]})
            .to_string(),
    );
    for messages in [
        vec![user_msg("hi"), turn, user_msg("next")],
        vec![user_msg("hi")],
    ] {
        provider
            .chat_stream(&messages, &[], &settings)
            .await
            .expect("chat_stream must succeed");
    }

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["messages"][1]["content"][0]["signature"], "s");
    assert_eq!(
        body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
        "drop_block"
    );
    let beta = reqs[0]
        .headers
        .get("anthropic-beta")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        beta.contains("thinking-binding-controls-2026-08-01"),
        "{beta}"
    );

    let plain: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert!(plain.get("thinking").is_none(), "{plain}");
}

// ── chat_stream() ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_consumes_anthropic_sse_event_format_via_real_tcp() {
    // Anthropic SSE uses a different chunk shape than OpenAI:
    // each event has a `type` field and `content_block_delta` events
    // carry text in `delta.text`.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo "}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":3}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let mut collector = provider
        .chat_stream(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat_stream must succeed");

    let mut text = String::new();
    while let Some(chunk) = collector.rx.recv().await {
        if let StreamChunk::TextDelta(s) = chunk {
            text.push_str(&s);
        }
    }

    assert_eq!(text, "hello world", "all SSE deltas must be reassembled");
}

// ── Cache diagnostics ─────────────────────────────────────────────────────

/// Each request names its conversation's previous response, so the API can
/// say what changed when the cache misses. A new conversation starts with
/// `null`, and its chain is its own.
#[tokio::test]
async fn chat_stream_chains_cache_diagnostics_per_conversation() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"output_tokens":0},"diagnostics":{"cache_miss_reason":{"type":"tools_changed","cache_missed_input_tokens":7000}}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;
    let provider =
        AnthropicProvider::new("k".into(), Some(&server.url())).with_cache_diagnostics(true);
    let first = vec![ChatMessage::text("system", "sys"), user_msg("hi")];
    let mut second = first.clone();
    second.push(ChatMessage::text("assistant", "hello"));
    second.push(user_msg("more"));
    let other = vec![ChatMessage::text("system", "sys"), user_msg("another task")];
    for messages in [&first, &second, &other] {
        let mut collector = provider
            .chat_stream(messages, &[], &settings())
            .await
            .unwrap();
        while collector.rx.recv().await.is_some() {}
    }

    let reqs = server.received_requests().await;
    let previous: Vec<Value> = reqs
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["diagnostics"]["previous_message_id"].clone()
        })
        .collect();
    assert_eq!(previous, [Value::Null, json!("msg_1"), Value::Null]);
}

#[tokio::test]
async fn chat_chains_cache_diagnostics() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;
    let provider =
        AnthropicProvider::new("k".into(), Some(&server.url())).with_cache_diagnostics(true);
    let first = vec![user_msg("hi")];
    let mut second = first.clone();
    second.push(ChatMessage::text("assistant", "ok"));
    second.push(user_msg("again"));
    for messages in [&first, &second] {
        provider.chat(messages, &[], &settings()).await.unwrap();
    }

    let reqs = server.received_requests().await;
    let body = |i: usize| -> Value { serde_json::from_slice(&reqs[i].body).unwrap() };
    assert_eq!(
        body(0)["diagnostics"],
        json!({ "previous_message_id": null })
    );
    assert_eq!(body(1)["diagnostics"]["previous_message_id"], "msg_test");
}

/// A proxy or gateway may reject the field, so another base URL sends none.
#[tokio::test]
async fn cache_diagnostics_are_off_away_from_anthropic() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;
    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("diagnostics").is_none(), "{body}");
}

// ── Effort ────────────────────────────────────────────────────────────────

async fn streamed_body(settings: &ModelSettings) -> Value {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse("POST", r".*/v1/messages$", &[r#"{"type":"message_stop"}"#])
        .await;
    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let mut collector = provider
        .chat_stream(&[user_msg("hi")], &[], settings)
        .await
        .unwrap();
    while collector.rx.recv().await.is_some() {}
    let reqs = server.received_requests().await;
    serde_json::from_slice(&reqs[0].body).unwrap()
}

/// A model in the catalog gets its level sent, the chosen one or Claude
/// Code's default; a model the catalog doesn't have yet gets none.
#[tokio::test]
async fn chat_stream_sends_the_catalog_effort() {
    let mut sonnet = ModelSettings::defaults_for("claude-sonnet-5-5", &ProviderType::Anthropic);
    assert_eq!(
        streamed_body(&sonnet).await["output_config"]["effort"],
        "medium"
    );
    sonnet.reasoning_effort = Some("high".into());
    assert_eq!(
        streamed_body(&sonnet).await["output_config"]["effort"],
        "high"
    );

    let newer = ModelSettings::defaults_for("claude-opus-9", &ProviderType::Anthropic);
    let body = streamed_body(&newer).await;
    assert!(body.get("output_config").is_none(), "{body}");
}

/// The turn's task budget goes in `output_config` with its beta header, to
/// a model that takes one, next to the effort; one that doesn't gets none.
#[tokio::test]
async fn chat_stream_sends_a_task_budget_to_a_model_that_takes_one() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse("POST", r".*/v1/messages$", &[r#"{"type":"message_stop"}"#])
        .await;
    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let budget = koda_providers::TaskBudget {
        total: 64_000,
        remaining: Some(40_000),
    };
    for model in ["claude-haiku-5-5", "claude-sonnet-5"] {
        let mut settings = ModelSettings::defaults_for(model, &ProviderType::Anthropic);
        settings.task_budget = Some(budget);
        let mut collector = provider
            .chat_stream(&[user_msg("hi")], &[], &settings)
            .await
            .unwrap();
        while collector.rx.recv().await.is_some() {}
    }
    let reqs = server.received_requests().await;
    let beta = |i: usize| {
        reqs[i]
            .headers
            .get("anthropic-beta")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    let haiku: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        haiku["output_config"]["task_budget"],
        json!({"type": "tokens", "total": 64000, "remaining": 40000})
    );
    assert_eq!(haiku["output_config"]["effort"], "medium");
    assert!(beta(0).contains("task-budgets-2026-03-13"), "{}", beta(0));
    // Sonnet 5 doesn't take one.
    let sonnet: Value = serde_json::from_slice(&reqs[1].body).unwrap();
    assert!(
        sonnet["output_config"].get("task_budget").is_none(),
        "{sonnet}"
    );
    assert!(!beta(1).contains("task-budgets"), "{}", beta(1));
}

/// Claude models take effort and refuse a token budget with a 400, so a
/// budget (an agent file's, or one meant for Gemini) is never sent to them.
#[tokio::test]
async fn chat_stream_never_sends_claude_a_thinking_budget() {
    let mut haiku = ModelSettings::defaults_for("claude-haiku-5-5", &ProviderType::Anthropic);
    haiku.thinking_budget = Some(8000);
    let body = streamed_body(&haiku).await;
    assert!(body.get("thinking").is_none(), "{body}");
    assert_eq!(body["output_config"]["effort"], "medium");

    let mut older = ModelSettings::defaults_for("claude-sonnet-4-5", &ProviderType::Anthropic);
    older.thinking_budget = Some(8000);
    let body = streamed_body(&older).await;
    assert!(body.get("thinking").is_none(), "an older model too: {body}");
}

#[tokio::test]
async fn chat_sends_the_catalog_effort() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;
    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let opus = ModelSettings::defaults_for("claude-opus-5-5", &ProviderType::Anthropic);
    provider.chat(&[user_msg("hi")], &[], &opus).await.unwrap();
    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["output_config"]["effort"], "medium");
}

/// MCP tools go behind the Claude API's tool search on a model that has it,
/// as Codex defers them: the search tool first, the up-front tools with the
/// cache breakpoint, the deferred ones with `defer_loading` and no cache
/// marker. A search the model ran comes back on the next request as it came.
/// A model without tool search gets every tool up front.
#[tokio::test]
async fn chat_stream_defers_mcp_tools_behind_tool_search() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_bm25"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"weather\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":"mcp__w__forecast"}]}}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;
    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let tool = |name: &str| koda_providers::ToolDefinition {
        name: name.into(),
        description: format!("{name} does a thing"),
        parameters: json!({"type": "object", "properties": {}}),
    };
    let tools = [tool("Read"), tool("Bash"), tool("mcp__w__forecast")];
    let mut settings = ModelSettings::defaults_for("claude-haiku-5-5", &ProviderType::Anthropic);
    settings.deferred_tools = vec!["mcp__w__forecast".into()];

    let mut collector = provider
        .chat_stream(&[user_msg("weather?")], &tools, &settings)
        .await
        .unwrap();
    let mut blocks = Vec::new();
    while let Some(chunk) = collector.rx.recv().await {
        if let StreamChunk::ThinkingBlock(block) = chunk {
            blocks.push(block);
        }
    }
    let first: Value = serde_json::from_slice(&server.received_requests().await[0].body).unwrap();
    let sent = first["tools"].as_array().unwrap();
    assert_eq!(sent[0]["type"], "tool_search_tool_bm25_20251119", "{first}");
    assert_eq!(sent[0]["name"], "tool_search_tool_bm25");
    let names: Vec<_> = sent[1..]
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Read", "Bash", "mcp__w__forecast"]);
    assert_eq!(sent[2]["cache_control"]["type"], "ephemeral", "{first}");
    assert_eq!(sent[3]["defer_loading"], true);
    assert!(sent[3].get("cache_control").is_none(), "{first}");
    assert_eq!(
        first.to_string().matches("cache_control").count(),
        first["messages"]
            .to_string()
            .matches("cache_control")
            .count()
            + 1,
        "one breakpoint in the tools: {first}"
    );

    // The search was kept, input included, and goes back next time.
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert_eq!(blocks[0]["input"], json!({"query": "weather"}));
    let stored =
        koda_providers::anthropic::stored_thinking("claude-haiku-5-5", blocks.clone()).unwrap();
    let mut reply = ChatMessage::text("assistant", "Found it.");
    reply.thinking_blocks = Some(stored.clone());
    let history = [
        user_msg("weather?"),
        reply.clone(),
        user_msg("and tomorrow?"),
    ];
    let mut collector = provider
        .chat_stream(&history, &tools, &settings)
        .await
        .unwrap();
    while collector.rx.recv().await.is_some() {}
    let second: Value = serde_json::from_slice(&server.received_requests().await[1].body).unwrap();
    let assistant = &second["messages"][1]["content"];
    assert_eq!(assistant[0]["type"], "server_tool_use", "{second}");
    assert_eq!(assistant[0]["input"], json!({"query": "weather"}));
    assert_eq!(assistant[1]["type"], "tool_search_tool_result", "{second}");

    // No tool search on this model: everything up front, and no search
    // replayed, since its tool isn't in the request.
    let older = ModelSettings {
        deferred_tools: settings.deferred_tools.clone(),
        ..ModelSettings::defaults_for("claude-opus-4-1", &ProviderType::Anthropic)
    };
    let mut collector = provider
        .chat_stream(&history, &tools, &older)
        .await
        .unwrap();
    while collector.rx.recv().await.is_some() {}
    let third: Value = serde_json::from_slice(&server.received_requests().await[2].body).unwrap();
    assert!(!third.to_string().contains("tool_search"), "{third}");
    assert!(!third.to_string().contains("defer_loading"), "{third}");
}
