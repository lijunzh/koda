//! TUI renderer: draws a turn from the client protocol's messages as
//! native ratatui `Line`s.
//!
//! All output is rendered as `ratatui::text::Line` / `Span` and written
//! above the viewport via `insert_before()`. No ANSI strings.
//!
//! ## Pipeline
//!
//! ```text
//! ServerNotification (from Koda's host), approval requests
//!     ↓
//! TuiRenderer::render(), TuiRenderer::approval()
//!     ↓  assembles styled Line<'static> values
//! ScrollBuffer::push()            ← render cache (VecDeque)
//!     ↓
//! tui_app  →  Paragraph::new(buffer.all_lines()).scroll()
//!     ↓
//! ratatui Terminal::draw()
//! ```
//!
//! ## Streaming text
//!
//! `item/agentMessage/delta` text accumulates in `text_buf`. Complete lines
//! (split at `\n`) are flushed through the markdown renderer
//! ([`crate::md_render`]) immediately so the user sees progressive output.
//! The partial tail is flushed when the item completes.
//!
//! ## Tool output
//!
//! `item/toolCall/outputDelta` streams output in real time during
//! long-running tool calls (e.g. `cargo build`). When the call's item
//! completes, if output was already streamed the renderer folds it and
//! shows the exit line rather than drawing all output again.
//!
//! ## Sub-agents
//!
//! A sub-agent's calls (`parentItemId` set) go in the full view only, as
//! Claude Code lists them when expanded; the overlay shows the latest while
//! it runs. When it ends, its call sums it up: `Done (3 tool uses · 12s)`.
//!
//! ## Verbose mode
//!
//! Set `TuiRenderer::verbose = true` to suppress the
//! [`crate::truncate`]-based collapsing. Every output line is shown.

use crate::ansi_parse::parse_ansi_spans;
use crate::protocol_text::{self, Tone};
use crate::scroll_buffer::Fold;
use crate::scroll_buffer::ScrollBuffer;
use crate::theme;
use crate::tui_output::{self, CYAN, DIM, RED, TOOL_PREFIX, YELLOW};
use koda_client::protocol::ServerNotification;
use koda_client::protocol::item::{FileUpdateChange, Item, OutputStream, ToolCallStatus, TurnItem};
use koda_client::protocol::turn::{Turn, TurnStatus};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::{HashMap, HashSet};

/// A tool call the renderer saw start.
struct Call {
    arguments: serde_json::Value,
    /// What it does, as the host classified it.
    effect: Option<koda_client::protocol::item::ToolEffect>,
    /// The call it runs under: a sub-agent's `InvokeAgent`.
    parent: Option<String>,
    /// Where in the buffer its first streamed output line went. The finished
    /// result isn't drawn again; the streamed lines are folded in place
    /// ([`ScrollBuffer::fold_since`]).
    stream_start: Option<u64>,
}

/// A sub-agent at work: its `InvokeAgent` call, for the line that sums it up.
struct AgentRun {
    /// When its call started, in Unix milliseconds, if recorded.
    started_ms: Option<i64>,
    /// Tool calls it has made.
    calls: usize,
}

