use anyhow::Result;
use petgraph::graph::{DiGraph, NodeIndex};
use serde_json::Value;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use tokio::sync::OnceCell;

use super::safe_resolve_path;
use crate::providers::ToolDefinition;

static AST_DB: OnceCell<SqlitePool> = OnceCell::const_new();

async fn get_ast_db() -> Result<&'static SqlitePool> {
    AST_DB
        .get_or_try_init(|| async {
            let db_dir = crate::db::db_dir()?;
            let db_path = db_dir.join("ast.db");

            // Ensure directory exists
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(db_path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .connect_with(options)
                .await?;

            sqlx::query(
                "CREATE TABLE IF NOT EXISTS ast_files (
                    file_id TEXT PRIMARY KEY,
                    file_hash TEXT NOT NULL,
                    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
                );
                CREATE TABLE IF NOT EXISTS ast_nodes (
                    id TEXT PRIMARY KEY,
                    file_id TEXT NOT NULL,
                    symbol_name TEXT NOT NULL,
                    FOREIGN KEY(file_id) REFERENCES ast_files(file_id) ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS ast_edges (
                    source_id TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    relation TEXT NOT NULL,
                    PRIMARY KEY(source_id, target_id, relation),
                    FOREIGN KEY(source_id) REFERENCES ast_nodes(id) ON DELETE CASCADE,
                    FOREIGN KEY(target_id) REFERENCES ast_nodes(id) ON DELETE CASCADE
                );",
            )
            .execute(&pool)
            .await?;

            Ok(pool)
        })
        .await
}

fn calculate_hash<T: Hash>(t: &T) -> String {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    format!("{:x}", s.finish())
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "AstAnalysis".to_string(),
        description: "Read-only AST code analysis. Supports Rust (.rs), Python (.py), JavaScript (.js), TypeScript (.ts). Use 'analyze_file' to get a structural summary (functions, classes, imports). Use 'get_call_graph' with a symbol name to find callers and callees. Results are cached in ast.db for performance. No files are modified.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: 'analyze_file' or 'get_call_graph'",
                    "enum": ["analyze_file", "get_call_graph"]
                },
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to analyze (e.g., src/main.rs)"
                },
                "symbol": {
                    "type": "string",
                    "description": "Target symbol (e.g., function or class name) required for get_call_graph"
                }
            },
            "required": ["action", "file_path"]
        }),
    }]
}

pub async fn ast_analysis(project_root: &Path, args: &Value) -> Result<String> {
    let action = args["action"].as_str().unwrap_or("").to_string();
    let file_path = args["file_path"].as_str().unwrap_or("");

    if action.is_empty() || file_path.is_empty() {
        return Ok("Error: action and file_path are required.".to_string());
    }

    let absolute_path = match safe_resolve_path(project_root, file_path) {
        Ok(path) => path,
        Err(_) => return Ok(format!("Error: Invalid file path '{}'", file_path)),
    };

    if !absolute_path.exists() {
        return Ok(format!("Error: File not found: {}", file_path));
    }

    let source_code = match fs::read_to_string(&absolute_path) {
        Ok(c) => c,
        Err(e) => return Ok(format!("Error: Failed to read file: {}", e)),
    };

    let db = get_ast_db().await?;
    let file_id = calculate_hash(&absolute_path.to_string_lossy());
    let source_hash = calculate_hash(&source_code);

    // TODO: We will use the db to check the cache here before parsing.
    // For now, we just pass the clippy check by executing a dummy query or proceeding.

    if action == "analyze_file" {
        // analyze_file is fast enough to run directly without caching for now
        let mut parser = tree_sitter::Parser::new();
        let extension = absolute_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let language = get_language(extension)?;
        parser
            .set_language(&language)
            .map_err(|e| anyhow::anyhow!("Lang init: {e}"))?;
        let tree = parser
            .parse(&source_code, None)
            .ok_or_else(|| anyhow::anyhow!("Parse failed"))?;

        return analyze_file_structure(&tree, source_code.as_bytes(), extension);
    } else if action == "get_call_graph" {
        let symbol = args["symbol"].as_str().unwrap_or("");
        if symbol.is_empty() {
            return Ok("Error: 'symbol' is required for get_call_graph".to_string());
        }

        // --- CACHE CHECK LOGIC ---
        let cached_file: Option<(String,)> =
            sqlx::query_as("SELECT file_hash FROM ast_files WHERE file_id = ?")
                .bind(&file_id)
                .fetch_optional(db)
                .await?;

        let is_fresh = cached_file
            .map(|(hash,)| hash == source_hash)
            .unwrap_or(false);

        if is_fresh {
            tracing::debug!("AST Cache hit for {}", file_path);
            return query_graph_from_db(db, &file_id, symbol).await;
        }

        tracing::debug!("AST Cache miss for {}", file_path);
        let extension = absolute_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let language = get_language(extension)?;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| anyhow::anyhow!("Lang init: {e}"))?;
        let tree = parser
            .parse(&source_code, None)
            .ok_or_else(|| anyhow::anyhow!("Parse failed"))?;

        return parse_and_cache_graph(
            db,
            &file_id,
            &source_hash,
            &tree,
            source_code.as_bytes(),
            extension,
            symbol,
        )
        .await;
    }

    Ok(format!("Error: Unknown action '{}'", action))
}

