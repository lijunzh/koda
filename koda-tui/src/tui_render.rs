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
//! ## Folding (Normal and Verbose)
//!
//! Every line is tagged with the view it belongs to ([`Fold`]); Ctrl+O
//! switches between the folded view (Normal) and the full one (Verbose), as
//! Claude Code's does. In Normal, as Claude Code shows a turn:
//!
//! - **Thinking** is a status line while it runs ([`WorkingArea`]) and then
//!   one line, `∴ Thought for 12s`. Its text is in the full view.
//! - **Lookups** run one after another (reads, searches, listings and
//!   read-only commands) are one row, `Read 3 files, ran 2 commands`, with no
//!   output; each call and its output are in the full view. A thought between
//!   two lookups goes into their row. Anything else shown ends the row.
//! - **A plan update** (`TodoWrite`) is the working area's plan panel, not a
//!   row; the call is in the full view.
//!
//! [`WorkingArea`]: crate::widgets::working_area::WorkingArea

use crate::ansi_parse::parse_ansi_spans;
use crate::protocol_text::{self, Tone};
use crate::scroll_buffer::Fold;
use crate::scroll_buffer::ScrollBuffer;
use crate::theme;
use crate::tui_output::{self, CYAN, DIM, RED, TOOL_PREFIX, YELLOW};
use crate::widgets::working_area::{Step, WorkingArea};
use koda_client::protocol::ServerNotification;
use koda_client::protocol::item::{
    CommandAction, FileUpdateChange, Item, MessagePhase, OutputStream, PatchChangeKind,
    ToolCallStatus, ToolEffect, TurnItem,
};
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
    /// Its header, not drawn yet: it started while another of the session's
    /// calls was open (a batch run together), so it is drawn with its
    /// result, where it lands under its own header.
    header: Option<Line<'static>>,
    /// Its lines are in the full view only: a lookup, whose row sums it up
    /// in the folded view, or a plan update, which the plan panel shows.
    quiet: bool,
    /// The lookup row it counts on, an index into `TuiRenderer::groups`.
    group: Option<usize>,
    /// The item its lines belong to in the buffer, which opens and closes
    /// as one: its lookup row's, its sub-agent's, or its own.
    item: Option<u64>,
}

impl Call {
    /// The view its own lines go in.
    fn fold(&self) -> Fold {
        if self.quiet { Fold::Full } else { Fold::Always }
    }
}

/// What a lookup does, for its row's words.
#[derive(Debug, Clone, Copy)]
enum Lookup {
    Read,
    Search,
    List,
    Command,
}

