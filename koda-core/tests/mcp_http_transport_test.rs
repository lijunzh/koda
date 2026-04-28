//! Integration tests for the MCP HTTP (Streamable HTTP) transport.
//!
//! ## Structure
//!
//! **Layer 1 — transport-level (no SSRF guard):**  
//! We create a `StreamableHttpClientTransport` directly and connect it to a
//! real rmcp echo server running on a loopback port.  This verifies the full
//! MCP handshake, tool discovery, and bearer-token forwarding without
//! touching `McpClient::connect()`.
//!
//! **Layer 2 — McpClient SSRF guard (unit tests):**  
//! We call `McpClient::connect()` against a loopback URL and verify that the
//! SSRF guard fires *before* any network connection is attempted.
//!
//! These tests bind to `127.0.0.1:0` (loopback only, random port) and make
//! no external network calls.

use std::sync::{Arc, Mutex};

use koda_core::mcp::{
    client::{McpClient, McpClientStatus},
    config::{McpServerConfig, McpTransport},
};
use rmcp::transport::{
    StreamableHttpClientTransport,
    streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_client::StreamableHttpClientTransportConfig,
};
use tokio_util::sync::CancellationToken;

// ── Minimal MCP echo server ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// The message to echo back.
    message: String,
}

#[derive(Debug, Clone)]
struct EchoServer {
    tool_router: ToolRouter<Self>,
}

impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EchoServer {
    #[tool(description = "Echo the message back")]
    fn echo(&self, Parameters(EchoParams { message }): Parameters<EchoParams>) -> CallToolResult {
        CallToolResult::success(vec![Content::text(message)])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("A minimal echo server for integration tests")
    }
}

// ── Captured auth headers (for bearer-token forwarding test) ─────────────────

type CapturedHeaders = Arc<Mutex<Vec<Option<String>>>>;

// ── Spawn the echo server ─────────────────────────────────────────────────────

/// Spins up the echo MCP server on a random loopback port.
///
/// Returns the base URL, a cancellation token to stop the server, and a
/// shared vec of captured Authorization headers (one entry per POST request).
async fn spawn_echo_server() -> (String, CancellationToken, CapturedHeaders) {
    use axum::{Router, body::Body, http::Request, middleware, response::Response};

    let ct = CancellationToken::new();
    let captured: CapturedHeaders = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    let record_auth = middleware::from_fn(move |req: Request<Body>, next: middleware::Next| {
        let c = Arc::clone(&captured_clone);
        async move {
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            c.lock().unwrap().push(auth);
            let resp: Response = next.run(req).await;
            resp
        }
    });

    let service: StreamableHttpService<EchoServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(EchoServer::new()),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true)
                .with_sse_keep_alive(None)
                .with_cancellation_token(ct.child_token()),
        );

    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(record_auth);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/mcp");

    tokio::spawn({
        let ct = ct.clone();
        async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct.cancelled_owned().await })
                .await
                .unwrap();
        }
    });

    (url, ct, captured)
}

// ── Layer 1: transport-level tests ───────────────────────────────────────────
//
// These connect using StreamableHttpClientTransport directly, bypassing the
// McpClient SSRF guard (which correctly blocks loopback in production).

/// Connect + tool discovery: the echo tool must appear in tools/list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_connect_discovers_echo_tool() {
    let (url, ct, _captured) = spawn_echo_server().await;

    let config = StreamableHttpClientTransportConfig::with_uri(url);
    let transport = StreamableHttpClientTransport::from_config(config);

    let client = ().serve(transport).await.expect("transport connect should succeed");
    let tools = client
        .list_all_tools()
        .await
        .expect("tools/list should succeed");

    assert!(
        !tools.is_empty(),
        "at least one tool must be discovered after connect"
    );
    let has_echo = tools.iter().any(|t| t.name.contains("echo"));
    assert!(
        has_echo,
        "tool list must include 'echo', got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let _ = client.cancel().await;
    ct.cancel();
}

