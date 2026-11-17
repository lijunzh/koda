//! `WriteStdin`: type into a background command's terminal and read what it
//! prints back, as Codex's `write_stdin` does for an `exec_command` session.
//!
//! The command was started by `Bash` with `tty: true`, which gives it a
//! terminal; `WriteStdin` finds that terminal by the command's PID. Empty
//! `chars` types nothing and only waits for more output.
//!
//! Waiting follows Codex: after typing, 250 ms by default and at most 30 s;
//! an empty poll, 5 s by default and from 5 s to 5 minutes. The wait ends
//! early when the command exits.
//!
//! Typing is a mutation and reading isn't: in Plan mode only polls run, Safe
//! mode confirms each input, and Auto types without asking, as Codex does by
//! default, since the command runs in the sandbox it started in.

use crate::providers::ToolDefinition;
use crate::tools::bg_process::TerminalLookup;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

/// Codex's `MIN_YIELD_TIME_MS`: the default and least wait after typing.
const TYPED_WAIT_MS: (u64, u64) = (250, 30_000);
/// Codex's `MIN_EMPTY_YIELD_TIME_MS` and its background terminal timeout:
/// the default and least, and the most, an empty poll waits.
const POLL_WAIT_MS: (u64, u64) = (5_000, 300_000);

/// `WriteStdin` — type into a background command's terminal.
pub struct WriteStdinTool;

fn chars(args: &Value) -> &str {
    args.get("chars").and_then(Value::as_str).unwrap_or("")
}

/// How long to wait for output: what was asked, within Codex's bounds for a
/// write or a poll, or the least of them by default.
fn wait_for(args: &Value) -> Duration {
    let (least, most) = if chars(args).is_empty() {
        POLL_WAIT_MS
    } else {
        TYPED_WAIT_MS
    };
    let asked = args
        .get("yield_time_ms")
        .and_then(Value::as_u64)
        .unwrap_or(least);
    Duration::from_millis(asked.clamp(least, most))
}

#[async_trait]
impl Tool for WriteStdinTool {
    fn name(&self) -> &'static str {
        "WriteStdin"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "WriteStdin".to_string(),
            description: "Type into a background command started with Bash{tty: true}, \
                and return what it printed since the last read. End a line with \"\\n\" \
                to send it; \"\\u0003\" is Ctrl+C and \"\\u0004\" is Ctrl+D. With no chars, \
                types nothing and waits for more output. The result ends with whether the \
                command is still running."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pid": {
                        "type": "integer",
                        "description": "The PID that Bash{tty: true} returned."
                    },
                    "chars": {
                        "type": "string",
                        "description": "What to type, exactly as typed. Empty or omitted: type \
                            nothing, only wait for output."
                    },
                    "yield_time_ms": {
                        "type": "integer",
                        "description": "How long to wait for output, in ms; less if the command \
                            exits. After typing: 250 by default, at most 30000. With no chars: \
                            5000 by default, from 5000 to 300000."
                    }
                },
                "required": ["pid"]
            }),
        }
    }

    /// Reading changes nothing; typing can do anything the command can.
    fn classify(&self, args: &Value) -> ToolEffect {
        if chars(args).is_empty() {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::LocalMutation
        }
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let fail = |output: String| ToolResult {
            output,
            success: false,
            full_output: None,
            images: Vec::new(),
        };
        let Some(pid) = args
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
        else {
            return fail("Error: pid is required: the PID that Bash{tty: true} returned".into());
        };
        let terminal = match ctx.bg_registry.terminal_as_caller(pid, ctx.caller_spawner) {
            Ok(terminal) => terminal,
            Err(TerminalLookup::NotFound) => {
                return fail(format!("Error: no background process has PID {pid}"));
            }
            Err(TerminalLookup::Forbidden) => {
                return fail(format!(
                    "Error: PID {pid} was started by another agent; only it can type there"
                ));
            }
            Err(TerminalLookup::NoTerminal) => {
                return fail(format!(
                    "Error: PID {pid} was started without a terminal, so it can't be typed \
                     into. Its output is in the log its start named. To type into a command, \
                     start it with Bash{{tty: true}}."
                ));
            }
        };
        let text = chars(args);
        let mut out = String::new();
        if text.is_empty() {
            out.push_str(&format!("Waited for output from PID {pid}."));
        } else if terminal.closed() {
            out.push_str(&format!("PID {pid} has exited, so nothing was typed."));
        } else if let Err(e) = terminal.write(text.as_bytes()) {
            return fail(format!("Error: couldn't type into PID {pid}: {e}"));
        } else {
            out.push_str(&format!(
                "Typed {} characters into PID {pid}.",
                text.chars().count()
            ));
        }
        let output = terminal.read(wait_for(args)).await;
        let status = crate::tools::shell::settled_status(ctx.bg_registry, pid, output.closed).await;
        out.push_str(&crate::tools::shell::terminal_report(&output, status));
        ToolResult {
            output: out,
            success: true,
            full_output: None,
            images: Vec::new(),
        }
    }
}

/// The approval line for a `WriteStdin` call: the PID and what would be
/// typed, with control characters shown (`\n`, `^C`).
pub(crate) fn describe(args: &Value) -> String {
    let pid = args.get("pid").and_then(Value::as_u64).unwrap_or(0);
    let shown: String = chars(args)
        .chars()
        .flat_map(|c| match c {
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c if (c as u32) < 0x20 => vec!['^', char::from(b'@' + c as u8)],
            c => vec![c],
        })
        .collect();
    let shown = if shown.chars().count() > 60 {
        format!("{}…", shown.chars().take(57).collect::<String>())
    } else {
        shown
    };
    format!("Type into PID {pid}: {shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_follow_codex_bounds() {
        assert_eq!(
            wait_for(&json!({"pid": 1, "chars": "x"})),
            Duration::from_millis(250)
        );
        assert_eq!(wait_for(&json!({"pid": 1})), Duration::from_millis(5_000));
        assert_eq!(
            wait_for(&json!({"pid": 1, "chars": "x", "yield_time_ms": 99_999})),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            wait_for(&json!({"pid": 1, "chars": "", "yield_time_ms": 10})),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            wait_for(&json!({"pid": 1, "yield_time_ms": 900_000})),
            Duration::from_millis(300_000)
        );
    }

    #[test]
    fn typing_is_a_mutation_and_polling_is_not() {
        let tool = WriteStdinTool;
        assert_eq!(tool.classify(&json!({"pid": 1})), ToolEffect::ReadOnly);
        assert_eq!(
            tool.classify(&json!({"pid": 1, "chars": ""})),
            ToolEffect::ReadOnly
        );
        assert_eq!(
            tool.classify(&json!({"pid": 1, "chars": "y\n"})),
            ToolEffect::LocalMutation
        );
    }

    #[test]
    fn the_approval_line_shows_control_characters() {
        assert_eq!(
            describe(&json!({"pid": 42, "chars": "y\n\u{3}"})),
            "Type into PID 42: y\\n^C"
        );
    }
}
