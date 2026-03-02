//! Shell command execution tool.
//!
//! Runs commands as child processes with timeout protection.
//! Output is capped at 150 lines to protect the context window.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use tokio::process::Command;

const MAX_OUTPUT_LINES: usize = 256;
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Bash".to_string(),
        description: "Execute a shell command and return stdout/stderr. \
            Prefer dedicated tools (Read, Grep, List, Edit) over shell equivalents. \
            Use Bash ONLY for: builds, tests, git operations, and commands without a dedicated tool. \
            IMPORTANT: Always limit output to avoid wasting tokens. Techniques: \
            `| tail -20` (last N lines), `| head -10` (first N lines), \
            `| grep 'pattern'` (filter relevant lines), \
            `2>/dev/null` (suppress stderr noise), \
            `--silent` or `--quiet` flags when available. \
            For test suites: `cargo test 2>&1 | tail -20`, `npm test -- --silent`, `pytest -q`. \
            For searches: prefer Grep tool, or pipe through `| head` if using shell grep. \
            Output is capped at 256 lines — pipe to limit before that.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 60)"
                }
            },
            "required": ["command"]
        }),
    }]
}

/// Execute a shell command with timeout and output capping.
pub async fn run_shell_command(project_root: &Path, args: &Value) -> Result<String> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let timeout_secs = args["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS);

    tracing::info!("Running shell command: [{} chars]", command.len());

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

            let stdout_capped = cap_output(&stdout);
            let stderr_capped = cap_output(&stderr);

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

/// Cap output to the last N lines to protect the context window.
fn cap_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() > MAX_OUTPUT_LINES {
        let skipped = lines.len() - MAX_OUTPUT_LINES;
        format!(
            "[... {skipped} lines truncated ...]\n{}",
            lines[lines.len() - MAX_OUTPUT_LINES..].join("\n")
        )
    } else {
        output.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cap_output_short() {
        let input = "line1\nline2\nline3";
        assert_eq!(cap_output(input), input);
    }

    #[test]
    fn test_cap_output_long() {
        let lines: Vec<String> = (0..500).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let capped = cap_output(&input);

        // Should contain the truncation notice
        assert!(capped.contains("truncated"));
        // Should contain the last line
        assert!(capped.contains("line 499"));
        // Should NOT contain the first line
        assert!(!capped.contains("line 0\n"));
    }

    #[test]
    fn test_cap_output_exactly_at_limit() {
        let lines: Vec<String> = (0..MAX_OUTPUT_LINES).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let capped = cap_output(&input);
        // Exactly at limit, no truncation
        assert!(!capped.contains("truncated"));
    }
}
