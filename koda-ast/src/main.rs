//! koda-ast: MCP server for tree-sitter AST analysis.
//!
//! Thin MCP wrapper around the `koda_ast` library crate.
//! Part of the koda ecosystem — auto-provisioned on first use.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Parameters for AstAnalysis tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AstAnalysisParams {
    /// Action: 'analyze_file' or 'get_call_graph'
    pub action: String,
    /// Path to the file to analyze (e.g., src/main.rs)
    pub file_path: String,
    /// Target symbol for get_call_graph (e.g., function name)
    #[serde(default)]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone)]
struct AstServer {
    cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl AstServer {
    fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            cwd,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AstServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("koda-ast", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "AST analysis server for Rust, Python, JavaScript, TypeScript, Go, Java, \
             C/C++, and Bash. Use AstAnalysis tool with action 'analyze_file' or \
             'get_call_graph'."
                .to_string(),
        );
        info
    }
}

#[tool_router]
impl AstServer {
    /// Read-only AST code analysis for Rust, Python, JavaScript, TypeScript.
    ///
    /// NOTE: The description below must stay in sync with
    /// `koda_ast::tool_definitions()` in lib.rs (the authoritative source).
    #[tool(
        name = "AstAnalysis",
        description = "Read-only AST code analysis. Use 'analyze_file' for functions/classes/structs summary, or 'get_call_graph' with a symbol name to find callers and callees. Supports .rs, .py, .pyi, .pyw, .js, .jsx, .mjs, .cjs, .ts, .mts, .cts, .tsx, .go, .java, .c, .h, .cpp, .cc, .cxx, .hpp, .hh, .sh, .bash files."
    )]
    async fn ast_analysis(
        &self,
        params: Parameters<AstAnalysisParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = &params.0;
        match koda_ast::execute(&self.cwd, &p.action, &p.file_path, p.symbol.as_deref()) {
            Ok(output) => Ok(CallToolResult::success(vec![Content::text(output)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {e}"
            ))])),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle --version flag
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("koda-ast {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("koda-ast MCP server starting...");

    let server = AstServer::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;

    service.waiting().await?;
    Ok(())
}