/// Draws a turn from the protocol's messages into the scroll buffer.
pub struct TuiRenderer {
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::tool_history::ToolOutputHistory,
    /// Current model name displayed in the status bar.
    pub model: String,
    /// The open session's own replies, oldest first, as they completed or
    /// were replayed, for `/copy`: Codex's TUI copies what it was sent too.
    replies: Vec<String>,
    /// Reply text not yet drawn: everything after its last `\n`.
    text_buf: String,
    /// Reasoning text not yet drawn.
    think_buf: String,
    /// Whether the reply item has drawn a line yet.
    has_emitted_text: bool,
    /// Whether the reply item has drawn its `───` line yet.
    ruled: bool,
    /// Streaming markdown renderer.
    md: crate::md_render::MarkdownRenderer,
    /// The session's own reply and reasoning items being drawn. A
    /// sub-agent's text isn't sent: its answer is.
    messages: HashSet<String>,
    reasoning: HashSet<String>,
    /// Tool calls started and not yet completed, by item id.
    calls: HashMap<String, Call>,
    /// Calls whose changes an approval drew, so a finished Write or Edit
    /// says only how long it is.
    changes_shown: HashSet<String>,
    /// Sub-agents at work, by their `InvokeAgent` call.
    agents: HashMap<String, AgentRun>,
    /// The turn's error was drawn, so its end needn't say it again.
    error_shown: bool,
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            tool_history: crate::tool_history::ToolOutputHistory::new(),
            model: String::new(),
            replies: Vec::new(),
            text_buf: String::new(),
            think_buf: String::new(),
            has_emitted_text: false,
            ruled: false,
            md: crate::md_render::MarkdownRenderer::new(),
            messages: HashSet::new(),
            reasoning: HashSet::new(),
            calls: HashMap::new(),
            changes_shown: HashSet::new(),
            agents: HashMap::new(),
            error_shown: false,
        }
    }

    /// The open session's own replies, oldest first.
    pub fn replies(&self) -> &[String] {
        &self.replies
    }

    /// Another session is open: its replies come with its turns.
    pub fn forget_replies(&mut self) {
        self.replies.clear();
    }

    /// Forget the last turn's state as a new one starts.
    pub fn start_turn(&mut self) {
        let tool_history = std::mem::take(&mut self.tool_history);
        let model = std::mem::take(&mut self.model);
        *self = Self {
            tool_history,
            model,
            ..Self::new()
        };
    }

    /// Whether the turn's error was drawn already.
    pub fn error_shown(&self) -> bool {
        self.error_shown
    }

    /// An approval request: its diff goes above the dialog.
    pub fn approval(
        &mut self,
        item_id: &str,
        changes: &[FileUpdateChange],
        buffer: &mut ScrollBuffer,
    ) {
        if !changes.is_empty() {
            self.changes_shown.insert(item_id.to_string());
            push_changes(changes, buffer);
        }
    }

    /// Draw a stored turn as it was drawn live, as Codex's TUI replays a
    /// resumed thread's turns through its live handlers: the prompt, each
    /// item started and completed, then how the turn ended.
    pub fn replay_turn(&mut self, turn: &Turn, buffer: &mut ScrollBuffer) {
        self.start_turn();
        let by_id: HashMap<&str, &TurnItem> = turn.items.iter().map(|t| (t.item.id(), t)).collect();
        let mut started = HashSet::new();
        for stored in &turn.items {
            self.replay_start(stored.item.id(), &by_id, &mut started, buffer);
            match &stored.item {
                Item::AgentMessage { id, text } if self.messages.contains(id) => {
                    self.reply_text(text, buffer);
                }
                Item::Reasoning { id, text } if self.reasoning.contains(id) => {
                    self.thinking_text(text, buffer);
                }
                _ => {}
            }
            self.item_completed(
                stored.parent_item_id.as_deref(),
                &stored.item,
                stored.completed_at_ms,
                buffer,
            );
        }
        match (&turn.status, &turn.error) {
            (TurnStatus::Failed, Some(error)) => {
                tui_output::emit_line(buffer, turn_failed_line(&error.message));
            }
            (TurnStatus::Failed, None) => {
                tui_output::emit_line(buffer, turn_failed_line("the turn failed"));
            }
            _ => {}
        }
        if let Some(ms) = turn.duration_ms {
            tui_output::emit_line(buffer, worked_for_line(ms));
        }
    }

    /// Start stored item `id` as it started live. Items are stored as they
    /// complete, so a sub-agent's items come before its call: the call is
    /// started first, as it was live.
    fn replay_start(
        &mut self,
        id: &str,
        by_id: &HashMap<&str, &TurnItem>,
        started: &mut HashSet<String>,
        buffer: &mut ScrollBuffer,
    ) {
        let Some(stored) = by_id.get(id) else {
            return;
        };
        if !started.insert(id.to_string()) {
            return;
        }
        if let Some(parent) = stored.parent_item_id.as_deref() {
            self.replay_start(parent, by_id, started, buffer);
        }
        match &stored.item {
            Item::UserMessage { text, .. } => buffer.push_lines(prompt_lines(text)),
            item => self.item_started(
                stored.parent_item_id.as_deref(),
                item,
                stored.started_at_ms,
                buffer,
            ),
        }
    }

    /// Draw one of the session's notifications.
    pub fn render(&mut self, notification: &ServerNotification, buffer: &mut ScrollBuffer) {
        match notification {
            ServerNotification::ItemStarted(started) => {
                self.item_started(
                    started.parent_item_id.as_deref(),
                    &started.item,
                    Some(started.started_at_ms),
                    buffer,
                );
            }
            ServerNotification::ItemCompleted(done) => {
                self.item_completed(
                    done.parent_item_id.as_deref(),
                    &done.item,
                    Some(done.completed_at_ms),
                    buffer,
                );
            }
            ServerNotification::AgentMessageDelta(delta)
                if self.messages.contains(&delta.item_id) =>
            {
                self.reply_text(&delta.delta, buffer);
            }
            ServerNotification::ReasoningTextDelta(delta)
                if self.reasoning.contains(&delta.item_id) =>
            {
                self.thinking_text(&delta.delta, buffer);
            }
            ServerNotification::ToolCallOutputDelta(delta) => {
                self.output(&delta.item_id, &delta.delta, delta.stream, buffer);
            }
            ServerNotification::ToolCallProgress(progress) => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(progress.message.clone(), CYAN),
                    ]),
                );
            }
            ServerNotification::TurnLoopGuard(guard) => notice(
                (
                    Tone::Warn,
                    protocol_text::loop_guard(guard.action, &guard.detail),
                ),
                buffer,
            ),
            ServerNotification::Warning(warning) => {
                notice((Tone::Warn, warning.message.clone()), buffer);
            }
            // A retry's notice says what happened and when Koda tries again.
            ServerNotification::Error(error) if error.will_retry => {
                notice((Tone::Warn, error.error.message.clone()), buffer);
            }
            ServerNotification::Error(error) => {
                self.error_shown = true;
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{2717} {}", error.error.message), RED),
                    ]),
                );
            }
            _ => {}
        }
    }

    fn item_started(
        &mut self,
        parent: Option<&str>,
        item: &Item,
        started_ms: Option<i64>,
        buffer: &mut ScrollBuffer,
    ) {
        match item {
            Item::AgentMessage { id, .. } if parent.is_none() => {
                self.messages.insert(id.clone());
                self.has_emitted_text = false;
                self.ruled = false;
            }
            Item::Reasoning { id, .. } if parent.is_none() => {
                self.reasoning.insert(id.clone());
                self.think_buf.clear();
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{1f4ad} Thinking...", DIM),
                    ]),
                );
            }
            Item::ToolCall {
                id,
                tool,
                arguments,
                effect,
                ..
            } => {
                self.calls.insert(
                    id.clone(),
                    Call {
                        arguments: arguments.clone(),
                        effect: *effect,
                        parent: parent.map(str::to_string),
                        stream_start: None,
                    },
                );
                if tool == "InvokeAgent" {
                    self.agents.insert(
                        id.clone(),
                        AgentRun {
                            started_ms,
                            calls: 0,
                        },
                    );
                }
                match parent {
                    // A sub-agent's call is in the full view only, as Claude
                    // Code lists a sub-agent's calls when expanded; the
                    // overlay shows the latest while it runs.
                    Some(parent) => {
                        if let Some(run) = self.agents.get_mut(parent) {
                            run.calls += 1;
                        }
                        let line =
                            crate::tool_header::build_header_line("  ", tool, arguments, *effect);
                        buffer.push_folded(line, Fold::Full);
                    }
                    None => {
                        let line =
                            crate::tool_header::build_header_line("", tool, arguments, *effect);
                        tui_output::emit_line(buffer, line);
                    }
                }
            }
            item @ Item::ContextCompaction { .. } => {
                if let Some(line) = protocol_text::compaction_started(item) {
                    notice(line, buffer);
                }
            }
            _ => {}
        }
    }

    fn item_completed(
        &mut self,
        parent: Option<&str>,
        item: &Item,
        completed_ms: Option<i64>,
        buffer: &mut ScrollBuffer,
    ) {
        match item {
            Item::AgentMessage { id, text } if self.messages.remove(id) => {
                if !text.trim().is_empty() {
                    self.replies.push(text.clone());
                }
                if !self.text_buf.is_empty() {
                    let rest = std::mem::take(&mut self.text_buf);
                    self.rule(&rest, buffer);
                    tui_output::emit_line(buffer, self.md.render_line(&rest));
                }
                self.has_emitted_text = false;
                self.md = crate::md_render::MarkdownRenderer::new();
            }
            Item::Reasoning { id, .. } if self.reasoning.remove(id) => {
                if !self.think_buf.is_empty() {
                    let rest = std::mem::take(&mut self.think_buf);
                    tui_output::emit_line(buffer, thinking_line(rest));
                }
            }
            item @ Item::ContextCompaction { .. } => {
                if let Some(line) = protocol_text::compaction_done(item) {
                    notice(line, buffer);
                }
            }
            Item::ToolCall {
                id,
                tool,
                status,
                output,
                detail,
                changes,
                ..
            } => {
                let call = self.calls.remove(id);
                let changes_shown = self.changes_shown.remove(id);
                // What happened to it, as both peers show a declined call:
                // what the model was told says which it was.
                if *status == ToolCallStatus::Declined {
                    let what = detail.as_deref().unwrap_or(tool);
                    let outcome = protocol_text::declined(output.as_deref().unwrap_or(""));
                    // The diff, unless the approval drew it already, as when
                    // the turn is replayed.
                    let diff = (!changes_shown).then_some(changes.as_slice());
                    declined_lines(outcome, what, diff, buffer);
                    return;
                }
                // A sub-agent's results aren't shown: its answer is.
                if parent.is_some() {
                    return;
                }
                let failed = *status == ToolCallStatus::Failed;
                if !(failed || *status == ToolCallStatus::Completed) {
                    return;
                }
                if let Some(run) = self.agents.remove(id) {
                    let ms = run
                        .started_ms
                        .zip(completed_ms)
                        .map(|(started, done)| done.saturating_sub(started).max(0) as u64);
                    tui_output::emit_line(buffer, agent_summary(run.calls, ms, failed));
                }
                let output = output.as_deref().unwrap_or("");
                self.result(tool, call, output, failed, changes_shown, buffer);
            }
            _ => {}
        }
    }

    /// A finished call's output: folded in place if it streamed, otherwise
    /// drawn as the finished output is ([`tool_output_view`]).
    #[allow(clippy::too_many_arguments)]
    fn result(
        &mut self,
        tool: &str,
        call: Option<Call>,
        output: &str,
        failed: bool,
        changes_shown: bool,
        buffer: &mut ScrollBuffer,
    ) {
        self.tool_history.push(tool, output);
        if let Some(start) = call.as_ref().and_then(|c| c.stream_start) {
            let tail = if failed { FOLD_TAIL } else { 0 };
            buffer.fold_since(start, FOLD_HEAD, tail, fold_marker);
            let exit_line = output.lines().next().unwrap_or("");
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2514} ", DIM),
                    Span::styled(exit_line.to_string(), DIM),
                ]),
            );
            return;
        }
        if changes_shown && matches!(tool, "Write" | "Edit" | "MemoryWrite") {
            // The diff is above the dialog: just how long the result is.
            let line_count = output.lines().count();
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2514} ", DIM),
                    Span::styled(format!("{tool}: {line_count} line(s)"), DIM),
                ]),
            );
            return;
        }
        let effect = call.as_ref().and_then(|c| c.effect);
        let arguments = call.map(|c| c.arguments).unwrap_or_default();
        let file_ext = extract_file_extension(&arguments.to_string());
        // Grep bolds the pattern it searched for.
        let grep_pattern = (tool == "Grep")
            .then(|| arguments["pattern"].as_str().map(str::to_string))
            .flatten();
        buffer.push_folded_lines(tool_output_view(
            tool,
            effect,
            output,
            failed,
            file_ext.as_deref(),
            grep_pattern.as_deref(),
        ));
    }

    /// A chunk of a call's live output, one line at a time.
    fn output(
        &mut self,
        item_id: &str,
        delta: &str,
        stream: OutputStream,
        buffer: &mut ScrollBuffer,
    ) {
        let Some(call) = self.calls.get_mut(item_id) else {
            return;
        };
        let stderr = stream == OutputStream::Stderr;
        let prefix = if stderr {
            "  \u{2502}e "
        } else {
            "  \u{2502} "
        };
        // Pull the body color from the central theme module so read-only
        // output stays legible (#804) and stderr is always red whatever
        // the tool.
        let style = theme::content_style_for(call.effect, stderr);
        let sub_agent = call.parent.is_some();
        if !sub_agent {
            call.stream_start.get_or_insert_with(|| buffer.next_seq());
        }
        for text in delta.strip_suffix('\n').unwrap_or(delta).split('\n') {
            let line = Line::from(vec![
                Span::styled(prefix, TOOL_PREFIX),
                Span::styled(text.to_string(), style),
            ]);
            if sub_agent {
                buffer.push_folded(line, Fold::Full);
            } else {
                tui_output::emit_line(buffer, line);
            }
        }
    }

    /// Reply text: complete lines are drawn as they arrive, through the
    /// markdown renderer, so the reply shows as it streams.
    fn reply_text(&mut self, text: &str, buffer: &mut ScrollBuffer) {
        self.text_buf.push_str(text);
        while let Some(pos) = self.text_buf.find('\n') {
            let line = self.text_buf[..pos].to_string();
            self.text_buf.drain(..=pos);
            // Blank lines at the very start of a reply are skipped.
            if line.is_empty() && !self.has_emitted_text {
                continue;
            }
            self.rule(&line, buffer);
            self.has_emitted_text = true;
            tui_output::emit_line(buffer, self.md.render_line(&line));
        }
    }

    /// Reasoning text: complete lines are drawn as they arrive.
    fn thinking_text(&mut self, text: &str, buffer: &mut ScrollBuffer) {
        self.think_buf.push_str(text);
        while let Some(pos) = self.think_buf.find('\n') {
            let line = self.think_buf[..pos].to_string();
            self.think_buf.drain(..=pos);
            tui_output::emit_line(buffer, thinking_line(line));
        }
    }

    /// The `───` line before a reply's first words.
    fn rule(&mut self, text: &str, buffer: &mut ScrollBuffer) {
        if !self.ruled && !text.trim().is_empty() {
            self.ruled = true;
            tui_output::emit_line(buffer, Line::styled("  \u{2500}\u{2500}\u{2500}", DIM));
        }
    }
}

