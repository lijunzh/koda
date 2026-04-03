//! Shell command execution tool.
//!
//! Runs commands as child processes with timeout protection.
//! Output line cap is set by `OutputCaps` (context-scaled).
//!
//! When `background: true` the command is spawned detached and control returns
//! immediately with the PID.  The process is tracked in `BgRegistry` and
//! SIGTERMed when the session ends.

use crate::providers::ToolDefinition;
use crate::tools::bg_process::BgRegistry;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling to prevent LLM-controlled DoS via huge timeout values.
const MAX_TIMEOUT_SECS: u64 = 300;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Bash".to_string(),
        description: "Execute a shell command. Use ONLY for builds, tests, git, \
            and commands without a dedicated tool. Never use for file ops \
            (use Read/Write/Edit/Grep/List instead). Suppress verbose output: \
            pipe to tail, use --quiet, avoid -v flags. \
            Set background=true for long-running processes (dev servers, watchers) \
            — returns immediately with the PID."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 60, ignored when background=true)"
                },
                "background": {
                    "type": "boolean",
                    "description": "Run in background and return immediately with PID (default: false). \
                        Use for dev servers, file watchers, and other long-running processes."
                }
            },
            "required": ["command"]
        }),
    }]
}

/// Execute a shell command with timeout and output capping.
///
/// When `args["background"]` is `true`, the process is spawned detached and
/// this function returns immediately with the PID.  The `BgRegistry` tracks
/// the child so it is cleaned up (SIGTERM) when the session ends.
pub async fn run_shell_command(
    project_root: &Path,
    args: &Value,
    max_output_lines: usize,
    bg: &BgRegistry,
) -> Result<String> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let background = args["background"].as_bool().unwrap_or(false);

    tracing::info!(
        "Running shell command (background={background}): [{} chars]",
        command.len()
    );

    if background {
        return spawn_background(project_root, command, bg);
    }

    let timeout_secs = args["timeout"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(project_root)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let stdout_capped = cap_output(&stdout, max_output_lines);
            let stderr_capped = cap_output(&stderr, max_output_lines);

            let mut response = format!("Exit code: {exit_code}\n");
            if !stdout_capped.is_empty() {
                response.push_str(&format!("\n--- stdout ---\n{stdout_capped}"));
            }
            if !stderr_capped.is_empty() {
                response.push_str(&format!("\n--- stderr ---\n{stderr_capped}"));
            }

            Ok(response)
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("Failed to execute command: {e}")),
        Err(_) => Ok(format!(
            "Command timed out after {timeout_secs}s: {command}"
        )),
    }
}

/// Spawn a command in the background and register it.
///
/// Returns immediately with PID + instructions. Sync because `spawn()` doesn't
/// need to await — only `output()` / `wait()` block.
fn spawn_background(project_root: &Path, command: &str, bg: &BgRegistry) -> Result<String> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root)
        // Detach stdio so the process doesn't block on terminal I/O.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn background command: {e}"))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Spawned process has no PID (already exited)"))?;

    bg.insert(pid, command.to_string(), child);

    Ok(format!(
        "Background process started.\n  PID:     {pid}\n  Command: {command}\n\
         To stop:  Bash{{command: \"kill {pid}\"}}\n\
         To force: Bash{{command: \"kill -9 {pid}\"}}\n\
         Note: process will be stopped automatically when the session ends."
    ))
}

/// Cap output to the last N lines to protect the context window.
fn cap_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() > max_lines {
        let skipped = lines.len() - max_lines;
        format!(
            "[... {skipped} lines truncated ...]\n{}",
            lines[lines.len() - max_lines..].join("\n")
        )
    } else {
        output.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bg_process::BgRegistry;

    fn bg() -> BgRegistry {
        BgRegistry::new()
    }

    #[tokio::test]
    async fn shell_timeout_returns_timeout_message() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "sleep 5", "timeout": 1});
        let result = run_shell_command(tmp.path(), &args, 256, &bg())
            .await
            .unwrap();
        assert!(
            result.contains("timed out"),
            "Expected timeout message, got: {result}"
        );
    }

    #[tokio::test]
    async fn shell_respects_custom_timeout_parameter() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo hello", "timeout": 5});
        let result = run_shell_command(tmp.path(), &args, 256, &bg())
            .await
            .unwrap();
        assert!(
            result.contains("hello"),
            "Fast command should succeed: {result}"
        );
    }

    #[tokio::test]
    async fn shell_default_timeout_is_applied_when_not_specified() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo world"});
        let result = run_shell_command(tmp.path(), &args, 256, &bg())
            .await
            .unwrap();
        assert!(
            result.contains("world"),
            "Command without explicit timeout should work: {result}"
        );
    }

    #[tokio::test]
    async fn background_spawn_returns_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let args = serde_json::json!({"command": "sleep 60", "background": true});
        let result = run_shell_command(tmp.path(), &args, 256, &registry)
            .await
            .unwrap();
        assert!(result.contains("Background process started"), "{result}");
        assert!(result.contains("PID:"), "{result}");
        assert!(result.contains("kill"), "{result}");
        assert_eq!(registry.len(), 1);
        // Drop kills it cleanly
    }

    #[tokio::test]
    async fn background_false_runs_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo sync", "background": false});
        let result = run_shell_command(tmp.path(), &args, 256, &bg())
            .await
            .unwrap();
        assert!(result.contains("sync"), "{result}");
        assert!(
            !result.contains("PID:"),
            "foreground should not have PID line: {result}"
        );
    }

    #[test]
    fn test_cap_output_short() {
        let input = "line1\nline2\nline3";
        assert_eq!(cap_output(input, 256), input);
    }

    #[test]
    fn test_cap_output_long() {
        let lines: Vec<String> = (0..500).map(|i| format!("line {i}")).collect();
        let capped = cap_output(&lines.join("\n"), 256);
        assert!(capped.contains("truncated"));
        assert!(capped.contains("line 499"));
        assert!(!capped.contains("line 0\n"));
    }

    #[test]
    fn test_cap_output_exactly_at_limit() {
        let lines: Vec<String> = (0..256).map(|i| format!("line {i}")).collect();
        assert!(!cap_output(&lines.join("\n"), 256).contains("truncated"));
    }

    #[test]
    fn test_timeout_capped_at_max() {
        let args = serde_json::json!({"command": "echo hi", "timeout": 99999});
        let t = args["timeout"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(t, MAX_TIMEOUT_SECS);
    }
}
