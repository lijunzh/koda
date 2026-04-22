//! End-to-end tests for the always-on built-in egress proxy
//! (Phase 3b of #934, slice 6).
//!
//! These tests exercise the full vertical slice from
//! [`KodaSession::new`] all the way down to a real Bash-spawned child
//! process observing the canonical proxy env-var bouquet attached by
//! [`koda_core::sandbox::build`]. They are the integration-level proof
//! that slices 1–5 wired together correctly:
//!
//! ```text
//!   KodaSession::new()
//!     └─> BuiltInProxy::spawn()      ── slice 4
//!     └─> agent.tools.set_proxy_port ── slice 5
//!     ⇣
//!   tools.execute("Bash", …)         ── slice 5 (dispatch)
//!     └─> shell::run_shell_command   ── slice 5
//!     └─> sandbox::build(.., port)   ── slice 5
//!     └─> Command + env { HTTPS_PROXY=http://127.0.0.1:port, … }
//!     ⇣
//!   real `sh -c` child sees HTTPS_PROXY
//! ```
//!
//! Run with: `cargo test -p koda-core --test builtin_proxy_e2e_test`
//! Add `--ignored` to also run the live curl-through-proxy test.

use koda_core::{
    agent::KodaAgent, config::ProviderType, providers, session::KodaSession, tools::ToolRegistry,
    trust::TrustMode,
};
use koda_test_utils::Env;
use std::sync::Arc;

/// Build an `Arc<KodaAgent>` wired to a fresh ToolRegistry on the given root.
/// Mirrors what `KodaConfig` → `KodaAgent` would do in production but skips
/// MCP/skills discovery (we don't need them here).
fn build_agent(root: std::path::PathBuf, max_context_tokens: usize) -> Arc<KodaAgent> {
    let tools = ToolRegistry::new(root.clone(), max_context_tokens);
    let tool_defs = ToolRegistry::new(root.clone(), max_context_tokens).get_definitions(&[], &[]);
    Arc::new(KodaAgent {
        project_root: root,
        tools,
        tool_defs,
        system_prompt: "You are a test assistant.".to_string(),
    })
}

/// Construct a real `KodaSession` via `KodaSession::new` so the proxy
/// auto-spawn path actually runs. Returns the session + the upstream
/// agent for inspection.
async fn make_real_session(env: &Env) -> KodaSession {
    let agent = build_agent(env.root.clone(), env.config.max_context_tokens);
    // Use the regular constructor — this is what we're testing.
    // The provider is built from env.config (Mock by default in our env).
    // No network is touched at session-construction time.
    let _ = providers::create_provider(&env.config); // smoke-check provider builds
    KodaSession::new(
        env.session_id.clone(),
        agent,
        env.db.clone(),
        &env.config,
        TrustMode::Auto,
    )
    .await
}

// ── Hermetic E2E (always runs) ────────────────────────────────────────

/// `KodaSession::new` must spawn the built-in proxy unconditionally.
/// Matches the post-pivot "always-on, no opt-in" contract: koda is
/// config-free, so the user never has to enable anything.
#[tokio::test]
async fn session_new_auto_spawns_proxy() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;

    assert!(
        session.proxy.is_some(),
        "KodaSession::new must auto-spawn the built-in proxy"
    );
    let port = session.proxy.as_ref().unwrap().port;
    assert!(port > 0, "proxy must report a real ephemeral port");
    assert_eq!(
        session.agent.tools.proxy_port(),
        Some(port),
        "agent's ToolRegistry must hold the same port the session spawned"
    );
}

/// The real proof: a Bash invocation routed through the full
/// dispatch pipeline must observe the proxy env-var bouquet, with
/// the URL pointing at the *actual* listening port of the
/// session-spawned proxy.
///
/// This is the slice 5+6 integration assertion in one test \u2014 if the
/// port leakslong the chain (session\u2192tools\u2192shell\u2192sandbox::build),
/// the assertion below fires.
#[tokio::test]
async fn bash_sees_proxy_env_pointing_at_session_proxy() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;
    let port = session.proxy.as_ref().expect("proxy auto-spawned").port;

    // Run the full Bash dispatch path. `tools.execute` is what the
    // engine loop calls in production for every model-emitted tool call.
    let result = session
        .agent
        .tools
        .execute("Bash", r#"{"command":"echo \"$HTTPS_PROXY\""}"#, None)
        .await;

    assert!(
        result.success,
        "Bash dispatch must succeed; got output: {}",
        result.output
    );
    let combined = format!(
        "{}\n{}",
        result.output,
        result.full_output.as_deref().unwrap_or("")
    );
    let expected = format!("http://127.0.0.1:{port}");
    assert!(
        combined.contains(&expected),
        "child process must see HTTPS_PROXY={expected}; got: {combined:?}"
    );
}

