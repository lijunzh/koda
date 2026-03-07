//! AST analysis logic — tree-sitter parsing, call graph extraction.
//!
//! Pure functions with no MCP dependency. The MCP server in main.rs
//! wraps these with the MCP protocol.

use anyhow::Result;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use std::path::Path;

/// Analyze a file's structure — functions, classes, structs.
pub fn analyze_file(file_path: &Path) -> Result<String> {
    let source_code = std::fs::read_to_string(file_path)?;
    let extension = file_path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut parser = tree_sitter::Parser::new();
    let language = get_language(extension)?;
    parser
        .set_language(&language)
        .map_err(|e| anyhow::anyhow!("Language init: {e}"))?;
    let tree = parser
        .parse(&source_code, None)
        .ok_or_else(|| anyhow::anyhow!("Parse failed"))?;

    analyze_file_structure(&tree, source_code.as_bytes(), extension)
}

/// Get call graph for a symbol in a file.
pub fn get_call_graph(file_path: &Path, symbol: &str) -> Result<String> {
    let source_code = std::fs::read_to_string(file_path)?;
    let extension = file_path.extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut parser = tree_sitter::Parser::new();
    let language = get_language(extension)?;
    parser
        .set_language(&language)
        .map_err(|e| anyhow::anyhow!("Language init: {e}"))?;
    let tree = parser
        .parse(&source_code, None)
        .ok_or_else(|| anyhow::anyhow!("Parse failed"))?;

    let (callers, callees) = build_call_graph(&tree, source_code.as_bytes(), extension, symbol);
    format_graph_output(symbol, callers, callees)
}

/// Language-specific AST node type names for structure analysis.
struct LangNodeTypes {
    func_type: &'static str,
    class_type: &'static str,
    func_name_field: &'static str,
    class_name_field: &'static str,
}

impl LangNodeTypes {
    fn for_extension(ext: &str) -> Self {
        match ext {
            "rs" => Self::new("function_item", "struct_item", "name", "name"),
            "py" | "pyi" | "pyw" => {
                Self::new("function_definition", "class_definition", "name", "name")
            }
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "tsx" => {
                Self::new("function_declaration", "class_declaration", "name", "name")
            }
            "go" => Self::new("function_declaration", "type_declaration", "name", "name"),
            "java" => Self::new("method_declaration", "class_declaration", "name", "name"),
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => Self::new(
                "function_definition",
                "struct_specifier",
                "declarator",
                "name",
            ),
            "sh" | "bash" => Self::new("function_definition", "", "name", "name"),
            _ => Self::new("", "", "", ""),
        }
    }

    fn new(
        func_type: &'static str,
        class_type: &'static str,
        func_name_field: &'static str,
        class_name_field: &'static str,
    ) -> Self {
        Self {
            func_type,
            class_type,
            func_name_field,
            class_name_field,
        }
    }
}

fn get_language(extension: &str) -> Result<tree_sitter::Language> {
    match extension {
        "rs" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" | "pyw" => Ok(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "mts" | "cts" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Ok(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => Ok(tree_sitter_go::LANGUAGE.into()),
        "java" => Ok(tree_sitter_java::LANGUAGE.into()),
        "c" | "h" => Ok(tree_sitter_c::LANGUAGE.into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => Ok(tree_sitter_cpp::LANGUAGE.into()),
        "sh" | "bash" => Ok(tree_sitter_bash::LANGUAGE.into()),
        _ => Err(anyhow::anyhow!(
            "Unsupported file type '.{extension}'. Supports: .rs, .py, .pyi, .pyw, \
             .js, .jsx, .mjs, .cjs, .ts, .mts, .cts, .tsx, .go, .java, \
             .c, .h, .cpp, .cc, .cxx, .hpp, .hh, .sh, .bash"
        )),
    }
}

fn analyze_file_structure(
    tree: &tree_sitter::Tree,
    source: &[u8],
    extension: &str,
) -> Result<String> {
    let mut output = String::from("### AST Structure Summary\n\n");
    let mut cursor = tree.root_node().walk();

    let types = LangNodeTypes::for_extension(extension);
    let mut funcs = Vec::new();
    let mut classes = Vec::new();

    traverse_structure(&mut cursor, source, &types, &mut funcs, &mut classes);

    if !classes.is_empty() {
        output.push_str("**Classes / Structs:**\n");
        for c in classes {
            output.push_str(&format!("{c}\n"));
        }
        output.push('\n');
    }
    if !funcs.is_empty() {
        output.push_str("**Functions:**\n");
        for f in funcs {
            output.push_str(&format!("{f}\n"));
        }
    }
    if output.len() < 50 {
        output.push_str("No major structures found.");
    }
    Ok(output)
}

fn traverse_structure(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    types: &LangNodeTypes,
    funcs: &mut Vec<String>,
    classes: &mut Vec<String>,
) {
    let node = cursor.node();
    let kind = node.kind();

    if kind == types.func_type {
        if let Some(name_node) = node.child_by_field_name(types.func_name_field)
            && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            // For C/C++ declarator, strip parameter list
            let clean_name = name.split('(').next().unwrap_or(name).trim();
            funcs.push(format!(
                "- `{clean_name}` (Line {})",
                name_node.start_position().row + 1
            ));
        }
    } else if kind == types.class_type
        && let Some(name_node) = node.child_by_field_name(types.class_name_field)
        && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
    {
        classes.push(format!(
            "- `{name}` (Line {})",
            name_node.start_position().row + 1
        ));
    }

    if cursor.goto_first_child() {
        traverse_structure(cursor, source, types, funcs, classes);
        while cursor.goto_next_sibling() {
            traverse_structure(cursor, source, types, funcs, classes);
        }
        cursor.goto_parent();
    }
}