/// Which of the session's calls only look: a read, search or listing, or a
/// command the host found read-only. They fold into one row, as Claude Code
/// folds reads and searches. A call stored before Koda sent its effect
/// keeps its own row.
///
/// A command counts as what its `commandActions` say it does, as Codex's
/// `Explored` cell reads them: `cat a.rs && rg x` is a read and a search.
/// One it can't read (`Unknown`) counts as a command.
fn lookup(
    tool: &str,
    effect: Option<ToolEffect>,
    actions: &[CommandAction],
) -> Option<Vec<Lookup>> {
    if effect != Some(ToolEffect::ReadOnly) {
        return None;
    }
    match tool {
        "Read" => Some(vec![Lookup::Read]),
        "Grep" | "Glob" => Some(vec![Lookup::Search]),
        "List" => Some(vec![Lookup::List]),
        "Bash"
            if actions.is_empty()
                || actions
                    .iter()
                    .any(|a| matches!(a, CommandAction::Unknown { .. })) =>
        {
            Some(vec![Lookup::Command])
        }
        "Bash" => Some(
            actions
                .iter()
                .map(|action| match action {
                    CommandAction::Read { .. } => Lookup::Read,
                    CommandAction::ListFiles { .. } => Lookup::List,
                    CommandAction::Search { .. } => Lookup::Search,
                    CommandAction::Unknown { .. } => Lookup::Command,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Lookups run one after another, drawn as one row.
#[derive(Default)]
struct Group {
    /// The row's item in the buffer ([`ScrollBuffer::push_row`]).
    row: u64,
    reads: usize,
    searches: usize,
    lists: usize,
    commands: usize,
    /// Calls still running: the row says what it is doing.
    running: usize,
    failed: usize,
}

impl Group {
    fn count(&mut self, lookup: Lookup) {
        match lookup {
            Lookup::Read => self.reads += 1,
            Lookup::Search => self.searches += 1,
            Lookup::List => self.lists += 1,
            Lookup::Command => self.commands += 1,
        }
    }

    /// The row, in Claude Code's words: `Read 3 files, searched 5 patterns,
    /// listed 2 directories`, then `ran 4 commands`; `Reading …` while a
    /// call runs. A failure is counted in red.
    fn line(&self) -> Line<'static> {
        let live = self.running > 0;
        let parts: Vec<String> = [
            (self.reads, ("read", "reading"), ("file", "files")),
            (
                self.searches,
                ("searched", "searching"),
                ("pattern", "patterns"),
            ),
            (
                self.lists,
                ("listed", "listing"),
                ("directory", "directories"),
            ),
            (self.commands, ("ran", "running"), ("command", "commands")),
        ]
        .into_iter()
        .filter(|(n, _, _)| *n > 0)
        .map(|(n, (done, doing), (one, many))| {
            let verb = if live { doing } else { done };
            let noun = if n == 1 { one } else { many };
            format!("{verb} {n} {noun}")
        })
        .collect();
        let mut words = parts.join(", ");
        if let Some(first) = words.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        if live {
            words.push('\u{2026}');
        }
        let mut spans = vec![
            Span::styled(
                "\u{25cf} ",
                theme::tool_dot("Read", Some(ToolEffect::ReadOnly)),
            ),
            Span::styled(words, tui_output::BOLD),
        ];
        if self.failed > 0 {
            spans.push(Span::styled(
                format!(" \u{00b7} {} failed", self.failed),
                RED,
            ));
        }
        spans.push(Span::styled(" (ctrl+o to expand)", DIM));
        Line::from(spans)
    }
}

/// A finished thought, not drawn yet: it goes in before whatever is shown
/// next, or into the lookup row it falls inside.
struct Thought {
    /// How long it took, when the items' times are known.
    ms: Option<u64>,
    /// Whether the model sent any text for it, which the full view shows.
    text: bool,
    /// The item its lines belong to in the buffer.
    item: u64,
}

/// A sub-agent at work: its `InvokeAgent` call, for the line that sums it up.
struct AgentRun {
    /// When its call started, in Unix milliseconds, if recorded.
    started_ms: Option<i64>,
    /// Tool calls it has made.
    calls: usize,
    /// The item its calls' lines and its summary belong to.
    item: u64,
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
    /// What Koda is doing now, and the turn's plan, for the working area
    /// above the prompt.
    pub working: WorkingArea,
    /// When the reasoning being drawn started, in Unix milliseconds.
    thinking_since: Option<i64>,
    /// Whether the reasoning being drawn has had any text.
    thinking_text: bool,
    /// The item the reasoning being drawn belongs to in the buffer.
    thinking_item: Option<u64>,
    /// A finished thought, not drawn yet.
    thought: Option<Thought>,
    /// The turn's lookup rows.
    groups: Vec<Group>,
    /// The row lookups join now; `None` once something else was shown.
    open_group: Option<usize>,
    /// Where the turn's steps start in the buffer, after its prompt.
    turn_start: Option<u64>,
    /// Where the reply being drawn starts in the buffer.
    message_start: Option<u64>,
    /// Where the turn's answer is in the buffer: its last reply that isn't
    /// commentary.
    answer: Option<(u64, u64)>,
    /// What the turn did, for its row in focus view.
    stats: TurnStats,
}

/// What a turn did, for the row its steps fold behind in focus view: its
/// own tool calls, and its edits' diffstat, as Claude Code's focus view.
#[derive(Default)]
struct TurnStats {
    calls: usize,
    files: HashSet<String>,
    added: usize,
    removed: usize,
}

impl TurnStats {
    fn count_changes(&mut self, changes: &[FileUpdateChange]) {
        for change in changes {
            self.files.insert(change.path.clone());
            match change.kind {
                PatchChangeKind::Add => self.added += change.diff.lines().count(),
                PatchChangeKind::Delete => {}
                PatchChangeKind::Update { .. } => {
                    for line in change.diff.lines() {
                        if line.starts_with('+') && !line.starts_with("+++") {
                            self.added += 1;
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            self.removed += 1;
                        }
                    }
                }
            }
        }
    }

    /// `▸ Worked for 2m 5s · 17 tool calls · 2 files +40 −12`.
    fn row(&self, ms: Option<u64>) -> Line<'static> {
        let mut parts = Vec::new();
        if let Some(ms) = ms {
            parts.push(format!("Worked for {}", worked_for(ms)));
        }
        match self.calls {
            0 => {}
            1 => parts.push("1 tool call".to_string()),
            n => parts.push(format!("{n} tool calls")),
        }
        match self.files.len() {
            0 => {}
            n => parts.push(format!(
                "{n} file{} +{} \u{2212}{}",
                if n == 1 { "" } else { "s" },
                self.added,
                self.removed
            )),
        }
        if parts.is_empty() {
            parts.push("Worked".to_string());
        }
        Line::styled(format!("  \u{25b8} {}", parts.join(" \u{00b7} ")), DIM)
    }
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
            working: WorkingArea::new(),
            thinking_since: None,
            thinking_text: false,
            thinking_item: None,
            thought: None,
            groups: Vec::new(),
            open_group: None,
            turn_start: None,
            message_start: None,
            answer: None,
            stats: TurnStats::default(),
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
            self.break_run(buffer);
            push_changes(changes, buffer);
        }
    }

    /// The turn ended: a thought not drawn yet goes in, and the lookup row
    /// closes. A row whose calls never ended, as when the turn was stopped,
    /// stops saying it is at work.
    pub fn end_turn(&mut self, buffer: &mut ScrollBuffer) {
        self.break_run(buffer);
        for group in self.groups.iter_mut().filter(|g| g.running > 0) {
            group.running = 0;
            buffer.set_row(group.row, group.line());
        }
    }

    /// The turn completed, its last line drawn: in focus view its steps go
    /// behind one row, `▸ Worked for 2m 5s · 17 tool calls`, between its
    /// prompt and its answer. `ms` is how long it worked. A turn that
    /// stopped or failed isn't folded: it has no answer to keep in view.
    pub fn fold_turn(&mut self, ms: Option<u64>, buffer: &mut ScrollBuffer) {
        if let (Some(start), Some(answer)) = (self.turn_start, self.answer) {
            buffer.fold_turn(start, answer, self.stats.row(ms));
        }
    }

    /// Something is about to be shown in both views: a thought waiting to be
    /// drawn goes in first, and the next lookup starts a row of its own.
    fn break_run(&mut self, buffer: &mut ScrollBuffer) {
        self.draw_thought(buffer);
        self.open_group = None;
    }

    /// Draw the finished thought waiting, if any: `∴ Thought for 12s`, in the
    /// folded view; the full one has its text.
    fn draw_thought(&mut self, buffer: &mut ScrollBuffer) {
        if let Some(thought) = self.thought.take() {
            buffer.push_in(thought_line(&thought), Fold::Heading, Some(thought.item));
        }
    }

    /// A line for both views, after whatever waits to go in first.
    fn show(&mut self, buffer: &mut ScrollBuffer, line: Line<'static>) {
        self.break_run(buffer);
        tui_output::emit_line(buffer, line);
    }

    /// Count a lookup that started on the open row, opening one if none is.
    /// A thought since the row's last call goes into it.
    fn join_group(&mut self, lookups: &[Lookup], buffer: &mut ScrollBuffer) -> usize {
        let index = match self.open_group {
            Some(index) => {
                // The thought opens with the row.
                if let Some(thought) = self.thought.take() {
                    buffer.retag(thought.item, self.groups[index].row);
                }
                index
            }
            None => {
                self.draw_thought(buffer);
                let row = buffer.push_row(Line::default());
                self.groups.push(Group {
                    row,
                    ..Group::default()
                });
                let index = self.groups.len() - 1;
                self.open_group = Some(index);
                index
            }
        };
        let group = &mut self.groups[index];
        for &lookup in lookups {
            group.count(lookup);
        }
        group.running += 1;
        buffer.set_row(group.row, group.line());
        index
    }

    /// A lookup on row `index` ended.
    fn leave_group(&mut self, index: usize, failed: bool, buffer: &mut ScrollBuffer) {
        if let Some(group) = self.groups.get_mut(index) {
            group.running = group.running.saturating_sub(1);
            if failed {
                group.failed += 1;
            }
            buffer.set_row(group.row, group.line());
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
                Item::AgentMessage { id, text, .. } if self.messages.contains(id) => {
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
        self.end_turn(buffer);
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
        if turn.status == TurnStatus::Completed {
            self.fold_turn(turn.duration_ms, buffer);
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
            Item::UserMessage { content, .. } => {
                buffer.push_lines(prompt_lines(&koda_client::protocol::turn::text_of(content)))
            }
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
        self.turn_start.get_or_insert(buffer.next_seq());
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
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(progress.message.clone(), CYAN),
                ]);
                match self.calls.get(&progress.item_id).map(Call::fold) {
                    Some(Fold::Full) => buffer.push_folded(line, Fold::Full),
                    _ => self.show(buffer, line),
                }
            }
            ServerNotification::TurnPlanUpdated(updated) => {
                self.working.set_plan(updated.plan.clone());
            }
            ServerNotification::TurnLoopGuard(guard) => {
                self.break_run(buffer);
                notice(
                    (
                        Tone::Warn,
                        protocol_text::loop_guard(guard.action, &guard.detail),
                    ),
                    buffer,
                );
            }
            ServerNotification::Warning(warning) => {
                self.break_run(buffer);
                notice((Tone::Warn, warning.message.clone()), buffer);
            }
            // A retry's notice says what happened and when Koda tries again.
            ServerNotification::Error(error) if error.will_retry => {
                self.break_run(buffer);
                notice((Tone::Warn, error.error.message.clone()), buffer);
            }
            ServerNotification::Error(error) => {
                self.error_shown = true;
                self.show(
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
        self.turn_start.get_or_insert(buffer.next_seq());
        match item {
            Item::AgentMessage { id, .. } if parent.is_none() => {
                self.messages.insert(id.clone());
                self.message_start = None;
                self.has_emitted_text = false;
                self.ruled = false;
                self.working.set(Step::Writing);
            }
            Item::Reasoning { id, .. } if parent.is_none() => {
                self.reasoning.insert(id.clone());
                self.think_buf.clear();
                self.thinking_since = started_ms;
                self.thinking_text = false;
                self.working.set(Step::Thinking);
                let thinking = buffer.new_item();
                self.thinking_item = Some(thinking);
                buffer.push_in(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{1f4ad} Thinking...", DIM),
                    ]),
                    Fold::Full,
                    Some(thinking),
                );
            }
            Item::ToolCall {
                id,
                tool,
                arguments,
                effect,
                command_actions,
                ..
            } => {
                // Another of the session's own calls is open: this one's
                // header waits for its result.
                let another_open =
                    parent.is_none() && self.calls.values().any(|call| call.parent.is_none());
                // The session's own lookups fold into a row, and its plan
                // updates into the plan panel; anything else ends the row.
                let mut group = None;
                let mut quiet = false;
                // The item its lines open and close with: a sub-agent's
                // call goes with the sub-agent, a lookup with its row, any
                // other call on its own.
                let mut line_item = parent
                    .and_then(|parent| self.agents.get(parent))
                    .map(|run| run.item);
                if parent.is_none() {
                    self.stats.calls += 1;
                    self.working
                        .call_started(id, tool, arguments, command_actions);
                    if let Some(lookups) = lookup(tool, *effect, command_actions) {
                        let index = self.join_group(&lookups, buffer);
                        line_item = Some(self.groups[index].row);
                        group = Some(index);
                        quiet = true;
                    } else if tool == "TodoWrite" {
                        quiet = true;
                    } else {
                        self.break_run(buffer);
                        line_item = Some(buffer.new_item());
                    }
                }
                self.calls.insert(
                    id.clone(),
                    Call {
                        arguments: arguments.clone(),
                        effect: *effect,
                        parent: parent.map(str::to_string),
                        stream_start: None,
                        header: None,
                        quiet,
                        group,
                        item: line_item,
                    },
                );
                if tool == "InvokeAgent"
                    && let Some(item) = line_item
                {
                    self.agents.insert(
                        id.clone(),
                        AgentRun {
                            started_ms,
                            calls: 0,
                            item,
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
                        buffer.push_in(line, Fold::Full, line_item);
                    }
                    None => {
                        let line =
                            crate::tool_header::build_header_line("", tool, arguments, *effect);
                        let fold = if quiet { Fold::Full } else { Fold::Always };
                        if another_open {
                            if let Some(call) = self.calls.get_mut(id) {
                                call.header = Some(line);
                            }
                        } else {
                            buffer.push_in(line, fold, line_item);
                        }
                    }
                }
            }
            item @ Item::ContextCompaction { .. } => {
                if let Some(line) = protocol_text::compaction_started(item) {
                    self.break_run(buffer);
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
            Item::AgentMessage {
                id, text, phase, ..
            } if self.messages.remove(id) => {
                if !text.trim().is_empty() {
                    self.replies.push(text.clone());
                }
                if !self.text_buf.is_empty() {
                    let rest = std::mem::take(&mut self.text_buf);
                    self.break_run(buffer);
                    self.rule(&rest, buffer);
                    tui_output::emit_line(buffer, self.md.render_line(&rest));
                }
                // The answer is the last reply that isn't commentary; with
                // no phase, as in a session stored before it was sent, the
                // last reply.
                if *phase != Some(MessagePhase::Commentary)
                    && let Some(start) = self.message_start.take()
                {
                    self.answer = Some((start, buffer.next_seq()));
                }
                self.has_emitted_text = false;
                self.md = crate::md_render::MarkdownRenderer::new();
                self.working.set(Step::Working);
            }
            Item::Reasoning { id, .. } if self.reasoning.remove(id) => {
                if !self.think_buf.is_empty() {
                    let rest = std::mem::take(&mut self.think_buf);
                    self.thinking_text |= !rest.trim().is_empty();
                    buffer.push_in(thinking_line(rest), Fold::Full, self.thinking_item);
                }
                // Its stub waits for what comes next. Thoughts one after
                // another add up to one, which opens to all their text.
                let ms = self
                    .thinking_since
                    .take()
                    .zip(completed_ms)
                    .map(|(started, done)| done.saturating_sub(started).max(0) as u64);
                let item = self
                    .thinking_item
                    .take()
                    .unwrap_or_else(|| buffer.new_item());
                if let Some(earlier) = &self.thought {
                    buffer.retag(item, earlier.item);
                }
                let thought = self.thought.get_or_insert(Thought {
                    ms: Some(0),
                    text: false,
                    item,
                });
                thought.ms = thought.ms.zip(ms).map(|(a, b)| a + b);
                thought.text |= self.thinking_text;
                self.working.set(Step::Working);
            }
            item @ Item::ContextCompaction { .. } => {
                if let Some(line) = protocol_text::compaction_done(item) {
                    self.break_run(buffer);
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
                let mut call = self.calls.remove(id);
                let changes_shown = self.changes_shown.remove(id);
                let failed = *status == ToolCallStatus::Failed;
                if parent.is_none() {
                    self.working.call_ended(id);
                    if *status == ToolCallStatus::Completed {
                        self.stats.count_changes(changes);
                    }
                }
                if let Some(index) = call.as_ref().and_then(|call| call.group) {
                    self.leave_group(index, failed, buffer);
                }
                // A header held back while another call ran goes first.
                if let Some(call) = call.as_mut()
                    && let Some(header) = call.header.take()
                {
                    buffer.push_in(header, call.fold(), call.item);
                }
                // What happened to it, as both peers show a declined call:
                // what the model was told says which it was.
                if *status == ToolCallStatus::Declined {
                    let what = detail.as_deref().unwrap_or(tool);
                    let outcome = protocol_text::declined(output.as_deref().unwrap_or(""));
                    // The diff, unless the approval drew it already, as when
                    // the turn is replayed.
                    let diff = (!changes_shown).then_some(changes.as_slice());
                    self.break_run(buffer);
                    declined_lines(outcome, what, diff, buffer);
                    return;
                }
                // A sub-agent's results aren't shown: its answer is.
                if parent.is_some() {
                    return;
                }
                if !(failed || *status == ToolCallStatus::Completed) {
                    return;
                }
                if let Some(run) = self.agents.remove(id) {
                    let ms = run
                        .started_ms
                        .zip(completed_ms)
                        .map(|(started, done)| done.saturating_sub(started).max(0) as u64);
                    self.break_run(buffer);
                    buffer.push_in(
                        agent_summary(run.calls, ms, failed),
                        Fold::Always,
                        Some(run.item),
                    );
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
        // A lookup or plan update: everything in the full view, nothing
        // in the folded one, which has its row or the plan panel.
        let quiet = call.as_ref().is_some_and(|c| c.quiet);
        let item = call.as_ref().and_then(|c| c.item);
        if let Some(start) = call.as_ref().and_then(|c| c.stream_start) {
            if !quiet {
                let tail = if failed { FOLD_TAIL } else { 0 };
                buffer.fold_since(start, FOLD_HEAD, tail, fold_marker, item);
                self.break_run(buffer);
            }
            let exit_line = output.lines().next().unwrap_or("");
            buffer.push_in(
                Line::from(vec![
                    Span::styled("  \u{2514} ", DIM),
                    Span::styled(exit_line.to_string(), DIM),
                ]),
                if quiet { Fold::Full } else { Fold::Always },
                item,
            );
            return;
        }
        if changes_shown && matches!(tool, "Write" | "Edit" | "MemoryWrite") {
            // The diff is above the dialog: just how long the result is.
            let line_count = output.lines().count();
            self.show(
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
        let view = tool_output_view(
            tool,
            effect,
            output,
            failed,
            file_ext.as_deref(),
            grep_pattern.as_deref(),
        );
        if quiet {
            buffer.push_folded_lines_in(
                view.into_iter()
                    .filter(|(_, fold)| *fold != Fold::Folded)
                    .map(|(line, _)| (line, Fold::Full)),
                item,
            );
        } else {
            if view.iter().any(|(_, fold)| *fold != Fold::Full) {
                self.break_run(buffer);
            }
            buffer.push_folded_lines_in(view, item);
        }
    }

    /// A chunk of a call's live output, one line at a time.
    fn output(
        &mut self,
        item_id: &str,
        delta: &str,
        stream: OutputStream,
        buffer: &mut ScrollBuffer,
    ) {
        let Some(call) = self.calls.get(item_id) else {
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
        // A sub-agent's output and a lookup's are in the full view only.
        let own = call.parent.is_none();
        let fold = if own { call.fold() } else { Fold::Full };
        let item = call.item;
        if fold == Fold::Always {
            self.break_run(buffer);
        }
        if own && let Some(call) = self.calls.get_mut(item_id) {
            // Its output says whose it is: a held-back header goes first.
            if let Some(header) = call.header.take() {
                buffer.push_in(header, fold, item);
            }
            call.stream_start.get_or_insert_with(|| buffer.next_seq());
        }
        for text in delta.strip_suffix('\n').unwrap_or(delta).split('\n') {
            let line = Line::from(vec![
                Span::styled(prefix, TOOL_PREFIX),
                Span::styled(text.to_string(), style),
            ]);
            buffer.push_in(line, fold, item);
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
            self.break_run(buffer);
            self.rule(&line, buffer);
            self.has_emitted_text = true;
            tui_output::emit_line(buffer, self.md.render_line(&line));
        }
    }

    /// Reasoning text: complete lines are drawn as they arrive, in the full
    /// view; the folded one has the status line, then the stub.
    fn thinking_text(&mut self, text: &str, buffer: &mut ScrollBuffer) {
        self.think_buf.push_str(text);
        while let Some(pos) = self.think_buf.find('\n') {
            let line = self.think_buf[..pos].to_string();
            self.think_buf.drain(..=pos);
            self.thinking_text |= !line.trim().is_empty();
            buffer.push_in(thinking_line(line), Fold::Full, self.thinking_item);
        }
    }

    /// The `───` line before a reply's first words.
    fn rule(&mut self, text: &str, buffer: &mut ScrollBuffer) {
        self.message_start.get_or_insert(buffer.next_seq());
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

/// What a finished thought leaves in the folded view, as Claude Code's:
/// `∴ Thought for 12s (ctrl+o to show thinking)`. Without the items' times
/// it says only that it thought; with no text there is nothing to show.
fn thought_line(thought: &Thought) -> Line<'static> {
    let mut words = match thought.ms {
        Some(ms) => format!("\u{2234} Thought for {}", worked_for(ms)),
        None => "\u{2234} Thought".to_string(),
    };
    if thought.text {
        words.push_str(" (ctrl+o to show thinking)");
    }
    Line::from(vec![Span::raw("  "), Span::styled(words, DIM)])
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
            command_actions: Vec::new(),
            progress: Vec::new(),
            exit_code: None,
            duration_ms: None,
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
                command_actions: Vec::new(),
                progress,
                exit_code: None,
                duration_ms: None,
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
                exit_code: None,
                duration_ms: None,
                status,
                output: Some(output.into()),
                detail: Some("Write a.txt".into()),
                changes: vec![change.clone()],
                command_actions: Vec::new(),
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

    /// Calls run together all start before any ends: each one's header is
    /// drawn with its result, so a result lands under its own header.
    #[test]
    fn calls_run_together_each_show_under_their_own_header() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let first = call("b1", "Bash", json!({"command": "echo first"}));
        let second = call("b2", "Bash", json!({"command": "echo second"}));
        renderer.render(&started(None, first.clone()), &mut buffer);
        renderer.render(&started(None, second.clone()), &mut buffer);
        renderer.render(
            &completed(None, finished(first, ToolCallStatus::Completed, "out-one")),
            &mut buffer,
        );
        renderer.render(
            &completed(None, finished(second, ToolCallStatus::Completed, "out-two")),
            &mut buffer,
        );
        let lines = text(&buffer);
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no {needle}: {lines:?}"))
        };
        assert!(at("echo first") < at("out-one"), "{lines:?}");
        assert!(at("out-one") < at("echo second"), "{lines:?}");
        assert!(at("echo second") < at("out-two"), "{lines:?}");
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
            exit_code: None,
            duration_ms: None,
            status: ToolCallStatus::Declined,
            output: Some("[plan mode] Not run: this session is read-only".into()),
            detail: Some("Write a.txt".into()),
            changes: vec![FileUpdateChange {
                path: "a.txt".into(),
                kind: PatchChangeKind::Add,
                diff: "hello\n".into(),
            }],
            command_actions: Vec::new(),
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

    // ── folding ──────────────────────────────────────────────────────────────

    /// `item` with the effect the host gives a call that only looks.
    fn read_only(item: Item) -> Item {
        match item {
            Item::ToolCall {
                id,
                tool,
                arguments,
                status,
                output,
                detail,
                changes,
                progress,
                exit_code,
                duration_ms,
                kind,
                ..
            } => Item::ToolCall {
                id,
                tool,
                arguments,
                effect: Some(ToolEffect::ReadOnly),
                kind,
                status,
                output,
                detail,
                changes,
                command_actions: Vec::new(),
                progress,
                exit_code,
                duration_ms,
            },
            other => other,
        }
    }

    fn thinking(id: &str) -> Item {
        Item::Reasoning {
            id: id.into(),
            text: String::new(),
        }
    }

    fn reasoning(id: &str, delta: &str) -> ServerNotification {
        ServerNotification::ReasoningTextDelta(TextDeltaNotification {
            session_id: "s".into(),
            turn_id: "t".into(),
            item_id: id.into(),
            delta: delta.into(),
        })
    }

    /// Run a call that only looks from start to end.
    fn look(
        renderer: &mut TuiRenderer,
        buffer: &mut ScrollBuffer,
        id: &str,
        tool: &str,
        arguments: serde_json::Value,
        status: ToolCallStatus,
        out: &str,
    ) {
        let item = read_only(call(id, tool, arguments));
        renderer.render(&started(None, item.clone()), buffer);
        renderer.render(
            &completed(None, read_only(finished(item, status, out))),
            buffer,
        );
    }

    /// Thinking leaves one line in the folded view, as Claude Code's does;
    /// its text is in the full view.
    #[test]
    fn a_thought_folds_to_a_stub() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.render(&started(None, thinking("r1")), &mut buffer);
        assert_eq!(renderer.working.label(), "Thinking");
        renderer.render(&reasoning("r1", "Look at the plan.\nThen"), &mut buffer);
        renderer.render(&completed(None, thinking("r1")), &mut buffer);
        // Nothing yet: the stub waits for what comes next.
        assert!(text(&buffer).is_empty(), "{:?}", text(&buffer));
        renderer.end_turn(&mut buffer);
        assert_eq!(
            text(&buffer),
            vec!["  \u{2234} Thought for 2s (ctrl+o to show thinking)"]
        );
        buffer.set_full(true);
        let full = text(&buffer);
        assert!(
            full.iter().any(|l| l.contains("Look at the plan.")),
            "{full:?}"
        );
        assert!(full.iter().any(|l| l.contains("Then")), "{full:?}");
        assert!(!full.iter().any(|l| l.contains("Thought for")), "{full:?}");
    }

    /// Lookups one after another are one row in Claude Code's words, with no
    /// output; a thought between them goes into the row, and a progress note
    /// ends it. Each call and its output are in the full view.
    #[test]
    fn lookups_fold_into_one_row() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let done = ToolCallStatus::Completed;
        renderer.render(&started(None, thinking("r1")), &mut buffer);
        renderer.render(&completed(None, thinking("r1")), &mut buffer);
        let read = json!({"file_path": "Cargo.toml"});
        look(
            &mut renderer,
            &mut buffer,
            "c1",
            "Read",
            read,
            done,
            "     1\t[package]",
        );
        renderer.render(&started(None, thinking("r2")), &mut buffer);
        renderer.render(&completed(None, thinking("r2")), &mut buffer);
        let grep = json!({"pattern": "GUI"});
        look(
            &mut renderer,
            &mut buffer,
            "c2",
            "Grep",
            grep,
            done,
            "a.rs:1:GUI",
        );
        for id in ["c3", "c4"] {
            let bash = json!({"command": "rg GUI docs"});
            look(
                &mut renderer,
                &mut buffer,
                id,
                "Bash",
                bash,
                done,
                "docs/a.md:GUI",
            );
        }
        let note = Item::AgentMessage {
            id: "m1".into(),
            text: String::new(),
            phase: None,
        };
        renderer.render(&started(None, note.clone()), &mut buffer);
        renderer.render(&reply("m1", "Checking the docs next.\n"), &mut buffer);
        renderer.render(&completed(None, note), &mut buffer);
        let list = json!({"path": "docs"});
        look(&mut renderer, &mut buffer, "c5", "List", list, done, "a.md");
        renderer.end_turn(&mut buffer);

        let folded = text(&buffer);
        assert_eq!(
            folded,
            vec![
                "  \u{2234} Thought for 2s",
                "\u{25cf} Read 1 file, searched 1 pattern, ran 2 commands (ctrl+o to expand)",
                "  \u{2500}\u{2500}\u{2500}",
                "  Checking the docs next.",
                "\u{25cf} Listed 1 directory (ctrl+o to expand)",
            ]
        );
        buffer.set_full(true);
        let full = text(&buffer);
        for needle in ["Cargo.toml", "[package]", "GUI", "docs/a.md:GUI", "a.md"] {
            assert!(
                full.iter().any(|l| l.contains(needle)),
                "{needle}: {full:?}"
            );
        }
        assert!(
            !full.iter().any(|l| l.contains("ran 2 commands")),
            "{full:?}"
        );
    }

    /// In focus view a completed turn is its prompt, one row and its
    /// answer, as Claude Code's `/focus`; the row opens to the rest. The
    /// default and full views are as they were.
    #[test]
    fn a_finished_turn_folds_to_one_row_in_focus_view() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        buffer.push_lines(prompt_lines("fix it"));
        renderer.start_turn();
        let message = |id: &str, phase| Item::AgentMessage {
            id: id.into(),
            text: String::new(),
            phase,
        };
        let note = message("m1", Some(MessagePhase::Commentary));
        renderer.render(&started(None, message("m1", None)), &mut buffer);
        renderer.render(&reply("m1", "Reading it first.\n"), &mut buffer);
        renderer.render(&completed(None, note), &mut buffer);
        let read = json!({"file_path": "a.rs"});
        let done = ToolCallStatus::Completed;
        look(
            &mut renderer,
            &mut buffer,
            "c1",
            "Read",
            read,
            done,
            "     1\tb",
        );
        let edit = call("c2", "Edit", json!({"file_path": "a.rs"}));
        renderer.render(&started(None, edit.clone()), &mut buffer);
        let mut edited = finished(edit, done, "ok");
        if let Item::ToolCall { changes, .. } = &mut edited {
            changes.push(FileUpdateChange {
                path: "a.rs".into(),
                kind: PatchChangeKind::Update { move_path: None },
                diff: "--- a.rs\n+++ a.rs\n@@ -1 +1,2 @@\n-b\n+a\n+c".into(),
            });
        }
        renderer.render(&completed(None, edited), &mut buffer);
        let answer = message("m2", Some(MessagePhase::FinalAnswer));
        renderer.render(&started(None, message("m2", None)), &mut buffer);
        renderer.render(&reply("m2", "It works.\n"), &mut buffer);
        renderer.render(&completed(None, answer), &mut buffer);
        renderer.end_turn(&mut buffer);
        buffer.push(worked_for_line(125_000));
        renderer.fold_turn(Some(125_000), &mut buffer);

        let default = text(&buffer);
        assert!(
            default.iter().any(|l| l.contains("Reading it first.")),
            "{default:?}"
        );
        assert!(
            !default.iter().any(|l| l.contains("\u{25b8}")),
            "{default:?}"
        );

        buffer.set_turns_folded(true);
        assert_eq!(
            text(&buffer),
            vec![
                "",
                "  \u{276f} fix it",
                "  \u{25b8} Worked for 2m 5s \u{00b7} 2 tool calls \u{00b7} 1 file +2 \u{2212}1",
                "  \u{2500}\u{2500}\u{2500}",
                "  It works.",
            ]
        );
        // The row opens to everything else, the end line included.
        let row = buffer.item_at(2, 80, 40).expect("the row opens");
        buffer.toggle(row, 80, 40);
        let opened = text(&buffer);
        for needle in [
            "Reading it first.",
            "Read 1 file",
            "Worked for 2m 5s \u{2500}",
        ] {
            assert!(
                opened.iter().any(|l| l.contains(needle)),
                "{needle}: {opened:?}"
            );
        }
        assert!(
            opened.iter().any(|l| l.contains("\u{25be} Worked")),
            "{opened:?}"
        );
        buffer.toggle(row, 80, 40);
        assert!(
            text(&buffer)[2].contains("\u{25b8} Worked"),
            "{:?}",
            text(&buffer)
        );

        buffer.set_full(true);
        let full = text(&buffer);
        assert!(!full.iter().any(|l| l.contains("\u{25b8}")), "{full:?}");
        assert!(
            full.iter().any(|l| l.contains("Reading it first.")),
            "{full:?}"
        );
    }

    /// A turn with nothing before its answer isn't folded.
    #[test]
    fn a_turn_that_only_answers_has_no_row() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        buffer.push_lines(prompt_lines("hi"));
        renderer.start_turn();
        let answer = Item::AgentMessage {
            id: "m1".into(),
            text: String::new(),
            phase: Some(MessagePhase::FinalAnswer),
        };
        renderer.render(&started(None, answer.clone()), &mut buffer);
        renderer.render(&reply("m1", "Hello.\n"), &mut buffer);
        renderer.render(&completed(None, answer), &mut buffer);
        renderer.end_turn(&mut buffer);
        buffer.push(worked_for_line(1_000));
        renderer.fold_turn(Some(1_000), &mut buffer);
        buffer.set_turns_folded(true);
        let shown = text(&buffer);
        assert!(!shown.iter().any(|l| l.contains("\u{25b8}")), "{shown:?}");
        assert!(
            shown.iter().any(|l| l.contains("Worked for 1s")),
            "{shown:?}"
        );
    }

    /// A command counts as what the host says it does: a read and a search
    /// here, named on the status line; one it can't read is a command.
    #[test]
    fn a_command_counts_as_what_it_does() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let with_actions = |id: &str, command: &str, actions: Vec<CommandAction>| match read_only(
            call(id, "Bash", json!({"command": command})),
        ) {
            Item::ToolCall {
                id,
                tool,
                arguments,
                effect,
                kind,
                status,
                output,
                detail,
                changes,
                progress,
                exit_code,
                duration_ms,
                ..
            } => Item::ToolCall {
                id,
                tool,
                arguments,
                effect,
                kind,
                status,
                output,
                detail,
                changes,
                command_actions: actions,
                progress,
                exit_code,
                duration_ms,
            },
            other => other,
        };
        let both = with_actions(
            "c1",
            "cat Cargo.toml && rg GUI docs",
            vec![
                CommandAction::Read {
                    command: "cat Cargo.toml".into(),
                    name: "Cargo.toml".into(),
                    path: "Cargo.toml".into(),
                },
                CommandAction::Search {
                    command: "rg GUI docs".into(),
                    query: Some("GUI".into()),
                    path: Some("docs".into()),
                },
            ],
        );
        renderer.render(&started(None, both.clone()), &mut buffer);
        assert_eq!(renderer.working.label(), "Reading Cargo.toml");
        renderer.render(
            &completed(None, finished(both, ToolCallStatus::Completed, "x")),
            &mut buffer,
        );
        let unknown = with_actions(
            "c2",
            "jq . a.json",
            vec![CommandAction::Unknown {
                command: "jq . a.json".into(),
            }],
        );
        renderer.render(&started(None, unknown.clone()), &mut buffer);
        renderer.render(
            &completed(None, finished(unknown, ToolCallStatus::Completed, "{}")),
            &mut buffer,
        );
        assert_eq!(
            text(&buffer),
            vec!["\u{25cf} Read 1 file, searched 1 pattern, ran 1 command (ctrl+o to expand)"]
        );
    }

    /// While a lookup runs its row says so; a failed one is counted on it.
    #[test]
    fn a_lookup_row_is_live_and_counts_a_failure() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let item = read_only(call("c1", "Read", json!({"file_path": "a.rs"})));
        renderer.render(&started(None, item.clone()), &mut buffer);
        assert_eq!(
            text(&buffer),
            vec!["\u{25cf} Reading 1 file\u{2026} (ctrl+o to expand)"]
        );
        assert_eq!(renderer.working.label(), "Reading a.rs");
        renderer.render(
            &completed(
                None,
                read_only(finished(item, ToolCallStatus::Failed, "No such file")),
            ),
            &mut buffer,
        );
        assert_eq!(
            text(&buffer),
            vec!["\u{25cf} Read 1 file \u{00b7} 1 failed (ctrl+o to expand)"]
        );
        assert_eq!(renderer.working.label(), "Working");

        // A turn stopped while a lookup ran: its row stops saying so.
        let item = read_only(call("c2", "Grep", json!({"pattern": "x"})));
        renderer.render(&started(None, item), &mut buffer);
        renderer.end_turn(&mut buffer);
        assert_eq!(
            text(&buffer).last().unwrap(),
            "\u{25cf} Read 1 file, searched 1 pattern \u{00b7} 1 failed (ctrl+o to expand)"
        );
    }

    /// A command that changes something keeps its own row and output, and
    /// ends the lookup row; streamed lookup output stays out of the folded
    /// view.
    #[test]
    fn a_command_that_changes_something_keeps_its_row() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let ls = read_only(call("c1", "Bash", json!({"command": "ls"})));
        renderer.render(&started(None, ls.clone()), &mut buffer);
        renderer.render(&output("c1", "a.rs\nb.rs\n"), &mut buffer);
        renderer.render(
            &completed(
                None,
                read_only(finished(ls, ToolCallStatus::Completed, "Exit code: 0")),
            ),
            &mut buffer,
        );
        let build = call("c2", "Bash", json!({"command": "cargo build"}));
        renderer.render(&started(None, build.clone()), &mut buffer);
        renderer.render(
            &completed(
                None,
                finished(build, ToolCallStatus::Completed, "Finished dev"),
            ),
            &mut buffer,
        );
        let folded = text(&buffer);
        assert_eq!(folded.len(), 3, "{folded:?}");
        assert!(folded[0].contains("Ran 1 command"), "{folded:?}");
        assert!(folded[1].contains("cargo build"), "{folded:?}");
        assert!(folded[2].contains("Finished dev"), "{folded:?}");
        buffer.set_full(true);
        let full = text(&buffer);
        assert!(full.iter().any(|l| l.contains("b.rs")), "{full:?}");
        assert!(full.iter().any(|l| l.contains("Exit code: 0")), "{full:?}");
    }

    /// One folded item opens on its own in the folded view: a lookup row to
    /// its calls and the thought between them, a thought to its text, a long
    /// output to all its lines.
    #[test]
    fn one_folded_item_opens_on_its_own() {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let done = ToolCallStatus::Completed;
        renderer.render(&started(None, thinking("r1")), &mut buffer);
        renderer.render(&reasoning("r1", "First look.\n"), &mut buffer);
        renderer.render(&completed(None, thinking("r1")), &mut buffer);
        let read = json!({"file_path": "a.rs"});
        look(
            &mut renderer,
            &mut buffer,
            "c1",
            "Read",
            read,
            done,
            "     1\tx",
        );
        renderer.render(&started(None, thinking("r2")), &mut buffer);
        renderer.render(&reasoning("r2", "Now grep.\n"), &mut buffer);
        renderer.render(&completed(None, thinking("r2")), &mut buffer);
        let grep = json!({"pattern": "GUI"});
        look(
            &mut renderer,
            &mut buffer,
            "c2",
            "Grep",
            grep,
            done,
            "a.rs:1:GUI",
        );
        let out: String = (1..=20).map(|i| format!("out {i}\n")).collect();
        let build = call("c3", "Bash", json!({"command": "cargo build"}));
        renderer.render(&started(None, build.clone()), &mut buffer);
        renderer.render(
            &completed(None, finished(build, ToolCallStatus::Completed, &out)),
            &mut buffer,
        );
        renderer.end_turn(&mut buffer);
        let folded = text(&buffer);
        assert!(folded[0].contains("Thought for 2s"), "{folded:?}");
        assert!(
            folded[1].contains("Read 1 file, searched 1 pattern"),
            "{folded:?}"
        );
        let rows = folded.len();

        // The row: its calls and the thought between them, under it.
        let at = |row: usize, buffer: &ScrollBuffer| buffer.item_at(row, 80, 500).expect("an item");
        let row = at(1, &buffer);
        buffer.toggle(row, 80, 500);
        let open = text(&buffer);
        let under = &open[2..open.len() - (rows - 2)];
        assert!(
            under
                .iter()
                .any(|l| l.contains("Read") && l.contains("a.rs")),
            "{open:?}"
        );
        assert!(under.iter().any(|l| l.contains("Now grep.")), "{open:?}");
        assert!(under.iter().any(|l| l.contains("a.rs:1:GUI")), "{open:?}");
        assert!(!open.iter().any(|l| l.contains("First look.")), "{open:?}");
        buffer.toggle(row, 80, 500);

        // The first thought opens to its own text.
        let stub = at(0, &buffer);
        buffer.toggle(stub, 80, 500);
        assert!(text(&buffer).iter().any(|l| l.contains("First look.")));
        buffer.toggle(stub, 80, 500);

        // The command's marker gives way to all 20 lines.
        let marker = folded.iter().position(|l| l.contains("+15 lines")).unwrap();
        let output = at(marker, &buffer);
        buffer.toggle(output, 80, 500);
        let open = text(&buffer);
        assert!(open.iter().any(|l| l.ends_with("out 12")), "{open:?}");
        assert!(!open.iter().any(|l| l.contains("+15 lines")), "{open:?}");
        assert_eq!(open.len(), rows + 15 - 1);
    }

    /// A plan update is the working area's plan, not a row.
    #[test]
    fn a_plan_update_goes_to_the_plan_panel() {
        use koda_client::protocol::turn::{PlanStep, PlanStepStatus, TurnPlanUpdatedNotification};
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        let todo = call("c1", "TodoWrite", json!({"todos": []}));
        renderer.render(&started(None, todo.clone()), &mut buffer);
        assert_eq!(renderer.working.label(), "Updating the plan");
        renderer.render(
            &ServerNotification::TurnPlanUpdated(TurnPlanUpdatedNotification {
                session_id: "s".into(),
                turn_id: "t".into(),
                plan: vec![PlanStep {
                    step: "Write the code".into(),
                    status: PlanStepStatus::InProgress,
                }],
            }),
            &mut buffer,
        );
        renderer.render(
            &completed(
                None,
                finished(todo, ToolCallStatus::Completed, "1 task in progress"),
            ),
            &mut buffer,
        );
        assert!(text(&buffer).is_empty(), "{:?}", text(&buffer));
        assert!(renderer.working.has_open_plan());
        buffer.set_full(true);
        assert!(text(&buffer).iter().any(|l| l.contains("TodoWrite")));
    }

    /// A stored turn folds as it did live.
    #[test]
    fn a_stored_turn_folds_as_it_did_live() {
        let reads: Vec<TurnItem> = ["a.rs", "b.rs"]
            .iter()
            .enumerate()
            .map(|(i, file)| {
                let item = read_only(finished(
                    call(&format!("c{i}"), "Read", json!({"file_path": file})),
                    ToolCallStatus::Completed,
                    "     1\tx",
                ));
                stored(None, item, Some((1_000, 2_000)))
            })
            .collect();
        let mut items = vec![stored(
            None,
            Item::Reasoning {
                id: "r1".into(),
                text: "Read both.".into(),
            },
            Some((0, 3_000)),
        )];
        items.extend(reads);
        items.push(stored(
            None,
            Item::AgentMessage {
                id: "m1".into(),
                text: "Both fine.".into(),
                phase: None,
            },
            None,
        ));
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.replay_turn(
            &turn(items, TurnStatus::Completed, Some(4_000)),
            &mut buffer,
        );
        let folded = text(&buffer);
        assert_eq!(
            folded[0],
            "  \u{2234} Thought for 3s (ctrl+o to show thinking)"
        );
        assert_eq!(folded[1], "\u{25cf} Read 2 files (ctrl+o to expand)");
        assert!(
            folded.iter().any(|l| l.contains("Both fine.")),
            "{folded:?}"
        );
        assert!(!folded.iter().any(|l| l.contains("b.rs")), "{folded:?}");
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
            phase: None,
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
            started_at: None,
            completed_at: None,
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
                    content: vec![koda_client::protocol::turn::UserInput::text("look around")],
                    client_user_message_ids: Vec::new(),
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
                    phase: None,
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
        // In focus view it folds as a live turn does; a sub-agent's calls
        // aren't the turn's own.
        buffer.set_full(false);
        buffer.set_turns_folded(true);
        assert_eq!(
            text(&buffer),
            vec![
                "",
                "  \u{276f} look around",
                "  \u{25b8} Worked for 13s \u{00b7} 1 tool call",
                "  \u{2500}\u{2500}\u{2500}",
                "  All done.",
            ]
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
            exit_code: None,
            duration_ms: None,
            status: ToolCallStatus::Declined,
            output: Some("User rejected this action.".into()),
            detail: Some("Edit a.rs".into()),
            changes: vec![FileUpdateChange {
                path: "a.rs".into(),
                kind: PatchChangeKind::Update { move_path: None },
                diff: "@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
            }],
            command_actions: Vec::new(),
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
