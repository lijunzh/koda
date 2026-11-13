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
//! Their stdout and stderr go to a log file under the temp directory, and the
//! result names it, as Claude Code does. Without it the output was lost, and a
//! model that backgrounded a test run got nothing back.
//!
//! ## Safety
//!
//! - Commands are classified by `bash_safety::classify_bash_command`
//! - Destructive commands (`rm -rf`, `git push --force`) always need confirmation
//! - Path escapes outside the project root are flagged by `bash_path_lint`
//! - Output is capped to prevent context overflow: the model sees stderr and
//!   stdout within a ~10,000-token budget, keeping the head and the tail with
//!   the omitted lines marked in the middle (the full output is stored)
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
use koda_sandbox::process_group::{self, ProcessGroup};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Claude Code's defaults: two minutes, and at most ten for a foreground command.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling to prevent LLM-controlled DoS via huge timeout values.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Token budget for the model-facing summary (stderr + stdout together).
/// Matches Codex's `DEFAULT_MAX_OUTPUT_TOKENS`: roomy enough that ordinary
/// command output arrives whole, small enough that one noisy command can't
/// flood the context window.
const SUMMARY_MAX_TOKENS: usize = 10_000;
/// Rough bytes per token, the same approximation Codex uses.
const BYTES_PER_TOKEN: usize = 4;
const SUMMARY_MAX_BYTES: usize = SUMMARY_MAX_TOKENS * BYTES_PER_TOKEN;
/// Hard memory ceiling for line collection. Pathological commands (`yes`,
/// `cat /dev/urandom | base64`) can produce gigabytes within the timeout.
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
    /// The command exited zero, or was started in the background.
    pub success: bool,
}

/// What the Bash description says about localhost. With a kernel sandbox a
/// command may reach the servers Koda's own background commands run
/// (`BgRegistry::loopback`) and no other localhost port.
const LOCALHOST: (&str, &str, &str) = if cfg!(any(target_os = "macos", target_os = "linux")) {
    (
        "; start a server this way so later commands can connect to it on localhost",
        ", plus the localhost ports of servers Koda runs in the background",
        "other localhost ports",
    )
} else {
    ("", "", "localhost")
};

/// macOS won't run a setuid program under `sandbox-exec`, and `ps` is one, so
/// it fails with "Operation not permitted"; `pgrep` works. Linux's sandbox
/// runs `ps`.
const FIND_PROCESSES: &str = if cfg!(target_os = "macos") {
    " ps can't run in the sandbox; find processes with pgrep -fl <pattern>."
} else {
    ""
};

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Bash".to_string(),
        description: format!(
            "Execute a shell command. Use ONLY for builds, tests, git, \
            and commands without a dedicated tool. Never use for file ops \
            (use Read/Write/Edit/Grep/List instead). Suppress verbose output: \
            pipe to tail, use --quiet, avoid -v flags. \
            Output over ~10,000 tokens is shown as its first and last part with \
            a marker in the middle naming the omitted lines; narrow the command \
            (grep, sed -n 'A,Bp') to see them. \
            Set background=true for long-running processes (dev servers, watchers) \
            — returns immediately with the PID and a log file holding the output{bg_localhost}. \
            Run a command whose result you need, such as tests, in the foreground \
            with a longer timeout instead. \
            Commands run in a sandbox: they write only inside the project, /tmp and \
            cache folders, can't read ~/.ssh or other credential folders, and reach \
            the network only through a proxy that allows GitHub and the main package \
            registries over HTTP(S){own_ports}. ssh, git over SSH, other hosts, the local \
            network and {other_localhost} are unreachable.{find_processes} When a command the user \
            needs fails because of \
            the sandbox (\"Operation not permitted\", or no connection to a host the \
            proxy doesn't allow, such as a git push over SSH), run it again at once \
            with outside_sandbox=true and a reason; the user is asked to approve that \
            one command. Don't ask in a message first, and don't use it for anything \
            the sandbox allows.",
            bg_localhost = LOCALHOST.0,
            own_ports = LOCALHOST.1,
            other_localhost = LOCALHOST.2,
            find_processes = FIND_PROCESSES,
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120, max: 600; ignored when background=true). A command that needs longer belongs in the background."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run in background and return immediately with PID (default: false). \
                        Use for dev servers, file watchers, and other long-running processes. \
                        Its stdout and stderr go to a log file named in the result."
                },
                "outside_sandbox": {
                    "type": "boolean",
                    "description": "Run this one command outside the sandbox (default: false). \
                        The user must approve it, every time and in every mode. Only for a \
                        command the user needs that the sandbox blocks."
                },
                "reason": {
                    "type": "string",
                    "description": "With outside_sandbox: what the command does and why it needs \
                        to leave the sandbox, shown to the user when they approve it, e.g. \
                        \"Push the commit to origin over SSH, which the sandbox blocks\"."
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
    // Phase E of #996: who invoked this Bash call? Threaded down to
    // `spawn_background` so the bg-process registry entry is tagged
    // with the right `spawner`. Without this, a sub-agent that calls
    // `Bash{background:true}` and later tries to `CancelTask` its
    // own process gets `Forbidden` because the entry is tagged with
    // `None` while the caller is `Some(my_invocation_id)`.
    //
    // Top-level callers pass `None` and the entry is also tagged
    // with `None` — the existing top-level happy path is preserved.
    caller_spawner: Option<u32>,
) -> Result<ShellOutput> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let background = args["background"].as_bool().unwrap_or(false);
    let outside_sandbox = wants_outside_sandbox(args);

    tracing::info!(
        "Running shell command (background={background}, outside_sandbox={outside_sandbox}): [{} chars]",
        command.len()
    );

    if background {
        let mut msg = spawn_background(
            project_root,
            command,
            bg,
            trust,
            policy,
            if outside_sandbox {
                Egress::Unsandboxed
            } else {
                let loopback = koda_sandbox::Loopback {
                    serve: loopback_socket(proxy_port),
                    ..bg.loopback().await
                };
                Egress::Sandboxed {
                    proxy_port,
                    socks5_port,
                    loopback,
                }
            },
            caller_spawner,
        )?;
        if args.get("timeout").is_some_and(|t| !t.is_null()) {
            msg.push_str(
                "\nThe timeout was ignored: a background command runs until it \
                 exits, is killed, or the session ends.",
            );
        }
        return Ok(ShellOutput {
            summary: msg,
            full_output: None,
            success: true,
        });
    }

    // A `kill` of background processes this caller started is Koda's to do:
    // the sandbox can't signal a process an earlier Bash call started.
    if !outside_sandbox
        && let Some(kill) = kill_command(command)
        && let Some(done) = signal_own_background(bg, &kill, caller_spawner)
    {
        return Ok(done);
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
    let mut cmd = command_for(
        command,
        project_root,
        trust,
        policy,
        if outside_sandbox {
            Egress::Unsandboxed
        } else {
            Egress::Sandboxed {
                proxy_port,
                socks5_port,
                loopback: bg.loopback().await,
            }
        },
    )?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    process_group::isolate(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to execute command: {e}"))?;
    // Nothing here listens for a cancel, so a turn that's given up on drops
    // this call. The command mustn't carry on with nobody watching it.
    let mut group = ProcessGroup::of(&child);

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut stderr_lines: Vec<String> = Vec::new();
    let mut uncaptured: usize = 0;

    // Read stdout and stderr concurrently, streaming lines as they arrive.
    // Lines are always streamed to the TUI, but collection into Vec stops
    // once max_lines or MAX_COLLECT_BYTES is reached (OOM protection).
    let sink_info = sink.map(|(s, id)| (s, id.to_string()));
    // One deadline for both: a command can close its output and keep running.
    // The call ends when the shell does, not when its pipes close: a process
    // it started with `&` may hold them open for as long as it runs (Codex
    // likewise returns from a command that's still going).
    let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        let reading = read_streams(
            stdout,
            stderr,
            &mut stdout_lines,
            &mut stderr_lines,
            max_lines,
            &mut uncaptured,
            &sink_info,
        );
        tokio::pin!(reading);
        tokio::select! {
            read = &mut reading => {
                read.map_err(|e| anyhow::anyhow!("Stream read error: {e}"))?;
                child.wait().await.map_err(|e| anyhow::anyhow!("wait: {e}"))
            }
            status = child.wait() => {
                let status = status.map_err(|e| anyhow::anyhow!("wait: {e}"))?;
                // Output still in the pipes arrives within the grace; past it,
                // whatever holds them open is left running.
                if let Ok(read) = tokio::time::timeout(PIPE_GRACE, &mut reading).await {
                    read.map_err(|e| anyhow::anyhow!("Stream read error: {e}"))?;
                }
                Ok(status)
            }
        }
    })
    .await;

    match result {
        Ok(Ok(status)) => {
            // Processes the command left running in its group are Koda's now,
            // as in Codex: checked and stopped by Koda, reachable through the
            // sandbox, stopped when the session ends.
            let left_running = match group.take() {
                Some(leader) if still_running_after(leader, EXIT_GRACE).await => Some(leader),
                _ => None,
            };
            if let Some(leader) = left_running {
                bg.adopt(leader, command.to_string(), caller_spawner);
            }
            let exit_code = status.code().unwrap_or(-1);

            // Phase 1 of #934: surface kernel-sandbox denials to the model.
            // The kernel makes the syscall fail with EACCES/EPERM and the
            // child shell prints a libc error to stderr; parse those lines
            // into structured violations and append a CC-style block.
            // Always-on (informational only — doesn't change exit codes or
            // change which lines we already collected).
            // A command run outside the sandbox can't hit a sandbox denial;
            // an "Operation not permitted" there is the OS's own. Nor can one
            // run where no sandbox backend exists (Safe mode falls back to
            // running unsandboxed).
            if !outside_sandbox && crate::sandbox::is_available() {
                // A sub-agent, or a run whose client answers approvals itself
                // (headless), is refused anything outside the sandbox. With
                // no sink to ask (tests), keep the usual hint.
                let approvable =
                    caller_spawner.is_none() && sink.is_none_or(|(sink, _)| sink.can_ask());
                annotate_violations(exit_code, command, &mut stderr_lines, approvable);
            }

            let mut summary = format_summary(exit_code, &stdout_lines, &stderr_lines, uncaptured);
            if let Some(leader) = left_running {
                summary.push_str(&left_running_note(leader));
            }
            // Phase 5 PR-8 of #934: thread the policy-supplied output cap
            // into format_full_output. `None` keeps the historical 2 MB
            // default so today's behavior is byte-identical until a
            // future PR populates `policy.limits.max_output_bytes`.
            let max_bytes = policy
                .limits
                .max_output_bytes
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                .unwrap_or(DEFAULT_MAX_FULL_OUTPUT_BYTES);
            let full = format_full_output(exit_code, &stdout_lines, &stderr_lines, max_bytes);

            Ok(ShellOutput {
                summary,
                full_output: Some(full),
                success: exit_code == 0,
            })
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // Timeout — kill the command and all it started.
            group.kill();
            let _ = child.kill().await;
            let msg = timeout_message(timeout_secs, args["timeout"].as_u64(), command);
            Ok(ShellOutput {
                summary: msg.clone(),
                full_output: Some(msg),
                success: false,
            })
        }
    }
}