fn build_call_graph(
    tree: &tree_sitter::Tree,
    source: &[u8],
    extension: &str,
    target_symbol: &str,
) -> (Vec<String>, Vec<String>) {
    let mut graph = DiGraph::<String, ()>::new();
    let mut node_indices: HashMap<String, NodeIndex> = HashMap::new();

    let mut cursor = tree.root_node().walk();
    walk_call_graph(
        &mut cursor,
        source,
        extension,
        None,
        &mut graph,
        &mut node_indices,
    );

    let symbol_idx = graph.node_indices().find(|i| graph[*i] == target_symbol);
    match symbol_idx {
        Some(idx) => {
            let mut callers: Vec<String> = graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .map(|i| graph[i].clone())
                .collect();
            let mut callees: Vec<String> = graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .map(|i| graph[i].clone())
                .collect();
            callers.sort();
            callers.dedup();
            callees.sort();
            callees.dedup();
            (callers, callees)
        }
        None => (vec![], vec![]),
    }
}

fn walk_call_graph(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    extension: &str,
    current_caller: Option<String>,
    graph: &mut DiGraph<String, ()>,
    node_indices: &mut HashMap<String, NodeIndex>,
) {
    let node = cursor.node();
    let kind = node.kind();
    let mut next_caller = current_caller.clone();

    // Detect function definitions
    let is_func = match extension {
        "rs" => kind == "function_item",
        "py" | "pyi" | "pyw" => kind == "function_definition",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "tsx" => {
            kind == "function_declaration"
        }
        "go" => kind == "function_declaration",
        "java" => kind == "method_declaration",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => kind == "function_definition",
        "sh" | "bash" => kind == "function_definition",
        _ => false,
    };

    if is_func
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
    {
        let name_str = name.to_string();
        next_caller = Some(name_str.clone());
        node_indices
            .entry(name_str.clone())
            .or_insert_with(|| graph.add_node(name_str));
    }

    // For C/C++ the function name is in the "declarator" field
    if is_func
        && matches!(extension, "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh")
        && let Some(decl_node) = node.child_by_field_name("declarator")
        && let Ok(name) = std::str::from_utf8(&source[decl_node.byte_range()])
    {
        // Strip parameter list if present (e.g. "main(int argc, ...)" -> "main")
        let name_str = name.split('(').next().unwrap_or(name).trim().to_string();
        if !name_str.is_empty() {
            next_caller = Some(name_str.clone());
            node_indices
                .entry(name_str.clone())
                .or_insert_with(|| graph.add_node(name_str));
        }
    }

    // Detect function calls
    let is_call = match extension {
        "rs" => kind == "call_expression",
        "py" | "pyi" | "pyw" => kind == "call",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "tsx" => kind == "call_expression",
        "go" => kind == "call_expression",
        "java" => kind == "method_invocation",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => kind == "call_expression",
        "sh" | "bash" => kind == "command",
        _ => false,
    };

    if let Some(caller) = &current_caller
        && is_call
        && let Some(func_node) = node.child_by_field_name("function")
        && let Ok(call_text) = std::str::from_utf8(&source[func_node.byte_range()])
    {
        let called_name = call_text
            .rsplit("::")
            .next()
            .unwrap_or(call_text)
            .rsplit('.')
            .next()
            .unwrap_or(call_text)
            .trim();
        let caller_idx = *node_indices
            .entry(caller.clone())
            .or_insert_with(|| graph.add_node(caller.clone()));
        let callee_idx = *node_indices
            .entry(called_name.to_string())
            .or_insert_with(|| graph.add_node(called_name.to_string()));
        graph.add_edge(caller_idx, callee_idx, ());
    }

    if cursor.goto_first_child() {
        walk_call_graph(
            cursor,
            source,
            extension,
            next_caller.clone(),
            graph,
            node_indices,
        );
        while cursor.goto_next_sibling() {
            walk_call_graph(
                cursor,
                source,
                extension,
                next_caller.clone(),
                graph,
                node_indices,
            );
        }
        cursor.goto_parent();
    }
}

