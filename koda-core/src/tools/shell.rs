//! Shell command execution tool.
//!
//! Runs commands as child processes with timeout protection.
//! Output line cap is set by `OutputCaps` (context-scaled).
//!
//! When `background: true` the command is spawned detached and control returns
//! immediately with the PID.  The process is tracked in `BgRegistry` and
//! SIGTERMed when the session ends.
//!
//! ## Smart summary
//!
//! The model receives a compact summary (exit code + stderr + tail of stdout)
//! while the full output is stored separately in the DB for retrieval via
//! `RecallContext`.

use crate::engine::{EngineEvent, EngineSink};
use crate::providers::ToolDefinition;
use crate::tools::bg_process::BgRegistry;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling to prevent LLM-controlled DoS via huge timeout values.
const MAX_TIMEOUT_SECS: u64 = 300;
/// Max stderr lines to include in the summary (stderr is high-signal).
const SUMMARY_STDERR_LINES: usize = 50;
/// Max stdout tail lines to include in the summary.
const SUMMARY_STDOUT_TAIL: usize = 20;

/// Result of a shell command with both a model-facing summary and full output.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    /// Compact summary for the model's context window.
    pub summary: String,
    /// Full untruncated output for DB storage / RecallContext retrieval.
    /// `None` for background commands (no output to capture).
    pub full_output: Option<String>,
}

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

/// Execute a shell command with timeout, output capping, and optional streaming.
///
/// When `sink` is provided, each line of stdout/stderr is emitted as a
/// `ToolOutputLine` event as it arrives — giving the TUI a live terminal feel.
/// The full output is still collected and returned as the tool result.
///
/// When `args["background"]` is `true`, the process is spawned detached and
/// this function returns immediately with the PID.  The `BgRegistry` tracks
/// the child so it is cleaned up (SIGTERM) when the session ends.
pub async fn run_shell_command(
    project_root: &Path,
    args: &Value,
    _max_output_lines: usize,
    bg: &BgRegistry,
    sink: Option<(&dyn EngineSink, &str)>,
) -> Result<ShellOutput> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let background = args["background"].as_bool().unwrap_or(false);

    tracing::info!(
        "Running shell command (background={background}): [{} chars]",
        command.len()
    );

    if background {
        let msg = spawn_background(project_root, command, bg)?;
        return Ok(ShellOutput {
            summary: msg,
            full_output: None,
        });
    }

    let timeout_secs = args["timeout"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);

    // Spawn with piped stdout/stderr for line-by-line streaming.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to execute command: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();

    // Read stdout and stderr concurrently, streaming lines as they arrive.
    let sink_info = sink.map(|(s, id)| (s, id.to_string()));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        read_streams(
            stdout,
            stderr,
            &mut stdout_lines,
            &mut stderr_lines,
            &sink_info,
        ),
    )
    .await;

    match result {
        Ok(Ok(())) => {
            // Wait for exit status after streams are drained.
            let status = child
                .wait()
                .await
                .map_err(|e| anyhow::anyhow!("wait: {e}"))?;
            let exit_code = status.code().unwrap_or(-1);

            let summary = format_summary(exit_code, &stdout_lines, &stderr_lines);
            let full = format_full_output(exit_code, &stdout_lines, &stderr_lines);

            Ok(ShellOutput {
                summary,
                full_output: Some(full),
            })
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("Stream read error: {e}")),
        Err(_) => {
            // Timeout — kill the child.
            let _ = child.kill().await;
            let msg = format!("Command timed out after {timeout_secs}s: {command}");
            Ok(ShellOutput {
                summary: msg.clone(),
                full_output: Some(msg),
            })
        }
    }
}

/// Read stdout and stderr concurrently, collecting lines and optionally streaming them.
async fn read_streams(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_lines: &mut Vec<String>,
    stderr_lines: &mut Vec<String>,
    sink_info: &Option<(&dyn EngineSink, String)>,
) -> std::io::Result<()> {
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_done = false;
    let mut stderr_done = false;

    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout_reader.next_line(), if !stdout_done => {
                match line? {
                    Some(l) => {
                        if let Some((sink, id)) = sink_info {
                            sink.emit(EngineEvent::ToolOutputLine {
                                id: id.clone(),
                                line: l.clone(),
                                is_stderr: false,
                            });
                        }
                        stdout_lines.push(l);
                    }
                    None => stdout_done = true,
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line? {
                    Some(l) => {
                        if let Some((sink, id)) = sink_info {
                            sink.emit(EngineEvent::ToolOutputLine {
                                id: id.clone(),
                                line: l.clone(),
                                is_stderr: true,
                            });
                        }
                        stderr_lines.push(l);
                    }
                    None => stderr_done = true,
                }
            }
        }
    }
    Ok(())
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