/// How long a foreground call keeps reading after its shell has exited: output
/// still in the pipes comes within it, and whatever holds them open past it
/// was left running.
const PIPE_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a group may take to empty after its shell exits before what's
/// left counts as left running: in `yes | head -1`, `yes` dies of SIGPIPE a
/// moment after the shell is done.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// Whether anything in the group `leader` led is still alive after `grace`,
/// checked every 50 ms so a group that empties sooner ends the wait.
async fn still_running_after(leader: u32, grace: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if !koda_sandbox::process_group::group_alive(leader) {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// What a foreground call says about the processes it left running.
fn left_running_note(group: u32) -> String {
    format!(
        "\n\nThe command left processes running (process group {group}). Koda now \
         tracks them as a background process: check one with Bash{{command: \"kill -0 PID\"}} \
         and stop it with Bash{{command: \"kill PID\"}}, using any PID it started, and they \
         stop when the session ends. Next time, start long-running commands with \
         background=true."
    )
}

/// What a foreground command that ran out of time reports. A `timeout` over
/// [`MAX_TIMEOUT_SECS`] was cut to it, so the model learns that, and where a
/// longer command goes.
fn timeout_message(timeout_secs: u64, asked: Option<u64>, command: &str) -> String {
    match asked {
        Some(asked) if asked > MAX_TIMEOUT_SECS => format!(
            "Command timed out after {timeout_secs}s, the most a foreground command gets \
             ({asked}s was asked for). Run it with background=true and read its log \
             instead: {command}"
        ),
        _ => format!("Command timed out after {timeout_secs}s: {command}"),
    }
}

/// Lines past the capture cap each stream still keeps from its end: a long
/// command's last lines are the ones that say how it went (a test run's
/// result, the error that stopped a build).
const CAPTURE_TAIL_LINES: usize = 100;
/// Bytes those tail lines may hold, per stream.
const CAPTURE_TAIL_BYTES: usize = MAX_COLLECT_BYTES / 10;

/// The last lines of a stream past the capture cap, kept in a rolling window.
#[derive(Default)]
struct Tail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
    dropped: usize,
}

impl Tail {
    fn keep(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > CAPTURE_TAIL_LINES || self.bytes > CAPTURE_TAIL_BYTES {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
                self.dropped += 1;
            }
        }
    }

    /// Append to `lines` what was kept: a marker for the lines dropped from
    /// the middle, then the tail. Returns how many were dropped.
    fn finish(self, lines: &mut Vec<String>) -> usize {
        if self.dropped > 0 {
            lines.push(format!(
                "[\u{2026} {} lines not kept \u{2026}]",
                self.dropped
            ));
        }
        lines.extend(self.lines);
        self.dropped
    }
}

/// Read stdout and stderr concurrently, collecting lines and optionally streaming them.
///
/// Lines are always streamed to the TUI sink (if present), but collection into
/// the Vecs is gated by two caps:
///   - `max_lines` — total stdout + stderr lines collected
///   - `MAX_COLLECT_BYTES` — total bytes collected (OOM protection)
///
/// Past either cap, each stream keeps its last [`CAPTURE_TAIL_LINES`] in a
/// rolling window, appended after a marker once the command ends, so the end
/// of a long output isn't lost along with its middle. The lines that fall
/// out of the window are counted in `uncaptured` so the summary can say so.
async fn read_streams(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdout_lines: &mut Vec<String>,
    stderr_lines: &mut Vec<String>,
    max_lines: usize,
    uncaptured: &mut usize,
    sink_info: &Option<(&dyn EngineSink, String)>,
) -> std::io::Result<()> {
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut collected_bytes: usize = 0;
    let mut collected_lines: usize = 0;
    let mut stdout_tail = Tail::default();
    let mut stderr_tail = Tail::default();

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
                        } else {
                            stdout_tail.keep(l);
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
                        } else {
                            stderr_tail.keep(l);
                        }
                    }
                    None => stderr_done = true,
                }
            }
        }
    }
    *uncaptured += stdout_tail.finish(stdout_lines) + stderr_tail.finish(stderr_lines);
    Ok(())
}

/// A new log file for a background command's output, under
/// `<temp dir>/koda-bg/`. Reads are unrestricted, so the model can `Read` it,
/// and the kernel sandbox already lets commands write to the temp directory.
fn background_log() -> Result<(std::path::PathBuf, std::fs::File)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join("koda-bg");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("Failed to create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("{}-{stamp}-{n}.log", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("Failed to create {}: {e}", path.display()))?;
    Ok((path, file))
}

/// Whether a Bash call asks to run outside the sandbox. The user approves
/// each such call: [`BashTool::classify`] makes it `Destructive`, which asks
/// in every mode and is refused where no one can answer (sub-agents, Plan,
/// headless).
pub(crate) fn wants_outside_sandbox(args: &Value) -> bool {
    args.get("outside_sandbox")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Where a command's network traffic may go.
#[derive(Clone)]
enum Egress {
    /// The kernel sandbox, with the session's egress proxies when it has them.
    Sandboxed {
        proxy_port: Option<u16>,
        socks5_port: Option<u16>,
        /// Koda's own background servers, which the command may reach.
        loopback: koda_sandbox::Loopback,
    },
    /// No sandbox and no proxy: a call the user approved with `outside_sandbox`.
    Unsandboxed,
}

/// The socket a background command serves its network namespace on, so later
/// commands can reach the servers it starts. bwrap with the egress proxy
/// only: that's when each command gets its own namespace.
fn loopback_socket(proxy_port: Option<u16>) -> Option<std::path::PathBuf> {
    if cfg!(not(target_os = "linux")) || proxy_port.is_none() {
        return None;
    }
    #[cfg(target_os = "linux")]
    return koda_sandbox::bwrap_loopback::new_socket_path()
        .inspect_err(|e| tracing::warn!("no loopback socket for a background command: {e}"))
        .ok();
    #[cfg(not(target_os = "linux"))]
    None
}

fn command_for(
    command: &str,
    project_root: &Path,
    trust: &crate::trust::TrustMode,
    policy: &koda_sandbox::SandboxPolicy,
    egress: Egress,
) -> Result<tokio::process::Command> {
    match egress {
        Egress::Sandboxed {
            proxy_port,
            socks5_port,
            loopback,
        } => crate::sandbox::build_with_loopback(
            command,
            project_root,
            trust,
            policy,
            proxy_port,
            socks5_port,
            &loopback,
        ),
        Egress::Unsandboxed => Ok(crate::sandbox::build_unsandboxed(
            command,
            project_root,
            policy,
        )),
    }
}

/// Spawn a command in the background and register it.
///
/// Returns immediately with PID, log file and instructions. Sync because
/// `spawn()` doesn't need to await — only `output()` / `wait()` block.
#[allow(clippy::too_many_arguments)]
fn spawn_background(
    project_root: &Path,
    command: &str,
    bg: &BgRegistry,
    trust: &crate::trust::TrustMode,
    policy: &koda_sandbox::SandboxPolicy,
    egress: Egress,
    caller_spawner: Option<u32>,
) -> Result<String> {
    // Spawn via sandbox wrapper (enforced for all trust modes).
    // No stdin, so the process doesn't block on terminal I/O; stdout and
    // stderr share one append-mode log, in the order they were written.
    // Phase 5 PR-2 of #934: policy threaded in from the registry
    // (see comment on `run_shell_command` above for the threading rationale).
    let (log_path, log) = background_log()?;
    let serve = match &egress {
        Egress::Sandboxed { loopback, .. } => loopback.serve.clone(),
        Egress::Unsandboxed => None,
    };
    let stderr_log = log
        .try_clone()
        .map_err(|e| anyhow::anyhow!("Failed to share {}: {e}", log_path.display()))?;
    let mut cmd = command_for(command, project_root, trust, policy, egress)?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(stderr_log);
    // Its own group, so the registry can find the ports its children listen
    // on and stop them with it (`npm run dev` and the server it starts).
    process_group::isolate(&mut cmd);
    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn background command: {e}"))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Spawned process has no PID (already exited)"))?;

    bg.insert(pid, command.to_string(), child, caller_spawner);
    if let Some(socket) = serve {
        bg.set_loopback_socket(pid, socket);
    }

    let log = log_path.display();
    Ok(format!(
        "Background process started.\n  PID:     {pid}\n  Command: {command}\n  \
         Output:  {log} (stdout and stderr; read it with Read, or Bash{{command: \"tail -50 {log}\"}})\n\
         Still running?  Bash{{command: \"kill -0 {pid}\"}}\n\
         To stop:  Bash{{command: \"kill {pid}\"}}\n\
         Send either on its own, not chained with other commands, and Koda \
         checks or stops the process itself.\n\
         Note: process will be stopped automatically when the session ends."
    ))
}

/// A command that does nothing but signal processes: `kill PID…`, with an
/// optional signal (`-9`, `-TERM`, `-s KILL`), or `kill -0 PID…`, which only
/// checks that they're running. These are the commands the background message
/// suggests; anything more (a chain, a pipe, a variable) isn't one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlainKill {
    /// `kill -0`: are they running?
    Check(Vec<u32>),
    /// Any other signal: stop them.
    Stop(Vec<u32>),
}

pub(crate) fn plain_kill(command: &str) -> Option<PlainKill> {
    let mut words: Vec<&str> = command.split_whitespace().collect();
    if words.first() != Some(&"kill") {
        return None;
    }
    words.remove(0);
    let mut check = false;
    match words.first().copied() {
        Some("-s" | "-n") => {
            check = *words.get(1)? == "0";
            words.drain(..2);
        }
        Some(flag) if flag.starts_with('-') => {
            let signal = &flag[1..];
            if signal.is_empty() || !signal.chars().all(|c| c.is_ascii_alphanumeric()) {
                return None;
            }
            check = signal == "0";
            words.remove(0);
        }
        _ => {}
    }
    if words.is_empty() {
        return None;
    }
    let pids = words
        .iter()
        .map(|w| w.parse::<u32>().ok().filter(|pid| *pid > 0))
        .collect::<Option<Vec<_>>>()?;
    Some(if check {
        PlainKill::Check(pids)
    } else {
        PlainKill::Stop(pids)
    })
}

/// A [`PlainKill`] with the `&& echo …` / `|| echo …` tails models put on a
/// check, `kill -0 PID && echo alive`, so Koda can run that too: the echo
/// prints the way the shell would print it after the kill.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct KillCommand {
    pub(crate) kill: PlainKill,
    /// Printed when the kill succeeds (`&& echo …`).
    pub(crate) on_success: Option<String>,
    /// Printed when it fails (`|| echo …`).
    pub(crate) on_failure: Option<String>,
}

