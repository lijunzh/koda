//! HTTP-client configuration tests for [`build_http_client`].
//!
//! Exercises proxy-honoring, localhost-bypass, basic-auth-via-env-vars,
//! and the graceful-degradation path for invalid `HTTP_PROXY` URLs.
//!
//! ## Why not an in-source unit test?
//!
//! The behaviors under test require a real TCP socket on loopback
//! (the reqwest proxy machinery only kicks in on actual `.send()`).
//! That makes them integration tests, not unit tests.
//!
//! ## Test isolation
//!
//! All tests in this file mutate process-wide state in
//! [`koda_core::runtime_env`] (the runtime env-var map).
//! [`koda_test_utils::ENV_MUTEX`] serializes them so concurrent test
//! runners don't trample each other, and [`EnvGuard`] (defined below)
//! removes any keys this test set when it returns — even on panic.
//!
//! Related: #914 covers the missing retry/backoff/timeout layer; once
//! that lands, retry-on-429 and timeout-on-stall tests can use the
//! same `FakeLlmServer` fixtures.

use std::sync::OnceLock;

use koda_core::providers::openai_compat::OpenAiCompatProvider;
use koda_core::providers::{ChatMessage, LlmProvider};
use koda_core::{config::ModelSettings, config::ProviderType, runtime_env};
use koda_test_utils::ENV_MUTEX;
use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};

// ── Helpers ───────────────────────────────────────────────────────────────

/// Drops registered runtime-env keys when scope ends. Survives test panics
/// thanks to RAII, so a failing test cannot leak env state into siblings.
struct EnvGuard<'a> {
    keys: Vec<&'a str>,
}

impl<'a> EnvGuard<'a> {
    fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Set the runtime-env var and remember to remove it at drop.
    fn set(&mut self, key: &'a str, value: &str) {
        runtime_env::set(key, value);
        self.keys.push(key);
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        for k in &self.keys {
            runtime_env::remove(k);
        }
    }
}

/// Pre-clear every proxy-related env var in the **process** environment.
///
/// We can't use [`runtime_env::remove`] for these because the runtime-env
/// fallback to `std::env::var` would still surface them.
/// Returns the original values so [`PROCESS_ENV_GUARD`] can restore them.
fn snapshot_and_clear_process_proxy_env() -> Vec<(&'static str, Option<String>)> {
    const KEYS: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "PROXY_USER",
        "PROXY_PASS",
    ];
    let snap: Vec<_> = KEYS.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    // SAFETY: ENV_MUTEX is held by the caller, so no other thread is reading
    // these vars concurrently. This block only runs in tests.
    unsafe {
        for k in KEYS {
            std::env::remove_var(k);
        }
    }
    snap
}

