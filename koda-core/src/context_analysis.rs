//! Context analysis — per-tool token breakdown and duplicate detection.
//!
//! Analyzes conversation history to identify where tokens are being spent.
//! Used by compaction decisions, `/usage` reporting, and microcompact (future).
//!
//! Inspired by Claude Code's `contextAnalysis.ts`.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::inference_helpers::{CHARS_PER_TOKEN, PER_MESSAGE_OVERHEAD};
use crate::persistence::Message;

/// Per-tool and per-role token breakdown of a conversation context.
#[derive(Debug, Clone, Default)]
pub struct ContextAnalysis {
    /// Tokens in tool *request* blocks (assistant → tool_use), keyed by tool name.
    pub tool_request_tokens: HashMap<String, usize>,
    /// Tokens in tool *result* blocks (tool → result), keyed by tool name.
    pub tool_result_tokens: HashMap<String, usize>,
    /// Tokens in human/user messages (excluding tool results).
    pub human_tokens: usize,
    /// Tokens in assistant messages (excluding tool requests).
    pub assistant_tokens: usize,
    /// Files read more than once, with count and estimated wasted tokens.
    pub duplicate_reads: HashMap<PathBuf, DuplicateRead>,
    /// Total estimated tokens across all messages.
    pub total: usize,
}

/// Info about a duplicated file read in context.
#[derive(Debug, Clone)]
pub struct DuplicateRead {
    /// Number of times this file was read.
    pub count: usize,
    /// Estimated tokens wasted by redundant reads (all but the last).
    pub wasted_tokens: usize,
}

impl ContextAnalysis {
    /// Total tokens consumed by all tool results.
    pub fn total_tool_result_tokens(&self) -> usize {
        self.tool_result_tokens.values().sum()
    }

    /// Total tokens consumed by all tool requests.
    pub fn total_tool_request_tokens(&self) -> usize {
        self.tool_request_tokens.values().sum()
    }

    /// Total tokens wasted by duplicate file reads.
    pub fn total_duplicate_waste(&self) -> usize {
        self.duplicate_reads.values().map(|d| d.wasted_tokens).sum()
    }

    /// Percentage of total context consumed by tool results.
    pub fn tool_result_percent(&self) -> usize {
        if self.total == 0 {
            return 0;
        }
        (self.total_tool_result_tokens() * 100) / self.total
    }

    /// Percentage of total context consumed by duplicate reads.
    pub fn duplicate_read_percent(&self) -> usize {
        if self.total == 0 {
            return 0;
        }
        (self.total_duplicate_waste() * 100) / self.total
    }

    /// Top N tools by result token consumption, descending.
    pub fn top_tool_results(&self, n: usize) -> Vec<(&str, usize)> {
        let mut sorted: Vec<_> = self
            .tool_result_tokens
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(n);
        sorted
    }

    /// Format a human-readable summary for `/usage` or context warnings.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("Context: ~{} tokens", self.total));
        lines.push(format!(
            "  Human: {} | Assistant: {} | Tool results: {} ({}%)",
            self.human_tokens,
            self.assistant_tokens,
            self.total_tool_result_tokens(),
            self.tool_result_percent(),
        ));

        let top = self.top_tool_results(5);
        if !top.is_empty() {
            lines.push("  Top tool results:".to_string());
            for (name, tokens) in &top {
                let pct = if self.total > 0 {
                    (*tokens * 100) / self.total
                } else {
                    0
                };
                lines.push(format!("    {name}: ~{tokens} tokens ({pct}%)"));
            }
        }

        let waste = self.total_duplicate_waste();
        if waste > 0 {
            lines.push(format!(
                "  Duplicate reads: ~{waste} wasted tokens ({}%) across {} files",
                self.duplicate_read_percent(),
                self.duplicate_reads.len(),
            ));
        }

        lines.join("\n")
    }
}