/// A prompt from an earlier turn: its first line after `❯`, the rest
/// indented, after a blank line.
fn prompt_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    let mut rest = text.lines();
    if let Some(first) = rest.next() {
        lines.push(Line::from(vec![
            Span::styled(
                "  \u{276f} ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled(
                first.to_string(),
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
        ]));
    }
    lines.extend(rest.map(|line| Line::from(vec![Span::raw("    "), Span::raw(line.to_string())])));
    lines
}

/// The line a failed turn ends with.
pub(crate) fn turn_failed_line(error: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("\u{2717} Turn failed: {error}"), RED),
    ])
}

/// A line of reasoning, dim behind a bar.
fn thinking_line(text: String) -> Line<'static> {
    Line::from(vec![
        Span::styled("  \u{2502} ", DIM),
        Span::styled(text, DIM),
    ])
}

/// A notice: news in cyan, a warning in yellow.
fn notice((tone, text): (Tone, String), buffer: &mut ScrollBuffer) {
    let line = match tone {
        Tone::Info => Line::from(vec![Span::raw("  "), Span::styled(text, CYAN)]),
        Tone::Warn => Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("\u{26a0} {text}"), YELLOW),
        ]),
    };
    tui_output::emit_line(buffer, line);
}

/// A call's changes, with the gutter copying leaves out.
fn push_changes(changes: &[FileUpdateChange], buffer: &mut ScrollBuffer) {
    for line in crate::diff_render::render_changes(changes) {
        buffer.push_with_gutter(line, crate::diff_render::GUTTER_WIDTH);
    }
}

/// What a finished sub-agent did, as Claude Code sums one up:
/// `Done (3 tool uses · 12s)`. Without its call's times, as in a session
/// stored before Koda recorded them, the time is left out.
fn agent_summary(calls: usize, ms: Option<u64>, failed: bool) -> Line<'static> {
    let uses = if calls == 1 {
        "1 tool use".to_string()
    } else {
        format!("{calls} tool uses")
    };
    let what = if failed { "Failed" } else { "Done" };
    let summary = match ms {
        Some(ms) => format!("{what} ({uses} \u{00b7} {})", worked_for(ms)),
        None => format!("{what} ({uses})"),
    };
    Line::from(vec![
        Span::styled("  \u{2514} ", DIM),
        Span::styled(summary, DIM),
    ])
}

/// A declined call: what happened to it, and its diff when there is one to
/// draw. A refusal shows its diff first, as the approval did live; a block
/// says what the call would have done, then shows it.
fn declined_lines(
    outcome: protocol_text::Declined<'_>,
    what: &str,
    diff: Option<&[FileUpdateChange]>,
    buffer: &mut ScrollBuffer,
) {
    use protocol_text::Declined;
    let line = |text: String, style| Line::from(vec![Span::raw("  "), Span::styled(text, style)]);
    match outcome {
        Declined::Blocked => {
            tui_output::emit_line(
                buffer,
                line(format!("\u{1f50d} Would execute: {what}"), YELLOW),
            );
            push_changes(diff.unwrap_or_default(), buffer);
        }
        Declined::Rejected { feedback } => {
            push_changes(diff.unwrap_or_default(), buffer);
            tui_output::emit_line(buffer, line(format!("\u{2717} Rejected: {what}"), RED));
            if let Some(feedback) = feedback {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::styled("  \u{2514} ", DIM),
                        Span::styled(format!("Feedback: {feedback}"), DIM),
                    ]),
                );
            }
        }
        Declined::AutoRejected { reason } => {
            push_changes(diff.unwrap_or_default(), buffer);
            tui_output::emit_line(
                buffer,
                line(
                    format!("\u{2717} Auto-rejected: {what} \u{2014} {reason}"),
                    RED,
                ),
            );
        }
        Declined::Other => {
            tui_output::emit_line(buffer, line(format!("\u{2717} Declined: {what}"), RED));
        }
    }
}

/// A duration as Codex's TUI writes it: `<1s`, `12s`, `3m 4s`, `1h 2m 3s`.
pub(crate) fn worked_for(ms: u64) -> String {
    let secs = ms / 1000;
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else if seconds == 0 {
        "<1s".to_string()
    } else {
        format!("{seconds}s")
    }
}

/// Tokens always in the context (system prompt, tool definitions), which
/// Codex leaves out of both sides so the meter starts at 100% left.
const BASELINE_TOKENS: u64 = 12_000;

