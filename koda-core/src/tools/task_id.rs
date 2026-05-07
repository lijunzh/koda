//! Background-task ID parsing.
//!
//! Pre-#1325 Phase 5b this module was `bg_task_tools.rs` and housed
//! the `ListBackgroundTasks` / `CancelTask` / `WaitTask` LLM tool
//! implementations. Phase 5b retired those tools in favor of
//! `SpawnAgent` (peer-spawn) + `WaitForMail` (mailbox bridge from
//! #1336), so the dispatch surface is gone — but the TUI still needs
//! to parse user-typed task IDs for slash commands like `/cancel 5`,
//! and the underlying `ChildAgentRegistry` / `BgRegistry` still issue
//! them. That parsing is the only thing left here.
//!
//! See `koda-cli/src/tui_bg_tasks.rs` for the consumer.

/// Discriminant for what `parse_task_id` resolves a string to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskId {
    /// Bg-agent task — `agent:N`.
    Agent(u32),
    /// Bg shell process — `process:N`.
    Process(u32),
}

/// Parse a model-supplied or user-typed `task_id` string.
///
/// Accepts:
/// - `"agent:N"` → [`TaskId::Agent`]
/// - `"process:N"` → [`TaskId::Process`]
/// - `"N"` (bare numeric) → [`TaskId::Agent`] for back-compat with the
///   `/cancel <id>` UX shipped in #1042. The original LLM tool
///   descriptions *required* the prefix; this parser tolerates the
///   bare form so the TUI can share the same logic without diverging.
///   Post-#1325 Phase 5b only the TUI calls this — the LLM no longer
///   has a `CancelTask` tool — so the back-compat branch exists purely
///   for the slash command.
///
/// Returns an `Err(message)` suitable for surfacing in the TUI when
/// the input is malformed.
pub fn parse_task_id(input: &str) -> Result<TaskId, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("task_id is empty".to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("agent:") {
        return rest
            .parse::<u32>()
            .map(TaskId::Agent)
            .map_err(|_| format!("invalid agent id: '{rest}' (expected non-negative integer)"));
    }
    if let Some(rest) = trimmed.strip_prefix("process:") {
        return rest
            .parse::<u32>()
            .map(TaskId::Process)
            .map_err(|_| format!("invalid process id: '{rest}' (expected non-negative integer)"));
    }
    // Bare numeric → agent (TUI back-compat, see doc above).
    if let Ok(n) = trimmed.parse::<u32>() {
        return Ok(TaskId::Agent(n));
    }
    Err(format!(
        "unrecognized task_id '{input}'; expected \"agent:N\" or \"process:N\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id_accepts_prefixed_forms() {
        assert_eq!(parse_task_id("agent:7").unwrap(), TaskId::Agent(7));
        assert_eq!(
            parse_task_id("process:12345").unwrap(),
            TaskId::Process(12345)
        );
        // Whitespace tolerance — models sometimes add it.
        assert_eq!(parse_task_id("  agent:1  ").unwrap(), TaskId::Agent(1));
    }

    #[test]
    fn parse_task_id_accepts_bare_numeric_as_agent() {
        // TUI back-compat: `/cancel 5` → agent:5.
        assert_eq!(parse_task_id("5").unwrap(), TaskId::Agent(5));
    }

    #[test]
    fn parse_task_id_rejects_bad_input() {
        assert!(parse_task_id("").is_err());
        assert!(parse_task_id("   ").is_err());
        assert!(parse_task_id("agent:").is_err());
        assert!(parse_task_id("agent:abc").is_err());
        assert!(parse_task_id("process:-1").is_err());
        assert!(parse_task_id("foobar").is_err());
        assert!(parse_task_id("mcp:1").is_err()); // future prefix not yet wired
    }
}
