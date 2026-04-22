//! Shell command execution tool (Bash).
//!
//! Runs commands as child processes with timeout protection.
//! Output line cap is set by `OutputCaps` (context-scaled).
//!
//! ## Parameters
//!
//! - **`command`** (required) — The shell command to execute
//! - **`timeout`** (optional, default 60) — Timeout in seconds
//! - **`background`** (optional, default false) — Run in background, return PID
//!
//! ## Background mode
//!
//! When `background: true` the command is spawned detached and control returns
//! immediately with the PID. Use for dev servers, file watchers, and other
//! long-running processes. Background processes are tracked in `BgRegistry`.
//!
//! ## Safety
//!
//! - Commands are classified by `bash_safety::classify_bash_command`
//! - Destructive commands (`rm -rf`, `git push --force`) always need confirmation
//! - Path escapes outside the project root are flagged by `bash_path_lint`
//! - Output is capped to prevent context overflow (verbose output is truncated)
//!
//! ## Best practices (sent to the model)
//!
//! - Use Bash only for builds, tests, git, and commands without a dedicated tool
//! - Never use Bash for file ops — use Read/Write/Edit/Grep/List instead
//! - Suppress verbose output: pipe to `tail`, use `--quiet`, avoid `-v` flags

use crate::engine::{EngineEvent, EngineSink};
use crate::providers::ToolDefinition;
use crate::tools::bg_process::BgRegistry;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling to prevent LLM-controlled DoS via huge timeout values.
const MAX_TIMEOUT_SECS: u64 = 300;
/// Max stderr lines to include in the summary (stderr is high-signal).
const SUMMARY_STDERR_LINES: usize = 50;
/// Max stdout tail lines to include in the summary.
const SUMMARY_STDOUT_TAIL: usize = 20;
/// Hard memory ceiling for line collection. Pathological commands (`yes`,
/// `cat /dev/urandom | base64`) can produce gigabytes within the 300s timeout.
/// Once this byte threshold is reached, lines are still streamed to the TUI
/// but no longer collected into the in-memory Vec. The DB cap
/// (`MAX_FULL_OUTPUT_BYTES`) handles what actually gets persisted.
const MAX_COLLECT_BYTES: usize = 10 * 1024 * 1024; // 10 MB

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
//
// 9 args is over clippy's default 7 — every one is load-bearing context
// (project root, args, output cap, bg registry, streaming sink, trust
// mode, two proxy ports). Bundling into a struct just to placate a lint
// would obscure the call sites; the named-keyword feel comes for free
// from Rust's positional-with-types discipline.
#[allow(clippy::too_many_arguments)]
pub async fn run_shell_command(
    project_root: &Path,
    args: &Value,
    max_lines: usize,
    bg: &BgRegistry,
    sink: Option<(&dyn EngineSink, &str)>,
    trust: &crate::trust::TrustMode,
    policy: &koda_sandbox::SandboxPolicy,
    proxy_port: Option<u16>,
    socks5_port: Option<u16>,
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
        let msg = spawn_background(
            project_root,
            command,
            bg,
            trust,
            policy,
            proxy_port,
            socks5_port,
        )?;
        return Ok(ShellOutput {
            summary: msg,
            full_output: None,
        });
    }

    // Phase 5 PR-3 of #934: timeout precedence is
    //   1. explicit `timeout` arg (model-supplied per-call)
    //   2. `policy.limits.wall_time_secs` (per-agent default)
    //   3. `DEFAULT_TIMEOUT_SECS` legacy constant fallback
    // Hard ceiling `MAX_TIMEOUT_SECS` clamps all three — keeps the
    // DoS protection that the LLM can't widen its own deadline by
    // asking for a 1-hour run.
    let timeout_secs = args["timeout"]
        .as_u64()
        .or(policy.limits.wall_time_secs)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);

    // Spawn via sandbox wrapper (enforced for all trust modes).
    // Phase 5 PR-2 of #934: policy is now threaded in from the
    // ToolRegistry (set via [`ToolRegistry::with_sandbox_policy`] in
    // sub-agent dispatch; defaults to `strict_default()` for the main
    // agent). PR-3 will start populating the policy with non-default
    // values via [`crate::sandbox::policy_for_agent`].
    let mut child = crate::sandbox::build(
        command,
        project_root,
        trust,
        policy,
        proxy_port,
        socks5_port,
    )?
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|e| anyhow::anyhow!("Failed to execute command: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();

    // Read stdout and stderr concurrently, streaming lines as they arrive.
    // Lines are always streamed to the TUI, but collection into Vec stops
    // once max_lines or MAX_COLLECT_BYTES is reached (OOM protection).
    let sink_info = sink.map(|(s, id)| (s, id.to_string()));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        read_streams(
            stdout,
            stderr,
            &mut stdout_lines,
            &mut stderr_lines,
            max_lines,
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

            // Phase 1 of #934: surface kernel-sandbox denials to the model.
            // The kernel makes the syscall fail with EACCES/EPERM and the
            // child shell prints a libc error to stderr; parse those lines
            // into structured violations and append a CC-style block.
            // Always-on (informational only — doesn't change exit codes or
            // change which lines we already collected).
            annotate_violations(exit_code, command, &mut stderr_lines);

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
///
/// Lines are always streamed to the TUI sink (if present), but collection into
/// the Vecs is gated by two caps:
///   - `max_lines` — total stdout + stderr lines collected
///   - `MAX_COLLECT_BYTES` — total bytes collected (OOM protection)
///
/// Once either cap is hit, new lines are still streamed to the TUI but silently
/// dropped from the Vecs. This keeps the TUI responsive while bounding memory
/// for pathological commands.
async fn read_streams(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_lines: &mut Vec<String>,
    stderr_lines: &mut Vec<String>,
    max_lines: usize,
    sink_info: &Option<(&dyn EngineSink, String)>,
) -> std::io::Result<()> {
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut collected_bytes: usize = 0;
    let mut collected_lines: usize = 0;

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
                        if collected_lines < max_lines
                            && collected_bytes < MAX_COLLECT_BYTES
                        {
                            collected_bytes += l.len();
                            collected_lines += 1;
                            stdout_lines.push(l);
                        }
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
                        if collected_lines < max_lines
                            && collected_bytes < MAX_COLLECT_BYTES
                        {
                            collected_bytes += l.len();
                            collected_lines += 1;
                            stderr_lines.push(l);
                        }
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
fn spawn_background(
    project_root: &Path,
    command: &str,
    bg: &BgRegistry,
    trust: &crate::trust::TrustMode,
    policy: &koda_sandbox::SandboxPolicy,
    proxy_port: Option<u16>,
    socks5_port: Option<u16>,
) -> Result<String> {
    // Spawn via sandbox wrapper (enforced for all trust modes).
    // Detach stdio so the process doesn't block on terminal I/O.
    // Phase 5 PR-2 of #934: policy threaded in from the registry
    // (see comment on `run_shell_command` above for the threading rationale).
    let child = crate::sandbox::build(
        command,
        project_root,
        trust,
        policy,
        proxy_port,
        socks5_port,
    )?
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
fn format_summary(exit_code: i32, stdout_lines: &[String], stderr_lines: &[String]) -> String {
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
        out.push_str("\n\nFull output stored. Use RecallContext to search if needed.");
    }

    out
}

/// Build the full output for DB storage.
///
/// Stored in `messages.full_content` and searchable via RecallContext.
/// Capped at 2 MB — generous enough for RecallContext to find errors deep in
/// build/test output, while still preventing pathological commands from
/// bloating the SQLite DB.
fn format_full_output(exit_code: i32, stdout_lines: &[String], stderr_lines: &[String]) -> String {
    const MAX_FULL_OUTPUT_BYTES: usize = 2 * 1024 * 1024; // 2 MB

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
        out.push_str("\n\n[... output truncated at 2MB ...]");
    }

    out
}

/// Phase 1 of #934: parse stderr for kernel-sandbox denials and append a
/// `<sandbox_violations>` block (CC pattern) so the model can react.
///
/// We only annotate when the command exited non-zero — a successful
/// command with `Permission denied` in its stderr is almost always a
/// false positive (e.g. `find / 2>/dev/null` swallows pre-sandbox
/// errors that aren't sandbox denials at all).
///
/// The block is appended *to* `stderr_lines` (not stdout) so existing
/// formatters carry it through unchanged. Violations are also recorded
/// in the process-wide [`koda_sandbox::global_store`] for `/sandbox
/// status` to surface later.
fn annotate_violations(exit_code: i32, command: &str, stderr_lines: &mut Vec<String>) {
    if exit_code == 0 {
        return;
    }
    let joined = stderr_lines.join("\n");
    let violations = koda_sandbox::monitor::parse_stderr(&joined, Some(command));
    if violations.is_empty() {
        return;
    }
    let store = koda_sandbox::global_store();
    for v in &violations {
        store.record(v.clone());
    }
    if let Some(block) = koda_sandbox::render_block(&violations) {
        // Push each line of the block as its own entry so the line-count
        // accounting in format_summary stays accurate.
        for line in block.lines() {
            stderr_lines.push(line.to_string());
        }
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
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
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
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
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
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
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
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            result.summary.contains("Background process started"),
            "{}",
            result.summary
        );
        assert!(result.summary.contains("PID:"), "{}", result.summary);
        assert!(result.summary.contains("kill"), "{}", result.summary);
        assert!(
            result.full_output.is_none(),
            "background has no full_output"
        );
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn background_false_runs_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({"command": "echo sync", "background": false});
        let result = run_shell_command(
            tmp.path(),
            &args,
            256,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
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
    fn test_format_full_output_capped_at_2mb() {
        // Each line is ~16 bytes; 200K lines ≈ 3.2 MB → should truncate.
        let stdout: Vec<String> = (0..200_000).map(|i| format!("line {i}: padding")).collect();
        let full = format_full_output(0, &stdout, &[]);
        assert!(full.len() <= 2 * 1024 * 1024 + 50); // 2MB + truncation message
        assert!(full.contains("truncated at 2MB"));
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

    #[tokio::test]
    async fn collection_stops_at_max_lines() {
        let tmp = tempfile::tempdir().unwrap();
        // Generate 50 lines of output but cap collection at 10.
        let args = serde_json::json!({
            "command": "seq 1 50"
        });
        let result = run_shell_command(
            tmp.path(),
            &args,
            10,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .await
        .unwrap();
        // Summary should reflect that we only collected 10 lines.
        assert!(
            result.summary.contains("stdout: 10 lines"),
            "Expected 10 collected lines, got: {}",
            result.summary
        );
        // Full output should NOT contain lines beyond the cap.
        let full = result.full_output.unwrap();
        assert!(full.contains("1"), "Should contain first line");
        assert!(!full.contains("\n50\n"), "Should NOT contain line 50");
    }

    #[test]
    fn test_timeout_capped_at_max() {
        // Mirrors the precedence formula in `run_shell_command` so a
        // refactor here forces a refactor there (and vice versa).
        let args = serde_json::json!({"command": "echo hi", "timeout": 99999});
        let policy = koda_sandbox::SandboxPolicy::strict_default();
        let t = args["timeout"]
            .as_u64()
            .or(policy.limits.wall_time_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(t, MAX_TIMEOUT_SECS);
    }

    // Phase 5 PR-3 of #934: timeout precedence is
    //   arg > policy.limits.wall_time_secs > DEFAULT_TIMEOUT_SECS
    // Each rung is pinned by its own test so a regression in one
    // (e.g. swapping arg and policy precedence — letting a per-agent
    // policy override an explicit per-call deadline) names the bug.

    #[test]
    fn timeout_precedence_arg_beats_policy() {
        let args = serde_json::json!({"command": "x", "timeout": 42});
        let mut policy = koda_sandbox::SandboxPolicy::strict_default();
        policy.limits.wall_time_secs = Some(120);
        let t = args["timeout"]
            .as_u64()
            .or(policy.limits.wall_time_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(
            t, 42,
            "explicit per-call timeout must win over policy default"
        );
    }

    #[test]
    fn timeout_precedence_policy_beats_legacy_default() {
        let args = serde_json::json!({"command": "x"}); // no timeout arg
        let mut policy = koda_sandbox::SandboxPolicy::strict_default();
        policy.limits.wall_time_secs = Some(45);
        let t = args["timeout"]
            .as_u64()
            .or(policy.limits.wall_time_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(
            t, 45,
            "policy-supplied wall_time_secs must beat the legacy DEFAULT_TIMEOUT_SECS const"
        );
    }

    #[test]
    fn timeout_precedence_legacy_default_when_arg_and_policy_absent() {
        let args = serde_json::json!({"command": "x"});
        let policy = koda_sandbox::SandboxPolicy::strict_default(); // wall_time_secs = None
        let t = args["timeout"]
            .as_u64()
            .or(policy.limits.wall_time_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(t, DEFAULT_TIMEOUT_SECS, "fallback chain bottom rung");
    }

    #[test]
    fn timeout_max_ceiling_clamps_policy_too() {
        // Defense against "sub-agent gets a generous wall_time policy
        // and bypasses the global DoS ceiling". The ceiling must clamp
        // *all* sources, not just user-supplied args.
        let args = serde_json::json!({"command": "x"});
        let mut policy = koda_sandbox::SandboxPolicy::strict_default();
        policy.limits.wall_time_secs = Some(99_999);
        let t = args["timeout"]
            .as_u64()
            .or(policy.limits.wall_time_secs)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        assert_eq!(
            t, MAX_TIMEOUT_SECS,
            "policy can't widen its own deadline past the hard DoS ceiling"
        );
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
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
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

    // ── Phase 1 of #934: violation annotation ────────────────────────────

    #[test]
    fn annotate_violations_skips_when_exit_code_zero() {
        // Successful command with denial-looking stderr → no annotation.
        // Otherwise `find / 2>/dev/null` would be falsely annotated every
        // time it hits an unreadable subdir.
        let mut stderr = vec!["touch: /etc/x: Permission denied".into()];
        annotate_violations(0, "find /", &mut stderr);
        assert_eq!(stderr.len(), 1, "no extra lines should be appended");
        assert!(!stderr.iter().any(|l| l.contains("<sandbox_violations>")));
    }

    #[test]
    fn annotate_violations_appends_block_on_real_denial() {
        let mut stderr = vec!["touch: /etc/passwd: Operation not permitted".into()];
        annotate_violations(1, "touch /etc/passwd", &mut stderr);
        assert!(
            stderr.iter().any(|l| l == "<sandbox_violations>"),
            "open tag must be appended: {stderr:?}"
        );
        assert!(
            stderr
                .iter()
                .any(|l| l.contains("deny file-write* /etc/passwd")),
            "violation line must be appended: {stderr:?}"
        );
        assert!(
            stderr.iter().any(|l| l == "</sandbox_violations>"),
            "close tag must be appended: {stderr:?}"
        );
    }

    #[test]
    fn annotate_violations_noop_when_no_denial_markers() {
        // Failed command (exit_code != 0) but stderr doesn't look like
        // a sandbox denial → no annotation. E.g. `cargo build` failure.
        let mut stderr = vec!["error[E0277]: trait bound not satisfied".into()];
        annotate_violations(101, "cargo build", &mut stderr);
        assert_eq!(stderr.len(), 1, "no annotation expected: {stderr:?}");
    }

    /// Phase 1 acceptance criterion of #934: a real sandboxed bash command
    /// that touches ~/.ssh returns annotated stderr.
    ///
    /// Goes through the full `run_shell_command` pipeline — sandbox build,
    /// process spawn, stream capture, kernel denial, stderr parse,
    /// `<sandbox_violations>` annotation. This is the proof-of-life test
    /// for the whole Phase 1 stack.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn run_shell_command_annotates_ssh_write_denial() {
        use serde_json::json;

        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let ssh_dir = format!("{home}/.ssh");
        if !std::path::Path::new(&ssh_dir).exists() {
            eprintln!("skip: {ssh_dir} does not exist");
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let canary = format!("{ssh_dir}/koda_phase1_annotation_canary");

        let result = run_shell_command(
            project.path(),
            &json!({"command": format!("touch {canary}")}),
            500,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
        )
        .await
        .expect("run_shell_command should succeed even when child fails");

        let full = result.full_output.expect("full output expected");
        assert!(
            full.contains("<sandbox_violations>"),
            "missing open tag in full output:\n{full}"
        );
        assert!(
            full.contains("deny file-write*"),
            "missing violation kind in full output:\n{full}"
        );
        assert!(
            full.contains("</sandbox_violations>"),
            "missing close tag in full output:\n{full}"
        );
        // Acceptance: file was not created (sandbox actually enforced).
        assert!(
            !std::path::Path::new(&canary).exists(),
            "canary file should NOT have been created: {canary}"
        );
    }
}