/// How much of the context window is left, in percent, from the last call's
/// tokens: Codex's `percent_of_context_window_remaining`.
pub(crate) fn context_left(tokens: u64, window: u64) -> u32 {
    if window <= BASELINE_TOKENS {
        return 0;
    }
    let effective = window - BASELINE_TOKENS;
    let used = tokens.saturating_sub(BASELINE_TOKENS);
    let left = effective.saturating_sub(used);
    ((left as f64 / effective as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u32
}

/// The line a turn ends with, as Codex's: `─ Worked for 12s ─`.
pub(crate) fn worked_for_line(ms: u64) -> Line<'static> {
    Line::styled(
        format!("  \u{2500} Worked for {} \u{2500}", worked_for(ms)),
        DIM,
    )
}

// ── Helper renderers ──────────────────────────────────

/// Extract file extension from tool call args JSON.
///
/// Walks the keys koda's tools actually use (`file_path` first, then
/// legacy `path`). Returning [`None`] disables Read syntax highlighting
/// for that call — fixed in this PR after we discovered the previous
/// implementation only checked `path`, silently disabling highlighting
/// for every Read call (since koda's Read tool advertises `file_path`).
pub(crate) fn extract_file_extension(args_json: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = args
        .get("file_path")
        .or_else(|| args.get("path"))
        .and_then(|v| v.as_str())?;
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(ext.to_string())
}

/// Lines of finished tool output a fold keeps at the top, in both views.
const FOLD_HEAD: usize = 5;
/// Lines a fold keeps at the bottom of a failed command's output, whose last
/// lines usually say what went wrong.
const FOLD_TAIL: usize = 5;

/// The line that stands in, in the folded view, for the lines it hides.
pub(crate) fn fold_marker(hidden: usize) -> Line<'static> {
    let noun = if hidden == 1 { "line" } else { "lines" };
    Line::from(vec![
        Span::styled("  \u{2502} ", TOOL_PREFIX),
        Span::styled(
            format!("\u{2026} +{hidden} {noun} (ctrl+o to show all)"),
            DIM,
        ),
    ])
}

/// What a `Read` result says about itself: its numbered lines run from
/// `first` to `last`, of `total` when the tool stopped early.
fn read_summary(output: &str) -> Option<String> {
    let numbers: Vec<usize> = output
        .lines()
        .filter_map(|l| l.split_once('\t')?.0.trim().parse().ok())
        .collect();
    let (&first, &last) = (numbers.first()?, numbers.last()?);
    let total = output.lines().find_map(|l| {
        let rest = l.split_once("[Showing lines ")?.1;
        rest.split_once(" of ")?
            .1
            .split_once('.')?
            .0
            .parse::<usize>()
            .ok()
    });
    Some(match total {
        Some(total) => format!("lines {first}\u{2013}{last} of {total}"),
        None if first == 1 => {
            let n = numbers.len();
            format!("{n} {}", if n == 1 { "line" } else { "lines" })
        }
        None => format!("lines {first}\u{2013}{last}"),
    })
}

/// Split a `Read` line, `{n:>6}\t{content}`, into its number and content.
fn split_numbered(line: &str) -> Option<(usize, &str)> {
    let (number, content) = line.split_once('\t')?;
    Some((number.trim_start().parse().ok()?, content))
}

/// A read's content for the full view, laid out as Qwen Code, Claude Code and
/// Codex lay out numbered code: the number right-aligned to the widest one
/// and dim, two spaces, then the content highlighted on its own. Handing the
/// highlighter the whole `     1\t# Title` line hid the `#` a markdown
/// heading needs at the start, and a tab draws as nothing, so the number ran
/// into the text. Tabs in the content become four spaces. A line that isn't
/// numbered, the tool's own note when it stopped early, stays dim.
fn numbered_read_lines(
    output: &str,
    mut highlighter: Option<&mut crate::highlight::CodeHighlighter>,
    body: Style,
) -> Vec<Line<'static>> {
    let width = output
        .lines()
        .filter_map(|l| split_numbered(l).map(|(n, _)| n.to_string().len()))
        .max()
        .unwrap_or(1);
    output
        .lines()
        .map(|line| {
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            match split_numbered(line) {
                Some((number, content)) => {
                    spans.push(Span::styled(format!("{number:>width$}  "), DIM));
                    let content = content.replace('\t', "    ");
                    match highlighter.as_deref_mut() {
                        Some(h) => spans.extend(h.highlight_spans(&content)),
                        None => spans.push(Span::styled(content, body)),
                    }
                }
                None => spans.push(Span::styled(line.to_string(), DIM)),
            }
            Line::from(spans)
        })
        .collect()
}

/// A finished tool result as the TUI shows it, each line tagged with the
/// view it belongs to. The renderer pushes these into the buffer, for live
/// turns and resumed ones alike, so both look alike and Ctrl+O switches
/// both.
///
/// - **Read:** one dim summary line (`└ 123 lines`, `└ lines 40–80 of
///   1,340`); the highlighted content is in the full view only. Most reads
///   aren't worth reading, and they were what made the reply hard to find.
/// - **Edit, Write:** the diff, not folded, as before.
/// - **Everything else:** the first [`FOLD_HEAD`] lines and, when the call
///   failed, the last [`FOLD_TAIL`], with a [`fold_marker`] for the rest.
///
/// Bodies take their effect's colour ([`theme::content_style_for`]) on top
/// of any ANSI colour of their own, so a command's output looks the same
/// whether it streamed or not.
pub(crate) fn tool_output_view(
    name: &str,
    effect: Option<koda_client::protocol::item::ToolEffect>,
    output: &str,
    failed: bool,
    file_ext: Option<&str>,
    grep_pattern: Option<&str>,
) -> Vec<(Line<'static>, Fold)> {
    use crate::truncate::{Truncated, truncate_for_display};

    if output.is_empty() {
        return Vec::new();
    }

    // Collapse consecutive blank lines (3+ → 1) to reduce visual noise,
    // especially from WebFetch HTML-to-text conversion.
    let collapsed = collapse_blank_lines(output);
    let output = &collapsed;

    // Syntax highlighting for Read tool output. Honors the global
    // `[tui] syntax_highlight = false` kill switch (see [`theme`]).
    let use_highlight = name == "Read" && file_ext.is_some() && theme::syntax_highlight_enabled();
    let is_diff_tool = matches!(name, "Edit" | "Write");
    let mut highlighter = if use_highlight {
        Some(crate::highlight::CodeHighlighter::new(file_ext.unwrap()))
    } else {
        None
    };
    let body = theme::content_style_for(effect, false);

    if name == "Read"
        && let Some(summary) = read_summary(output)
    {
        let mut lines = vec![(
            Line::from(vec![
                Span::styled("  \u{2514} ", TOOL_PREFIX),
                Span::styled(summary, DIM),
            ]),
            Fold::Folded,
        )];
        lines.extend(
            numbered_read_lines(output, highlighter.as_mut(), body)
                .into_iter()
                .map(|l| (l, Fold::Full)),
        );
        return lines;
    }

    let mut render_line = |line: &str| -> Line<'static> {
        if name == "Grep" {
            crate::tool_output_lines::grep_line(line, grep_pattern)
        } else if name == "List" {
            crate::tool_output_lines::list_line(line)
        } else if name == "Glob" {
            crate::tool_output_lines::glob_line(line)
        } else if let Some(h) = highlighter.as_mut() {
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            spans.extend(h.highlight_spans(line));
            Line::from(spans)
        } else if is_diff_tool && line.starts_with('+') {
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(line.to_string(), Style::default().fg(Color::Green)),
            ])
        } else if is_diff_tool && line.starts_with('-') {
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(line.to_string(), Style::default().fg(Color::Red)),
            ])
        } else if is_diff_tool && line.starts_with('@') {
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(line.to_string(), Style::default().fg(Color::Cyan)),
            ])
        } else {
            // Parse ANSI escape codes into native ratatui Spans.
            // Colored output from tools (cargo, git, pytest, etc.)
            // renders with proper styles instead of raw escape codes.
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            spans.extend(parse_ansi_spans(line).into_iter().map(|mut span| {
                span.style = body.patch(span.style);
                span
            }));
            Line::from(spans)
        }
    };

    if is_diff_tool {
        let always = |line| (line, Fold::Always);
        return match truncate_for_display(output) {
            Truncated::Full(_) => output.lines().map(|l| always(render_line(l))).collect(),
            Truncated::Split {
                head,
                tail,
                hidden,
                total,
            } => {
                let mut lines: Vec<_> = head.iter().map(|l| always(render_line(l))).collect();
                lines.push(always(Line::from(vec![Span::styled(
                    crate::truncate::separator(hidden, total),
                    DIM,
                )])));
                lines.extend(tail.iter().map(|l| always(render_line(l))));
                lines
            }
        };
    }

    let rendered: Vec<Line<'static>> = output.lines().map(&mut render_line).collect();
    let tail = if failed { FOLD_TAIL } else { 0 };
    if rendered.len() <= FOLD_HEAD + tail + 1 {
        return rendered.into_iter().map(|l| (l, Fold::Always)).collect();
    }
    let hidden = rendered.len() - FOLD_HEAD - tail;
    let mut lines = Vec::with_capacity(rendered.len() + 1);
    for (i, line) in rendered.into_iter().enumerate() {
        if i == FOLD_HEAD {
            lines.push((fold_marker(hidden), Fold::Folded));
        }
        let fold = if i < FOLD_HEAD || i >= FOLD_HEAD + hidden {
            Fold::Always
        } else {
            Fold::Full
        };
        lines.push((line, fold));
    }
    lines
}

