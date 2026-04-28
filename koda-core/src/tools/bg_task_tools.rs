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
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use crate::bg_agent::{
    AgentStatus, BgAgentRegistry, BgAgentResult, BgTaskSnapshot, CancelOutcome, WaitOutcome,
};
use crate::tools::ToolResult;
use crate::tools::bg_process::{
    BgProcessSnapshot, BgProcessStatus, BgRegistry, ProcessWaitOutcome,
};

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

// ══ Execution ══════════════════════════════════════════════════════════════════════════════════════
//
// Dispatch entry point for the three Layer-2 tools. Called from
// `tool_dispatch::execute_one_tool` when `tool_name` matches; never
// goes through `ToolRegistry::execute()` because we need the
// `Arc<BgAgentRegistry>` (not stored on the registry) and the caller's
// spawner identity (only known at the dispatch layer).

/// Render an [`AgentStatus`] as the lower-case status string we
/// surface to the model. Stable strings — they're documented in the
/// `ListBackgroundTasks` tool description and become part of the
/// tool API surface.
fn agent_status_str(s: &AgentStatus) -> &'static str {
    match s {
        AgentStatus::Pending => "pending",
        AgentStatus::Running { .. } => "running",
        AgentStatus::Completed { .. } => "completed",
        AgentStatus::Errored { .. } => "errored",
        AgentStatus::Cancelled => "cancelled",
    }
}

/// Render a [`BgProcessStatus`] as the lower-case status string.
fn process_status_str(s: &BgProcessStatus) -> &'static str {
    match s {
        BgProcessStatus::Running => "running",
        BgProcessStatus::Exited { .. } => "exited",
        BgProcessStatus::Killed => "killed",
    }
}

fn agent_snapshot_to_json(s: &BgTaskSnapshot) -> Value {
    json!({
        "task_id": format!("agent:{}", s.task_id),
        "task_type": "agent",
        "description": format!("{}: {}", s.agent_name, s.prompt),
        "status": agent_status_str(&s.status),
        "age_secs": s.age.as_secs(),
    })
}

fn process_snapshot_to_json(s: &BgProcessSnapshot) -> Value {
    let mut obj = json!({
        "task_id": format!("process:{}", s.pid),
        "task_type": "process",
        "description": s.command.clone(),
        "status": process_status_str(&s.status),
        "age_secs": s.age.as_secs(),
    });
    if let BgProcessStatus::Exited { code } = s.status {
        obj.as_object_mut()
            .unwrap()
            .insert("exit_code".into(), json!(code));
    }
    obj
}

/// Helper for emitting an Err-shaped [`ToolResult`] with a model-readable
/// message. The dispatch layer surfaces this back to the model as a
/// failed tool call — same convention as every other tool.
fn err(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        output: msg.into(),
        success: false,
        full_output: None,
    }
}

fn ok(value: Value) -> ToolResult {
    ToolResult {
        output: value.to_string(),
        success: true,
        full_output: None,
    }
}

/// Dispatch a Layer-2 tool call. Returns a [`ToolResult`] in the same
/// shape as `ToolRegistry::execute()` so the dispatch layer can plug
/// it in without special-casing further.
///
/// `tool_name` must be one of `"ListBackgroundTasks"`, `"CancelTask"`,
/// `"WaitTask"`. Any other name is a programmer error in the dispatch
/// router — we return an `err` so it's loud-but-safe in production.
pub async fn execute(
    tool_name: &str,
    arguments: &str,
    bg_agents: &Arc<BgAgentRegistry>,
    bg_processes: &BgRegistry,
    caller_spawner: Option<u32>,
) -> ToolResult {
    match tool_name {
        "ListBackgroundTasks" => execute_list(bg_agents, bg_processes, caller_spawner),
        "CancelTask" => execute_cancel(arguments, bg_agents, bg_processes, caller_spawner),
        "WaitTask" => execute_wait(arguments, bg_agents, bg_processes, caller_spawner).await,
        other => err(format!(
            "bg_task_tools::execute called with unknown tool '{other}' \
             (router bug — should have matched in tool_dispatch)"
        )),
    }
}