fn get_language(extension: &str) -> Result<tree_sitter::Language> {
    match extension {
        "rs" => Ok(tree_sitter_rust::LANGUAGE.into()),
        "py" => Ok(tree_sitter_python::LANGUAGE.into()),
        "js" => Ok(tree_sitter_javascript::LANGUAGE.into()),
        "ts" => Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        _ => Err(anyhow::anyhow!(
            "Unsupported file type '.{}'. AstAnalysis currently supports .rs, .py, .js, and .ts",
            extension
        )),
    }
}

async fn query_graph_from_db(db: &SqlitePool, file_id: &str, symbol: &str) -> Result<String> {
    let node: Option<(String,)> =
        sqlx::query_as("SELECT id FROM ast_nodes WHERE file_id = ? AND symbol_name = ?")
            .bind(file_id)
            .bind(symbol)
            .fetch_optional(db)
            .await?;

    let Some((target_id,)) = node else {
        return Ok(format!(
            "Symbol `{}` not found in the file's call graph. (It might not be defined or called here).",
            symbol
        ));
    };

    let callers: Vec<(String,)> = sqlx::query_as("SELECT n.symbol_name FROM ast_edges e JOIN ast_nodes n ON e.source_id = n.id WHERE e.target_id = ?")
        .bind(&target_id)
        .fetch_all(db)
        .await?;
    let callers = callers.into_iter().map(|(s,)| s).collect();

    let callees: Vec<(String,)> = sqlx::query_as("SELECT n.symbol_name FROM ast_edges e JOIN ast_nodes n ON e.target_id = n.id WHERE e.source_id = ?")
        .bind(&target_id)
        .fetch_all(db)
        .await?;
    let callees = callees.into_iter().map(|(s,)| s).collect();

    format_graph_output(symbol, callers, callees)
}