/// Build a compact summary for the model's context window.
///
/// Includes all stderr (high-signal — errors/warnings) and only the tail
/// of stdout (low-signal — build progress noise).  Line counts let the
/// model decide whether to retrieve the full output via RecallContext.
fn format_summary(
    exit_code: i32,
    stdout_lines: &[String],
    stderr_lines: &[String],
) -> String {
    let mut out = format!(
        "Exit code: {exit_code} | stdout: {} lines | stderr: {} lines",
        stdout_lines.len(),
        stderr_lines.len(),
    );

    // Stderr first — always include (capped at SUMMARY_STDERR_LINES).
    if !stderr_lines.is_empty() {
        let (label, text) = if stderr_lines.len() > SUMMARY_STDERR_LINES {
            let skipped = stderr_lines.len() - SUMMARY_STDERR_LINES;
            (
                format!(
                    "\n\n--- stderr (last {} of {}, {skipped} skipped) ---",
                    SUMMARY_STDERR_LINES,
                    stderr_lines.len(),
                ),
                stderr_lines[stderr_lines.len() - SUMMARY_STDERR_LINES..].join("\n"),
            )
        } else {
            (
                format!("\n\n--- stderr ({} lines) ---", stderr_lines.len()),
                stderr_lines.join("\n"),
            )
        };
        out.push_str(&label);
        out.push('\n');
        out.push_str(&text);
    }

    // Stdout tail — only last N lines.
    if !stdout_lines.is_empty() {
        let (label, text) = if stdout_lines.len() > SUMMARY_STDOUT_TAIL {
            (
                format!(
                    "\n\n--- stdout (last {} of {}) ---",
                    SUMMARY_STDOUT_TAIL,
                    stdout_lines.len(),
                ),
                stdout_lines[stdout_lines.len() - SUMMARY_STDOUT_TAIL..].join("\n"),
            )
        } else {
            (
                format!("\n\n--- stdout ({} lines) ---", stdout_lines.len()),
                stdout_lines.join("\n"),
            )
        };
        out.push_str(&label);
        out.push('\n');
        out.push_str(&text);
    }

    // Hint for the model.
    if stdout_lines.len() > SUMMARY_STDOUT_TAIL || stderr_lines.len() > SUMMARY_STDERR_LINES {
        out.push_str(
            "\n\nFull output stored. Use RecallContext to search if needed.",
        );
    }

    out
}

