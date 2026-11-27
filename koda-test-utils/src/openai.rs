//! A scripted OpenAI-compatible model, for tests that run Koda against a
//! real HTTP server: replies built from streamed chunks, a server that plays
//! them in order, and what it was sent. One copy, as Codex's
//! `core_test_support::responses` is, in place of one per test file.
//!
//! ```rust,ignore
//! use koda_test_utils::openai::{model, text, tool_call, chat_requests};
//!
//! let server = model(vec![tool_call("call_1", "Read", json!({"file_path": "a"})), text("Done.")]).await;
//! // … run Koda against `server.url()` …
//! assert_eq!(chat_requests(&server).await.len(), 2);
//! ```

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::network::{FakeLlmServer, Request, ResponseTemplate};

/// A streamed reply made of `chunks`, ended by `[DONE]`.
pub fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// The usage chunk a stream ends with.
pub fn usage(prompt: u32, completion: u32) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion}})
}

/// A streamed text reply in one delta: 200 tokens in, 20 out.
pub fn text(reply: &str) -> ResponseTemplate {
    text_in(&[reply])
}

/// A streamed text reply in `pieces`, one delta each.
pub fn text_in(pieces: &[&str]) -> ResponseTemplate {
    let mut chunks: Vec<Value> = pieces
        .iter()
        .map(|p| json!({"choices": [{"delta": {"content": p}}]}))
        .collect();
    chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
    chunks.push(usage(200, 20));
    sse(&chunks)
}

/// A streamed reply that calls one tool: 100 tokens in, 10 out.
pub fn tool_call(id: &str, name: &str, args: Value) -> ResponseTemplate {
    tool_calls(&[(id, name, args)])
}

/// A streamed reply that calls several tools at once.
pub fn tool_calls(calls: &[(&str, &str, Value)]) -> ResponseTemplate {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            })
        })
        .collect();
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": calls}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        usage(100, 10),
    ])
}

/// A model server that answers each chat request with the next of
/// `replies`, then with "(no more replies)".
pub async fn model(replies: Vec<ResponseTemplate>) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let queue = Mutex::new(VecDeque::from(replies));
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| text("(no more replies)"))
        })
        .await;
    server
}

/// The chat requests `server` received, as JSON, in order.
pub async fn chat_requests(server: &FakeLlmServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .iter()
        .filter(|r: &&Request| r.url.path().ends_with("/chat/completions"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}
