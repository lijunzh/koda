//! Shell command execution tool.
//!
//! Runs commands as child processes with timeout protection.
//! Output line cap is set by `OutputCaps` (context-scaled).

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Bash".to_string(),
        description: "Execute a shell command. Use ONLY for builds, tests, git, \
            and commands without a dedicated tool. Never use for file ops \
            (use Read/Write/Edit/Grep/List instead). Suppress verbose output: \
            pipe to tail, use --quiet, avoid -v flags."
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
                    "description": "Timeout in seconds (default: 60)"
                }
            },
            "required": ["command"]
        }),
    }]
}

/// Execute a shell command with timeout and output capping.
pub async fn run_shell_command(
    project_root: &Path,
    args: &Value,
    max_output_lines: usize,
) -> Result<String> {
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

    #[test]
    fn test_cap_output_short() {
        let input = "line1\nline2\nline3";
        assert_eq!(cap_output(input, 256), input);
    }

    #[test]
    fn test_cap_output_long() {
        let lines: Vec<String> = (0..500).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let capped = cap_output(&input, 256);

        // Should contain the truncation notice
        assert!(capped.contains("truncated"));
        // Should contain the last line
        assert!(capped.contains("line 499"));
        // Should NOT contain the first line
        assert!(!capped.contains("line 0\n"));
    }

    #[test]
    fn test_cap_output_exactly_at_limit() {
        let lines: Vec<String> = (0..256).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let capped = cap_output(&input, 256);
        // Exactly at limit, no truncation
        assert!(!capped.contains("truncated"));
    }
}