/// Analyze conversation history and produce a token breakdown.
///
/// Walks the message list, classifying each message by role and extracting
/// tool names from `tool_calls` JSON and `tool_call_id` linkage.
pub fn analyze_context(messages: &[Message]) -> ContextAnalysis {
    let mut analysis = ContextAnalysis::default();

    // Phase 1: Build a map from tool_call_id → tool_name by scanning
    // assistant messages with tool_calls JSON.
    let mut id_to_tool: HashMap<String, String> = HashMap::new();
    // Track Read tool: tool_call_id → file_path (extracted from args).
    let mut read_tool_paths: HashMap<String, PathBuf> = HashMap::new();

    for msg in messages {
        if msg.role == crate::persistence::Role::Assistant
            && let Some(ref tc_json) = msg.tool_calls
        {
            extract_tool_call_ids(tc_json, &mut id_to_tool, &mut read_tool_paths);
        }
    }

    // Phase 2: Classify each message and accumulate tokens.
    // Also track per-file read stats for duplicate detection.
    let mut file_read_stats: HashMap<PathBuf, FileReadAccum> = HashMap::new();

    for msg in messages {
        let tokens = estimate_message_tokens(msg);
        analysis.total += tokens;

        match msg.role {
            crate::persistence::Role::User => {
                analysis.human_tokens += tokens;
            }
            crate::persistence::Role::Assistant => {
                if let Some(ref tc_json) = msg.tool_calls {
                    // Split tokens between text content and tool requests.
                    let text_tokens = msg.content.as_deref().map_or(0, estimate_str_tokens);
                    let tool_tokens = tokens.saturating_sub(text_tokens);
                    analysis.assistant_tokens += text_tokens;

                    // Attribute tool request tokens to each tool name.
                    distribute_tool_request_tokens(
                        tc_json,
                        tool_tokens,
                        &mut analysis.tool_request_tokens,
                    );
                } else {
                    analysis.assistant_tokens += tokens;
                }
            }
            crate::persistence::Role::Tool => {
                // Look up which tool produced this result.
                let tool_name = msg
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| id_to_tool.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                *analysis
                    .tool_result_tokens
                    .entry(tool_name.clone())
                    .or_default() += tokens;

                // Track file read stats for duplicate detection.
                if (tool_name == "Read" || tool_name == "read")
                    && let Some(path) = msg
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| read_tool_paths.get(id))
                {
                    let entry =
                        file_read_stats
                            .entry(path.clone())
                            .or_insert_with(|| FileReadAccum {
                                count: 0,
                                total_tokens: 0,
                            });
                    entry.count += 1;
                    entry.total_tokens += tokens;
                }
            }
            crate::persistence::Role::System => {
                // System prompt — count toward total but not any category.
            }
        }
    }

    // Phase 3: Compute duplicate read waste.
    for (path, accum) in file_read_stats {
        if accum.count > 1 {
            let avg_tokens = accum.total_tokens / accum.count;
            let wasted = avg_tokens * (accum.count - 1);
            analysis.duplicate_reads.insert(
                path,
                DuplicateRead {
                    count: accum.count,
                    wasted_tokens: wasted,
                },
            );
        }
    }

    analysis
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Accumulator for per-file read token stats.
struct FileReadAccum {
    count: usize,
    total_tokens: usize,
}

/// Estimate tokens for a single DB message row.
fn estimate_message_tokens(msg: &Message) -> usize {
    let content_len = msg.content.as_deref().map_or(0, |c| c.len());
    let tc_len = msg.tool_calls.as_deref().map_or(0, |c| c.len());
    ((content_len + tc_len) as f64 / CHARS_PER_TOKEN) as usize + PER_MESSAGE_OVERHEAD
}

/// Estimate tokens for a raw string.
fn estimate_str_tokens(s: &str) -> usize {
    (s.len() as f64 / CHARS_PER_TOKEN) as usize
}

