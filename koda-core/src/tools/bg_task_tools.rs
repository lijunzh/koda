//! Background task management tools (Layer 2 of #996).
//!
//! Three LLM tools that let the model see, cancel, and wait for any
//! background work it has spawned — both background sub-agents
//! (`InvokeAgent { background: true }`) and background shell processes
//! (`Bash { background: true }`):
//!
//! | Tool | Purpose |
//! |------|---------|
//! | `ListBackgroundTasks` | Snapshot every running task (no args). |
//! | `CancelTask`          | Send cancel/SIGTERM by `task_id`. |
//! | `WaitTask`            | Block until a task finishes, with timeout. |
//!
//! ## ID format
//!
//! Task IDs are prefixed strings so the model can tell agent tasks and
//! shell processes apart at a glance:
//!
//! - `agent:N`   — bg-agent task (the `task_id` from
//!   [`crate::bg_agent::BgAgentRegistry::reserve`]).
//! - `process:N` — bg shell process (the OS PID from
//!   [`crate::tools::bg_process::BgRegistry::insert`]).
//!
//! Bare numeric IDs (`5`) are accepted by the TUI's `/cancel` and
//! resolve to `agent:5` for back-compat with #1042; the LLM tools
//! always require the prefix.
//!
//! ## Scope (Model E, see #996 discussion)
//!
//! Each tool is filtered to the caller's own tasks: top-level sees
//! only top-level-spawned, sub-agent N sees only its own. Cross-spawner
//! cancel/wait returns a `Forbidden` error. This is enforced at the
//! [`BgAgentRegistry`] / [`BgRegistry`] layer; the tool layer just
//! produces a useful message when it sees `CancelOutcome::Forbidden`
//! / `WaitOutcome::Forbidden`.
//!
//! [`BgAgentRegistry`]: crate::bg_agent::BgAgentRegistry
//! [`BgRegistry`]: crate::tools::bg_process::BgRegistry

use crate::providers::ToolDefinition;
use serde_json::json;

/// Maximum `timeout_secs` a `WaitTask` call may request. Higher values
/// are clamped down by the dispatch layer before reaching the registry.
///
/// Bounds the worst-case time the inference loop can be parked on a
/// single tool call. 300 s = 5 min is generous for "wait for a build
/// to finish" while still preventing a confused model from asking for
/// `timeout_secs: 86400`.
pub const WAIT_TASK_MAX_TIMEOUT_SECS: u32 = 300;

/// Default `timeout_secs` when the model omits the parameter.
pub const WAIT_TASK_DEFAULT_TIMEOUT_SECS: u32 = 30;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "ListBackgroundTasks".to_string(),
            description:
                "List every background task you have running — both background sub-agents (spawned via \
                InvokeAgent { background: true }) and background shell processes (spawned via Bash \
                { background: true }).\n\n\
                Returns a JSON array of objects, each with:\n\
                - task_id: prefixed string. \"agent:N\" for sub-agent tasks, \"process:N\" for shell processes.\n\
                - task_type: \"agent\" or \"process\".\n\
                - description: agent name + prompt for agents; the original command for processes.\n\
                - status: \"pending\" | \"running\" | \"completed\" | \"errored\" | \"cancelled\" \
                (agents) or \"running\" | \"exited\" | \"killed\" (processes).\n\
                - age_secs: wall-clock seconds since the task was spawned.\n\
                - exit_code: present only for exited processes.\n\n\
                Use this when:\n\
                - You launched background work and want to check progress before doing more.\n\
                - You need a task_id to feed CancelTask or WaitTask.\n\n\
                Do NOT use this when:\n\
                - You're not sure whether you launched anything (you'd see an empty array — \
                cheap, but pointless if you didn't intend to background work).\n\n\
                Scope: returns only YOUR tasks. You will never see another agent's tasks or \
                the user's top-level tasks here."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "CancelTask".to_string(),
            description:
                "Cancel a single background task by its task_id (from ListBackgroundTasks).\n\n\
                For sub-agent tasks (\"agent:N\"): fires the per-task cancel token. The agent \
                observes it on its next inference iteration and shuts down cleanly. The \
                cancellation result will appear in your conversation as a normal sub-agent \
                completion with a cancelled marker.\n\n\
                For shell processes (\"process:N\"): sends SIGTERM. The process status \
                transitions to \"killed\" immediately; the OS exit code surfaces on the next \
                ListBackgroundTasks / WaitTask call once the process is fully reaped.\n\n\
                Idempotent — calling on an already-cancelled / already-exited task is a \
                successful no-op. Returns an error if the task_id is unknown OR if you don't \
                own the task (Model E scope: you can only cancel tasks you spawned)."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Prefixed task id from ListBackgroundTasks: \
                                        \"agent:N\" or \"process:N\"."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "WaitTask".to_string(),
            description: format!(
                "Block until a background task finishes (or timeout fires).\n\n\
                Returns the task's terminal state and result so you don't have to keep \
                polling ListBackgroundTasks. Prefer WaitTask over a polling loop — one \
                tool call instead of many.\n\n\
                For sub-agent tasks (\"agent:N\"): on completion, returns the agent's full \
                output. The result will NOT also appear in the auto-drain on the next \
                iteration — WaitTask consumes it.\n\n\
                For shell processes (\"process:N\"): on exit, returns the OS exit code. \
                Process stdout/stderr is NOT captured — if you need the output, redirect \
                inside the command (e.g. `Bash {{ command: \"cmd > /tmp/out.log 2>&1\", \
                background: true }}`) and Read the file separately.\n\n\
                If the task hasn't finished by `timeout_secs`, returns the current status \
                without consuming the task — you can call again to keep waiting. Default \
                {default}s, max {max}s. Returns an error if the task_id is unknown or \
                doesn't belong to you.",
                default = WAIT_TASK_DEFAULT_TIMEOUT_SECS,
                max = WAIT_TASK_MAX_TIMEOUT_SECS,
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Prefixed task id: \"agent:N\" or \"process:N\"."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": WAIT_TASK_MAX_TIMEOUT_SECS,
                        "description": format!(
                            "Maximum seconds to wait. Default {default}, capped at {max} to \
                             prevent runaway parks of the inference loop.",
                            default = WAIT_TASK_DEFAULT_TIMEOUT_SECS,
                            max = WAIT_TASK_MAX_TIMEOUT_SECS,
                        )
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Parsed task id (prefix + numeric).
///
/// `parse_task_id` produces this from the model-supplied string so the
/// dispatch layer can route to the right registry without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskId {
    /// Bg-agent task — `agent:N`.
    Agent(u32),
    /// Bg shell process — `process:N`.
    Process(u32),
}