fn format_graph_output(
    target_symbol: &str,
    callers: Vec<String>,
    callees: Vec<String>,
) -> Result<String> {
    let mut output = format!("### Call Graph for `{target_symbol}`\n\n");
    output.push_str("**Called by (Callers):**\n");
    if callers.is_empty() {
        output.push_str("- *No callers found in this file*\n");
    } else {
        for c in &callers {
            output.push_str(&format!("- `{c}`\n"));
        }
    }
    output.push_str("\n**Calls (Callees):**\n");
    if callees.is_empty() {
        output.push_str("- *No outgoing calls found*\n");
    } else {
        for c in &callees {
            output.push_str(&format!("- `{c}`\n"));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_analyze_rust_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        write!(tmp, "fn main() {{}}\nfn helper() {{}}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("main"));
        assert!(result.contains("helper"));
    }

    #[test]
    fn test_analyze_python_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        write!(tmp, "def hello():\n    pass\ndef world():\n    pass").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_unsupported_extension() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".xyz").unwrap();
        write!(tmp, "some random content").unwrap();
        let result = analyze_file(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_call_graph() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        write!(tmp, "fn main() {{ helper(); }}\nfn helper() {{}}").unwrap();
        let result = get_call_graph(tmp.path(), "main").unwrap();
        assert!(
            result.contains("helper"),
            "Should find helper as callee: {result}"
        );
    }

    #[test]
    fn test_analyze_go_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".go").unwrap();
        write!(tmp, "package main\n\nfunc main() {{}}\nfunc helper() {{}}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("main"), "Should find main: {result}");
        assert!(result.contains("helper"), "Should find helper: {result}");
    }

    #[test]
    fn test_analyze_java_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".java").unwrap();
        write!(
            tmp,
            "public class Main {{\n  public void hello() {{}}\n  public void world() {{}}\n}}"
        )
        .unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("Main"), "Should find class Main: {result}");
        assert!(result.contains("hello"), "Should find hello: {result}");
        assert!(result.contains("world"), "Should find world: {result}");
    }

    #[test]
    fn test_analyze_c_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".c").unwrap();
        write!(tmp, "int main() {{ return 0; }}\nvoid helper() {{}}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("main"), "Should find main: {result}");
        assert!(result.contains("helper"), "Should find helper: {result}");
    }

    #[test]
    fn test_analyze_cpp_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".cpp").unwrap();
        write!(
            tmp,
            "struct Config {{\n  int x;\n}};\nint main() {{ return 0; }}"
        )
        .unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(
            result.contains("Config"),
            "Should find struct Config: {result}"
        );
        assert!(result.contains("main"), "Should find main: {result}");
    }

    #[test]
    fn test_analyze_bash_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".sh").unwrap();
        write!(tmp, "#!/bin/bash\nmy_func() {{\n  echo hello\n}}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("my_func"), "Should find my_func: {result}");
    }

    #[test]
    fn test_analyze_jsx_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".jsx").unwrap();
        write!(tmp, "function App() {{ return null; }}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("App"), "Should find App: {result}");
    }

    #[test]
    fn test_analyze_tsx_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".tsx").unwrap();
        write!(tmp, "function Widget() {{ return null; }}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(result.contains("Widget"), "Should find Widget: {result}");
    }

    #[test]
    fn test_analyze_pyi_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".pyi").unwrap();
        write!(tmp, "def stub_func() -> None: ...").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(
            result.contains("stub_func"),
            "Should find stub_func: {result}"
        );
    }

    #[test]
    fn test_analyze_mjs_file() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".mjs").unwrap();
        write!(tmp, "function esModule() {{}}").unwrap();
        let result = analyze_file(tmp.path()).unwrap();
        assert!(
            result.contains("esModule"),
            "Should find esModule: {result}"
        );
    }
}
