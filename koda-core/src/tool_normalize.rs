//! Tool name normalization — maps model-emitted variants to canonical PascalCase.
//!
//! Models (especially smaller/local ones via OpenAI-compatible APIs) sometimes
//! emit tool names in lowercase (`list`, `read`) or snake_case (`list_files`,
//! `read_file`) instead of the canonical PascalCase (`List`, `Read`). This
//! module provides a single normalization point at the API boundary so all
//! downstream code (dispatch, approval, loop guard, undo) sees canonical names.
//!
//! ## Design
//!
//! - Normalization is applied *once*, in `inference.rs`, after collecting
//!   the streamed response — before dispatch, approval, or persistence.
//! - Unknown names pass through unchanged so the dispatcher can surface
//!   a clear `Unknown tool` error.
//! - The alias map covers lowercase, snake_case, and camelCase variants.
//!
//! See: <https://github.com/lijunzh/koda/issues/548>
//!      <https://github.com/lijunzh/koda/issues/49>

use crate::providers::ToolCall;
use std::collections::HashMap;
use std::sync::LazyLock;

/// All canonical (PascalCase) built-in tool names.
const CANONICAL: &[&str] = &[
    "ActivateSkill",
    "AskUser",
    "Bash",
    "Delete",
    "Edit",
    "EmailRead",
    "EmailSearch",
    "EmailSend",
    "Glob",
    "Grep",
    "InvokeAgent",
    "List",
    "ListAgents",
    "ListSkills",
    "MemoryRead",
    "MemoryWrite",
    "Read",
    "RecallContext",
    "WebFetch",
    "WebSearch",
    "Write",
];

/// Static alias map: lowercased variant → canonical name.
///
/// Built once on first access.  Includes:
/// - self-mappings for every canonical name (lowercased key → itself)
/// - unambiguous snake_case alternatives   (`"list_files"` → `"List"`)
///
/// **Only unambiguous aliases are included.** If a name could plausibly
/// map to more than one tool (e.g. `"search"` → Grep or Glob?), it is
/// intentionally omitted — surfacing an `Unknown tool` error is better
/// than silently misrouting to the wrong tool.
static ALIASES: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();

    // Self-mappings: canonical names (lowercased) → themselves.
    // This lets normalize_tool_name() do a single O(1) lookup for
    // every path, including the fast-path where the name is already
    // canonical.
    for &name in CANONICAL {
        m.insert(name.to_lowercase(), name);
    }

    // ── Unambiguous snake_case / camelCase aliases ───────────────
    //
    // Only include aliases where the mapping is unambiguous.
    // If a short name could plausibly mean multiple tools, leave it
    // out — an "Unknown tool" error is better than silent misrouting.

    // AskUser
    m.insert("ask_user".into(), "AskUser");
    m.insert("ask_question".into(), "AskUser");
    m.insert("askquestion".into(), "AskUser");

    // File tools
    m.insert("list_files".into(), "List");
    m.insert("listfiles".into(), "List");
    m.insert("list_directory".into(), "List");
    m.insert("ls".into(), "List");

    m.insert("read_file".into(), "Read");
    m.insert("readfile".into(), "Read");
    m.insert("file_read".into(), "Read");

    m.insert("write_file".into(), "Write");
    m.insert("writefile".into(), "Write");
    m.insert("create_file".into(), "Write");
    m.insert("file_write".into(), "Write");

    m.insert("edit_file".into(), "Edit");
    m.insert("editfile".into(), "Edit");
    m.insert("file_edit".into(), "Edit");

    m.insert("delete_file".into(), "Delete");
    m.insert("deletefile".into(), "Delete");
    m.insert("remove_file".into(), "Delete");
    m.insert("rm".into(), "Delete");

    // Search tools
    m.insert("grep_search".into(), "Grep");
    m.insert("ripgrep".into(), "Grep");
    m.insert("rg".into(), "Grep");

    m.insert("glob_search".into(), "Glob");
    m.insert("glob_pattern".into(), "Glob");

    // Shell — only unambiguous aliases
    m.insert("shell".into(), "Bash");
    m.insert("run_command".into(), "Bash");
    m.insert("run_shell_command".into(), "Bash");

    // Web
    m.insert("web_fetch".into(), "WebFetch");
    m.insert("http_get".into(), "WebFetch");
    m.insert("curl".into(), "WebFetch");
    m.insert("web_search".into(), "WebSearch");
    m.insert("search_web".into(), "WebSearch");

    // Memory
    m.insert("memory_read".into(), "MemoryRead");
    m.insert("memory_write".into(), "MemoryWrite");

    // Agent tools
    m.insert("list_agents".into(), "ListAgents");
    m.insert("invoke_agent".into(), "InvokeAgent");

    // Skill tools
    m.insert("list_skills".into(), "ListSkills");
    m.insert("activate_skill".into(), "ActivateSkill");

    // Email
    m.insert("email_read".into(), "EmailRead");
    m.insert("email_send".into(), "EmailSend");
    m.insert("email_search".into(), "EmailSearch");

    // Recall
    m.insert("recall_context".into(), "RecallContext");
    m.insert("recall".into(), "RecallContext");

    m
});