/// Split `command` at `&&` and `||` outside quotes, keeping each operator.
fn split_and_or(command: &str) -> Vec<(Option<&'static str>, &str)> {
    let bytes = command.as_bytes();
    let (mut parts, mut start, mut quote, mut op) = (Vec::new(), 0, None, None);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'\'' || c == b'"' => quote = Some(c),
            None if i + 1 < bytes.len()
                && (&bytes[i..i + 2] == b"&&" || &bytes[i..i + 2] == b"||") =>
            {
                parts.push((op, &command[start..i]));
                op = Some(if c == b'&' { "&&" } else { "||" });
                start = i + 2;
                i += 1;
            }
            None => {}
        }
        i += 1;
    }
    parts.push((op, &command[start..]));
    parts
}

/// The literal text of `echo TEXT`, quoted or not, when nothing in it would
/// make the shell do more than print it.
fn literal_echo(command: &str) -> Option<String> {
    let text = command.trim().strip_prefix("echo")?;
    if !text.starts_with(char::is_whitespace) {
        return None;
    }
    let text = text.trim();
    let quoted = |q: char| {
        text.strip_prefix(q)
            .and_then(|t| t.strip_suffix(q))
            .filter(|inner| !inner.contains(q))
    };
    let inner = quoted('\'')
        .or_else(|| quoted('"').filter(|t| !t.contains(['$', '`', '\\'])))
        .or_else(|| {
            text.chars()
                .all(|c| c.is_ascii_alphanumeric() || " _.,:/-=+".contains(c))
                .then_some(text)
        })?;
    Some(inner.to_string())
}

pub(crate) fn kill_command(command: &str) -> Option<KillCommand> {
    let mut parts = split_and_or(command).into_iter();
    let (_, first) = parts.next()?;
    let mut found = KillCommand {
        kill: plain_kill(first.trim())?,
        on_success: None,
        on_failure: None,
    };
    for (op, part) in parts {
        let text = literal_echo(part)?;
        let slot = match op {
            Some("&&") => &mut found.on_success,
            _ => &mut found.on_failure,
        };
        if slot.replace(text).is_some() {
            return None;
        }
    }
    Some(found)
}

/// The caller's background entry `pid` belongs to: one it leads, or a running
/// one whose process group `pid` is in (the server a `server & echo $!`
/// started, whose shell has exited).
fn owner_of(
    pid: u32,
    own: &[crate::tools::bg_process::BgProcessSnapshot],
) -> Option<&crate::tools::bg_process::BgProcessSnapshot> {
    use crate::tools::bg_process::BgProcessStatus;
    own.iter()
        .find(|s| s.pid == pid || s.members.contains(&pid))
        .or_else(|| {
            let group = koda_sandbox::process_group::group_of(pid)?;
            own.iter()
                .find(|s| s.pid == group && s.status == BgProcessStatus::Running)
        })
}

/// Whether this Bash call only checks or stops background processes the
/// caller started, which [`run_shell_command`] then does itself. The
/// dispatchers use it to approve such a call in Auto even though `kill`
/// classifies as destructive: the process is Koda's, like a file Koda
/// created (#465).
pub(crate) fn signals_only_own_background(
    args: &Value,
    bg: &BgRegistry,
    caller_spawner: Option<u32>,
) -> bool {
    if wants_outside_sandbox(args) || args["background"].as_bool().unwrap_or(false) {
        return false;
    }
    let Some(found) = args["command"].as_str().and_then(kill_command) else {
        return false;
    };
    let (PlainKill::Check(pids) | PlainKill::Stop(pids)) = &found.kill;
    let own = bg.snapshot_for_caller(caller_spawner);
    pids.iter().all(|pid| owner_of(*pid, &own).is_some())
}

/// For a Bash call that isn't a plain `kill` but does `kill` (or `kill -0`)
/// only Koda's own background processes among other commands, the message
/// that sends the model to Koda's way: the sandbox can't signal a process an
/// earlier call started, so run as written the kill fails even if approved.
/// PIDs may be written out or read from a file (`kill $(cat app.pid)`).
/// `None` when there's no such kill, or one names a PID that isn't Koda's.
///
/// Codex stops a running command through its harness too (Ctrl-C to the exec
/// session), and a shell `kill` there meets the sandbox like any command.
pub(crate) fn own_kill_in_longer_command(
    args: &Value,
    bg: &BgRegistry,
    caller_spawner: Option<u32>,
) -> Option<String> {
    if wants_outside_sandbox(args) || args["background"].as_bool().unwrap_or(false) {
        return None;
    }
    let command = args["command"].as_str()?;
    if kill_command(command).is_some() {
        return None; // a plain kill: Koda runs it
    }
    let own = bg.snapshot_for_caller(caller_spawner);
    let mut pids: Vec<u32> = Vec::new();
    // `PID=$(cat app.pid)` (or `PID=123`) earlier in the command, for `kill $PID`.
    let mut vars: Vec<(String, String)> = Vec::new();
    for segment in command.split(['\n', ';', '|', '&']) {
        let segment = segment.trim().trim_start_matches(['(', '{', '!']).trim();
        if let Some((name, value)) = segment.split_once('=')
            && !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && let Some(value) = resolve_pid_files(value.trim_matches('"'))
            && value.parse::<u32>().is_ok()
        {
            vars.push((name.to_string(), value));
            continue;
        }
        if !segment.starts_with("kill ") {
            continue;
        }
        let mut segment = resolve_pid_files(segment)?;
        for (name, value) in &vars {
            for form in [
                format!("\"${{{name}}}\""),
                format!("\"${name}\""),
                format!("${{{name}}}"),
                format!("${name}"),
            ] {
                segment = segment.replace(&form, value);
            }
        }
        let (PlainKill::Check(found) | PlainKill::Stop(found)) =
            plain_kill(segment.split("2>").next().unwrap_or(&segment).trim())?;
        if !found.iter().all(|pid| owner_of(*pid, &own).is_some()) {
            return None;
        }
        pids.extend(found);
    }
    if pids.is_empty() {
        return None;
    }
    pids.sort_unstable();
    pids.dedup();
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "Not run: {list} {is} Koda's background {process}, and the sandbox can't signal \
         {it} from inside a longer command. Send the kill on its own and Koda does it \
         without asking: Bash{{command: \"kill {list}\"}} to stop, or \
         Bash{{command: \"kill -0 {list}\"}} to check. Run the rest of the command \
         separately.",
        is = if pids.len() == 1 { "is" } else { "are" },
        process = if pids.len() == 1 {
            "process"
        } else {
            "processes"
        },
        it = if pids.len() == 1 { "it" } else { "them" },
    ))
}