async fn parse_and_cache_graph(
    db: &SqlitePool,
    file_id: &str,
    source_hash: &str,
    tree: &tree_sitter::Tree,
    source: &[u8],
    extension: &str,
    target_symbol: &str,
) -> Result<String> {
    let mut graph = DiGraph::<String, ()>::new();
    let mut node_indices: HashMap<String, NodeIndex> = HashMap::new();

    fn walk(
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

        if ((extension == "rs" && kind == "function_item")
            || (extension == "py" && kind == "function_definition")
            || ((extension == "js" || extension == "ts") && kind == "function_declaration"))
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            let name_str = name.to_string();
            next_caller = Some(name_str.clone());
            node_indices
                .entry(name_str.clone())
                .or_insert_with(|| graph.add_node(name_str));
        }

        if let Some(caller) = &current_caller
            && ((extension == "rs" && kind == "call_expression")
                || (extension == "py" && kind == "call")
                || ((extension == "js" || extension == "ts") && kind == "call_expression"))
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
            walk(
                cursor,
                source,
                extension,
                next_caller.clone(),
                graph,
                node_indices,
            );
            while cursor.goto_next_sibling() {
                walk(
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

    let mut cursor = tree.root_node().walk();
    walk(
        &mut cursor,
        source,
        extension,
        None,
        &mut graph,
        &mut node_indices,
    );

    // --- SAVE TO SQLITE ---
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM ast_files WHERE file_id = ?")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO ast_files (file_id, file_hash) VALUES (?, ?)")
        .bind(file_id)
        .bind(source_hash)
        .execute(&mut *tx)
        .await?;

    for node_idx in graph.node_indices() {
        let symbol_name = &graph[node_idx];
        let node_id = format!("{}::{}", file_id, symbol_name);
        sqlx::query("INSERT INTO ast_nodes (id, file_id, symbol_name) VALUES (?, ?, ?)")
            .bind(&node_id)
            .bind(file_id)
            .bind(symbol_name)
            .execute(&mut *tx)
            .await?;
    }

    for edge in graph.edge_indices() {
        let (source_idx, target_idx) = graph.edge_endpoints(edge).unwrap();
        let source_name = &graph[source_idx];
        let target_name = &graph[target_idx];
        let source_id = format!("{}::{}", file_id, source_name);
        let target_id = format!("{}::{}", file_id, target_name);

        // Use INSERT OR IGNORE to prevent duplicate edge errors
        sqlx::query("INSERT OR IGNORE INTO ast_edges (source_id, target_id, relation) VALUES (?, ?, 'CALLS')")
            .bind(&source_id).bind(&target_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;

    let symbol_idx = graph.node_indices().find(|i| graph[*i] == target_symbol);
    if let Some(idx) = symbol_idx {
        let callers = graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .map(|i| graph[i].clone())
            .collect();
        let callees = graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing)
            .map(|i| graph[i].clone())
            .collect();
        format_graph_output(target_symbol, callers, callees)
    } else {
        Ok(format!(
            "Symbol `{}` not found in the file's call graph. (It might not be defined or called here).",
            target_symbol
        ))
    }
}

fn format_graph_output(
    target_symbol: &str,
    mut callers: Vec<String>,
    mut callees: Vec<String>,
) -> Result<String> {
    callers.sort();
    callers.dedup();
    callees.sort();
    callees.dedup();

    let mut output = format!("### Call Graph for `{}`\n\n", target_symbol);
    output.push_str("**Called by (Callers):**\n");
    if callers.is_empty() {
        output.push_str("- *No callers found in this file*\n");
    } else {
        for c in callers {
            output.push_str(&format!("- `{}`\n", c));
        }
    }

    output.push_str("\n**Calls (Callees):**\n");
    if callees.is_empty() {
        output.push_str("- *No outgoing calls found*\n");
    } else {
        for c in callees {
            output.push_str(&format!("- `{}`\n", c));
        }
    }

    Ok(output)
}

fn analyze_file_structure(
    tree: &tree_sitter::Tree,
    source: &[u8],
    extension: &str,
) -> Result<String> {
    let mut output = String::new();
    output.push_str("### AST Structure Summary\n\n");

    let root_node = tree.root_node();
    let mut cursor = root_node.walk();

    let (func_type, class_type, name_field) = match extension {
        "rs" => ("function_item", "struct_item", "name"),
        "py" => ("function_definition", "class_definition", "name"),
        "js" | "ts" => ("function_declaration", "class_declaration", "name"),
        _ => ("", "", ""),
    };

    let mut found_functions = Vec::new();
    let mut found_classes = Vec::new();

    fn traverse(
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

        if kind == func_type
            && let Some(name_node) = node.child_by_field_name(name_field)
            && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            funcs.push(format!(
                "- `{}` (Line {})",
                name,
                name_node.start_position().row + 1
            ));
        } else if kind == class_type
            && let Some(name_node) = node.child_by_field_name(name_field)
            && let Ok(name) = std::str::from_utf8(&source[name_node.byte_range()])
        {
            classes.push(format!(
                "- `{}` (Line {})",
                name,
                name_node.start_position().row + 1
            ));
        }

        if cursor.goto_first_child() {
            traverse(
                cursor, source, func_type, class_type, name_field, funcs, classes,
            );
            while cursor.goto_next_sibling() {
                traverse(
                    cursor, source, func_type, class_type, name_field, funcs, classes,
                );
            }
            cursor.goto_parent();
        }
    }

    traverse(
        &mut cursor,
        source,
        func_type,
        class_type,
        name_field,
        &mut found_functions,
        &mut found_classes,
    );

    if !found_classes.is_empty() {
        output.push_str("**Classes / Structs:**\n");
        for c in found_classes {
            output.push_str(&format!("{}\n", c));
        }
        output.push('\n');
    }

    if !found_functions.is_empty() {
        output.push_str("**Functions:**\n");
        for f in found_functions {
            output.push_str(&format!("{}\n", f));
        }
    }

    if output.len() < 50 {
        output.push_str("No major structures found.");
    }

    Ok(output)
}