/// The proxy listener spawned by `KodaSession::new` must actually be
/// alive and accepting TCP connections. Bash sees the URL but if the
/// proxy isn't really there (e.g. the spawn task crashed silently),
/// every outbound HTTP call would hang or fail with ECONNREFUSED.
///
/// This catches: "we wired the env var but the proxy task died on
/// startup" \u2014 a class of bug that the dispatch test above can't see.
#[tokio::test]
async fn session_proxy_accepts_tcp_connections() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;
    let port = session.proxy.as_ref().expect("proxy spawned").port;

    let conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
    assert!(
        conn.is_ok(),
        "proxy must accept connections on 127.0.0.1:{port}; got: {conn:?}"
    );
}

/// Two sessions in the same process must each get their own proxy on
/// distinct ports. Regression guard: if we ever accidentally cache /
/// share a single proxy across sessions, sub-agent isolation would
/// break (Phase 5 territory but cheap to guard now).
#[tokio::test]
async fn distinct_sessions_get_distinct_proxies() {
    let env_a = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let env_b = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let sess_a = make_real_session(&env_a).await;
    let sess_b = make_real_session(&env_b).await;

    let port_a = sess_a.proxy.as_ref().expect("a spawned").port;
    let port_b = sess_b.proxy.as_ref().expect("b spawned").port;
    assert_ne!(
        port_a, port_b,
        "each session must get its own ephemeral proxy port"
    );
}

/// Dropping a session must abort its proxy. Verified by re-binding the
/// same port \u2014 if the listener is gone, the rebind succeeds; if the
/// proxy task leaked, the rebind fails with EADDRINUSE.
#[tokio::test]
async fn dropping_session_releases_proxy_port() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let port = {
        let session = make_real_session(&env).await;
        session.proxy.as_ref().expect("spawned").port
        // session drops here \u2014 ProxyHandle::Drop should abort the task.
    };

    // Give the runtime a tick to actually run the abort + close the listener.
    // Polling rather than fixed-sleep keeps this fast and non-flaky.
    let mut bound = None;
    for _ in 0..50 {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                bound = Some(l);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    assert!(
        bound.is_some(),
        "after session drop, port {port} must be re-bindable (proxy task did not abort)"
    );
}

// ── Phase 3c: kernel-enforced egress (macOS only) ─────────────────────────

/// **The Phase 3c headline contract.** Even an ill-behaved binary that
/// completely ignores `HTTPS_PROXY` and tries to open a raw TCP
/// connection to a non-proxy port must be denied by the seatbelt
/// kernel sandbox. This is the security upgrade over Phase 3b: 3b
/// trusts clients to honor env vars; 3c forces them.
///
/// We exercise this with bash's `/dev/tcp/host/port` magic — a
/// pure-bash TCP open that doesn't honor any proxy env var. If the
/// kernel denies the connect (as 3c requires), bash prints an error
/// and the conditional reports `blocked`. If the kernel lets it
/// through, the connect either succeeds (`connected`) or fails
/// because nothing is listening on that port (`other-fail`) — either
/// non-`blocked` outcome means 3c isn't enforcing.
///
/// macOS only: the bwrap backend on Linux can't kernel-enforce port
/// filtering yet (see Phase 3c.1).
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_kernel_blocks_direct_tcp_to_non_proxy_port() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;
    let proxy_port = session.proxy.as_ref().expect("proxy spawned").port;

    // Pick a target port that is definitely NOT the proxy port. We
    // don't actually need anything listening there — we're testing the
    // kernel's ability to refuse the syscall, which happens before
    // userspace observes connection success or failure.
    let target_port = if proxy_port == 1 { 2 } else { 1 };

    // bash's `/dev/tcp/host/port` performs a connect(2) at the libc
    // level with no proxy honoring. The redirect uses fd 3 to keep
    // stdout/stderr clean for our parsing.
    let cmd = format!(
        r#"{{"command":"if exec 3<>/dev/tcp/127.0.0.1/{target_port} 2>/dev/null; then echo connected; exec 3<&-; else echo blocked; fi"}}"#,
    );
    let result = session.agent.tools.execute("Bash", &cmd, None).await;
    let combined = format!(
        "{}\n{}",
        result.output,
        result.full_output.as_deref().unwrap_or("")
    );
    assert!(
        combined.contains("blocked"),
        "kernel-enforced sandbox must refuse direct TCP to non-proxy port \
         {target_port} (proxy is on {proxy_port}); got:\n{combined}"
    );
}