/// Bearer token forwarding: every POST must carry `Authorization: Bearer <t>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_bearer_token_forwarded_on_every_request() {
    let (url, ct, captured) = spawn_echo_server().await;
    let token = "super-secret-test-token-xyz";

    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    // rmcp's bearer_auth() prepends "Bearer " automatically — store raw token.
    config.auth_header = Some(token.to_string());
    let transport = StreamableHttpClientTransport::from_config(config);

    let client = ().serve(transport).await.expect("transport connect should succeed");
    // Trigger one more request (tools/list) to accumulate header samples.
    let _ = client.list_all_tools().await;

    let headers = captured.lock().unwrap().clone();
    assert!(
        !headers.is_empty(),
        "at least one request must have been captured"
    );

    let expected = format!("Bearer {token}");
    let all_authorized = headers
        .iter()
        .all(|h| h.as_deref() == Some(expected.as_str()));

    assert!(
        all_authorized,
        "every captured request must carry 'Authorization: Bearer {token}', got: {headers:?}"
    );

    let _ = client.cancel().await;
    ct.cancel();
}

/// No token: when no bearer_token is configured the Authorization header must
/// be absent from every request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_no_token_omits_auth_header() {
    let (url, ct, captured) = spawn_echo_server().await;

    let config = StreamableHttpClientTransportConfig::with_uri(url);
    let transport = StreamableHttpClientTransport::from_config(config);

    let client = ().serve(transport).await.expect("transport connect should succeed");
    let _ = client.list_all_tools().await;

    let headers = captured.lock().unwrap().clone();
    assert!(
        !headers.is_empty(),
        "at least one request must have been captured"
    );

    let any_auth = headers.iter().any(|h| h.is_some());
    assert!(
        !any_auth,
        "no Authorization header expected when bearer_token is None, got: {headers:?}"
    );

    let _ = client.cancel().await;
    ct.cancel();
}

// ── Layer 2: McpClient SSRF guard ────────────────────────────────────────────
//
// McpClient::connect() must reject loopback/private URLs before opening
// any socket, even when a real server is listening there.

fn loopback_config(port: u16, bearer: Option<String>) -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Http {
            url: format!("http://127.0.0.1:{port}/mcp"),
            bearer_token: bearer,
            headers: Default::default(),
        },
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        enabled_tools: None,
        disabled_tools: None,
    }
}

/// McpClient::connect() must fail with an SSRF error for loopback URLs,
/// even when a real MCP server is listening on that port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_client_connect_rejects_loopback_ssrf() {
    let (url, ct, _) = spawn_echo_server().await;
    // Extract the port from the URL to build a loopback config.
    let port: u16 = url
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut client = McpClient::new("test".into(), loopback_config(port, None));
    let result = client.connect().await;

    assert!(
        result.is_err(),
        "connect to loopback must be rejected by SSRF guard"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("not allowed") || msg.contains("SSRF") || msg.contains("blocked"),
        "error should mention SSRF or 'not allowed', got: {msg}"
    );
    assert_eq!(
        client.status(),
        McpClientStatus::Failed,
        "status must be Failed after SSRF rejection"
    );

    ct.cancel();
}

/// Connecting to a metadata endpoint must also be rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_client_connect_rejects_metadata_endpoint() {
    let config = McpServerConfig {
        transport: McpTransport::Http {
            url: "http://169.254.169.254/latest/meta-data".into(),
            bearer_token: None,
            headers: Default::default(),
        },
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        enabled_tools: None,
        disabled_tools: None,
    };
    let mut client = McpClient::new("test".into(), config);
    let result = client.connect().await;
    assert!(result.is_err(), "metadata endpoint must be rejected");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("not allowed") || msg.contains("SSRF") || msg.contains("blocked"),
        "expected SSRF rejection, got: {msg}"
    );
}