/// Parse a model-supplied `task_id` string.
///
/// Accepts:
/// - `"agent:N"` → [`TaskId::Agent`]
/// - `"process:N"` → [`TaskId::Process`]
/// - `"N"` (bare numeric) → [`TaskId::Agent`] for back-compat with the
///   `/cancel <id>` UX shipped in #1042. The LLM tool descriptions
///   *require* the prefix; this lookup tolerates the bare form so the
///   TUI can share the same parser without diverging.
///
/// Returns an `Err(message)` the dispatch layer can hand back to the
/// model verbatim when the input is malformed.
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

/// Clamp a model-supplied `timeout_secs` into the allowed range.
///
/// `None` → [`WAIT_TASK_DEFAULT_TIMEOUT_SECS`].
/// `Some(0)` → 1 (degenerate but harmless; we don't error out on 0).
/// `Some(> WAIT_TASK_MAX_TIMEOUT_SECS)` → cap.
pub fn clamp_wait_timeout_secs(requested: Option<u32>) -> u32 {
    let raw = requested.unwrap_or(WAIT_TASK_DEFAULT_TIMEOUT_SECS);
    raw.clamp(1, WAIT_TASK_MAX_TIMEOUT_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_returns_three_tools_with_expected_names() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["ListBackgroundTasks", "CancelTask", "WaitTask"]);
    }

    #[test]
    fn list_background_tasks_takes_no_arguments() {
        let defs = definitions();
        let list = defs
            .iter()
            .find(|d| d.name == "ListBackgroundTasks")
            .unwrap();
        // Required must be empty (or absent).
        let required = list.parameters.get("required");
        assert!(
            required.is_none() || required.unwrap().as_array().unwrap().is_empty(),
            "ListBackgroundTasks must take no required args"
        );
    }

    #[test]
    fn cancel_and_wait_require_task_id() {
        let defs = definitions();
        for name in ["CancelTask", "WaitTask"] {
            let def = defs.iter().find(|d| d.name == name).unwrap();
            let required = def.parameters["required"].as_array().unwrap();
            assert!(
                required.iter().any(|v| v == "task_id"),
                "{name} must require task_id"
            );
        }
    }

    #[test]
    fn parse_task_id_accepts_prefixed_forms() {
        assert_eq!(parse_task_id("agent:7").unwrap(), TaskId::Agent(7));
        assert_eq!(parse_task_id("process:12345").unwrap(), TaskId::Process(12345));
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

    #[test]
    fn clamp_wait_timeout_handles_none_default() {
        assert_eq!(
            clamp_wait_timeout_secs(None),
            WAIT_TASK_DEFAULT_TIMEOUT_SECS
        );
    }

    #[test]
    fn clamp_wait_timeout_caps_at_max() {
        assert_eq!(
            clamp_wait_timeout_secs(Some(86400)),
            WAIT_TASK_MAX_TIMEOUT_SECS
        );
    }

    #[test]
    fn clamp_wait_timeout_floors_at_one() {
        assert_eq!(clamp_wait_timeout_secs(Some(0)), 1);
    }

    #[test]
    fn clamp_wait_timeout_passes_through_in_range() {
        assert_eq!(clamp_wait_timeout_secs(Some(45)), 45);
        assert_eq!(
            clamp_wait_timeout_secs(Some(WAIT_TASK_MAX_TIMEOUT_SECS)),
            WAIT_TASK_MAX_TIMEOUT_SECS
        );
    }
}