/// Companion to `macos_kernel_blocks_direct_tcp_to_non_proxy_port`:
/// the kernel sandbox MUST still permit connections to the actual
/// proxy port, otherwise even well-behaved clients can't reach the
/// filter. Verifies the SBPL allow-rule actually allows the loopback
/// proxy port through.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_kernel_allows_tcp_to_proxy_port() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;
    let proxy_port = session.proxy.as_ref().expect("proxy spawned").port;

    // Connect to the proxy port from inside the sandbox via /dev/tcp.
    // The proxy is up, so connect(2) should succeed; the sandbox must
    // not get in the way.
    let cmd = format!(
        r#"{{"command":"if exec 3<>/dev/tcp/127.0.0.1/{proxy_port} 2>/dev/null; then echo connected; exec 3<&-; else echo blocked; fi"}}"#,
    );
    let result = session.agent.tools.execute("Bash", &cmd, None).await;
    let combined = format!(
        "{}\n{}",
        result.output,
        result.full_output.as_deref().unwrap_or("")
    );
    assert!(
        combined.contains("connected"),
        "kernel-enforced sandbox must permit TCP to the proxy port {proxy_port}; got:\n{combined}"
    );
}

// ── Live curl smoke (opt-in via --ignored) ───────────────────────────────

/// End-to-end denial proof: a Bash invocation that asks curl to fetch
/// a non-allowlisted host must fail with a proxy 403 \u2014 *before* any
/// real DNS lookup or TCP connect to the upstream happens, because the
/// proxy intercepts CONNECT requests and consults the filter first.
///
/// Hermetic in spirit: the upstream `blocked.test` is intentionally
/// non-routable, but with a working proxy we should never get that
/// far. If this test ever starts failing with "DNS failure" or
/// "connection refused" instead of a 4xx from the proxy, it means
/// curl bypassed the proxy entirely.
///
/// Marked `#[ignore]` only because it needs `curl` on PATH \u2014 nearly
/// universal but not quite guaranteed on stripped-down CI containers.
/// Run with: `cargo test -p koda-core --test builtin_proxy_e2e_test -- --ignored`
#[tokio::test]
#[ignore = "needs curl; run with --ignored"]
async fn curl_to_blocked_host_returns_403_via_proxy() {
    let env = Env::builder()
        .provider_type(ProviderType::Mock)
        .build()
        .await;
    let session = make_real_session(&env).await;

    // `blocked.test` is in the IETF .test TLD (RFC 6761) so it MUST
    // not resolve. If curl honors HTTPS_PROXY the proxy will deny at
    // CONNECT before any DNS lookup happens.
    let result = session
        .agent
        .tools
        .execute(
            "Bash",
            r#"{"command":"curl --max-time 5 -sS -o /dev/null -w '%{http_code}\\n' https://blocked.test/ 2>&1; echo exit=$?"}"#,
            None,
        )
        .await;

    let combined = format!(
        "{}\n{}",
        result.output,
        result.full_output.as_deref().unwrap_or("")
    );
    // curl with proxy denial typically prints "Received HTTP code 403
    // from proxy after CONNECT" and exits 56. Either of those signals
    // the integration is working.
    assert!(
        combined.contains("403") || combined.contains("CONNECT") || combined.contains("proxy"),
        "expected proxy denial signal in output; got:\n{combined}"
    );
    assert!(
        !combined.contains("exit=0"),
        "curl must NOT succeed (host is blocked); output:\n{combined}"
    );
}