/// Normalize a single tool name to its canonical PascalCase form.
///
/// Returns the canonical name if a mapping exists, otherwise returns
/// the input unchanged (so the dispatcher can surface a proper error).
pub fn normalize_tool_name(name: &str) -> String {
    // Single O(1) lookup: lowercase the input and check the alias map.
    // Canonical names are self-mapped (e.g. "list" → "List"), so this
    // handles both the fast-path and the alias-path in one operation.
    let lower = name.to_lowercase();
    if let Some(&canonical) = ALIASES.get(&lower) {
        return canonical.to_string();
    }

    // Unknown — pass through for the dispatcher to handle
    name.to_string()
}

/// Normalize all tool calls in a batch.
///
/// Called once after `collect_stream()` returns, before dispatch/approval.
pub fn normalize_tool_calls(tool_calls: Vec<ToolCall>) -> Vec<ToolCall> {
    tool_calls
        .into_iter()
        .map(|mut tc| {
            tc.function_name = normalize_tool_name(&tc.function_name);
            tc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Canonical names pass through unchanged ──────────────────

    #[test]
    fn canonical_names_unchanged() {
        for &name in CANONICAL {
            assert_eq!(normalize_tool_name(name), name);
        }
    }

    // ── Lowercase variants ──────────────────────────────────────

    #[test]
    fn lowercase_variants() {
        assert_eq!(normalize_tool_name("list"), "List");
        assert_eq!(normalize_tool_name("read"), "Read");
        assert_eq!(normalize_tool_name("write"), "Write");
        assert_eq!(normalize_tool_name("edit"), "Edit");
        assert_eq!(normalize_tool_name("delete"), "Delete");
        assert_eq!(normalize_tool_name("bash"), "Bash");
        assert_eq!(normalize_tool_name("grep"), "Grep");
        assert_eq!(normalize_tool_name("glob"), "Glob");
        assert_eq!(normalize_tool_name("webfetch"), "WebFetch");
    }

    // ── Snake_case variants ─────────────────────────────────────

    #[test]
    fn snake_case_variants() {
        assert_eq!(normalize_tool_name("list_files"), "List");
        assert_eq!(normalize_tool_name("read_file"), "Read");
        assert_eq!(normalize_tool_name("write_file"), "Write");
        assert_eq!(normalize_tool_name("edit_file"), "Edit");
        assert_eq!(normalize_tool_name("delete_file"), "Delete");
        assert_eq!(normalize_tool_name("run_shell_command"), "Bash");
        assert_eq!(normalize_tool_name("grep_search"), "Grep");
        assert_eq!(normalize_tool_name("glob_search"), "Glob");
        assert_eq!(normalize_tool_name("web_fetch"), "WebFetch");
        assert_eq!(normalize_tool_name("list_agents"), "ListAgents");
        assert_eq!(normalize_tool_name("invoke_agent"), "InvokeAgent");
        assert_eq!(normalize_tool_name("list_skills"), "ListSkills");
        assert_eq!(normalize_tool_name("activate_skill"), "ActivateSkill");
        assert_eq!(normalize_tool_name("email_read"), "EmailRead");
        assert_eq!(normalize_tool_name("email_send"), "EmailSend");
        assert_eq!(normalize_tool_name("email_search"), "EmailSearch");
        assert_eq!(normalize_tool_name("memory_read"), "MemoryRead");
        assert_eq!(normalize_tool_name("memory_write"), "MemoryWrite");
        assert_eq!(normalize_tool_name("recall_context"), "RecallContext");
    }

    // ── Short aliases (model hallucinations) ────────────────────

    #[test]
    fn short_aliases() {
        assert_eq!(normalize_tool_name("ls"), "List");
        assert_eq!(normalize_tool_name("rm"), "Delete");
        assert_eq!(normalize_tool_name("rg"), "Grep");
        assert_eq!(normalize_tool_name("shell"), "Bash");
        assert_eq!(normalize_tool_name("curl"), "WebFetch");
        assert_eq!(normalize_tool_name("recall"), "RecallContext");
    }

    // ── Ambiguous names are NOT mapped (silent misrouting prevention) ──

    #[test]
    fn ambiguous_names_not_mapped() {
        // These could plausibly map to multiple tools.
        // Better to surface "Unknown tool" than silently misroute.
        for name in [
            "search",
            "execute",
            "exec",
            "patch",
            "terminal",
            "find_files",
            "fetch",
        ] {
            let result = normalize_tool_name(name);
            assert_eq!(
                result, name,
                "'{name}' should NOT be mapped — it's ambiguous"
            );
        }
    }

    // ── Case insensitivity ──────────────────────────────────────

    #[test]
    fn mixed_case_normalized() {
        assert_eq!(normalize_tool_name("LIST"), "List");
        assert_eq!(normalize_tool_name("List"), "List");
        assert_eq!(normalize_tool_name("lIsT"), "List");
        assert_eq!(normalize_tool_name("READ"), "Read");
        assert_eq!(normalize_tool_name("BASH"), "Bash");
        assert_eq!(normalize_tool_name("LIST_FILES"), "List");
        assert_eq!(normalize_tool_name("Read_File"), "Read");
    }

    // ── Unknown names pass through ──────────────────────────────

    #[test]
    fn unknown_names_pass_through() {
        assert_eq!(normalize_tool_name("FooBar"), "FooBar");
        assert_eq!(normalize_tool_name("totally_unknown"), "totally_unknown");
        assert_eq!(normalize_tool_name(""), "");
    }

    // ── Batch normalization ─────────────────────────────────────

    #[test]
    fn normalize_batch() {
        let calls = vec![
            ToolCall {
                id: "1".into(),
                function_name: "list".into(),
                arguments: "{}".into(),
                thought_signature: None,
            },
            ToolCall {
                id: "2".into(),
                function_name: "read_file".into(),
                arguments: r#"{"path":"x"}"#.into(),
                thought_signature: None,
            },
            ToolCall {
                id: "3".into(),
                function_name: "Read".into(), // already canonical
                arguments: r#"{"path":"y"}"#.into(),
                thought_signature: None,
            },
        ];
        let normalized = normalize_tool_calls(calls);
        assert_eq!(normalized[0].function_name, "List");
        assert_eq!(normalized[1].function_name, "Read");
        assert_eq!(normalized[2].function_name, "Read");
    }

    // ── Every canonical name has a lowercase alias ──────────────

    #[test]
    fn all_canonical_names_have_lowercase_alias() {
        for &name in CANONICAL {
            let lower = name.to_lowercase();
            assert_eq!(
                normalize_tool_name(&lower),
                name,
                "Missing lowercase alias for '{name}'"
            );
        }
    }

    // ── Every alias target must be a canonical tool name ────────

    #[test]
    fn all_alias_targets_are_canonical() {
        let canonical_set: std::collections::HashSet<&str> = CANONICAL.iter().copied().collect();
        for (alias, &target) in ALIASES.iter() {
            assert!(
                canonical_set.contains(target),
                "Alias '{alias}' maps to '{target}' which is not in CANONICAL"
            );
        }
    }
}