/// Collapse runs of consecutive blank lines down to at most 1.
///
/// WebFetch HTML-to-text conversion often produces dozens of empty lines
/// from page footers, nav elements, etc. This keeps output scannable
/// without losing meaningful whitespace (single blank lines are preserved).
fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive_blanks = 0u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks <= 1 {
                result.push('\n');
            }
        } else {
            consecutive_blanks = 0;
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_client::protocol::item::{
        ItemCompletedNotification, ItemStartedNotification, PatchChangeKind, TextDeltaNotification,
        ToolCallOutputDeltaNotification,
    };
    use serde_json::json;

    // ── protocol messages ────────────────────────────────────────────────────

    fn call(id: &str, tool: &str, arguments: serde_json::Value) -> Item {
        Item::ToolCall {
            id: id.into(),
            tool: tool.into(),
            arguments,
            effect: None,
            kind: None,
            status: ToolCallStatus::InProgress,
            output: None,
            detail: None,
            changes: Vec::new(),
            progress: Vec::new(),
        }
    }

    /// `item` as it completes, with `status` and `output`.
    fn finished(item: Item, status: ToolCallStatus, output: &str) -> Item {
        match item {
            Item::ToolCall {
                id,
                tool,
                arguments,
                detail,
                changes,
                progress,
                ..
            } => Item::ToolCall {
                id,
                tool,
                arguments,
                effect: None,
                kind: None,
                status,
                output: Some(output.into()),
                detail,
                changes,
                progress,
            },
            other => other,
        }
    }

    /// When every test item starts; each completes 2 s later.
    const STARTED_MS: i64 = 1_700_000_000_000;

    fn started(parent: Option<&str>, item: Item) -> ServerNotification {
        ServerNotification::ItemStarted(ItemStartedNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            parent_item_id: parent.map(str::to_string),
            item,
            started_at_ms: STARTED_MS,
        })
    }

    fn completed(parent: Option<&str>, item: Item) -> ServerNotification {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            parent_item_id: parent.map(str::to_string),
            item,
            completed_at_ms: STARTED_MS + 2_000,
        })
    }

    fn output(id: &str, delta: &str) -> ServerNotification {
        ServerNotification::ToolCallOutputDelta(ToolCallOutputDeltaNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            item_id: id.into(),
            delta: delta.into(),
            stream: OutputStream::Stdout,
        })
    }

    fn reply(id: &str, delta: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(TextDeltaNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            item_id: id.into(),
            delta: delta.into(),
        })
    }

    fn text(buffer: &ScrollBuffer) -> Vec<String> {
        buffer
            .all_lines()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn view_text(view: &[(Line<'static>, Fold)], hide: Fold) -> Vec<String> {
        view.iter()
            .filter(|(_, f)| *f != hide)
            .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_read_folds_to_its_summary() {
        let out = "     1\tfn a() {}\n     2\tfn b() {}\n     3\tfn c() {}";
        let view = tool_output_view(
            "Read",
            Some(koda_client::protocol::item::ToolEffect::ReadOnly),
            out,
            false,
            Some("rs"),
            None,
        );
        let folded = view_text(&view, Fold::Full);
        assert_eq!(folded, vec!["  \u{2514} 3 lines"]);
        let full = view_text(&view, Fold::Folded);
        assert_eq!(full.len(), 3);
        assert!(full[1].contains("fn b"), "{full:?}");
    }

    /// The full view lays a read out as peers lay out code: numbers
    /// right-aligned in their own dim column, two spaces, then the content,
    /// highlighted on its own so a markdown heading is seen as one.
    #[test]
    fn a_read_shows_numbers_in_a_column_and_highlights_the_content() {
        let out = "     9\t# Title\n    10\tplain words\n    11\t\tindented\n\n... [Showing lines 9-11 of 40. Continue with start_line=12.]";
        let view = tool_output_view(
            "Read",
            Some(koda_client::protocol::item::ToolEffect::ReadOnly),
            out,
            false,
            Some("md"),
            None,
        );
        let full: Vec<&Line<'static>> = view
            .iter()
            .filter(|(_, f)| *f == Fold::Full)
            .map(|(l, _)| l)
            .collect();
        let text =
            |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
        assert_eq!(text(full[0]), "  \u{2502}  9  # Title");
        assert_eq!(text(full[1]), "  \u{2502} 10  plain words");
        assert_eq!(text(full[2]), "  \u{2502} 11      indented");
        // The number has a span of its own, dim.
        assert_eq!(full[0].spans[1].content, " 9  ");
        assert_eq!(full[0].spans[1].style, DIM);
        // The heading is highlighted as one: not the colour of plain text.
        if theme::syntax_highlight_enabled() {
            let heading = full[0].spans[2].style.fg;
            let plain = full[1].spans[2].style.fg;
            assert!(
                heading.is_some() && heading != plain,
                "{heading:?} vs {plain:?}"
            );
        }
        // The tool's note isn't numbered: dim, as it is.
        let note = full.last().unwrap();
        assert!(
            text(note).contains("[Showing lines 9-11 of 40"),
            "{}",
            text(note)
        );
        assert_eq!(note.spans[1].style, DIM);
    }

    #[test]
    fn a_read_summary_names_a_range_and_a_cut() {
        assert_eq!(
            read_summary("    40\ta\n    41\tb").as_deref(),
            Some("lines 40\u{2013}41")
        );
        let cut =
            "     1\ta\n     2\tb\n\n... [Showing lines 1-2 of 1340. Continue with start_line=3.]";
        assert_eq!(
            read_summary(cut).as_deref(),
            Some("lines 1\u{2013}2 of 1340")
        );
        assert_eq!(read_summary("     1\tonly").as_deref(), Some("1 line"));
        // Not numbered (an empty file, an error): shown as it is.
        assert_eq!(read_summary("(empty file)"), None);
        let view = tool_output_view(
            "Read",
            Some(koda_client::protocol::item::ToolEffect::ReadOnly),
            "(empty file)",
            false,
            Some("rs"),
            None,
        );
        assert_eq!(
            view_text(&view, Fold::Full),
            vec!["  \u{2502} (empty file)"]
        );
    }

    #[test]
    fn long_output_folds_after_its_first_lines() {
        let out: String = (1..=20).map(|i| format!("out {i}\n")).collect();
        let view = tool_output_view("Bash", None, &out, false, None, None);
        let folded = view_text(&view, Fold::Full);
        assert_eq!(folded.len(), 6, "{folded:?}");
        assert!(folded[4].ends_with("out 5"));
        assert!(folded[5].contains("+15 lines (ctrl+o to show all)"));
        assert_eq!(view_text(&view, Fold::Folded).len(), 20);

        // A failure keeps its last lines too: they say what went wrong.
        let failed = view_text(
            &tool_output_view("Bash", None, &out, true, None, None),
            Fold::Full,
        );
        assert_eq!(failed.len(), 11, "{failed:?}");
        assert!(failed[5].contains("+10 lines"));
        assert!(failed[10].ends_with("out 20"));

        // Six lines fold nothing away worth a marker.
        let short: String = (1..=6).map(|i| format!("out {i}\n")).collect();
        let view = tool_output_view("Bash", None, &short, false, None, None);
        assert!(view.iter().all(|(_, f)| *f == Fold::Always));
    }

    #[test]
    fn output_takes_its_effect_colour() {
        let view = tool_output_view(
            "WebFetch",
            Some(koda_client::protocol::item::ToolEffect::ReadOnly),
            "plain text",
            false,
            None,
            None,
        );
        let body = &view[0].0.spans[1];
        assert_eq!(
            body.style.fg,
            theme::content_style_for(
                Some(koda_client::protocol::item::ToolEffect::ReadOnly),
                false
            )
            .fg
        );
        // An ANSI colour of its own still wins.
        let view = tool_output_view("Bash", None, "\x1b[31mred\x1b[0m", false, None, None);
        assert_eq!(view[0].0.spans[1].style.fg, Some(Color::Red));
    }

    /// Streamed output can't be folded as it arrives, so it's folded in place
    /// when the command ends.
    #[test]
    fn streamed_output_folds_when_the_command_ends() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let bash = call("b1", "Bash", json!({ "command": "seq 20" }));
        renderer.render(&started(None, bash.clone()), &mut buffer);
        for i in 1..=20 {
            renderer.render(&output("b1", &format!("{i}\n")), &mut buffer);
        }
        renderer.render(
            &completed(
                None,
                finished(
                    bash,
                    ToolCallStatus::Completed,
                    "Exit code: 0 | stdout: 20 lines",
                ),
            ),
            &mut buffer,
        );
        let folded = text(&buffer);
        // Header, 5 lines, the marker, the exit line.
        assert_eq!(folded.len(), 8, "{folded:?}");
        assert!(folded[6].contains("+15 lines"), "{folded:?}");
        assert!(folded[7].contains("Exit code: 0"), "{folded:?}");
        buffer.set_full(true);
        assert_eq!(text(&buffer).len(), 22);
    }

    /// A refused call says so under its approval's diff, which it doesn't
    /// draw again; a stopped call adds nothing.
    #[test]
    fn a_refused_call_says_so_and_a_stopped_one_adds_nothing() {
        let change = FileUpdateChange {
            path: "a.txt".into(),
            kind: PatchChangeKind::Add,
            diff: "x\n".into(),
        };
        for (status, output, says) in [
            (
                ToolCallStatus::Declined,
                "User rejected this action.",
                Some("\u{2717} Rejected: Write a.txt"),
            ),
            (
                ToolCallStatus::Declined,
                "User rejected this action with feedback: use b.txt",
                Some("Feedback: use b.txt"),
            ),
            (ToolCallStatus::Cancelled, "[cancelled]", None),
        ] {
            let mut renderer = TuiRenderer::new();
            let mut buffer = ScrollBuffer::new(256);
            let write = call("w1", "Write", json!({ "file_path": "a.txt" }));
            renderer.render(&started(None, write.clone()), &mut buffer);
            renderer.approval("w1", std::slice::from_ref(&change), &mut buffer);
            let before = buffer.all_lines().count();
            let Item::ToolCall {
                id,
                tool,
                arguments,
                ..
            } = write
            else {
                unreachable!()
            };
            let done = Item::ToolCall {
                id,
                tool,
                arguments,
                effect: None,
                kind: None,
                status,
                output: Some(output.into()),
                detail: Some("Write a.txt".into()),
                changes: vec![change.clone()],
                progress: Vec::new(),
            };
            renderer.render(&completed(None, done), &mut buffer);
            let added: Vec<String> = text(&buffer).split_off(before);
            match says {
                Some(says) => assert!(added.iter().any(|l| l.contains(says)), "{added:?}"),
                None => assert!(added.is_empty(), "{added:?}"),
            }
            // The diff isn't drawn twice.
            assert!(!added.iter().any(|l| l.contains("+ x")), "{added:?}");
            assert!(renderer.calls.is_empty());
        }
    }

    /// A call blocked without asking, as in plan mode, says what it would
    /// have done, with its changes.
    #[test]
    fn a_blocked_call_shows_what_it_would_have_done() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let Item::ToolCall {
            id,
            tool,
            arguments,
            ..
        } = call("w1", "Write", json!({ "file_path": "a.txt" }))
        else {
            unreachable!()
        };
        let blocked = Item::ToolCall {
            id,
            tool,
            arguments,
            effect: None,
            kind: None,
            status: ToolCallStatus::Declined,
            output: Some("[plan mode] Not run: this session is read-only".into()),
            detail: Some("Write a.txt".into()),
            changes: vec![FileUpdateChange {
                path: "a.txt".into(),
                kind: PatchChangeKind::Add,
                diff: "hello\n".into(),
            }],
            progress: Vec::new(),
        };
        renderer.render(&started(None, blocked.clone()), &mut buffer);
        renderer.render(&completed(None, blocked), &mut buffer);
        let lines = text(&buffer);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Would execute: Write a.txt")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("+ hello")), "{lines:?}");
    }

    /// An approval draws the diff above its dialog, so the finished Write
    /// only says how long it is.
    #[test]
    fn an_approved_edit_shows_its_diff_once() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let edit = call("e1", "Edit", json!({ "file_path": "a.rs" }));
        renderer.render(&started(None, edit.clone()), &mut buffer);
        let change = FileUpdateChange {
            path: "a.rs".into(),
            kind: PatchChangeKind::Update { move_path: None },
            diff: "@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
        };
        renderer.approval("e1", &[change], &mut buffer);
        renderer.render(
            &completed(
                None,
                finished(edit, ToolCallStatus::Completed, "Edited a.rs"),
            ),
            &mut buffer,
        );
        let lines = text(&buffer);
        assert!(lines.iter().any(|l| l.contains("   1 - old")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("   1 + new")), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  \u{2514} Edit: 1 line(s)");
    }

    /// A sub-agent's calls are in the full view only; when it ends, its call
    /// sums it up in both, as Claude Code does.
    #[test]
    fn a_sub_agents_calls_fold_into_its_summary() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let agent = call(
            "a1",
            "InvokeAgent",
            json!({ "agent_name": "explore", "prompt": "look" }),
        );
        renderer.render(&started(None, agent.clone()), &mut buffer);
        for (id, file) in [("r1", "a.rs"), ("r2", "b.rs")] {
            let read = call(id, "Read", json!({ "file_path": file }));
            renderer.render(&started(Some("a1"), read.clone()), &mut buffer);
            renderer.render(&output(id, "line\n"), &mut buffer);
            renderer.render(
                &completed(Some("a1"), finished(read, ToolCallStatus::Completed, "x")),
                &mut buffer,
            );
        }
        renderer.render(
            &completed(
                None,
                finished(agent, ToolCallStatus::Completed, "Found it."),
            ),
            &mut buffer,
        );
        let folded = text(&buffer);
        assert!(!folded.iter().any(|l| l.contains("Read")), "{folded:?}");
        let done = folded
            .iter()
            .find(|l| l.contains("Done (2 tool uses"))
            .unwrap_or_else(|| panic!("{folded:?}"));
        // From the call's item times, as on a resumed session.
        assert!(done.ends_with("2 tool uses \u{00b7} 2s)"), "{done}");
        assert!(folded.iter().any(|l| l.contains("Found it.")), "{folded:?}");
        buffer.set_full(true);
        let full = text(&buffer);
        assert!(
            full.iter()
                .any(|l| l.contains("Read") && l.contains("b.rs")),
            "{full:?}"
        );
        assert!(full.iter().any(|l| l.contains("\u{2502} line")), "{full:?}");
    }

    /// The `───` line goes before a reply's first words, and the reply is
    /// drawn line by line as it streams.
    #[test]
    fn a_reply_streams_under_its_rule() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let message = Item::AgentMessage {
            id: "m1".into(),
            text: String::new(),
        };
        renderer.render(&started(None, message.clone()), &mut buffer);
        renderer.render(&reply("m1", "\nHello"), &mut buffer);
        renderer.render(&reply("m1", " there\nsecond"), &mut buffer);
        assert_eq!(text(&buffer).len(), 2, "{:?}", text(&buffer));
        renderer.render(&completed(None, message), &mut buffer);
        let lines = text(&buffer);
        assert_eq!(lines[0], "  \u{2500}\u{2500}\u{2500}");
        assert!(lines[1].contains("Hello there"), "{lines:?}");
        assert!(lines[2].contains("second"), "{lines:?}");
        // Another item's text isn't this reply's.
        renderer.render(&reply("other", "stray\n"), &mut buffer);
        assert_eq!(text(&buffer).len(), 3);
    }

    fn stored(parent: Option<&str>, item: Item, times: Option<(i64, i64)>) -> TurnItem {
        TurnItem {
            parent_item_id: parent.map(str::to_string),
            item,
            started_at_ms: times.map(|(s, _)| s),
            completed_at_ms: times.map(|(_, c)| c),
        }
    }

    fn turn(items: Vec<TurnItem>, status: TurnStatus, duration_ms: Option<u64>) -> Turn {
        Turn {
            id: "t".into(),
            status,
            error: None,
            usage: None,
            duration_ms,
            items,
        }
    }

    /// A stored turn is drawn as it was live: its prompt, reply, calls and
    /// how long it worked. A sub-agent's items are stored before its call,
    /// as they complete first, and still fold into its summary, timed from
    /// the call's stored times.
    #[test]
    fn a_stored_turn_replays_as_it_was_drawn_live() {
        let agent = finished(
            call(
                "a1",
                "InvokeAgent",
                json!({"agent_name": "explore", "prompt": "look"}),
            ),
            ToolCallStatus::Completed,
            "Found it.",
        );
        let read = finished(
            call("r1", "Read", json!({"file_path": "WOMBAT.md"})),
            ToolCallStatus::Completed,
            "     1\tx",
        );
        let items = vec![
            stored(
                None,
                Item::UserMessage {
                    id: "u1".into(),
                    text: "look around".into(),
                    client_message_ids: Vec::new(),
                    mentions: Vec::new(),
                },
                None,
            ),
            stored(Some("a1"), read, Some((1_000, 2_000))),
            stored(None, agent, Some((0, 12_000))),
            stored(
                None,
                Item::AgentMessage {
                    id: "m1".into(),
                    text: "All done.".into(),
                },
                None,
            ),
        ];
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.replay_turn(
            &turn(items, TurnStatus::Completed, Some(13_000)),
            &mut buffer,
        );
        let folded = text(&buffer);
        let at = |needle: &str| {
            folded
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} in {folded:?}"))
        };
        assert!(at("\u{276f} look around") < at("InvokeAgent"));
        assert!(at("InvokeAgent") < at("Done (1 tool use \u{00b7} 12s)"));
        assert!(at("Done (1 tool use") < at("Found it."));
        assert!(at("Found it.") < at("All done."));
        assert!(at("All done.") < at("Worked for 13s"));
        assert!(!folded.iter().any(|l| l.contains("WOMBAT")), "{folded:?}");
        buffer.set_full(true);
        let full = text(&buffer);
        let header = full.iter().position(|l| l.contains("InvokeAgent")).unwrap();
        let child = full.iter().position(|l| l.contains("WOMBAT")).unwrap();
        assert!(
            header < child,
            "the call starts before its sub-agent's: {full:?}"
        );
    }

    /// On replay a refused edit shows its diff, which the approval drew
    /// live, then that it was refused; a failed turn says why.
    #[test]
    fn a_replayed_refusal_shows_its_diff_and_a_failed_turn_its_error() {
        let refused = Item::ToolCall {
            id: "e1".into(),
            tool: "Edit".into(),
            arguments: json!({"file_path": "a.rs"}),
            effect: None,
            kind: None,
            status: ToolCallStatus::Declined,
            output: Some("User rejected this action.".into()),
            detail: Some("Edit a.rs".into()),
            changes: vec![FileUpdateChange {
                path: "a.rs".into(),
                kind: PatchChangeKind::Update { move_path: None },
                diff: "@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
            }],
            progress: Vec::new(),
        };
        let mut failed = turn(vec![stored(None, refused, None)], TurnStatus::Failed, None);
        failed.error = Some(koda_client::protocol::turn::TurnError {
            message: "the provider went away".into(),
            code: None,
        });
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.replay_turn(&failed, &mut buffer);
        let lines = text(&buffer);
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        assert!(
            at("   1 + new") < at("\u{2717} Rejected: Edit a.rs"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Turn failed: the provider went away"))
        );
        // A turn with no recorded duration says nothing of it.
        assert!(!lines.iter().any(|l| l.contains("Worked for")), "{lines:?}");
    }

    /// A sub-agent stored without times, as before Koda recorded them, is
    /// summed up without its time.
    #[test]
    fn a_sub_agent_without_times_is_summed_up_without_one() {
        let agent = finished(
            call("a1", "InvokeAgent", json!({"agent_name": "explore"})),
            ToolCallStatus::Completed,
            "ok",
        );
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.replay_turn(
            &turn(vec![stored(None, agent, None)], TurnStatus::Completed, None),
            &mut buffer,
        );
        assert!(
            text(&buffer)
                .iter()
                .any(|l| l.ends_with("Done (0 tool uses)"))
        );
    }

    #[test]
    fn worked_for_reads_as_codex_writes_it() {
        assert_eq!(worked_for(400), "<1s");
        assert_eq!(worked_for(12_300), "12s");
        assert_eq!(worked_for(184_000), "3m 4s");
        assert_eq!(worked_for(3_723_000), "1h 2m 3s");
        let line: String = worked_for_line(12_000)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(line, "  \u{2500} Worked for 12s \u{2500}");
    }

    /// Codex's meter: the baseline is left out, so a first call reads 100%.
    #[test]
    fn context_left_reads_as_codex_reads_it() {
        assert_eq!(context_left(0, 100_000), 100);
        assert_eq!(context_left(12_000, 100_000), 100);
        assert_eq!(context_left(56_000, 100_000), 50);
        assert_eq!(context_left(100_000, 100_000), 0);
        assert_eq!(context_left(150_000, 100_000), 0);
        assert_eq!(context_left(5_000, 10_000), 0);
    }

    /// Drive `TuiRenderer` with a complete Read tool call and return all
    /// buffer lines for inspection.
    fn render_read(file_path: &str, content: &str) -> Vec<ratatui::text::Line<'static>> {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let read = call("t1", "Read", json!({ "file_path": file_path }));
        renderer.render(&started(None, read.clone()), &mut buffer);
        renderer.render(
            &completed(None, finished(read, ToolCallStatus::Completed, content)),
            &mut buffer,
        );
        // The content is in the full view; a numbered read's folded view is
        // just its summary.
        buffer.set_full(true);
        buffer.all_lines().cloned().collect()
    }

    /// Drive `TuiRenderer` with any tool call (List/Glob/Grep/etc) and return
    /// the full buffer. The `args` value is whatever JSON the tool expects;
    /// the renderer just forwards it through `extract_file_extension` and the
    /// per-tool dispatch in `render_tool_output`.
    fn render_tool(
        tool_name: &str,
        args: serde_json::Value,
        output: &str,
    ) -> Vec<ratatui::text::Line<'static>> {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let item = call("t1", tool_name, args);
        renderer.render(&started(None, item.clone()), &mut buffer);
        renderer.render(
            &completed(None, finished(item, ToolCallStatus::Completed, output)),
            &mut buffer,
        );
        buffer.all_lines().cloned().collect()
    }

    /// Return all spans from lines whose *joined text* contains `needle`.
    fn spans_on_lines_containing(
        lines: &[ratatui::text::Line<'static>],
        needle: &str,
    ) -> Vec<ratatui::text::Span<'static>> {
        lines
            .iter()
            .filter(|line| {
                let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                joined.contains(needle)
            })
            .flat_map(|line| line.spans.iter().cloned())
            .collect()
    }

    // ── extract_file_extension ────────────────────────────────────────────────

    #[test]
    fn extract_file_extension_handles_file_path_key() {
        // The bug fix: koda’s tools advertise `file_path`, not `path`.
        // Pre-fix, this returned None and silently disabled Read
        // syntax highlighting on every Read call.
        let args = r#"{"file_path": "src/main.rs"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("rs"));
    }

    #[test]
    fn extract_file_extension_falls_back_to_path() {
        let args = r#"{"path": "src/lib.py"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("py"));
    }

    #[test]
    fn extract_file_extension_prefers_file_path_when_both_present() {
        let args = r#"{"file_path": "a.rs", "path": "b.py"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("rs"));
    }

    #[test]
    fn extract_file_extension_returns_none_for_extensionless() {
        let args = r#"{"file_path": "Makefile"}"#;
        assert_eq!(extract_file_extension(args), None);
    }

    // ── Read tool coloring regression tests ───────────────────────────────────
    // These drive the full TuiRenderer → ScrollBuffer pipeline to confirm that
    // Read output is actually syntax-highlighted, not rendered flat-white.
    // Each test seeds a specific extension so a future SyntaxSet regression
    // fails loudly here rather than silently in the user’s terminal.

    /// Headline regression: Cargo.toml was showing flat white because syntect’s
    /// default set had no TOML grammar. End-to-end proof it’s now colored.
    #[test]
    fn read_toml_produces_multiple_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read(
            "Cargo.toml",
            "[package]\nname = \"koda\"\nversion = \"0.2.16\"",
        );
        let spans = spans_on_lines_containing(&lines, "[package]");
        assert!(
            spans.len() > 2,
            "[package] line should have multiple colored spans (got {}); \
             TOML syntax highlighting broken — check two-face dep",
            spans.len()
        );
        // At least one content span must carry a non-default foreground.
        let colored = spans.iter().any(|s| {
            s.style
                .fg
                .is_some_and(|c| c != ratatui::style::Color::Reset)
        });
        assert!(
            colored,
            "no colored spans on [package] line — highlighter is a no-op"
        );
    }

    /// Rust was working before two-face; ensure the switch didn’t regress it.
    #[test]
    fn read_rust_still_produces_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read("src/main.rs", "fn main() {}\nprintln!(\"hello\");");
        let spans = spans_on_lines_containing(&lines, "fn main");
        assert!(
            spans.len() > 2,
            "`fn main` line should have multiple spans; Rust highlighting broken"
        );
    }

    /// TypeScript was silently broken (no grammar in syntect defaults).
    #[test]
    fn read_typescript_produces_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read(
            "index.ts",
            "interface Foo { bar: string }\nconst x: number = 42;",
        );
        let spans = spans_on_lines_containing(&lines, "interface");
        assert!(
            spans.len() > 2,
            "`interface` line should have multiple spans; TypeScript highlighting broken"
        );
    }

    /// Unknown extensions must not panic and must render as plain text
    /// (single content span, not zero spans).
    #[test]
    fn read_unknown_extension_falls_back_safely() {
        let lines = render_read("data.xyz123", "some content here");
        let spans = spans_on_lines_containing(&lines, "some content");
        assert!(
            !spans.is_empty(),
            "unknown-extension Read must still emit spans (got none)"
        );
    }

    /// Extensionless files (Makefile, Dockerfile) must render without panicking.
    #[test]
    fn read_extensionless_file_renders_safely() {
        let lines = render_read("Makefile", ".PHONY: all\nall:\n\techo done");
        // Just assert we get output lines containing the content.
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all_text.contains(".PHONY"),
            "Makefile content missing from output"
        );
    }

    // ── Other read-only tools — E2E pipeline coverage ─────────────────────────
    // List / Glob / Grep have unit tests in tool_output_lines, but those
    // bypass the dispatch in render_tool_output. These tests catch the
    // class of bugs where a tool name typo or extension extraction quirk
    // silently flattens output — same shape as the Read tests above.

    /// List output: directory entries should render with non-default styling
    /// (bold), proving the dispatch → render_list_line path works end-to-end.
    #[test]
    fn list_directory_entries_render_styled() {
        let lines = render_tool(
            "List",
            json!({ "path": "." }),
            "d src\nd tests\n  Cargo.toml\n  README.md",
        );
        // Find the line containing "src" — directory entry.
        let src_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "src"));
        let src_line = src_line.expect("src directory entry not in output");
        let dir_span = src_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "src")
            .unwrap();
        assert!(
            dir_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "directory entry should be bold; render_list_line dispatch broken"
        );
    }

    /// List output: file entries get extension-bucket coloring (e.g. .toml → yellow).
    #[test]
    fn list_file_entries_colored_by_extension() {
        let lines = render_tool("List", json!({ "path": "." }), "  Cargo.toml\n  src.rs");
        let toml_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "Cargo.toml"))
            .expect("Cargo.toml entry not in output");
        let toml_span = toml_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Cargo.toml")
            .unwrap();
        assert_eq!(
            toml_span.style.fg,
            Some(ratatui::style::Color::Yellow),
            ".toml file should be yellow (config bucket); style_for_extension dispatch broken"
        );
    }

    /// Glob output: file paths get extension-bucket coloring; meta lines dim.
    #[test]
    fn glob_paths_colored_by_extension() {
        let lines = render_tool(
            "Glob",
            json!({ "pattern": "**/*.rs" }),
            "3 files matched:\nsrc/main.rs\nsrc/lib.rs\ntests/it.rs",
        );
        let rs_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "src/main.rs"))
            .expect("src/main.rs not in Glob output");
        let rs_span = rs_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "src/main.rs")
            .unwrap();
        assert_eq!(
            rs_span.style.fg,
            Some(ratatui::style::Color::Green),
            ".rs file should be green (source bucket); render_glob_line dispatch broken"
        );
    }

    /// Grep output: matched substrings inside content render with MATCH_HIT style.
    #[test]
    fn grep_pattern_hits_styled_with_match_hit() {
        let lines = render_tool(
            "Grep",
            json!({ "pattern": "TODO", "path": "." }),
            "src/main.rs:42:    // TODO: refactor this",
        );
        // The hit span should carry the MATCH_HIT style (amber + bold).
        let hit_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "TODO");
        let hit_span = hit_span.expect("TODO match span missing; Grep dispatch broken");
        assert!(
            hit_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "Grep match should be bold (MATCH_HIT); pattern propagation broken"
        );
        assert_eq!(
            hit_span.style.fg,
            Some(ratatui::style::Color::Rgb(255, 191, 0)),
            "Grep match should be amber (MATCH_HIT)"
        );
    }

    /// Grep without a pattern (e.g. structured query) still renders path/lineno coloring.
    #[test]
    fn grep_path_and_lineno_styled_without_pattern() {
        let lines = render_tool(
            "Grep",
            json!({ "path": "." }),
            "src/main.rs:42:fn main() {}",
        );
        // File path span should be present and styled (cyan).
        let path_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "src/main.rs");
        assert!(
            path_span.is_some(),
            "Grep file path span missing — render_grep_line dispatch broken"
        );
        assert!(
            path_span.unwrap().style.fg.is_some(),
            "Grep file path should have a foreground color"
        );
    }

    // ── collapse_blank_lines ────────────────────────────────────────────────────

    #[test]
    fn test_collapse_preserves_single_blank() {
        assert_eq!(collapse_blank_lines("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_collapse_many_blanks() {
        assert_eq!(collapse_blank_lines("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_collapse_no_blanks() {
        assert_eq!(collapse_blank_lines("a\nb\nc"), "a\nb\nc");
    }

    #[test]
    fn test_collapse_all_blank() {
        assert_eq!(collapse_blank_lines("\n\n\n\n"), "\n");
    }
}