fn execute_list(
    bg_agents: &BgAgentRegistry,
    bg_processes: &BgRegistry,
    caller_spawner: Option<u32>,
) -> ToolResult {
    // Refresh process statuses so the model sees the latest exit codes.
    bg_processes.reap();

    let mut entries: Vec<Value> = bg_agents
        .snapshot_for_caller(caller_spawner)
        .iter()
        .map(agent_snapshot_to_json)
        .collect();
    entries.extend(
        bg_processes
            .snapshot_for_caller(caller_spawner)
            .iter()
            .map(process_snapshot_to_json),
    );
    ok(Value::Array(entries))
}

fn execute_cancel(
    arguments: &str,
    bg_agents: &BgAgentRegistry,
    bg_processes: &BgRegistry,
    caller_spawner: Option<u32>,
) -> ToolResult {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return err(format!("CancelTask: invalid JSON arguments: {e}")),
    };
    let task_id_str = match args.get("task_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("CancelTask: missing required 'task_id' (string)"),
    };
    let task_id = match parse_task_id(task_id_str) {
        Ok(t) => t,
        Err(e) => return err(format!("CancelTask: {e}")),
    };

    let outcome = match task_id {
        TaskId::Agent(n) => bg_agents.cancel_as_caller(n, caller_spawner),
        TaskId::Process(n) => bg_processes.kill_as_caller(n, caller_spawner),
    };

    match outcome {
        CancelOutcome::Cancelled => ok(json!({
            "task_id": task_id_str,
            "cancelled": true,
        })),
        CancelOutcome::NotFound => err(format!(
            "CancelTask: no background task with id '{task_id_str}' \
             (already finished, never existed, or already drained)"
        )),
        CancelOutcome::Forbidden => err(format!(
            "CancelTask: task '{task_id_str}' is not owned by this caller"
        )),
    }
}

async fn execute_wait(
    arguments: &str,
    bg_agents: &BgAgentRegistry,
    bg_processes: &BgRegistry,
    caller_spawner: Option<u32>,
) -> ToolResult {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return err(format!("WaitTask: invalid JSON arguments: {e}")),
    };
    let task_id_str = match args.get("task_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return err("WaitTask: missing required 'task_id' (string)"),
    };
    let task_id = match parse_task_id(task_id_str) {
        Ok(t) => t,
        Err(e) => return err(format!("WaitTask: {e}")),
    };
    let timeout_secs = clamp_wait_timeout_secs(
        args.get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32),
    );
    let timeout = Duration::from_secs(timeout_secs as u64);

    match task_id {
        TaskId::Agent(n) => {
            let outcome = bg_agents
                .wait_for_completion(n, caller_spawner, timeout)
                .await;
            agent_wait_to_tool_result(task_id_str, outcome)
        }
        TaskId::Process(n) => {
            let outcome = bg_processes
                .wait_for_exit_as_caller(n, caller_spawner, timeout)
                .await;
            process_wait_to_tool_result(task_id_str, outcome)
        }
    }
}

fn agent_wait_to_tool_result(task_id_str: &str, outcome: WaitOutcome) -> ToolResult {
    match outcome {
        WaitOutcome::Completed(BgAgentResult {
            agent_name,
            prompt,
            output,
            success,
            events,
            // #1108 P2a: only used by the inference loop's drain
            // handler for transcript persistence. The model-facing
            // `WaitTask` JSON doesn't need it — the model already
            // knows which call_id it's awaiting.
            parent_tool_call_id: _,
        }) => ok(json!({
            "task_id": task_id_str,
            "status": if success { "completed" } else { "errored" },
            "agent_name": agent_name,
            "prompt": prompt,
            "output": output,
            "events": events,
        })),
        WaitOutcome::Cancelled => ok(json!({
            "task_id": task_id_str,
            "status": "cancelled",
        })),
        WaitOutcome::TimedOut(snap) => ok(json!({
            "task_id": task_id_str,
            "status": "timed_out",
            "current": agent_snapshot_to_json(&snap),
        })),
        WaitOutcome::NotFound => err(format!(
            "WaitTask: no background task with id '{task_id_str}'"
        )),
        WaitOutcome::Forbidden => err(format!(
            "WaitTask: task '{task_id_str}' is not owned by this caller"
        )),
    }
}