fn restore_process_env(snap: Vec<(&'static str, Option<String>)>) {
    // SAFETY: ENV_MUTEX still held by the caller.
    unsafe {
        for (k, v) in snap {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn ok_chat_body() -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

fn settings() -> ModelSettings {
    static S: OnceLock<ModelSettings> = OnceLock::new();
    S.get_or_init(|| ModelSettings::defaults_for("gpt-4o", &ProviderType::OpenAI))
        .clone()
}

fn user_msg() -> ChatMessage {
    ChatMessage::text("user", "hi")
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_proxy_routes_remote_requests_through_proxy() {
    let _g = ENV_MUTEX.lock().await;
    let proc_snap = snapshot_and_clear_process_proxy_env();

    // The proxy is a FakeLlmServer — it won't actually forward, just match
    // the path regex on the inbound proxied request and respond 200. That's
    // enough to prove "the request hit the proxy, not the origin".
    let proxy = FakeLlmServer::spawn().await;
    proxy.mount_chat_ok(ok_chat_body()).await;

    let mut env = EnvGuard::new();
    env.set("HTTP_PROXY", &proxy.url());

    // Target a non-localhost host so the proxy is consulted (the bypass
    // list excludes only localhost/127.0.0.1/::1).
    let provider =
        OpenAiCompatProvider::new("http://upstream.test.invalid", Some("sk-test".into()));
    provider
        .chat(&[user_msg()], &[], &settings())
        .await
        .expect("chat must succeed via proxy");

    let proxied = proxy.received_requests().await;
    assert_eq!(proxied.len(), 1, "request must be routed through the proxy");

    drop(env);
    restore_process_env(proc_snap);
}

#[tokio::test]
async fn localhost_traffic_bypasses_proxy_even_when_set() {
    let _g = ENV_MUTEX.lock().await;
    let proc_snap = snapshot_and_clear_process_proxy_env();

    // Two servers: a "proxy" we expect to be skipped, an "upstream" we
    // expect to receive the request directly because the URL is localhost.
    let proxy = FakeLlmServer::spawn().await;
    proxy.mount_chat_ok(ok_chat_body()).await;
    let upstream = FakeLlmServer::spawn().await;
    upstream.mount_chat_ok(ok_chat_body()).await;

    let mut env = EnvGuard::new();
    env.set("HTTP_PROXY", &proxy.url());

    let provider = OpenAiCompatProvider::new(&upstream.url(), Some("sk-test".into()));
    provider
        .chat(&[user_msg()], &[], &settings())
        .await
        .expect("chat must succeed directly to localhost upstream");

    assert_eq!(
        proxy.received_requests().await.len(),
        0,
        "proxy must be bypassed for localhost targets"
    );
    assert_eq!(
        upstream.received_requests().await.len(),
        1,
        "upstream must receive the request directly"
    );

    drop(env);
    restore_process_env(proc_snap);
}

#[tokio::test]
async fn proxy_basic_auth_from_env_vars_attaches_proxy_authorization_header() {
    let _g = ENV_MUTEX.lock().await;
    let proc_snap = snapshot_and_clear_process_proxy_env();

    let proxy = FakeLlmServer::spawn().await;
    proxy.mount_chat_ok(ok_chat_body()).await;

    let mut env = EnvGuard::new();
    env.set("HTTP_PROXY", &proxy.url());
    env.set("PROXY_USER", "alice");
    env.set("PROXY_PASS", "s3cret");

    let provider =
        OpenAiCompatProvider::new("http://upstream.test.invalid", Some("sk-test".into()));
    provider
        .chat(&[user_msg()], &[], &settings())
        .await
        .expect("chat must succeed via authenticated proxy");

    let proxied = proxy.received_requests().await;
    assert_eq!(proxied.len(), 1);
    let auth = proxied[0]
        .headers
        .get("proxy-authorization")
        .expect("Proxy-Authorization header must be set when PROXY_USER/PROXY_PASS are present");
    // Basic alice:s3cret = YWxpY2U6czNjcmV0
    assert_eq!(auth, "Basic YWxpY2U6czNjcmV0");

    drop(env);
    restore_process_env(proc_snap);
}

#[tokio::test]
async fn invalid_proxy_url_degrades_gracefully_to_no_proxy() {
    let _g = ENV_MUTEX.lock().await;
    let proc_snap = snapshot_and_clear_process_proxy_env();

    // Garbage proxy URL: the production code logs a warning and returns a
    // proxy-less client rather than panicking. Verify by making a request
    // to a localhost upstream and checking it actually arrives.
    let upstream = FakeLlmServer::spawn().await;
    upstream.mount_chat_ok(ok_chat_body()).await;

    let mut env = EnvGuard::new();
    env.set("HTTP_PROXY", "://this is not a url@@@");

    let provider = OpenAiCompatProvider::new(&upstream.url(), Some("sk-test".into()));
    provider
        .chat(&[user_msg()], &[], &settings())
        .await
        .expect("chat must still succeed when HTTP_PROXY is malformed");

    assert_eq!(
        upstream.received_requests().await.len(),
        1,
        "malformed proxy URL must not block legitimate localhost traffic"
    );

    drop(env);
    restore_process_env(proc_snap);
}