/// Parse tool_calls JSON and populate id→name + read-path maps.
fn extract_tool_call_ids(
    tc_json: &str,
    id_to_tool: &mut HashMap<String, String>,
    read_paths: &mut HashMap<String, PathBuf>,
) {
    let calls: Vec<serde_json::Value> = match serde_json::from_str(tc_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    for call in &calls {
        let id = call.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let name = call
            .get("function_name")
            .or_else(|| call.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if !id.is_empty() {
            id_to_tool.insert(id.to_string(), name.to_string());
        }

        // Extract file path for Read tool calls.
        if (name == "Read" || name == "read")
            && let Some(args) = call.get("arguments")
        {
            // `arguments` may be a JSON string or an object.
            let args_obj: Option<serde_json::Value> = if let Some(s) = args.as_str() {
                serde_json::from_str(s).ok()
            } else {
                Some(args.clone())
            };
            if let Some(obj) = args_obj
                && let Some(path) = obj
                    .get("file_path")
                    .or_else(|| obj.get("path"))
                    .and_then(|v| v.as_str())
            {
                read_paths.insert(id.to_string(), PathBuf::from(path));
            }
        }
    }
}

/// Distribute tool_tokens proportionally across the tool calls in a JSON array.
fn distribute_tool_request_tokens(
    tc_json: &str,
    total_tool_tokens: usize,
    request_map: &mut HashMap<String, usize>,
) {
    let calls: Vec<serde_json::Value> = match serde_json::from_str(tc_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    if calls.is_empty() {
        return;
    }
    let per_call = total_tool_tokens / calls.len();
    for call in &calls {
        let name = call
            .get("function_name")
            .or_else(|| call.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *request_map.entry(name.to_string()).or_default() += per_call;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Message, Role};

    fn msg(
        role: Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role,
            content: content.map(String::from),
            tool_calls: tool_calls.map(String::from),
            tool_call_id: tool_call_id.map(String::from),
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
        }
    }

    #[test]
    fn test_empty_history() {
        let analysis = analyze_context(&[]);
        assert_eq!(analysis.total, 0);
        assert_eq!(analysis.human_tokens, 0);
        assert_eq!(analysis.assistant_tokens, 0);
        assert!(analysis.tool_result_tokens.is_empty());
        assert!(analysis.duplicate_reads.is_empty());
    }

    #[test]
    fn test_simple_conversation() {
        let messages = vec![
            msg(Role::User, Some("Hello world"), None, None),
            msg(Role::Assistant, Some("Hi there!"), None, None),
        ];
        let analysis = analyze_context(&messages);
        assert!(analysis.total > 0);
        assert!(analysis.human_tokens > 0);
        assert!(analysis.assistant_tokens > 0);
        assert_eq!(analysis.total_tool_result_tokens(), 0);
    }

    #[test]
    fn test_tool_call_attribution() {
        let tc_json =
            r#"[{"id":"tc_1","function_name":"Read","arguments":"{\"file_path\":\"foo.rs\"}"}]"#;
        let messages = vec![
            msg(Role::User, Some("Read foo.rs"), None, None),
            msg(Role::Assistant, None, Some(tc_json), None),
            msg(
                Role::Tool,
                Some("contents of foo.rs which is a pretty long file with lots of code"),
                None,
                Some("tc_1"),
            ),
        ];
        let analysis = analyze_context(&messages);
        assert!(analysis.tool_result_tokens.contains_key("Read"));
        assert!(*analysis.tool_result_tokens.get("Read").unwrap() > 0);
    }

    #[test]
    fn test_duplicate_read_detection() {
        let tc1 =
            r#"[{"id":"tc_1","function_name":"Read","arguments":"{\"file_path\":\"foo.rs\"}"}]"#;
        let tc2 =
            r#"[{"id":"tc_2","function_name":"Read","arguments":"{\"file_path\":\"foo.rs\"}"}]"#;
        let tc3 =
            r#"[{"id":"tc_3","function_name":"Read","arguments":"{\"file_path\":\"bar.rs\"}"}]"#;

        let messages = vec![
            msg(Role::User, Some("Read foo.rs"), None, None),
            msg(Role::Assistant, None, Some(tc1), None),
            msg(Role::Tool, Some("contents of foo"), None, Some("tc_1")),
            msg(Role::User, Some("Read it again"), None, None),
            msg(Role::Assistant, None, Some(tc2), None),
            msg(Role::Tool, Some("contents of foo"), None, Some("tc_2")),
            msg(Role::User, Some("Read bar.rs"), None, None),
            msg(Role::Assistant, None, Some(tc3), None),
            msg(Role::Tool, Some("contents of bar"), None, Some("tc_3")),
        ];

        let analysis = analyze_context(&messages);

        // foo.rs was read twice → should appear in duplicate_reads
        let foo_path = PathBuf::from("foo.rs");
        assert!(analysis.duplicate_reads.contains_key(&foo_path));
        assert_eq!(analysis.duplicate_reads[&foo_path].count, 2);
        assert!(analysis.duplicate_reads[&foo_path].wasted_tokens > 0);

        // bar.rs was read once → should NOT appear
        let bar_path = PathBuf::from("bar.rs");
        assert!(!analysis.duplicate_reads.contains_key(&bar_path));
    }

    #[test]
    fn test_top_tool_results() {
        let tc1 = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
        let tc2 = r#"[{"id":"tc_2","function_name":"Bash","arguments":"{}"}]"#;

        let long_content = "x".repeat(1000);
        let short_content = "y".repeat(100);

        let messages = vec![
            msg(Role::Assistant, None, Some(tc1), None),
            msg(Role::Tool, Some(&long_content), None, Some("tc_1")),
            msg(Role::Assistant, None, Some(tc2), None),
            msg(Role::Tool, Some(&short_content), None, Some("tc_2")),
        ];

        let analysis = analyze_context(&messages);
        let top = analysis.top_tool_results(5);
        assert!(!top.is_empty());
        // Read should be first (more tokens)
        assert_eq!(top[0].0, "Read");
    }

    #[test]
    fn test_summary_format() {
        let tc1 = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
        let messages = vec![
            msg(Role::User, Some("hello"), None, None),
            msg(Role::Assistant, Some("let me read"), Some(tc1), None),
            msg(Role::Tool, Some("file contents here"), None, Some("tc_1")),
        ];
        let analysis = analyze_context(&messages);
        let summary = analysis.summary();
        assert!(summary.contains("Context:"));
        assert!(summary.contains("Human:"));
        assert!(summary.contains("Tool results:"));
    }

    #[test]
    fn test_multiple_tool_calls_in_one_message() {
        let tc = r#"[
            {"id":"tc_1","function_name":"Read","arguments":"{}"},
            {"id":"tc_2","function_name":"Grep","arguments":"{}"}
        ]"#;
        let messages = vec![
            msg(Role::Assistant, None, Some(tc), None),
            msg(Role::Tool, Some("read result"), None, Some("tc_1")),
            msg(Role::Tool, Some("grep result"), None, Some("tc_2")),
        ];
        let analysis = analyze_context(&messages);
        assert!(analysis.tool_result_tokens.contains_key("Read"));
        assert!(analysis.tool_result_tokens.contains_key("Grep"));
    }
}