fn process_wait_to_tool_result(task_id_str: &str, outcome: ProcessWaitOutcome) -> ToolResult {
    match outcome {
        ProcessWaitOutcome::Exited { code } => ok(json!({
            "task_id": task_id_str,
            "status": "exited",
            "exit_code": code,
        })),
        ProcessWaitOutcome::TimedOut(snap) => ok(json!({
            "task_id": task_id_str,
            "status": "timed_out",
            "current": process_snapshot_to_json(&snap),
        })),
        ProcessWaitOutcome::NotFound => err(format!(
            "WaitTask: no background task with id '{task_id_str}'"
        )),
        ProcessWaitOutcome::Forbidden => err(format!(
            "WaitTask: task '{task_id_str}' is not owned by this caller"
        )),
    }
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

    // ── execute() dispatch tests ─────────────────────────────────────────────────────

    fn fresh_registries() -> (Arc<BgAgentRegistry>, BgRegistry) {
        (Arc::new(BgAgentRegistry::new()), BgRegistry::new())
    }

    /// `ListBackgroundTasks` on empty registries returns `[]`, success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_list_returns_empty_array_when_no_tasks() {
        let (agents, processes) = fresh_registries();
        let r = execute("ListBackgroundTasks", "{}", &agents, &processes, None).await;
        assert!(r.success);
        assert_eq!(r.output, "[]");
    }

    /// `ListBackgroundTasks` shows the caller's agent tasks with the
    /// agreed-upon shape (prefixed task_id, lower-case status).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_list_includes_caller_agent_tasks() {
        let (agents, processes) = fresh_registries();
        let (id, _tx, _, _) = agents.register_test_with_status("explore", "map repo", None);

        let r = execute("ListBackgroundTasks", "{}", &agents, &processes, None).await;
        assert!(r.success);
        let arr: Value = serde_json::from_str(&r.output).unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["task_id"], format!("agent:{id}"));
        assert_eq!(arr[0]["task_type"], "agent");
        assert_eq!(arr[0]["status"], "pending");
        assert_eq!(arr[0]["description"], "explore: map repo");
    }

    /// Caller scoping: a sub-agent caller (Some(7)) must not see the
    /// top-level (None) task. Defence-in-depth on top of the
    /// sub_agent_dispatch denylist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_list_filters_out_other_callers_tasks() {
        let (agents, processes) = fresh_registries();
        agents.register_test_with_status("a", "top", None);
        agents.register_test_with_status("b", "sub", Some(7));

        let top = execute("ListBackgroundTasks", "{}", &agents, &processes, None).await;
        let arr: Value = serde_json::from_str(&top.output).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 1, "top sees only its own");

        let sub = execute("ListBackgroundTasks", "{}", &agents, &processes, Some(7)).await;
        let arr: Value = serde_json::from_str(&sub.output).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 1, "sub sees only its own");
    }

    /// `CancelTask` routes `agent:N` to BgAgentRegistry and reports
    /// success in the structured payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_cancel_succeeds_for_owned_agent_task() {
        let (agents, processes) = fresh_registries();
        let (id, _tx, _, observer) = agents.register_test_with_status("x", "y", None);

        let r = execute(
            "CancelTask",
            &json!({ "task_id": format!("agent:{id}") }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(r.success, "got: {}", r.output);
        assert!(observer.is_cancelled(), "cancel token must fire");
        let payload: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(payload["cancelled"], true);
        assert_eq!(payload["task_id"], format!("agent:{id}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_cancel_returns_not_found_for_unknown_id() {
        let (agents, processes) = fresh_registries();
        let r = execute(
            "CancelTask",
            &json!({ "task_id": "agent:9999" }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(!r.success);
        assert!(r.output.contains("no background task"), "got: {}", r.output);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_cancel_returns_forbidden_for_cross_caller() {
        let (agents, processes) = fresh_registries();
        let (id, _tx, _, observer) = agents.register_test_with_status("x", "y", Some(5));

        // Top-level (None) tries to cancel sub-agent 5's task.
        let r = execute(
            "CancelTask",
            &json!({ "task_id": format!("agent:{id}") }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(!r.success);
        assert!(
            r.output.contains("not owned by this caller"),
            "got: {}",
            r.output
        );
        assert!(!observer.is_cancelled(), "forbidden must NOT fire token");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_cancel_rejects_malformed_json() {
        let (agents, processes) = fresh_registries();
        let r = execute("CancelTask", "not-json", &agents, &processes, None).await;
        assert!(!r.success);
        assert!(r.output.contains("invalid JSON"), "got: {}", r.output);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_cancel_rejects_missing_task_id() {
        let (agents, processes) = fresh_registries();
        let r = execute("CancelTask", "{}", &agents, &processes, None).await;
        assert!(!r.success);
        assert!(r.output.contains("missing required"), "got: {}", r.output);
    }

    /// `WaitTask` on a completed agent task returns `status:completed`
    /// + the agent's output, and consumes the entry (drain sees nothing).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_wait_returns_completed_for_finished_agent() {
        let (agents, processes) = fresh_registries();
        let (id, tx, status_tx, _) = agents.register_test_with_status("explore", "map", None);
        tx.send(Ok(("final answer".into(), vec!["e1".into()])))
            .unwrap();
        status_tx
            .send(AgentStatus::Completed {
                summary: "final".into(),
            })
            .unwrap();

        let r = execute(
            "WaitTask",
            &json!({ "task_id": format!("agent:{id}"), "timeout_secs": 1 }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(r.success, "got: {}", r.output);
        let payload: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["output"], "final answer");
        assert_eq!(payload["events"].as_array().unwrap().len(), 1);
        // Consumed — not in registry anymore.
        assert_eq!(agents.snapshot().len(), 0);
    }

    /// `WaitTask` timeout returns `status:timed_out` + a snapshot of
    /// the still-running task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_wait_returns_timed_out_with_snapshot() {
        let (agents, processes) = fresh_registries();
        // Bind ALL four to keep the channels alive — if status_tx
        // drops, the watch sender is gone and `wait_for_terminal_status`
        // early-returns, surfacing as Cancelled instead of TimedOut.
        let (id, _tx, _status_tx, _observer) = agents.register_test_with_status("slow", "x", None);

        let r = execute(
            "WaitTask",
            // Timeout below 1s gets clamped to 1s by clamp_wait_timeout_secs;
            // we still want the test to be fast — 1 s is the minimum the
            // tool surface allows.
            &json!({ "task_id": format!("agent:{id}"), "timeout_secs": 1 }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(r.success);
        let payload: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(payload["status"], "timed_out");
        assert_eq!(payload["current"]["task_id"], format!("agent:{id}"));
        // Entry preserved.
        assert_eq!(agents.snapshot().len(), 1);
    }

    /// `WaitTask` returns `status:cancelled` when the cancellation token
    /// fires and the status channel reflects `AgentStatus::Cancelled`.
    ///
    /// The three terminal states are `completed`, `timed_out`, and
    /// `cancelled`. The first two have existing tests; this is the
    /// third path — the one that was missing (#1048).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_wait_returns_cancelled_when_token_fires() {
        let (agents, processes) = fresh_registries();
        let (id, tx, status_tx, observer) = agents.register_test_with_status("slow", "x", None);

        // Fire the cancellation token and push the terminal status so
        // `wait_for_terminal_status` unblocks immediately.
        observer.cancel();
        status_tx.send(AgentStatus::Cancelled).unwrap();
        // Drop the result sender so `entry.rx.await` inside
        // `wait_for_completion` resolves immediately as `Err` rather
        // than waiting out the 50 ms inner timeout.
        drop(tx);

        let r = execute(
            "WaitTask",
            &json!({ "task_id": format!("agent:{id}"), "timeout_secs": 5 }).to_string(),
            &agents,
            &processes,
            None,
        )
        .await;
        assert!(
            r.success,
            "WaitTask on a cancelled task must still succeed: {}",
            r.output
        );
        let payload: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(payload["status"], "cancelled");
        assert_eq!(payload["task_id"], format!("agent:{id}"));
        // Consumed — entry removed from registry.
        assert_eq!(agents.snapshot().len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_unknown_tool_name_returns_error() {
        let (agents, processes) = fresh_registries();
        let r = execute("NotAToolWeKnow", "{}", &agents, &processes, None).await;
        assert!(!r.success);
        assert!(r.output.contains("unknown tool"));
    }
}
