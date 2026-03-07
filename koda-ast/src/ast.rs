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

fn get_language(extension: &str) -> Result<tree_sitter::Language> {
    match extension {
        "rs" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "py" => Ok(tree_sitter_python::LANGUAGE.into()),
        "js" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "ts" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        _ => Err(anyhow::anyhow!(
            "Unsupported file type '.{extension}'. Supports: .rs, .py, .js, .ts"
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

    let (func_type, class_type, name_field) = match extension {
        "rs" => ("function_item", "struct_item", "name"),
        "py" => ("function_definition", "class_definition", "name"),
        "js" | "ts" => ("function_declaration", "class_declaration", "name"),
        _ => ("", "", ""),
    };

    let mut funcs = Vec::new();
    let mut classes = Vec::new();

    traverse_structure(
        &mut cursor,
        source,
        func_type,
        class_type,
        name_field,
        &mut funcs,
        &mut classes,
    );

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
    func_type: &str,
    class_type: &str,
    name_field: &str,
    funcs: &mut Vec<String>,
    classes: &mut Vec<String>,
) {
    let node = cursor.node();
    let kind = node.kind();

    if kind == func_type {
        if let Some(name_node) = node.child_by_field_name(name_field)
            && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            funcs.push(format!(
                "- `{name}` (Line {})",
                name_node.start_position().row + 1
            ));
        }
    } else if kind == class_type
        && let Some(name_node) = node.child_by_field_name(name_field)
        && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
    {
        classes.push(format!(
            "- `{name}` (Line {})",
            name_node.start_position().row + 1
        ));
    }

    if cursor.goto_first_child() {
        traverse_structure(
            cursor, source, func_type, class_type, name_field, funcs, classes,
        );
        while cursor.goto_next_sibling() {
            traverse_structure(
                cursor, source, func_type, class_type, name_field, funcs, classes,
            );
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
    let is_func = (extension == "rs" && kind == "function_item")
        || (extension == "py" && kind == "function_definition")
        || ((extension == "js" || extension == "ts") && kind == "function_declaration");

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

    // Detect function calls
    let is_call = (extension == "rs" && kind == "call_expression")
        || (extension == "py" && kind == "call")
        || ((extension == "js" || extension == "ts") && kind == "call_expression");

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
        let mut tmp = tempfile::NamedTempFile::with_suffix(".go").unwrap();
        write!(tmp, "package main").unwrap();
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
}
