//! koda-ast: Tree-sitter AST analysis library.
//!
//! Provides file structure analysis and call graph extraction
//! for Rust, Python, JavaScript, TypeScript, Go, Java, C/C++, and Bash.
//!
//! This is the library crate. For the MCP server binary, see `main.rs`.

pub mod ast;

use std::path::{Path, PathBuf};

/// Re-export core analysis functions at the crate root for convenience.
pub use ast::{analyze_file, get_call_graph};

/// Tool definition metadata for consumers (koda-cli, capability_registry).
///
/// This is the single source of truth for the AstAnalysis tool schema.
/// Both the MCP wrapper (`main.rs`) and direct integrations use this.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_json: &'static str,
}

/// Returns tool definitions exported by this crate.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "AstAnalysis",
        description: "Read-only AST code analysis. Use 'analyze_file' for functions/classes/structs \
             summary, or 'get_call_graph' with a symbol name to find callers and callees. \
             Supports .rs, .py, .pyi, .pyw, .js, .jsx, .mjs, .cjs, .ts, .mts, .cts, .tsx, \
             .go, .java, .c, .h, .cpp, .cc, .cxx, .hpp, .hh, .sh, .bash files.",
        parameters_json: r#"{"type":"object","properties":{"action":{"type":"string","description":"'analyze_file' or 'get_call_graph'"},"file_path":{"type":"string","description":"Path to file"},"symbol":{"type":"string","description":"Symbol for get_call_graph"}},"required":["action","file_path"]}"#,
    }]
}

/// Execute an AST analysis action, returning the result as a string.
///
/// This is the unified entry point for both MCP and direct library usage.
/// Handles path resolution relative to `project_root`, action dispatch,
/// and error formatting.
pub fn execute(
    project_root: &Path,
    action: &str,
    file_path: &str,
    symbol: Option<&str>,
) -> Result<String, String> {
    let path = if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        project_root.join(file_path)
    };

    if !path.exists() {
        return Err(format!("File not found: {file_path}"));
    }

    match action {
        "analyze_file" => analyze_file(&path).map_err(|e| e.to_string()),
        "get_call_graph" => {
            let sym = symbol.unwrap_or("");
            if sym.is_empty() {
                return Err("'symbol' is required for get_call_graph".to_string());
            }
            get_call_graph(&path, sym).map_err(|e| e.to_string())
        }
        other => Err(format!(
            "Unknown action '{other}'. Use 'analyze_file' or 'get_call_graph'."
        )),
    }
}