/// Build the full (untruncated) output for DB storage.
///
/// Stored in `messages.full_content` and searchable via RecallContext.
/// Capped at 200 KB to prevent pathological commands from bloating the DB.
fn format_full_output(
    exit_code: i32,
    stdout_lines: &[String],
    stderr_lines: &[String],
) -> String {
    const MAX_FULL_OUTPUT_BYTES: usize = 200 * 1024;

    let mut out = format!("Exit code: {exit_code}\n");
    if !stdout_lines.is_empty() {
        out.push_str("\n--- stdout ---\n");
        out.push_str(&stdout_lines.join("\n"));
    }
    if !stderr_lines.is_empty() {
        out.push_str("\n\n--- stderr ---\n");
        out.push_str(&stderr_lines.join("\n"));
    }

    // Hard cap to prevent DB bloat from pathological commands.
    if out.len() > MAX_FULL_OUTPUT_BYTES {
        out.truncate(MAX_FULL_OUTPUT_BYTES);
        // Find safe char boundary
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
        out.push_str("\n\n[... output truncated at 200KB ...]");
    }

    out
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
        let result = run_shell_command(tmp.path(), &args, 256, &bg(), None)
            .await
            .unwrap();
        assert!(
            result.summary.contains("timed out"),
            "Expected timeout message, got: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn shell_respects_custom_timeout_parameter() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo hello", "timeout": 5});
        let result = run_shell_command(tmp.path(), &args, 256, &bg(), None)
            .await
            .unwrap();
        assert!(
            result.summary.contains("hello"),
            "Fast command should succeed: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn shell_default_timeout_is_applied_when_not_specified() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo world"});
        let result = run_shell_command(tmp.path(), &args, 256, &bg(), None)
            .await
            .unwrap();
        assert!(
            result.summary.contains("world"),
            "Command without explicit timeout should work: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn background_spawn_returns_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let args = serde_json::json!({"command": "sleep 60", "background": true});
        let result = run_shell_command(tmp.path(), &args, 256, &registry, None)
            .await
            .unwrap();
        assert!(
            result.summary.contains("Background process started"),
            "{}",
            result.summary
        );
        assert!(result.summary.contains("PID:"), "{}", result.summary);
        assert!(result.summary.contains("kill"), "{}", result.summary);
        assert!(result.full_output.is_none(), "background has no full_output");
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn background_false_runs_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo sync", "background": false});
        let result = run_shell_command(tmp.path(), &args, 256, &bg(), None)
            .await
            .unwrap();
        assert!(result.summary.contains("sync"), "{}", result.summary);
        assert!(
            !result.summary.contains("PID:"),
            "foreground should not have PID line: {}",
            result.summary
        );
    }

    #[test]
    fn test_format_summary_short_output() {
        let stdout: Vec<String> = vec!["hello", "world"]
            .into_iter()
            .map(String::from)
            .collect();
        let stderr: Vec<String> = vec![];
        let summary = format_summary(0, &stdout, &stderr);
        assert!(summary.contains("Exit code: 0"));
        assert!(summary.contains("stdout: 2 lines"));
        assert!(summary.contains("hello"));
        assert!(summary.contains("world"));
        // Short output should NOT have the RecallContext hint
        assert!(!summary.contains("RecallContext"));
    }

    #[test]
    fn test_format_summary_long_stdout_truncated() {
        let stdout: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let stderr: Vec<String> = vec!["warning: something".into()];
        let summary = format_summary(0, &stdout, &stderr);
        // Should contain last 20 lines
        assert!(summary.contains("line 99"));
        assert!(summary.contains("line 80"));
        // Should NOT contain early lines
        assert!(!summary.contains("line 0\n"));
        // Should show truncation metadata
        assert!(summary.contains("last 20 of 100"));
        // Stderr should be fully included
        assert!(summary.contains("warning: something"));
        // Should have RecallContext hint
        assert!(summary.contains("RecallContext"));
    }

    #[test]
    fn test_format_full_output_includes_everything() {
        let stdout: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let stderr: Vec<String> = vec!["err1".into(), "err2".into()];
        let full = format_full_output(1, &stdout, &stderr);
        assert!(full.contains("Exit code: 1"));
        assert!(full.contains("line 0"));
        assert!(full.contains("line 99"));
        assert!(full.contains("err1"));
        assert!(full.contains("err2"));
    }

    #[test]
    fn test_format_full_output_capped_at_200kb() {
        let stdout: Vec<String> = (0..50_000).map(|i| format!("line {i}: padding")).collect();
        let full = format_full_output(0, &stdout, &[]);
        assert!(full.len() <= 200 * 1024 + 50); // 200KB + truncation message
        assert!(full.contains("truncated at 200KB"));
    }

    #[test]
    fn test_shell_output_has_full_output() {
        // Verify ShellOutput struct works correctly
        let so = ShellOutput {
            summary: "Exit code: 0".into(),
            full_output: Some("full output here".into()),
        };
        assert_eq!(so.summary, "Exit code: 0");
        assert_eq!(so.full_output.unwrap(), "full output here");
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

    #[tokio::test]
    async fn streaming_emits_lines_to_sink() {
        use std::sync::{Arc, Mutex};

        /// Collects ToolOutputLine events for testing.
        #[derive(Debug, Default)]
        struct CaptureSink {
            lines: Mutex<Vec<(String, bool)>>,
        }
        impl crate::engine::EngineSink for CaptureSink {
            fn emit(&self, event: EngineEvent) {
                if let EngineEvent::ToolOutputLine {
                    line, is_stderr, ..
                } = event
                {
                    self.lines.lock().unwrap().push((line, is_stderr));
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let sink = Arc::new(CaptureSink::default());
        let args = serde_json::json!({
            "command": "echo alpha && echo bravo && echo charlie >&2"
        });
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &bg(),
            Some((sink.as_ref(), "test_id")),
        )
        .await
        .unwrap();

        // Summary should contain the output
        assert!(result.summary.contains("alpha"));
        assert!(result.summary.contains("bravo"));
        assert!(result.summary.contains("charlie"));

        // Full output should contain everything
        let full = result.full_output.unwrap();
        assert!(full.contains("alpha"));
        assert!(full.contains("bravo"));
        assert!(full.contains("charlie"));

        // Streaming lines should have been emitted
        let lines = sink.lines.lock().unwrap();
        assert!(
            lines.len() >= 3,
            "Expected at least 3 streamed lines, got {}: {lines:?}",
            lines.len()
        );
        // At least one stdout and one stderr line
        assert!(lines.iter().any(|(_, is_stderr)| !is_stderr));
        assert!(lines.iter().any(|(_, is_stderr)| *is_stderr));
    }
}