/// `kill $(cat FILE)` and `` kill `cat FILE` `` with the PID read from FILE,
/// as the shell would; the segment unchanged when there's no such read, and
/// `None` when the file can't be read.
fn resolve_pid_files(segment: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = segment;
    loop {
        let (start, open, close) = match (rest.find("$(cat "), rest.find("`cat ")) {
            (Some(a), Some(b)) if b < a => (b, "`cat ", "`"),
            (Some(a), _) => (a, "$(cat ", ")"),
            (None, Some(b)) => (b, "`cat ", "`"),
            (None, None) => break,
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let end = after.find(close)?;
        let path = after[..end].trim().trim_matches(['"', '\'']);
        out.push_str(std::fs::read_to_string(path).ok()?.trim());
        rest = &after[end + close.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// Check or stop background processes through the registry, when every PID
/// is one the caller started. `None` means at least one isn't, and the
/// command runs as written.
fn signal_own_background(
    bg: &BgRegistry,
    found: &KillCommand,
    caller_spawner: Option<u32>,
) -> Option<ShellOutput> {
    use crate::tools::bg_process::BgProcessStatus;
    let kill = &found.kill;
    let (PlainKill::Check(pids) | PlainKill::Stop(pids)) = kill;
    let own = bg.snapshot_for_caller(caller_spawner);
    // A PID is this caller's when it leads one of its groups, or is in a
    // group of its that's still running: the server a `server & echo $!`
    // started, whose shell has exited.
    let entries = pids
        .iter()
        .map(|pid| owner_of(*pid, &own).map(|entry| (*pid, entry)))
        .collect::<Option<Vec<_>>>()?;
    for (pid, entry) in &entries {
        bg.note_member(entry.pid, *pid);
    }
    let mut lines = Vec::new();
    let mut all_running = true;
    for (pid, entry) in entries {
        let command = &entry.command;
        // A member's own state for a check; the group's for its leader.
        let member_gone = pid != entry.pid && !koda_sandbox::process_group::process_alive(pid);
        let gone = match entry.status {
            BgProcessStatus::Running if member_gone => Some("exited".to_string()),
            BgProcessStatus::Running => None,
            BgProcessStatus::Exited { code: Some(code) } => {
                Some(format!("exited with code {code}"))
            }
            BgProcessStatus::Exited { code: None } => Some("exited".to_string()),
            BgProcessStatus::Killed => Some("stopped".to_string()),
        };
        match (kill, gone) {
            (PlainKill::Check(_), None) => lines.push(format!("{pid} is running: {command}")),
            (PlainKill::Check(_), Some(how)) => {
                all_running = false;
                lines.push(format!("{pid} is not running ({how}): {command}"));
            }
            (PlainKill::Stop(_), None) => {
                // The snapshot was this caller's, so the kill is allowed. It
                // stops the whole group, as closing a Codex exec session does.
                let _ = bg.kill_as_caller(entry.pid, caller_spawner);
                lines.push(format!("Stopped background process {pid}: {command}"));
            }
            (PlainKill::Stop(_), Some(how)) => {
                lines.push(format!("{pid} had already {how}: {command}"));
            }
        }
    }
    // `kill -0` fails when a process is gone; stopping one that already
    // ended isn't a failure.
    let killed = all_running || matches!(kill, PlainKill::Stop(_));
    // Then the echo the shell would run, and the status it would end with.
    let echo = if killed {
        &found.on_success
    } else {
        &found.on_failure
    };
    if let Some(text) = echo {
        lines.push(text.clone());
    }
    let success = killed || found.on_failure.is_some();
    Some(ShellOutput {
        summary: format_summary(i32::from(!success), &lines, &[], 0),
        full_output: None,
        success,
    })
}

/// Build a compact summary for the model's context window.
///
/// Stderr comes first (high-signal: errors and warnings), then stdout. Both
/// share a [`SUMMARY_MAX_BYTES`] budget. Output that fits is passed through
/// whole; otherwise each stream keeps its head and its tail (whole lines) and
/// a marker in the middle says which lines were left out, so the model never
/// mistakes a partial view for the full output. This is Codex's shape
/// (`format_exec_output_for_model`): head + tail within a token budget, an
/// explicit marker, and a header with the original size.
fn format_summary(
    exit_code: i32,
    stdout_lines: &[String],
    stderr_lines: &[String],
    uncaptured: usize,
) -> String {
    let mut out = format!(
        "Exit code: {exit_code} | stdout: {} lines | stderr: {} lines",
        stdout_lines.len(),
        stderr_lines.len(),
    );

    let stdout_bytes = lines_cost(stdout_lines);
    let stderr_bytes = lines_cost(stderr_lines);
    let total_bytes = stdout_bytes + stderr_bytes;
    let truncated = total_bytes > SUMMARY_MAX_BYTES;

    // Each stream may use up to half the budget; whatever one leaves unused
    // goes to the other.
    let stderr_budget =
        stderr_bytes.min(SUMMARY_MAX_BYTES - stdout_bytes.min(SUMMARY_MAX_BYTES / 2));
    let stdout_budget = SUMMARY_MAX_BYTES - stderr_budget;

    if uncaptured > 0 {
        // The collection cap (`OutputCaps::shell_output_lines`) stopped capture:
        // what follows is the start of the output and its last lines, with a
        // `[… N lines not kept …]` marker where the middle was.
        out.push_str(&format!(
            "\nWarning: truncated output: {uncaptured} lines from the middle were not \
             kept; the start and the last lines are below"
        ));
    }
    if truncated {
        out.push_str(&format!(
            "\nWarning: truncated output (original token count: {})\nTotal output lines: {}",
            tokens_for_bytes(total_bytes),
            stdout_lines.len() + stderr_lines.len(),
        ));
    }

    for (name, lines, budget) in [
        ("stderr", stderr_lines, stderr_budget),
        ("stdout", stdout_lines, stdout_budget),
    ] {
        if !lines.is_empty() {
            out.push_str(&format!("\n\n--- {name} ({} lines) ---\n", lines.len()));
            out.push_str(&head_and_tail(lines, budget));
        }
    }

    if truncated || uncaptured > 0 {
        out.push_str(
            "\n\nTo see what is missing, narrow the command (grep, sed -n 'A,Bp', tail). \
             The captured output is also stored; RecallContext can search it.",
        );
    }

    out
}

/// Bytes `lines` take in the summary: the text plus one newline per line.
fn lines_cost(lines: &[String]) -> usize {
    lines.iter().map(|l| l.len() + 1).sum()
}

fn tokens_for_bytes(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Join `lines`, or, if they cost more than `budget` bytes, keep whole lines
/// from the start and end (half the budget each) around a marker naming the
/// omitted line range.
///
/// A side that can't fit even one whole line (a minified blob on one line) keeps
/// as much of that line as fits instead, so the model still sees something.
fn head_and_tail(lines: &[String], budget: usize) -> String {
    let total = lines.len();
    if lines_cost(lines) <= budget {
        return lines.join("\n");
    }
    let (head_budget, tail_budget) = (budget / 2, budget - budget / 2);

    let mut head: Vec<&str> = Vec::new();
    let mut used = 0;
    for line in lines {
        used += line.len() + 1;
        if used > head_budget {
            break;
        }
        head.push(line);
    }
    let mut tail_start = total;
    let mut used = 0;
    for (i, line) in lines.iter().enumerate().skip(head.len().max(1)).rev() {
        used += line.len() + 1;
        if used > tail_budget {
            break;
        }
        tail_start = i;
    }

    // Nothing whole fits on a side: clip the line next to the gap. A single
    // over-budget line has no gap to speak of, so clip its middle.
    if total == 1 {
        let line = &lines[0];
        let (cut_head, cut_tail) = (
            line.floor_char_boundary(head_budget),
            line.ceil_char_boundary(line.len() - tail_budget),
        );
        return format!(
            "{}…{} bytes of this line truncated…{}",
            &line[..cut_head],
            cut_tail - cut_head,
            &line[cut_tail..],
        );
    }
    let mut head_text = head.join("\n");
    let mut head_shown = head.len();
    if head.is_empty() {
        let line = &lines[0];
        head_text = format!(
            "{}…[line cut]",
            &line[..line.floor_char_boundary(head_budget)]
        );
        head_shown = 1;
    }
    let mut tail_text = lines[tail_start..].join("\n");
    let mut tail_from = tail_start;
    if tail_start == total {
        let line = &lines[total - 1];
        tail_text = format!(
            "[line cut]…{}",
            &line[line.ceil_char_boundary(line.len() - tail_budget)..]
        );
        tail_from = total - 1;
    }

    let omitted: usize = lines[head_shown..tail_from]
        .iter()
        .map(|l| l.len() + 1)
        .sum();
    let marker = if head_shown < tail_from {
        format!(
            "…{} lines truncated (lines {}-{} of {total}, ~{} tokens)…",
            tail_from - head_shown,
            head_shown + 1,
            tail_from,
            tokens_for_bytes(omitted),
        )
    } else {
        "…truncated…".to_string()
    };
    format!("{head_text}\n{marker}\n{tail_text}")
}

/// Default per-command full-output cap when the active sandbox policy
/// doesn't override it. 2 MB is the historical value — generous enough
/// for RecallContext to find errors deep in build/test output, while
/// still preventing pathological commands from bloating the SQLite DB.
///
/// Per-policy override comes in via `policy.limits.max_output_bytes`,
/// threaded through to [`format_full_output`] (Phase 5 PR-8 of #934 —
/// the last declared-but-not-enforced field in the sandbox policy).
const DEFAULT_MAX_FULL_OUTPUT_BYTES: usize = 2 * 1024 * 1024; // 2 MB

/// Render a byte count as a human-friendly string for the truncation
/// marker. Round numbers get clean labels ("2MB", "512KB"); everything
/// else falls back to raw bytes so the model + human reader can always
/// tell exactly where the cap landed.
fn format_byte_cap(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB && bytes.is_multiple_of(MB) {
        format!("{}MB", bytes / MB)
    } else if bytes >= KB && bytes.is_multiple_of(KB) {
        format!("{}KB", bytes / KB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Build the full output for DB storage.
///
/// Stored in `messages.full_content` and searchable via RecallContext.
///
/// `max_bytes` is the hard cap. The caller computes it from
/// `policy.limits.max_output_bytes`, falling back to
/// [`DEFAULT_MAX_FULL_OUTPUT_BYTES`] when the policy is silent. Threading
/// the cap as a parameter (rather than reading the policy here) keeps
/// this function trivially testable without spinning up a `SandboxPolicy`.
fn format_full_output(
    exit_code: i32,
    stdout_lines: &[String],
    stderr_lines: &[String],
    max_bytes: usize,
) -> String {
    let mut out = format!("Exit code: {exit_code}\n");
    if !stdout_lines.is_empty() {
        out.push_str("\n--- stdout ---\n");
        out.push_str(&stdout_lines.join("\n"));
    }
    if !stderr_lines.is_empty() {
        out.push_str("\n\n--- stderr ---\n");
        out.push_str(&stderr_lines.join("\n"));
    }

    // Hard cap to prevent DB bloat from pathological commands. Cap is
    // policy-controlled (see `max_bytes` doc above); the message names
    // the actual byte count so a future per-trust-mode override is
    // legible to the model and the human reading the DB.
    if out.len() > max_bytes {
        out.truncate(out.floor_char_boundary(max_bytes));
        out.push_str(&format!(
            "\n\n[... output truncated at {} ...]",
            format_byte_cap(max_bytes)
        ));
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
///
/// `approvable` says whether someone can approve a run outside the sandbox:
/// the hint after the block offers that way out only then, so a sub-agent or
/// a headless run isn't sent toward a call it will be refused.
pub(crate) fn annotate_violations(
    exit_code: i32,
    command: &str,
    stderr_lines: &mut Vec<String>,
    approvable: bool,
) {
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
        stderr_lines.push(
            if approvable {
                OUTSIDE_SANDBOX_HINT
            } else {
                NO_WAY_OUT_HINT
            }
            .to_string(),
        );
    }
}

/// Follows a sandbox denial, so the model knows the way out and that the
/// user decides, rather than guessing at the cause (it once took a blocked
/// `nc` to mean the user was off their home network).
const OUTSIDE_SANDBOX_HINT: &str = "Koda's sandbox blocked this command. If the user \
     needs it, run it again with Bash, outside_sandbox=true and a reason; they'll be \
     asked to approve it.";

/// [`OUTSIDE_SANDBOX_HINT`] where no one can approve: a sub-agent, or a
/// headless run.
const NO_WAY_OUT_HINT: &str = "Koda's sandbox blocked this command, and this run can't \
     leave the sandbox: no one is here to approve it. Do without it, or say which \
     command the user should run themselves.";

// =============================================================
// Tool trait implementation (#1265 item 5, PR-6/N).
//
// `Bash` is the most state-heavy built-in: it reads bg_registry,
// trust, sandbox_policy, both proxy ports, the streaming sink, and
// the caller-spawner id off the context. PR-6 added all of those
// to `ToolExecCtx`.
//
// Bash is also the **showcase** for input-aware classification:
// `classify(args)` extracts the command string and delegates to
// `bash_safety::classify_bash_command`, so `rm -rf /` classifies
// `Destructive` while `echo hi` classifies `ReadOnly`. Pre-#1265
// this happened in a *separate* match arm in `tools::classify_tool`
// (which then delegated to `bash_safety` itself). Folding it into
// the trait closes the per-tool/per-call gap that `tools::
// classify_tool` had to paper over.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `Bash` — sandboxed shell command execution.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "Bash")
            .expect("shell::definitions() must contain Bash")
    }
    /// Per-call classification: extract `command` and delegate to
    /// the `bash_safety` analyzer. Falls back to `Destructive` if
    /// the args are malformed (defensive: better to over-gate than
    /// to under-gate when we can't read the command).
    fn classify(&self, args: &serde_json::Value) -> ToolEffect {
        // Leaving the sandbox needs the user's yes whatever the command
        // does: `Destructive` asks in Auto too, and is refused where no one
        // can answer.
        if wants_outside_sandbox(args) {
            return ToolEffect::Destructive;
        }
        match args.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => crate::bash_safety::classify_bash_command(cmd),
            None => ToolEffect::Destructive,
        }
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        // Bash is special-cased on result mapping: success returns
        // a `ShellOutput { summary, full_output }` rather than a
        // `Result<String>`, so we can't use `wrap_result`. We open-
        // code the conversion here, matching the legacy match arm
        // byte-for-byte (#1232 §4 `{:#}` for error chains).
        match run_shell_command(
            ctx.project_root,
            args,
            ctx.caps.shell_output_lines,
            ctx.bg_registry,
            ctx.sink,
            ctx.trust,
            ctx.sandbox_policy,
            ctx.proxy_port,
            ctx.socks5_port,
            ctx.caller_spawner,
        )
        .await
        {
            Ok(so) => ToolResult {
                output: so.summary,
                success: so.success,
                full_output: so.full_output,
            },
            Err(e) => ToolResult {
                output: format!("Error: {e:#}"),
                success: false,
                full_output: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bg_process::{BgProcessStatus, BgRegistry};

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

    /// Phase 5 PR-8 of #934: end-to-end test that
    /// `policy.limits.max_output_bytes` actually flows through
    /// `run_shell_command` to truncate `full_output`. Pins the
    /// integration that the unit tests on `format_full_output` can't
    /// see — without this, a future refactor could silently drop the
    /// policy lookup at the call site and only the unit tests would
    /// stay green (because they call `format_full_output` directly).
    ///
    /// Generates ~10KB of stdout under a 1KB cap and asserts the
    /// stored full_output respects the cap + carries the policy-aware
    /// truncation marker.
    #[tokio::test]
    async fn run_shell_command_honors_policy_max_output_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        // ~10KB of output: yes printed 1000 times of a 10-char string.
        let args = serde_json::json!({
            "command": "yes 'aaaaaaaaa' | head -1000"
        });
        let mut policy = koda_sandbox::SandboxPolicy::strict_default();
        policy.limits.max_output_bytes = Some(1024); // 1 KB cap

        let result = run_shell_command(
            tmp.path(),
            &args,
            2000,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &policy,
            None,
            None,
            None,
        )
        .await
        .expect("shell command must succeed");

        let full = result.full_output.expect("full_output should be populated");
        assert!(
            full.len() <= 1024 + 50,
            "full_output ({} bytes) must respect policy cap (1024) + small marker overhead",
            full.len()
        );
        assert!(
            full.contains("truncated at 1KB"),
            "truncation marker should name the policy-supplied cap, got tail: {:?}",
            full.lines().last()
        );
    }

    /// Run `command` in the foreground with the arguments these tests share.
    async fn run_in(
        dir: &std::path::Path,
        args: &serde_json::Value,
    ) -> anyhow::Result<ShellOutput> {
        run_shell_command(
            dir,
            args,
            256,
            &BgRegistry::new(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn dropping_a_foreground_call_kills_all_it_started() {
        let tmp = tempfile::tempdir().unwrap();
        // The shell's next line, and a job it forked.
        let command = "(sleep 1; touch forked) & sleep 1; touch finished";
        let args = serde_json::json!({ "command": command });
        let call = run_in(tmp.path(), &args);
        let cut_off = tokio::time::timeout(std::time::Duration::from_millis(300), call);
        assert!(cut_off.await.is_err(), "the command was still running");

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!tmp.path().join("finished").exists());
        assert!(!tmp.path().join("forked").exists());
    }

    #[tokio::test]
    async fn a_timeout_kills_all_the_command_started() {
        let tmp = tempfile::tempdir().unwrap();
        let command = "(sleep 2; touch forked) & sleep 30";
        let args = serde_json::json!({ "command": command, "timeout": 1 });
        let result = run_in(tmp.path(), &args).await.unwrap();
        assert!(!result.success, "{}", result.summary);

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!tmp.path().join("forked").exists());
    }

    #[tokio::test]
    async fn the_timeout_holds_after_a_command_closes_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let command = "exec > /dev/null 2>&1; sleep 30";
        let args = serde_json::json!({ "command": command, "timeout": 1 });
        let call = run_in(tmp.path(), &args);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("the call outlived its timeout")
            .unwrap();
        assert!(!result.success, "{}", result.summary);
        assert!(result.summary.contains("timed out"), "{}", result.summary);
    }

    /// Run `args` against `registry`, the session's background registry.
    async fn run_with(
        dir: &std::path::Path,
        registry: &BgRegistry,
        args: serde_json::Value,
    ) -> ShellOutput {
        run_shell_command(
            dir,
            &args,
            256,
            registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// The first number in a call's output: the PID an `echo $!` printed.
    fn first_pid(out: &ShellOutput) -> u32 {
        let text = out
            .full_output
            .clone()
            .unwrap_or_else(|| out.summary.clone());
        text.split(|c: char| !c.is_ascii_digit())
            .filter_map(|w| w.parse().ok())
            .find(|n: &u32| *n > 1)
            .unwrap_or_else(|| panic!("no PID in {text}"))
    }

    /// What a finished command left running is Koda's, as in Codex: it keeps
    /// running, the result says so, and it shows as a background process.
    #[tokio::test]
    async fn a_finished_command_hands_its_background_jobs_to_koda() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let command = "(sleep 1; touch survived) > /dev/null 2>&1 &";
        let result = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": command }),
        )
        .await;
        assert!(result.success, "{}", result.summary);
        assert!(
            result.summary.contains("left processes running"),
            "{}",
            result.summary
        );
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1, "{snap:?}");
        assert_eq!(snap[0].status, BgProcessStatus::Running);

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(tmp.path().join("survived").exists());
        // Once nothing in the group is left, the entry has exited.
        assert!(matches!(
            registry.snapshot()[0].status,
            BgProcessStatus::Exited { .. }
        ));
    }

    /// Koda's background processes stop with the session, taken-over ones too.
    #[tokio::test]
    async fn left_running_jobs_stop_when_the_session_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let command = "(sleep 1; touch too_late) > /dev/null 2>&1 &";
        run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": command }),
        )
        .await;
        drop(registry);
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!tmp.path().join("too_late").exists());
    }

    /// A job that keeps the call's output pipe open doesn't hold the call
    /// until its timeout: the call ends when its shell does. `kill -0` and
    /// `kill` then work on the job's own PID, without asking.
    #[tokio::test]
    async fn a_job_holding_the_pipe_doesnt_hold_the_call() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let started = std::time::Instant::now();
        let result = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": "sleep 30 & echo $!", "timeout": 20 }),
        )
        .await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(result.success, "{}", result.summary);
        let pid = first_pid(&result);

        let check = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill -0 {pid}") }),
        )
        .await;
        assert!(
            check.summary.contains(&format!("{pid} is running")),
            "{}",
            check.summary
        );
        let stop = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill {pid}") }),
        )
        .await;
        assert!(
            stop.summary.contains("Stopped background process"),
            "{}",
            stop.summary
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!koda_sandbox::process_group::process_alive(pid));
    }

    /// `server & echo $!` run with background=true: the shell Koda started
    /// exits at once, but the server is in its group, so the entry stays
    /// running and the server's PID is Koda's to check.
    #[tokio::test]
    async fn a_background_shell_that_exits_keeps_its_jobs_koda_s() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let log = tmp.path().join("pid");
        let command = format!("sleep 30 > /dev/null 2>&1 & echo $! > {}", log.display());
        run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": command, "background": true }),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let pid: u32 = std::fs::read_to_string(&log)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let snap = registry.snapshot();
        assert_eq!(snap[0].status, BgProcessStatus::Running, "{snap:?}");

        let check = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill -0 {pid}") }),
        )
        .await;
        assert!(
            check.summary.contains(&format!("{pid} is running")),
            "{}",
            check.summary
        );
        let stop = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill {pid}") }),
        )
        .await;
        assert!(
            stop.summary.contains("Stopped background process"),
            "{}",
            stop.summary
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!koda_sandbox::process_group::process_alive(pid));
    }

    /// A `kill` of Koda's own process inside a longer command gets the hint;
    /// a plain kill, a PID that isn't Koda's, or a PID Koda can't read don't.
    #[tokio::test]
    async fn a_chained_kill_of_koda_s_process_gets_the_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let started = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": "sleep 30 > /dev/null 2>&1 & echo $!" }),
        )
        .await;
        let pid = first_pid(&started);
        let pidfile = tmp.path().join("app.pid");
        std::fs::write(&pidfile, format!("{pid}\n")).unwrap();
        let hint = |command: String| {
            own_kill_in_longer_command(&serde_json::json!({ "command": command }), &registry, None)
        };

        for command in [
            format!("kill {pid}; sleep 1; kill -0 {pid} && echo up || echo down"),
            format!("kill -0 {pid} 2>/dev/null && echo alive; cat log.txt"),
            format!("kill $(cat {}); sleep 1; echo done", pidfile.display()),
            format!(
                "curl -s localhost:1/health; kill `cat {}`",
                pidfile.display()
            ),
        ] {
            let text = hint(command.clone()).unwrap_or_else(|| panic!("no hint for {command}"));
            assert!(
                text.starts_with(&format!("Not run: {pid} is Koda's")),
                "{text}"
            );
            assert!(text.contains(&format!("kill {pid}")), "{text}");
        }
        // A plain kill is Koda's to run, not to explain.
        assert_eq!(hint(format!("kill {pid}")), None);
        // A PID that isn't Koda's, alone or beside one that is.
        assert_eq!(hint("kill 1; echo done".into()), None);
        assert_eq!(hint(format!("kill {pid} 1; echo done")), None);
        // A PID file that can't be read, or a variable: nothing to go on.
        assert_eq!(hint("kill $(cat /nonexistent/app.pid); echo".into()), None);
        assert_eq!(hint("PID=1; kill $PID; echo".into()), None);
        // No kill at all.
        assert_eq!(hint("echo hi; ls".into()), None);
        // A variable read from the PID file earlier in the command (Haiku,
        // 2026-11-10).
        let with_var = format!(
            "PID=$(cat {}); kill $PID; sleep 1; kill -0 \"$PID\" && echo up || echo down",
            pidfile.display()
        );
        assert!(hint(with_var.clone()).is_some(), "{with_var}");

        // Stopped through Koda, the PID stays known: a check chained after the
        // stop still gets the hint, and the plain check says it's gone.
        let stop = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill {pid}") }),
        )
        .await;
        assert!(
            stop.summary.contains("Stopped background process"),
            "{}",
            stop.summary
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!koda_sandbox::process_group::process_alive(pid));
        assert!(
            hint(format!(
                "kill -0 {pid} && echo up || echo down; curl -s localhost:1"
            ))
            .is_some()
        );
        let check = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": format!("kill -0 {pid}") }),
        )
        .await;
        assert!(
            check.summary.contains(&format!("{pid} is not running")),
            "{}",
            check.summary
        );
        let _ = started_group;
    }

    /// The one entry in `registry`: the group a test's call left running.
    fn started_group(registry: &BgRegistry) -> u32 {
        registry.snapshot()[0].pid
    }

    /// A pipeline whose upstream dies a moment after the shell isn't "left
    /// running".
    #[tokio::test]
    async fn a_pipeline_that_finishes_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let result = run_with(
            tmp.path(),
            &registry,
            serde_json::json!({ "command": "yes | head -1" }),
        )
        .await;
        assert!(result.success, "{}", result.summary);
        assert!(
            !result.summary.contains("left processes running"),
            "{}",
            result.summary
        );
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn a_command_reading_stdin_sees_end_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let args = serde_json::json!({ "command": "cat; echo done" });
        let result = run_in(tmp.path(), &args).await.unwrap();
        assert!(result.success, "{}", result.summary);
        assert!(result.full_output.unwrap().contains("done"));
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
        assert!(
            !result.summary.contains("timeout was ignored"),
            "{}",
            result.summary
        );
    }

    /// The output of a background command lands in the log the result
    /// names, stdout and stderr both, and a passed timeout is reported as
    /// ignored. Before, both streams went to /dev/null and the result said
    /// nothing about either (seen on Ornith: a backgrounded test run with
    /// `timeout: 600` came back with only a PID).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_output_goes_to_the_named_log() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let args = serde_json::json!({
            "command": "echo to-stdout; echo to-stderr >&2",
            "background": true,
            "timeout": 600,
        });
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
            None,
        )
        .await
        .unwrap();
        assert!(
            result.summary.contains("timeout was ignored"),
            "{}",
            result.summary
        );
        let log = result
            .summary
            .lines()
            .find_map(|l| l.trim().strip_prefix("Output:"))
            .and_then(|rest| rest.trim().split(' ').next())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("no log path in {}", result.summary));
        let mut text = String::new();
        for _ in 0..100 {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("to-stdout") && text.contains("to-stderr") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            text.contains("to-stdout"),
            "log {}: {text:?}",
            log.display()
        );
        assert!(
            text.contains("to-stderr"),
            "log {}: {text:?}",
            log.display()
        );
        let _ = std::fs::remove_file(&log);
    }

    /// Phase E of #996 regression: when a sub-agent (caller_spawner =
    /// `Some(N)`) does `Bash{background:true}`, the bg-process entry
    /// must be tagged with the same `Some(N)`. Without this fix, the
    /// entry was hard-coded `None`, which meant the sub-agent's later
    /// `CancelTask` / `WaitTask` (also `Some(N)`) would get
    /// `Forbidden` because `None != Some(N)`.
    ///
    /// We assert at the registry level (`snapshot()` exposes the
    /// `spawner` field) rather than driving CancelTask, because the
    /// scoping check is what this PR fixes — the cancel mechanics are
    /// already covered by `bg_process::tests::kill_as_caller_*`.
    #[tokio::test]
    async fn background_spawn_tags_entry_with_caller_spawner() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = BgRegistry::new();
        let args = serde_json::json!({"command": "sleep 60", "background": true});
        let _ = run_shell_command(
            tmp.path(),
            &args,
            256,
            &registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            Some(42), // sub-agent invocation id
        )
        .await
        .unwrap();

        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1, "one bg process expected");
        assert_eq!(
            snap[0].spawner,
            Some(42),
            "bg-process entry must carry the caller's spawner id so a \
             same-spawner CancelTask doesn't get Forbidden"
        );
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
        let summary = format_summary(0, &stdout, &stderr, 0);
        assert!(summary.contains("Exit code: 0"));
        assert!(summary.contains("stdout: 2 lines"));
        assert!(summary.contains("hello"));
        assert!(summary.contains("world"));
        // Short output should NOT have the RecallContext hint
        assert!(!summary.contains("RecallContext"));
    }

    /// `n` numbered lines of about 70 bytes each.
    fn numbered_lines(n: usize) -> Vec<String> {
        (1..=n)
            .map(|i| format!("line {i:05} {}", "x".repeat(60)))
            .collect()
    }

    #[test]
    fn test_format_summary_moderate_output_is_whole() {
        // Far past the old 20-line tail, well inside the budget: nothing is cut.
        let stdout = numbered_lines(300);
        let summary = format_summary(0, &stdout, &[], 0);
        assert!(summary.contains("--- stdout (300 lines) ---"));
        assert!(summary.contains("line 00001 "));
        assert!(summary.contains("line 00150 "));
        assert!(summary.contains("line 00300 "));
        assert!(!summary.contains("truncated"));
        assert!(!summary.contains("RecallContext"));
    }

    #[test]
    fn test_format_summary_long_stdout_keeps_head_and_tail() {
        let stdout = numbered_lines(2000);
        let summary = format_summary(0, &stdout, &[], 0);
        // Header names the original size.
        assert!(summary.contains("Warning: truncated output (original token count:"));
        assert!(summary.contains("Total output lines: 2000"));
        // Head and tail survive, the middle does not.
        assert!(summary.contains("line 00001 "));
        assert!(summary.contains("line 02000 "));
        assert!(!summary.contains("line 01000 "));
        // The marker says which lines are missing, so a follow-up `sed -n`
        // can be aimed at them.
        let marker = summary
            .lines()
            .find(|l| l.contains("lines truncated"))
            .expect("marker line");
        assert!(marker.contains("of 2000"), "{marker}");
        assert!(summary.contains("RecallContext"));
        // Stays inside the budget (plus a few hundred bytes of framing).
        assert!(summary.len() < SUMMARY_MAX_BYTES + 500, "{}", summary.len());
    }

    #[test]
    fn test_format_summary_marker_range_matches_omitted_lines() {
        let stdout = numbered_lines(2000);
        let summary = format_summary(0, &stdout, &[], 0);
        let marker = summary
            .lines()
            .find(|l| l.contains("lines truncated"))
            .unwrap();
        // "…N lines truncated (lines A-B of 2000, ~T tokens)…"
        let range = marker.split("(lines ").nth(1).unwrap();
        let (a, rest) = range.split_once('-').unwrap();
        let (b, _) = rest.split_once(' ').unwrap();
        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
        // The line just before A and just after B are present; A and B are not.
        assert!(summary.contains(&format!("line {:05} ", a - 1)));
        assert!(summary.contains(&format!("line {:05} ", b + 1)));
        assert!(!summary.contains(&format!("line {a:05} ")));
        assert!(!summary.contains(&format!("line {b:05} ")));
    }

    #[test]
    fn test_format_summary_stderr_first_with_long_stdout() {
        let stdout = numbered_lines(2000);
        let stderr: Vec<String> = vec!["warning: something".into(), "error: boom".into()];
        let summary = format_summary(1, &stdout, &stderr, 0);
        assert!(summary.starts_with("Exit code: 1 | stdout: 2000 lines | stderr: 2 lines"));
        let (err_at, out_at) = (
            summary.find("--- stderr (2 lines) ---").unwrap(),
            summary.find("--- stdout (2000 lines) ---").unwrap(),
        );
        assert!(err_at < out_at);
        // Short stderr is complete, and costs stdout almost nothing.
        assert!(summary.contains("warning: something\nerror: boom"));
        assert!(summary.contains("line 00001 "));
        assert!(summary.contains("line 02000 "));
        assert!(summary.contains("Total output lines: 2002"));
    }

    #[test]
    fn test_format_summary_long_stderr_cannot_starve_stdout() {
        let stdout = numbered_lines(10);
        let stderr = numbered_lines(2000);
        let summary = format_summary(2, &stdout, &stderr, 0);
        // Stdout fits whole; stderr keeps head and tail.
        assert!(summary.contains("--- stdout (10 lines) ---"));
        assert!(summary.contains("line 00010 "));
        assert!(summary.contains("lines truncated"));
        assert!(summary.len() < SUMMARY_MAX_BYTES + 500, "{}", summary.len());
    }

    #[test]
    fn test_format_summary_both_streams_long_share_budget() {
        let summary = format_summary(1, &numbered_lines(2000), &numbered_lines(2000), 0);
        assert_eq!(summary.matches("lines truncated").count(), 2);
        assert!(summary.len() < SUMMARY_MAX_BYTES + 800, "{}", summary.len());
    }

    /// Past the capture cap the end of the output is kept too: a long test
    /// run's result line was what the model never saw.
    #[tokio::test]
    async fn a_long_output_keeps_its_last_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run_shell_command(
            tmp.path(),
            &serde_json::json!({"command": "seq 1 5000; echo 'test result: FAILED. 1 failed' >&2"}),
            50,
            &bg(),
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let s = &out.summary;
        assert!(s.contains("\n1\n2\n"), "the start: {s}");
        assert!(s.contains("\n5000"), "the last line: {s}");
        assert!(s.contains("test result: FAILED"), "stderr's end: {s}");
        assert!(s.contains("lines not kept \u{2026}]"), "the marker: {s}");
        assert!(s.contains("lines from the middle were not kept"), "{s}");
        assert!(!s.contains("\n2500\n"), "the middle is dropped: {s}");
    }

    #[test]
    fn a_tail_keeps_a_rolling_window_of_the_last_lines() {
        let mut tail = Tail::default();
        for i in 0..250 {
            tail.keep(format!("l{i}"));
        }
        let mut lines = vec!["head".to_string()];
        assert_eq!(tail.finish(&mut lines), 150);
        assert_eq!(lines[1], "[\u{2026} 150 lines not kept \u{2026}]");
        assert_eq!(lines[2], "l150");
        assert_eq!(lines.last().unwrap(), "l249");
        assert_eq!(lines.len(), 2 + CAPTURE_TAIL_LINES);
        // Under the window: kept whole, no marker.
        let mut tail = Tail::default();
        tail.keep("only".into());
        let mut lines = Vec::new();
        assert_eq!(tail.finish(&mut lines), 0);
        assert_eq!(lines, vec!["only"]);
    }

    #[test]
    fn test_format_summary_says_when_capture_stopped_early() {
        // Few enough lines to fit the budget, but the collection cap dropped
        // 99,744 more: the model must not read this as the whole output.
        let summary = format_summary(0, &numbered_lines(256), &[], 99_744);
        assert!(summary.contains("99744 lines from the middle were not kept"));
        assert!(summary.contains("stdout: 256 lines"));
        assert!(summary.contains("narrow the command"));
        // Nothing dropped, nothing cut: no warning at all.
        assert!(!format_summary(0, &numbered_lines(256), &[], 0).contains("Warning"));
    }

    #[test]
    fn test_format_summary_single_huge_line_is_clipped_in_the_middle() {
        let blob = format!("START{}END", "z".repeat(200_000));
        let summary = format_summary(0, &[blob], &[], 0);
        assert!(summary.contains("START"));
        assert!(summary.contains("END"));
        assert!(summary.contains("bytes of this line truncated"));
        assert!(summary.len() < SUMMARY_MAX_BYTES + 500, "{}", summary.len());
    }

    #[test]
    fn test_format_summary_huge_lines_on_both_ends_still_show_something() {
        let big = |c: &str| c.repeat(100_000);
        let stdout = vec![big("a"), "middle".to_string(), big("b")];
        let summary = format_summary(0, &stdout, &[], 0);
        assert!(summary.contains("[line cut]"));
        assert!(summary.contains("lines truncated (lines 2-2 of 3"));
        assert!(!summary.contains("middle"));
        assert!(summary.len() < SUMMARY_MAX_BYTES + 500, "{}", summary.len());
    }

    #[test]
    fn test_format_summary_multibyte_text_cuts_on_char_boundaries() {
        // 3-byte characters, so budget-sized byte cuts land mid-character.
        let stdout: Vec<String> = (0..4000)
            .map(|i| format!("行{i} {}", "你".repeat(10)))
            .collect();
        let summary = format_summary(0, &stdout, &[], 0);
        assert!(summary.contains("lines truncated"));
        let blob = vec!["你".repeat(50_000)];
        assert!(format_summary(0, &blob, &[], 0).contains("bytes of this line truncated"));
    }

    #[test]
    fn test_format_full_output_includes_everything() {
        let stdout: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let stderr: Vec<String> = vec!["err1".into(), "err2".into()];
        let full = format_full_output(1, &stdout, &stderr, DEFAULT_MAX_FULL_OUTPUT_BYTES);
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
        let full = format_full_output(0, &stdout, &[], DEFAULT_MAX_FULL_OUTPUT_BYTES);
        assert!(full.len() <= 2 * 1024 * 1024 + 50); // 2MB + truncation message
        assert!(full.contains("truncated at 2MB"));
    }

    /// Phase 5 PR-8 of #934: the policy-supplied `max_output_bytes`
    /// overrides the historical 2 MB default. Pins the contract that
    /// `format_full_output` honors its `max_bytes` parameter and
    /// surfaces the actual cap in the truncation marker (so the model
    /// + human reading the DB can always tell where output stopped).
    #[test]
    fn format_full_output_cap_inside_a_character_does_not_panic() {
        let header = "Exit code: 0\n\n--- stdout ---\n".len();
        for character in ["é", "界", "🐻"] {
            for inside in 1..character.len() {
                let cap = 4 * 1024;
                let line = "x".repeat(cap - header - inside) + character;
                let full = format_full_output(0, &[line], &[], cap);
                assert!(full.contains("truncated at 4KB"), "{character} {inside}");
            }
        }
    }

    #[test]
    fn format_full_output_honors_explicit_cap() {
        // Tight 4KB cap; the marker should show "4KB", and total output
        // length must not exceed the cap + a small marker overhead.
        let stdout: Vec<String> = (0..2000).map(|i| format!("line {i}: padding")).collect();
        let cap = 4 * 1024;
        let full = format_full_output(0, &stdout, &[], cap);
        assert!(
            full.len() <= cap + 50,
            "output {} should fit within cap {} + marker",
            full.len(),
            cap
        );
        assert!(
            full.contains("truncated at 4KB"),
            "truncation marker should name the actual cap, got: {}",
            full.lines().last().unwrap_or("")
        );
    }

    /// `format_byte_cap` formats round MB / KB cleanly so the truncation
    /// marker is human-legible for the common cases (2 MB default,
    /// future per-trust-mode caps that'll likely be round numbers).
    /// Falls back to raw bytes for anything weird so the marker is
    /// always exact, never lossy.
    #[test]
    fn format_byte_cap_renders_round_units() {
        assert_eq!(format_byte_cap(2 * 1024 * 1024), "2MB");
        assert_eq!(format_byte_cap(4 * 1024), "4KB");
        assert_eq!(format_byte_cap(1500), "1500 bytes");
        assert_eq!(format_byte_cap(0), "0 bytes");
    }

    #[test]
    fn test_shell_output_has_full_output() {
        // Verify ShellOutput struct works correctly
        let so = ShellOutput {
            summary: "Exit code: 0".into(),
            full_output: Some("full output here".into()),
            success: true,
        };
        assert_eq!(so.summary, "Exit code: 0");
        assert_eq!(so.full_output.unwrap(), "full output here");
    }

    #[tokio::test]
    async fn collection_keeps_the_first_lines_and_the_last_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        // 500 lines, collection capped at 10: the first 10, a marker for the
        // 390 in the middle, and the last 100.
        let args = serde_json::json!({
            "command": "seq 1 500"
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
            None,
        )
        .await
        .unwrap();
        // 10 + the marker + 100.
        assert!(
            result.summary.contains("stdout: 111 lines"),
            "{}",
            result.summary
        );
        let full = result.full_output.unwrap();
        assert!(
            full.contains("\n10\n[\u{2026} 390 lines not kept \u{2026}]\n401\n"),
            "{full}"
        );
        assert!(full.ends_with("\n500"), "{full}");
        assert!(!full.contains("\n200\n"), "the middle isn't kept: {full}");
    }

    /// A timeout cut to the ceiling says so; one within it doesn't.
    #[test]
    fn a_cut_timeout_says_so() {
        let cut = timeout_message(MAX_TIMEOUT_SECS, Some(900), "cargo test");
        assert!(cut.contains("600s, the most a foreground command gets (900s was asked for)"));
        assert!(cut.contains("background=true"));
        assert_eq!(
            timeout_message(30, Some(30), "sleep 99"),
            "Command timed out after 30s: sleep 99"
        );
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
        annotate_violations(0, "find /", &mut stderr, true);
        assert_eq!(stderr.len(), 1, "no extra lines should be appended");
        assert!(!stderr.iter().any(|l| l.contains("<sandbox_violations>")));
    }

    #[test]
    fn annotate_violations_appends_block_on_real_denial() {
        let mut stderr = vec!["touch: /etc/passwd: Operation not permitted".into()];
        annotate_violations(1, "touch /etc/passwd", &mut stderr, true);
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
    fn a_sandbox_denial_points_to_outside_sandbox() {
        let mut stderr =
            vec!["ssh: connect to host 192.168.10.50 port 2222: Operation not permitted".into()];
        annotate_violations(128, "git push", &mut stderr.clone(), true);
        let mut nobody = stderr.clone();
        annotate_violations(128, "git push", &mut nobody, false);
        assert_eq!(nobody.last().map(String::as_str), Some(NO_WAY_OUT_HINT));
        annotate_violations(128, "git push", &mut stderr, true);
        assert!(
            stderr.iter().any(|l| l.contains("deny network-outbound")),
            "{stderr:?}"
        );
        assert_eq!(
            stderr.last().map(String::as_str),
            Some(OUTSIDE_SANDBOX_HINT)
        );
    }

    #[test]
    fn annotate_violations_noop_when_no_denial_markers() {
        // Failed command (exit_code != 0) but stderr doesn't look like
        // a sandbox denial → no annotation. E.g. `cargo build` failure.
        let mut stderr = vec!["error[E0277]: trait bound not satisfied".into()];
        annotate_violations(101, "cargo build", &mut stderr, true);
        assert_eq!(stderr.len(), 1, "no annotation expected: {stderr:?}");
    }

    /// An approved `outside_sandbox` call runs without the sandbox's proxy,
    /// so it reaches what the user's own shell would, with the environment
    /// still scrubbed; the same command in the sandbox gets the proxy
    /// variables.
    #[tokio::test]
    async fn outside_sandbox_runs_without_the_proxy() {
        use serde_json::json;
        let project = tempfile::tempdir().unwrap();
        let run = |outside: bool| {
            let project = project.path().to_path_buf();
            async move {
                run_shell_command(
                    &project,
                    &json!({
                        "command": "echo \"proxy=[$HTTPS_PROXY][$ALL_PROXY] env=[$CARGO_MANIFEST_DIR]\"",
                        "outside_sandbox": outside,
                    }),
                    500,
                    &bg(),
                    None,
                    &crate::trust::TrustMode::Safe,
                    &koda_sandbox::SandboxPolicy::strict_default(),
                    Some(31415),
                    Some(31416),
                    None,
                )
                .await
                .unwrap()
                .summary
            }
        };
        let outside = run(true).await;
        assert!(outside.contains("proxy=[][]"), "{outside}");
        // The environment scrub still applies outside the sandbox: cargo
        // sets this for the test process, and it isn't on the allowlist.
        assert!(outside.contains("env=[]"), "{outside}");
        let inside = run(false).await;
        assert!(inside.contains("127.0.0.1:31415"), "{inside}");
    }

    /// Run `command` the way the model would, with a proxy port set so the
    /// proxied profile (the one that restricts loopback) is in force.
    async fn proxied(registry: &BgRegistry, command: &str, background: bool) -> ShellOutput {
        // Outlives the call: a background server keeps working in it.
        static PROJECT: std::sync::LazyLock<tempfile::TempDir> =
            std::sync::LazyLock::new(|| tempfile::tempdir().unwrap());
        run_shell_command(
            PROJECT.path(),
            &serde_json::json!({"command": command, "background": background}),
            256,
            registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            Some(9),
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// A dev server Koda starts in the background is reachable from a later
    /// sandboxed command; a server Koda didn't start, on another localhost
    /// port, still isn't. Before, both were refused.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sandboxed_command_reaches_only_koda_s_own_background_server() {
        if !crate::sandbox::is_available()
            || std::process::Command::new("python3")
                .arg("--version")
                .output()
                .is_err()
        {
            eprintln!("skip: needs the seatbelt backend and python3");
            return;
        }
        let registry = BgRegistry::new();
        let theirs = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let their_port = theirs.local_addr().unwrap().port();

        // Port 0: the server picks a free port and says which, so no other
        // test can take it between a check and the bind. `-u` writes that
        // line to the log at once instead of when a buffer fills.
        let started = proxied(
            &registry,
            "python3 -u -m http.server 0 --bind 127.0.0.1",
            true,
        )
        .await;
        let log = started
            .summary
            .lines()
            .find_map(|l| l.trim().strip_prefix("Output:"))
            .and_then(|rest| rest.trim().split(' ').next())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("no log in {}", started.summary));
        // A busy CI runner can take a while to start Python in the sandbox.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let port = loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let announced = text
                .split("port ")
                .nth(1)
                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse::<u16>().ok());
            if let Some(port) = announced
                && registry.listening_ports().await.contains(&port)
            {
                break port;
            }
            if std::time::Instant::now() >= deadline {
                let lsof_started = std::time::Instant::now();
                let ports = registry.listening_ports().await;
                panic!(
                    "server never seen listening. Log:\n{text}\nprocesses: {:?}\n\
                     listening ports {ports:?}, found in {:?}",
                    registry
                        .snapshot()
                        .iter()
                        .map(|s| (s.pid, s.status))
                        .collect::<Vec<_>>(),
                    lsof_started.elapsed(),
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        assert!(!registry.listening_ports().await.contains(&their_port));

        let reach = |port: u16| {
            format!(
                "curl -s --noproxy '*' -o /dev/null -w 'code=%{{http_code}}' http://127.0.0.1:{port}/"
            )
        };
        let own = proxied(&registry, &reach(port), false).await;
        assert!(own.summary.contains("code=200"), "{}", own.summary);
        let other = proxied(&registry, &reach(their_port), false).await;
        assert!(other.summary.contains("code=000"), "{}", other.summary);

        let pid = registry.snapshot()[0].pid;
        registry.kill(pid);
    }

    /// A background command leads its own process group, so stopping it
    /// stops what it started too, not just the shell.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_a_background_command_stops_its_children() {
        let registry = BgRegistry::new();
        // 71 and 72 s: no other test sleeps that long, so pgrep finds only these.
        // Anchored, so it finds the sleeps and not the shell that runs them.
        proxied(&registry, "sleep 71 & sleep 72; wait", true).await;
        let running = || {
            std::process::Command::new("pgrep")
                .args(["-f", "^sleep 7[12]$"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
                .unwrap_or(0)
        };
        let alive = || running() > 0;
        // Both children started, so the stop comes after the shell's forks.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while running() < 2 {
            assert!(std::time::Instant::now() < deadline, "never started");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pid = registry.snapshot()[0].pid;
        let stop = bash(&registry, &format!("kill {pid}")).await;
        assert!(
            stop.summary.contains("Stopped background process"),
            "{}",
            stop.summary
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive() {
            assert!(
                std::time::Instant::now() < deadline,
                "a child outlived the stop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_description_says_to_find_processes_with_pgrep() {
        let d = &definitions()[0].description;
        assert!(d.contains("ps can't run in the sandbox"), "{d}");
        assert!(d.contains("pgrep -fl"), "{d}");
    }

    #[test]
    fn a_kill_check_may_carry_literal_echoes() {
        let k = kill_command(r#"kill -0 28992 && echo "alive""#).unwrap();
        assert_eq!(k.kill, PlainKill::Check(vec![28992]));
        assert_eq!(k.on_success.as_deref(), Some("alive"));
        assert_eq!(k.on_failure, None);
        let k = kill_command("kill -0 7 && echo 'up' || echo gone now").unwrap();
        assert_eq!(k.on_success.as_deref(), Some("up"));
        assert_eq!(k.on_failure.as_deref(), Some("gone now"));
        assert_eq!(kill_command("kill 7").unwrap().on_success, None);
        // `&&` inside the echo's quotes doesn't split it.
        let k = kill_command("kill -0 7 && echo 'a && b'").unwrap();
        assert_eq!(k.on_success.as_deref(), Some("a && b"));
        // Anything that would make the shell do more runs as written.
        for not_ours in [
            "kill -0 7 && echo $HOME",
            r#"kill -0 7 && echo "$PID""#,
            "kill -0 7 && echo `id`",
            "kill -0 7 && rm -rf x",
            "kill -0 7 && echo a && echo b",
            "kill -0 7; echo done",
            "kill -0 7 && echo a | tee log",
            "echo hi && kill 7",
        ] {
            assert_eq!(kill_command(not_ours), None, "{not_ours}");
        }
    }

    /// The check models write, `kill -0 PID && echo alive`, is Koda's to run
    /// too: it asked in Auto as a destructive `kill`, and the sandbox refused
    /// the signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn koda_runs_a_chained_check_on_its_own_background_process() {
        let registry = BgRegistry::new();
        let pid = start_in_background(&registry, "sleep 60").await;
        let check = format!(r#"kill -0 {pid} && echo "alive" || echo "gone""#);
        let args = serde_json::json!({"command": check});
        assert!(signals_only_own_background(&args, &registry, None));
        let up = bash(&registry, &check).await;
        assert!(up.success, "{}", up.summary);
        assert!(up.summary.contains("is running"), "{}", up.summary);
        assert!(up.summary.contains("\nalive"), "{}", up.summary);
        assert!(!up.summary.contains("gone"), "{}", up.summary);

        registry.kill(pid);
        let down = bash(&registry, &check).await;
        assert!(
            down.success,
            "the || echo makes it succeed: {}",
            down.summary
        );
        assert!(down.summary.contains("gone"), "{}", down.summary);
        let bare = bash(&registry, &format!("kill -0 {pid} && echo alive")).await;
        assert!(!bare.success, "{}", bare.summary);
        assert!(!bare.summary.contains("\nalive"), "{}", bare.summary);
    }

    #[test]
    fn plain_kill_reads_only_a_bare_kill_of_pids() {
        use PlainKill::{Check, Stop};
        assert_eq!(plain_kill("kill 42"), Some(Stop(vec![42])));
        assert_eq!(plain_kill("  kill -9 42 43 "), Some(Stop(vec![42, 43])));
        assert_eq!(plain_kill("kill -TERM 42"), Some(Stop(vec![42])));
        assert_eq!(plain_kill("kill -s KILL 42"), Some(Stop(vec![42])));
        assert_eq!(plain_kill("kill -0 42"), Some(Check(vec![42])));
        assert_eq!(plain_kill("kill -s 0 42"), Some(Check(vec![42])));
        // Anything more is an ordinary command.
        for not_plain in [
            "kill",
            "kill -9",
            "kill -l",
            "kill 0",
            "kill -- 42",
            "kill %1",
            "kill $PID",
            "kill 42; rm -rf /",
            "kill 42 && echo gone",
            "kill 42 | tee log",
            "pkill python",
            "sudo kill 42",
        ] {
            assert_eq!(plain_kill(not_plain), None, "{not_plain}");
        }
    }

    async fn start_in_background(registry: &BgRegistry, command: &str) -> u32 {
        let args = serde_json::json!({"command": command, "background": true});
        run_shell_command(
            tempfile::tempdir().unwrap().path(),
            &args,
            256,
            registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        registry.snapshot().last().unwrap().pid
    }

    async fn bash(registry: &BgRegistry, command: &str) -> ShellOutput {
        run_shell_command(
            tempfile::tempdir().unwrap().path(),
            &serde_json::json!({"command": command}),
            256,
            registry,
            None,
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// The sandbox can't signal a process an earlier Bash call started, so
    /// `kill PID` of Koda's own background process used to fail and need a
    /// run outside the sandbox. Koda now checks and stops it itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn koda_checks_and_stops_its_own_background_process() {
        use crate::tools::bg_process::BgProcessStatus;
        let registry = BgRegistry::new();
        let pid = start_in_background(&registry, "sleep 60").await;

        let args = serde_json::json!({"command": format!("kill {pid}")});
        assert!(signals_only_own_background(&args, &registry, None));
        assert!(!signals_only_own_background(&args, &registry, Some(7)));

        let check = bash(&registry, &format!("kill -0 {pid}")).await;
        assert!(check.success, "{}", check.summary);
        assert!(
            check.summary.contains("is running: sleep 60"),
            "{}",
            check.summary
        );

        let stop = bash(&registry, &format!("kill {pid}")).await;
        assert!(stop.success, "{}", stop.summary);
        assert!(
            stop.summary
                .contains(&format!("Stopped background process {pid}: sleep 60")),
            "{}",
            stop.summary
        );
        assert_ne!(registry.snapshot()[0].status, BgProcessStatus::Running);

        let again = bash(&registry, &format!("kill -0 {pid}")).await;
        assert!(!again.success, "{}", again.summary);
        assert!(
            again.summary.contains("is not running"),
            "{}",
            again.summary
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_kill_that_names_another_process_runs_as_written() {
        let registry = BgRegistry::new();
        let pid = start_in_background(&registry, "sleep 60").await;
        // One PID isn't Koda's, so the whole command is left to the shell.
        let args = serde_json::json!({"command": format!("kill {pid} 999999")});
        assert!(!signals_only_own_background(&args, &registry, None));
        let ran = bash(&registry, &format!("kill {pid} 999999")).await;
        assert!(
            !ran.summary.contains("Stopped background process"),
            "{}",
            ran.summary
        );
        // Leaving the sandbox is never Koda's to do on the model's behalf.
        let outside =
            serde_json::json!({"command": format!("kill {pid}"), "outside_sandbox": true});
        assert!(!signals_only_own_background(&outside, &registry, None));
        registry.kill(pid);
    }

    #[test]
    fn classify_outside_sandbox_is_destructive() {
        let tool = BashTool;
        assert_eq!(
            tool.classify(&serde_json::json!({"command": "ls", "outside_sandbox": true})),
            ToolEffect::Destructive
        );
        assert_eq!(
            tool.classify(&serde_json::json!({"command": "ls", "outside_sandbox": false})),
            ToolEffect::ReadOnly
        );
    }

    /// Phase 1 acceptance criterion of #934: a real sandboxed bash command
    /// that touches ~/.ssh returns annotated stderr.
    ///
    /// Goes through the full `run_shell_command` pipeline — sandbox build,
    /// process spawn, stream capture, kernel denial, stderr parse,
    /// `<sandbox_violations>` annotation. This is the proof-of-life test
    /// for the whole Phase 1 stack.
    /// A sandbox denial points to `outside_sandbox` only where someone can
    /// approve it: not for a headless run, whose client answers approvals
    /// itself, and not for a sub-agent.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_way_out_is_offered_only_where_someone_can_approve_it() {
        use crate::engine::EngineSink;
        struct Nobody;
        impl EngineSink for Nobody {
            fn emit(&self, _: EngineEvent) {}
            fn can_ask(&self) -> bool {
                false
            }
        }
        struct Someone;
        impl EngineSink for Someone {
            fn emit(&self, _: EngineEvent) {}
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let ssh = format!("{home}/.ssh");
        if !crate::sandbox::is_available() || !std::path::Path::new(&ssh).exists() {
            eprintln!("skip: needs the seatbelt backend and ~/.ssh");
            return;
        }
        let project = tempfile::tempdir().unwrap();
        let run = |sink: &'static dyn EngineSink, caller: Option<u32>| {
            let project = project.path().to_path_buf();
            let ssh = ssh.clone();
            async move {
                let out = run_shell_command(
                    &project,
                    &serde_json::json!({"command": format!("touch {ssh}/koda_hint_canary")}),
                    500,
                    &bg(),
                    Some((sink, "c1")),
                    &crate::trust::TrustMode::Safe,
                    &koda_sandbox::SandboxPolicy::strict_default(),
                    None,
                    None,
                    caller,
                )
                .await
                .unwrap();
                out.full_output.unwrap_or(out.summary)
            }
        };
        let tui = run(&Someone, None).await;
        assert!(tui.contains(OUTSIDE_SANDBOX_HINT), "{tui}");
        let headless = run(&Nobody, None).await;
        assert!(headless.contains(NO_WAY_OUT_HINT), "{headless}");
        assert!(!headless.contains("outside_sandbox=true"), "{headless}");
        let sub_agent = run(&Someone, Some(7)).await;
        assert!(sub_agent.contains(NO_WAY_OUT_HINT), "{sub_agent}");
    }

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

    // ── Tool trait invariants (#1265 PR-6) ─────────────────────

    #[test]
    fn bash_tool_metadata_matches_definition() {
        let t = BashTool;
        assert_eq!(t.name(), "Bash");
        assert_eq!(t.definition().name, "Bash");
        // A command can change anything, so it names no one file.
        assert!(t.mutated_path(&serde_json::json!({})).is_none());
    }

    /// Per-call classify is the headline feature of PR-6: same
    /// tool, different commands, different effects. Pre-#1265
    /// `tools::classify_tool("Bash")` ignored args entirely and
    /// delegated to `bash_safety::classify_bash_command` via a
    /// separate code path; trait-folded here.
    #[test]
    fn bash_classify_is_input_aware() {
        let t = BashTool;
        // Read-only command → ReadOnly.
        assert_eq!(
            t.classify(&serde_json::json!({"command": "echo hi"})),
            ToolEffect::ReadOnly,
        );
        // Plainly destructive → Destructive.
        assert_eq!(
            t.classify(&serde_json::json!({"command": "rm -rf /tmp/somewhere"})),
            ToolEffect::Destructive,
        );
    }

    /// Defensive: if the args don't carry a `command`, classify must
    /// over-gate (Destructive), not under-gate (ReadOnly). Better to
    /// prompt unnecessarily than to skip approval on a malformed call.
    #[test]
    fn bash_classify_defaults_destructive_on_missing_command() {
        let t = BashTool;
        assert_eq!(t.classify(&serde_json::json!({})), ToolEffect::Destructive,);
        assert_eq!(
            t.classify(&serde_json::json!({"command": 42})),
            ToolEffect::Destructive,
        );
    }
}
