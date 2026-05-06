//! Engine output sink trait.
//!
//! The `EngineSink` trait abstracts how the engine delivers events to clients.
//! Implementations decide how to render or transport events:
//! - `CliSink` (in koda-cli): renders to terminal
//! - Future `AcpSink`: serializes over WebSocket
//! - `TestSink`: collects events for assertions

use super::event::EngineEvent;

/// Trait for consuming engine events.
///
/// Implementors decide how to render or transport events:
/// - `CliSink`: renders to terminal via `display::` and `markdown::`
/// - Future `AcpSink`: serializes over WebSocket
/// - `TestSink`: collects events for assertions
pub trait EngineSink: Send + Sync {
    /// Emit an engine event to the client.
    fn emit(&self, event: EngineEvent);
}

/// A no-op sink that discards all events.
///
/// Used by background sub-agents that don't have a live channel to
/// the user. **#1022 B9**: superseded for bg-agent use by
/// [`BufferingSink`], which captures a narrative trace so the user
/// can see what the bg agent did at result-injection time. `NullSink`
/// is still useful for tests and for any future fully-detached
/// execution path.
pub struct NullSink;

impl EngineSink for NullSink {
    fn emit(&self, _event: EngineEvent) {}
}

/// A sink that buffers a *narrative trace* of bg-agent activity.
///
/// **#1022 B9**: pre-fix, bg agents ran with [`NullSink`] so every
/// event inside them — tool calls, info lines, approval requests,
/// errors — was silently dropped. The user only saw two lines: the
/// spawn message and the completion message. The model only saw the
/// final output. *What the bg agent actually did* was opaque.
///
/// `BufferingSink` records short, human-readable lines for events
/// that matter for traceability:
/// - `ToolCallStart` → `"  🔧 ToolName"`
/// - `Info` → forwarded as-is (sub-agent emits info for things like
///   nested spawn / cache hit)
/// - `ApprovalRequest` / `AskUserRequest` → short auto-reject note
///   (they auto-reject on closed channel — see B10)
/// - Streaming text (`TextDelta`/`TextDone`) is *not* recorded — the
///   final output already crosses the result oneshot, so capturing
///   text here would duplicate it.
///
/// Drained at result-injection time and emitted as a multi-line
/// `Info` event so the user sees `✅ bg agent X completed\n  🔧 Read\n
/// 🔧 Bash\n  …` instead of just `✅ bg agent X completed`.
///
/// Cap is intentionally generous (256 lines): a runaway bg agent
/// could otherwise grow this unboundedly. After the cap we record a
/// single `… (trace truncated at N lines)` marker and stop.
pub struct BufferingSink {
    lines: std::sync::Mutex<Vec<String>>,
    cap: usize,
}

impl BufferingSink {
    /// Create a buffering sink with the default 256-line cap.
    pub fn new() -> Self {
        Self::with_cap(256)
    }

    /// Create a buffering sink with a custom cap (mainly for tests).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            lines: std::sync::Mutex::new(Vec::new()),
            cap,
        }
    }

    /// Drain and return all buffered lines. The sink is empty after
    /// this returns.
    pub fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }

    /// Append a line, honoring the cap. Idempotent on the truncation
    /// marker so a single overflow only produces one marker.
    fn push_capped(&self, line: String) {
        let mut guard = self.lines.lock().unwrap();
        if guard.len() < self.cap {
            guard.push(line);
        } else if guard.last().map(|l| !l.starts_with('…')).unwrap_or(true) {
            // Cap reached — emit one truncation marker and stop.
            guard.push(format!("… (trace truncated at {} lines)", self.cap));
        }
    }
}

impl Default for BufferingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineSink for BufferingSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::ToolCallStart { name, .. } => {
                self.push_capped(format!("  \u{1f527} {name}"));
            }
            EngineEvent::Info { message } => {
                // Sub-agent already prefixes its own info lines with
                // two spaces and an emoji — forward as-is so the
                // visual hierarchy survives.
                self.push_capped(message);
            }
            EngineEvent::ApprovalRequest { tool_name, .. } => {
                // B10: bg agents have no user channel — these always
                // auto-reject. Record so the model's apparent
                // "failure to do X" is debuggable.
                self.push_capped(format!(
                    "  \u{2398} approval auto-rejected for {tool_name} (no user channel)"
                ));
            }
            EngineEvent::AskUserRequest { question, .. } => {
                self.push_capped(format!(
                    "  \u{2398} ask-user auto-skipped: {}",
                    question.chars().take(80).collect::<String>()
                ));
            }
            // Everything else (streaming text, thinking, status, etc.)
            // is intentionally dropped — either redundant with the
            // result oneshot or noisy without context.
            _ => {}
        }
    }
}

// ── ForwardingChildSink (#1201 B) ──────────────────────────

/// A decorator around [`BufferingSink`] that *also* forwards select
/// events as [`crate::engine::event::EngineEvent::ChildAgentActivity`]
/// up to the parent's sink via the bg-task's status emitter.
///
/// **#1201 B**: pre-this-decorator the parent's TUI had zero live
/// signal from inside a running bg agent — only `ChildTaskUpdate`
/// heartbeats (`Running { iter: N }`), which tell you "still going"
/// but not "doing what". The narrative trace from `BufferingSink`
/// only surfaced at result-injection time, so a 30-second tool call
/// inside a bg agent looked identical to a 30-second hang.
///
/// `ForwardingChildSink` is the live tap. For each event interesting
/// enough to surface in the parent's feed, it builds a
/// [`crate::engine::event::ChildAgentActivityKind`] and pushes it onto
/// the registry's status-event queue via
/// [`crate::child_agent::ChildStatusEmitter::send_activity`]. The
/// inference loop's existing drain in `inference.rs` forwards the
/// resulting `ChildAgentActivity` event to whatever sink is active
/// (TUI / headless / ACP) without further plumbing.
///
/// **The narrative trace is preserved.** Every event that hits this
/// sink is also forwarded to the inner `BufferingSink`, so the
/// authoritative post-completion trace (drained at result-injection
/// time and persisted to the transcript) is unchanged. Live and
/// post-completion are deliberately two separate channels:
/// - Live (`ChildAgentActivity`) is for real-time UX; events are
///   ephemeral and may be coalesced or dropped by the renderer.
/// - Post-completion (the `BufferingSink::take_lines` dump) is the
///   load-bearing record — it's what the model sees in the result
///   message and what the transcript exporter persists.
///
/// ## Sink wrapping order
///
/// `PersistingSink` wraps `ForwardingChildSink` wraps `BufferingSink`.
/// Persistence sees every event first (so the transcript captures
/// `SubAgentEvent` rows in real time), then forwarding fans out to
/// the parent's queue, then buffering captures the line for the
/// post-completion drain.
pub struct ForwardingChildSink {
    inner: BufferingSink,
    emitter: crate::child_agent::ChildStatusEmitter,
}

impl ForwardingChildSink {
    /// Wrap a `BufferingSink` and forward live activity through the
    /// emitter. The emitter is cheap to clone (two `Arc`s and a
    /// `watch::Sender`); pass a clone and keep the original for the
    /// terminal-status sends in `run_bg_agent`.
    pub fn new(inner: BufferingSink, emitter: crate::child_agent::ChildStatusEmitter) -> Self {
        Self { inner, emitter }
    }

    /// Drain the inner buffering sink. Same semantics as
    /// [`BufferingSink::take_lines`] — the buffer is empty after
    /// this returns. Only the post-completion narrative is drained
    /// here; live `ChildAgentActivity` events have already been
    /// forwarded individually as they happened.
    pub fn take_lines(&self) -> Vec<String> {
        self.inner.take_lines()
    }
}

impl EngineSink for ForwardingChildSink {
    fn emit(&self, event: EngineEvent) {
        // Forward the *live* signal first while we still own the
        // event by reference. Dropping a delta on the floor here
        // (e.g. an unknown future variant) is silently fine — the
        // post-completion trace via the inner BufferingSink remains
        // authoritative.
        match &event {
            EngineEvent::ToolCallStart { name, args, .. } => {
                self.emitter.send_activity(
                    crate::engine::event::ChildAgentActivityKind::ToolStart {
                        tool_name: name.clone(),
                        summary: summarize_tool_call(name, args),
                    },
                );
            }
            EngineEvent::ToolCallResult { name, output, .. } => {
                // Best-effort success classification: tool dispatchers
                // prefix failed results with "Error:" or "❌". Cheap
                // string sniff — the live feed only uses this for an
                // icon hint, not for any control-flow decision.
                let success = !looks_like_tool_error(output);
                self.emitter
                    .send_activity(crate::engine::event::ChildAgentActivityKind::ToolEnd {
                        tool_name: name.clone(),
                        success,
                    });
            }
            EngineEvent::Info { message } => {
                self.emitter
                    .send_activity(crate::engine::event::ChildAgentActivityKind::Info {
                        message: message.clone(),
                    });
            }
            // Streaming text, thinking, status, approval, etc. are
            // intentionally not forwarded — too noisy for a feed,
            // duplicative with the result oneshot, or already covered
            // by `ChildTaskUpdate` heartbeats.
            _ => {}
        }
        // Forward to the inner buffering sink for the post-completion
        // narrative trace.
        self.inner.emit(event);
    }
}

/// Live-activity tap for **foreground** sub-agents.
///
/// **PR-A of #1232 §1**: foreground sub-agents share the parent's
/// sink (the parent is blocked awaiting them inline), so unlike
/// [`ForwardingChildSink`] there's no `BufferingSink` to wrap and
/// no post-completion narrative to drain. The parent already sees
/// every event the child emits because they go to the same sink.
///
/// What's missing without this wrapper is the
/// [`crate::engine::event::EngineEvent::ChildAgentActivity`] fan-out
/// that powers the `/agents` overlay's per-row "last activity"
/// snippet. The bg path gets that fan-out via
/// [`ForwardingChildSink`]'s `emitter.send_activity` calls; this
/// type is the fg analog — same emitter API, no buffering.
///
/// Borrows the parent sink to avoid an `Arc` allocation per
/// invocation. The borrow lives for the duration of the inline
/// sub-agent call, which is exactly the scope of the
/// [`crate::child_agent::FgRegistrationGuard`] that owns the
/// emitter — they form a matched pair on the call stack.
pub struct FgForwardingSink<'a> {
    inner: &'a dyn EngineSink,
    emitter: crate::child_agent::ChildStatusEmitter,
}

impl<'a> FgForwardingSink<'a> {
    /// Construct from the parent's sink + the emitter handed back by
    /// [`crate::child_agent::ChildAgentRegistry::register_fg_with_emitter`].
    pub fn new(inner: &'a dyn EngineSink, emitter: crate::child_agent::ChildStatusEmitter) -> Self {
        Self { inner, emitter }
    }
}

impl EngineSink for FgForwardingSink<'_> {
    fn emit(&self, event: EngineEvent) {
        // Same fan-out shape as `ForwardingChildSink::emit` — keep
        // the two arms in sync. Diverged behavior here would mean
        // bg and fg overlay rows show different content for the
        // same tool call, which is exactly the inconsistency PR-A
        // is trying to eliminate.
        match &event {
            EngineEvent::ToolCallStart { name, args, .. } => {
                self.emitter.send_activity(
                    crate::engine::event::ChildAgentActivityKind::ToolStart {
                        tool_name: name.clone(),
                        summary: summarize_tool_call(name, args),
                    },
                );
            }
            EngineEvent::ToolCallResult { name, output, .. } => {
                let success = !looks_like_tool_error(output);
                self.emitter
                    .send_activity(crate::engine::event::ChildAgentActivityKind::ToolEnd {
                        tool_name: name.clone(),
                        success,
                    });
            }
            EngineEvent::Info { message } => {
                self.emitter
                    .send_activity(crate::engine::event::ChildAgentActivityKind::Info {
                        message: message.clone(),
                    });
            }
            _ => {}
        }
        // Forward to the parent's sink so the master TUI's normal
        // event stream is preserved — fg sub-agents have always
        // emitted directly to the parent and downstream clients
        // (CLI, ACP, headless) depend on that. The wrapper only
        // *adds* the activity fan-out; it never swallows events.
        self.inner.emit(event);
    }
}

/// Build a one-line summary of a tool call for the live activity
/// feed. Output is rendered as-is by clients, so trim hard.
///
/// Per-tool special cases live here so every client sees the same
/// string without having to know each tool's argument schema. New
/// tools fall through to a generic `"<name> <truncated args>"`.
fn summarize_tool_call(name: &str, args: &serde_json::Value) -> String {
    /// Per-line cap. The activity feed is rendered inline under the
    /// bg-task spawn cell where horizontal real estate is tight.
    const MAX_LEN: usize = 80;

    let body = match name {
        "Read" | "Edit" | "Write" | "Delete" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        "Grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            Some(format!("{pattern} {path}"))
        }
        "InvokeAgent" => args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    };

    let body = body.unwrap_or_default();
    let combined = if body.is_empty() {
        name.to_string()
    } else {
        format!("{name} {body}")
    };

    if combined.chars().count() <= MAX_LEN {
        combined
    } else {
        // Char-aware truncation — byte slicing would explode on
        // multi-byte chars in tool args (paths, commit messages, etc.).
        let truncated: String = combined.chars().take(MAX_LEN.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    }
}

/// Best-effort "did this tool result indicate failure" check.
///
/// Tool dispatchers in `tools/` produce result strings, not Result
/// enums, so this is the only signal available at the sink. Used
/// purely for a render hint (success vs error icon) — callers must
/// not depend on this for correctness.
fn looks_like_tool_error(output: &str) -> bool {
    let head = output.trim_start();
    head.starts_with("Error:") || head.starts_with("\u{274c}")
}

// ── PersistingSink (#1108 P1b/P2a) ───────────────────────────────

/// A decorator that persists `Info` and `ChildTaskUpdate` events to the
/// `session_events` table before forwarding to an inner sink.
///
/// Pre-#1108 these events were sink-only and never reached the DB,
/// so the markdown transcript export had no record of:
/// - bg-agent narrative traces (what each task did during the wait window)
/// - microcompact / loop-detector / rate-limit messages
/// - bg-task status transitions (`Pending → Running { iter: N } → …`)
///
/// ## Wiring
///
/// - **Top-level** (P1b): wrap the user-facing sink (CliSink/AcpSink)
///   with `parent_tool_call_id = None`.
/// - **Sub-agent** (P2a): wrap the [`BufferingSink`] with
///   `parent_tool_call_id = Some(invoke_agent_call_id)` so the
///   transcript renderer can fold the trace under the parent's
///   `InvokeAgent` tool result.
///
/// ## Failure handling
///
/// Inserts run on a fire-and-forget tokio task and **never** propagate
/// errors back to the inference loop. A DB hiccup must not crash a
/// session in progress — the worst case is a missing event in the
/// transcript, not a lost turn.
pub struct PersistingSink<'a> {
    inner: &'a dyn EngineSink,
    db: std::sync::Arc<dyn crate::persistence::Persistence>,
    session_id: String,
    /// Set on sub-agent sinks so their events can be folded under the
    /// parent's `InvokeAgent` tool result. `None` for top-level.
    parent_tool_call_id: Option<String>,
}

impl<'a> PersistingSink<'a> {
    /// Wrap an inner sink. The decorator persists Info/ChildTaskUpdate
    /// events as a side effect; everything else passes through
    /// untouched.
    pub fn new(
        inner: &'a dyn EngineSink,
        db: std::sync::Arc<dyn crate::persistence::Persistence>,
        session_id: String,
        parent_tool_call_id: Option<String>,
    ) -> Self {
        Self {
            inner,
            db,
            session_id,
            parent_tool_call_id,
        }
    }

    /// Spawn a fire-and-forget DB insert. Any error is logged via
    /// `tracing::warn!` and otherwise swallowed (see struct doc).
    fn persist(&self, kind: &'static str, payload: String) {
        let db = self.db.clone();
        let session_id = self.session_id.clone();
        let parent = self.parent_tool_call_id.clone();
        tokio::spawn(async move {
            if let Err(e) = db
                .insert_session_event(&session_id, kind, &payload, parent.as_deref())
                .await
            {
                tracing::warn!(
                    error = %e, kind, session_id,
                    "failed to persist session event"
                );
            }
        });
    }
}

/// Pure classification of an [`EngineEvent`] into a persistence
/// decision. `None` means "skip — do not write a row"; `Some((kind,
/// payload))` is exactly the row [`PersistingSink::persist`] would
/// spawn.
///
/// Pulled out of [`PersistingSink::emit`] so the routing contract
/// ("which events persist on which path?") is testable as a pure
/// function. Without this, the only way to assert "event X must NOT
/// persist" was to send X, sleep an arbitrary wall-clock duration
/// hoping any erroneous spawn would land, then check the DB was
/// still empty — a fundamentally racy negative-assertion shape.
///
/// Branches on `parent_tool_call_id`:
/// - `None` (top-level) — persist `Info` and `ChildTaskUpdate` only.
///   Other events are already in `messages.*`.
/// - `Some(_)` (sub-agent) — persist a richer set matching what
///   [`BufferingSink::emit`] renders, so the parent transcript can
///   reconstruct the sub-agent trace.
pub(crate) fn classify_for_persist(
    event: &EngineEvent,
    parent_tool_call_id: Option<&str>,
) -> Option<(&'static str, String)> {
    use crate::persistence::session_event_kind as sek;
    if parent_tool_call_id.is_some() {
        match event {
            EngineEvent::Info { message } => Some((sek::SUB_AGENT_EVENT, message.clone())),
            EngineEvent::ToolCallStart { name, .. } => {
                Some((sek::SUB_AGENT_EVENT, format!("  \u{1f527} {name}")))
            }
            EngineEvent::ApprovalRequest { tool_name, .. } => Some((
                sek::SUB_AGENT_EVENT,
                format!("  \u{2398} approval auto-rejected for {tool_name} (no user channel)"),
            )),
            EngineEvent::AskUserRequest { question, .. } => {
                let truncated: String = question.chars().take(80).collect();
                Some((
                    sek::SUB_AGENT_EVENT,
                    format!("  \u{2398} ask-user auto-skipped: {truncated}"),
                ))
            }
            _ => None,
        }
    } else {
        match event {
            EngineEvent::Info { message } => Some((sek::INFO, message.clone())),
            EngineEvent::ChildTaskUpdate { .. } => {
                // serde_json::to_string on EngineEvent is infallible in
                // practice (every variant is `Serialize`-clean), but
                // we preserve the original silent-skip-on-error
                // behaviour rather than panic. A future test could
                // construct an event that fails serialization — we'd
                // log and skip, not crash a session.
                serde_json::to_string(event)
                    .ok()
                    .map(|json| (sek::BG_TASK_UPDATE, json))
            }
            _ => None,
        }
    }
}

impl EngineSink for PersistingSink<'_> {
    fn emit(&self, event: EngineEvent) {
        // The routing decision is a pure function (see
        // [`classify_for_persist`]); `emit` only wires it to the
        // fire-and-forget spawner and forwards unconditionally.
        if let Some((kind, payload)) =
            classify_for_persist(&event, self.parent_tool_call_id.as_deref())
        {
            self.persist(kind, payload);
        }
        self.inner.emit(event);
    }
}

/// A sink that collects events into a Vec for testing.
///
/// Optionally also broadcasts each event to subscribers (#1109 F3) so
/// tests can wait deterministically for a specific event (e.g.
/// `ToolCallStart`) instead of guessing wall-clock delays.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct TestSink {
    events: std::sync::Mutex<Vec<EngineEvent>>,
    /// `Some` after [`Self::subscribe`] is called; broadcasts every emit().
    /// Lazy so tests that don't need it pay no allocation.
    broadcaster: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<EngineEvent>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestSink {
    /// Create an empty test sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all collected events.
    pub fn events(&self) -> Vec<EngineEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Get the count of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Check if no events were collected.
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }

    /// Subscribe to a live broadcast of events as they're emitted.
    ///
    /// **#1109 F3**: replaces `loop { sleep; check sink.events() }`
    /// patterns with `recv().await`. The broadcaster is created lazily
    /// on first call — emits before subscription are still captured
    /// in [`Self::events`] but won't appear in the receiver stream.
    ///
    /// Channel capacity is 256, more than enough for any test
    /// scenario; lagging receivers will see
    /// [`tokio::sync::broadcast::error::RecvError::Lagged`].
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        let mut guard = self.broadcaster.lock().unwrap();
        let sender = guard.get_or_insert_with(|| {
            let (tx, _) = tokio::sync::broadcast::channel(256);
            tx
        });
        sender.subscribe()
    }

    /// Wait for the first event matching `pred` or until `timeout`.
    /// Returns `Ok(event)` on match, `Err` on timeout or channel close.
    ///
    /// Convenience wrapper around [`Self::subscribe`]: handles the
    /// already-emitted-before-subscribe case by scanning [`Self::events`]
    /// once, then waits on the live channel for fresh events.
    pub async fn wait_for<F>(
        &self,
        timeout: std::time::Duration,
        pred: F,
    ) -> Result<EngineEvent, &'static str>
    where
        F: Fn(&EngineEvent) -> bool,
    {
        // Subscribe BEFORE the historical scan so we don't miss events
        // emitted between the scan and subscribe (the classic
        // "check-then-wait" race).
        let mut rx = self.subscribe();
        // Scan history first — maybe the event has already fired.
        if let Some(ev) = self.events().into_iter().find(|e| pred(e)) {
            return Ok(ev);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for predicate");
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return Ok(ev),
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err("sink closed");
                }
                Err(_) => return Err("timeout waiting for predicate"),
            }
        }
    }

    /// Wait for the first event for which `extract` returns `Some(T)`,
    /// or until `timeout`. Returns the extracted `T` on match, `Err`
    /// on timeout or channel close.
    ///
    /// This is the [`Iterator::find_map`] sister of [`Self::wait_for`]:
    /// useful when the test needs a *field* of the awaited event
    /// (e.g. the `id` of an `AskUserRequest`) rather than just
    /// confirmation that one was emitted. Eliminates the
    /// `wait_for(...).await + events().find_map(...)` two-step that
    /// previously required a fixed sleep before the historical scan.
    ///
    /// Same race-free subscribe-before-scan ordering as [`Self::wait_for`].
    pub async fn wait_for_map<F, T>(
        &self,
        timeout: std::time::Duration,
        mut extract: F,
    ) -> Result<T, &'static str>
    where
        F: FnMut(&EngineEvent) -> Option<T>,
    {
        let mut rx = self.subscribe();
        if let Some(value) = self.events().iter().find_map(&mut extract) {
            return Ok(value);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for predicate");
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if let Some(value) = extract(&ev) {
                        return Ok(value);
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err("sink closed");
                }
                Err(_) => return Err("timeout waiting for predicate"),
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EngineSink for TestSink {
    fn emit(&self, event: EngineEvent) {
        // Best-effort broadcast first (cheap if no subscribers).
        // Acquiring the lock briefly is fine because emit is always
        // called from a tokio task, never from a sync hot loop.
        if let Some(tx) = self.broadcaster.lock().unwrap().as_ref() {
            // Ignore the SendError on zero subscribers; storage path
            // below is still authoritative.
            let _ = tx.send(event.clone());
        }
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sink_collects_events() {
        let sink = TestSink::new();
        assert!(sink.is_empty());

        sink.emit(EngineEvent::ResponseStart);
        sink.emit(EngineEvent::TextDelta {
            text: "hello".into(),
        });
        sink.emit(EngineEvent::TextDone);

        assert_eq!(sink.len(), 3);
        let events = sink.events();
        assert!(matches!(events[0], EngineEvent::ResponseStart));
        assert!(matches!(&events[1], EngineEvent::TextDelta { text } if text == "hello"));
        assert!(matches!(events[2], EngineEvent::TextDone));
    }

    #[test]
    fn test_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TestSink>();
    }

    #[test]
    fn test_trait_object_works() {
        let sink: Box<dyn EngineSink> = Box::new(TestSink::new());
        sink.emit(EngineEvent::Info {
            message: "test".into(),
        });
    }

    // ── BufferingSink (#1022 B9) ─────────────────────────────────

    #[test]
    fn buffering_sink_records_tool_calls_and_info() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::ToolCallStart {
            id: "t1".into(),
            name: "Read".into(),
            args: serde_json::json!({"path": "foo.txt"}),
            is_sub_agent: false,
        });
        sink.emit(EngineEvent::Info {
            message: "  \u{26a1} cache hit".into(),
        });
        sink.emit(EngineEvent::ToolCallStart {
            id: "t2".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            is_sub_agent: false,
        });

        let lines = sink.take_lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("Read"), "got: {}", lines[0]);
        assert!(lines[1].contains("cache hit"), "got: {}", lines[1]);
        assert!(lines[2].contains("Bash"), "got: {}", lines[2]);
    }

    #[test]
    fn buffering_sink_drops_streaming_text() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::TextDelta {
            text: "hello".into(),
        });
        sink.emit(EngineEvent::TextDelta {
            text: " world".into(),
        });
        sink.emit(EngineEvent::TextDone);
        sink.emit(EngineEvent::ThinkingDelta {
            text: "reasoning".into(),
        });
        // Streaming text crosses the result oneshot already — capturing
        // it here would duplicate the model's final output in the
        // user-facing trace.
        assert!(sink.take_lines().is_empty());
    }

    #[test]
    fn buffering_sink_records_auto_reject_for_approval() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::ApprovalRequest {
            id: "a1".into(),
            tool_name: "Delete".into(),
            detail: "foo.txt".into(),
            preview: None,
            effect: crate::tools::ToolEffect::Destructive,
        });
        let lines = sink.take_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Delete"));
        assert!(
            lines[0].contains("auto-rejected"),
            "approval-without-channel must be marked as auto-rejected; got: {}",
            lines[0]
        );
    }

    #[test]
    fn buffering_sink_caps_runaway_traces() {
        let sink = BufferingSink::with_cap(3);
        for i in 0..10 {
            sink.emit(EngineEvent::Info {
                message: format!("line {i}"),
            });
        }
        let lines = sink.take_lines();
        // 3 real lines + 1 truncation marker. Marker is idempotent
        // even though we tried to push 7 more lines.
        assert_eq!(lines.len(), 4, "got: {lines:?}");
        assert!(lines.last().unwrap().starts_with('\u{2026}'));
        assert!(lines.last().unwrap().contains("truncated"));
    }

    #[test]
    fn buffering_sink_take_drains() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::Info {
            message: "a".into(),
        });
        assert_eq!(sink.take_lines().len(), 1);
        // Second take returns empty — not a snapshot, a drain.
        assert!(sink.take_lines().is_empty());
    }

    #[test]
    fn buffering_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BufferingSink>();
    }

    // ── ForwardingChildSink (#1201 B) ────────────────────

    /// Build a [`crate::child_agent::ChildStatusEmitter`] hooked up to a
    /// real registry so we can drain forwarded events. The registry
    /// is the load-bearing piece — it's what the inference loop
    /// drains in production, so testing through it (rather than
    /// against a mock emitter) catches wire-up regressions.
    fn make_test_emitter(
        task_id: u32,
    ) -> (
        std::sync::Arc<crate::child_agent::ChildAgentRegistry>,
        crate::child_agent::ChildStatusEmitter,
    ) {
        let registry = crate::child_agent::new_shared();
        let (status_tx, _status_rx) =
            tokio::sync::watch::channel(crate::child_agent::AgentStatus::Pending);
        let emitter = crate::child_agent::ChildStatusEmitter::new(
            task_id,
            None,
            true,
            status_tx,
            registry.clone(),
        );
        (registry, emitter)
    }

    #[test]
    fn forwarding_child_sink_emits_tool_start_and_end_to_registry() {
        let (registry, emitter) = make_test_emitter(7);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        sink.emit(EngineEvent::ToolCallStart {
            id: "t1".into(),
            name: "Read".into(),
            args: serde_json::json!({"path": "src/auth.rs"}),
            is_sub_agent: false,
        });
        sink.emit(EngineEvent::ToolCallResult {
            id: "t1".into(),
            name: "Read".into(),
            output: "<file contents>".into(),
        });

        let drained = registry.drain_status_events();
        assert_eq!(
            drained.len(),
            2,
            "each interesting event should fan out exactly one ChildAgentActivity"
        );
        assert!(matches!(
            &drained[0],
            EngineEvent::ChildAgentActivity {
                task_id: 7,
                kind: crate::engine::event::ChildAgentActivityKind::ToolStart { tool_name, summary },
                ..
            } if tool_name == "Read" && summary.contains("src/auth.rs")
        ));
        assert!(matches!(
            &drained[1],
            EngineEvent::ChildAgentActivity {
                kind: crate::engine::event::ChildAgentActivityKind::ToolEnd { tool_name, success: true },
                ..
            } if tool_name == "Read"
        ));
    }

    #[test]
    fn forwarding_child_sink_classifies_tool_errors() {
        let (registry, emitter) = make_test_emitter(1);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        sink.emit(EngineEvent::ToolCallResult {
            id: "t1".into(),
            name: "Bash".into(),
            output: "Error: command not found".into(),
        });
        let drained = registry.drain_status_events();
        assert!(matches!(
            &drained[0],
            EngineEvent::ChildAgentActivity {
                kind: crate::engine::event::ChildAgentActivityKind::ToolEnd { success: false, .. },
                ..
            }
        ));
    }

    #[test]
    fn forwarding_child_sink_preserves_buffering_for_post_completion_drain() {
        // The whole point of the decorator is "live AND buffered" —
        // dropping the inner buffer would silently break the
        // model-facing narrative trace. This test pins that.
        let (_registry, emitter) = make_test_emitter(1);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        sink.emit(EngineEvent::ToolCallStart {
            id: "t1".into(),
            name: "Read".into(),
            args: serde_json::json!({"path": "foo"}),
            is_sub_agent: false,
        });
        sink.emit(EngineEvent::Info {
            message: "  \u{26a1} cache hit".into(),
        });

        let lines = sink.take_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Read"));
        assert!(lines[1].contains("cache hit"));
    }

    #[test]
    fn forwarding_child_sink_drops_streaming_text() {
        // Streaming text is forwarded neither live nor to the buffer:
        // the model's final output already crosses the result oneshot,
        // so capturing it here would duplicate it AND spam the parent
        // feed with per-token noise.
        let (registry, emitter) = make_test_emitter(1);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        sink.emit(EngineEvent::TextDelta {
            text: "hello".into(),
        });
        sink.emit(EngineEvent::ThinkingDelta {
            text: "reasoning".into(),
        });
        sink.emit(EngineEvent::TextDone);

        assert!(registry.drain_status_events().is_empty());
        assert!(sink.take_lines().is_empty());
    }

    #[test]
    fn forwarding_child_sink_summarizes_known_tool_args() {
        // Per-tool special cases live inside the sink so every client
        // renders the same summary string. Pin the contracts the TUI
        // depends on — if the summary format changes, the activity
        // feed render needs to update too.
        let (registry, emitter) = make_test_emitter(1);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        // Bash: command
        sink.emit(EngineEvent::ToolCallStart {
            id: "a".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "cargo test"}),
            is_sub_agent: false,
        });
        // Grep: pattern + path
        sink.emit(EngineEvent::ToolCallStart {
            id: "b".into(),
            name: "Grep".into(),
            args: serde_json::json!({"pattern": "TODO", "path": "src/"}),
            is_sub_agent: false,
        });
        // InvokeAgent: agent name
        sink.emit(EngineEvent::ToolCallStart {
            id: "c".into(),
            name: "InvokeAgent".into(),
            args: serde_json::json!({"agent": "reviewer", "prompt": "x"}),
            is_sub_agent: false,
        });

        let drained = registry.drain_status_events();
        let summaries: Vec<String> = drained
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ChildAgentActivity {
                    kind: crate::engine::event::ChildAgentActivityKind::ToolStart { summary, .. },
                    ..
                } => Some(summary.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(summaries.len(), 3);
        assert!(summaries[0].contains("cargo test"), "got: {}", summaries[0]);
        assert!(
            summaries[1].contains("TODO") && summaries[1].contains("src/"),
            "got: {}",
            summaries[1]
        );
        assert!(summaries[2].contains("reviewer"), "got: {}", summaries[2]);
    }

    #[test]
    fn forwarding_child_sink_truncates_long_summaries() {
        // Summaries land in tight horizontal real estate (inline
        // under the bg-task spawn cell). A 5KB commit message in the
        // args must not blow up the feed.
        let (registry, emitter) = make_test_emitter(1);
        let sink = ForwardingChildSink::new(BufferingSink::new(), emitter);

        let long_cmd = "x".repeat(500);
        sink.emit(EngineEvent::ToolCallStart {
            id: "a".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": long_cmd}),
            is_sub_agent: false,
        });

        let drained = registry.drain_status_events();
        let summary = match &drained[0] {
            EngineEvent::ChildAgentActivity {
                kind: crate::engine::event::ChildAgentActivityKind::ToolStart { summary, .. },
                ..
            } => summary.clone(),
            _ => panic!("expected ToolStart"),
        };
        assert!(summary.chars().count() <= 80);
        assert!(summary.ends_with('\u{2026}'));
    }

    #[test]
    fn forwarding_child_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ForwardingChildSink>();
    }

    // ── wait_for_map ────────────────────────────────────────────────────
    //
    // Why these tests live here, not in the consumer modules: `wait_for_map`
    // is the deterministic-readiness primitive that lets approval_flow tests
    // (and others) drop fixed `tokio::time::sleep` calls. If the helper
    // breaks subtly — wrong ordering, missed-event race, timeout off-by-one
    // — every consumer test goes flaky in a way that's hard to bisect. Pin
    // the contract here.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_map_returns_extracted_value_for_already_emitted_event() {
        // The historical-scan path: event was emitted *before* wait_for_map
        // is called. Must still resolve immediately, not wait for the next
        // broadcast. (This is what makes the helper safe to use after
        // spawning a task without a fixed pre-sleep.)
        let sink = TestSink::new();
        sink.emit(EngineEvent::Info {
            message: "first".into(),
        });
        sink.emit(EngineEvent::Info {
            message: "second".into(),
        });

        let result: Result<String, _> = sink
            .wait_for_map(std::time::Duration::from_secs(1), |e| match e {
                EngineEvent::Info { message } if message == "second" => Some(message.clone()),
                _ => None,
            })
            .await;

        assert_eq!(result.unwrap(), "second");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_map_resolves_on_event_emitted_after_subscribe() {
        // The live-channel path: the awaited event is emitted *after*
        // wait_for_map subscribes. This is the common case for the
        // approval_flow tests — the spawned task hasn't run yet.
        let sink = std::sync::Arc::new(TestSink::new());
        let sink2 = std::sync::Arc::clone(&sink);
        let task = tokio::spawn(async move {
            // Small natural delay; not load-bearing, just exercising the
            // "emit happens after wait_for_map starts polling" path.
            tokio::task::yield_now().await;
            sink2.emit(EngineEvent::Info {
                message: "hello".into(),
            });
        });

        let value: String = sink
            .wait_for_map(std::time::Duration::from_secs(5), |e| match e {
                EngineEvent::Info { message } => Some(message.clone()),
                _ => None,
            })
            .await
            .expect("should resolve before timeout");

        assert_eq!(value, "hello");
        task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_map_skips_non_matching_events() {
        // The live channel can carry many events the predicate doesn't
        // care about. wait_for_map must keep polling, not return on the
        // first arrival.
        use crate::engine::event::TurnEndReason;
        let sink = std::sync::Arc::new(TestSink::new());
        let sink2 = std::sync::Arc::clone(&sink);
        tokio::spawn(async move {
            sink2.emit(EngineEvent::Info {
                message: "noise-1".into(),
            });
            sink2.emit(EngineEvent::Info {
                message: "noise-2".into(),
            });
            sink2.emit(EngineEvent::TurnEnd {
                turn_id: "t-1".into(),
                reason: TurnEndReason::Complete,
            });
            sink2.emit(EngineEvent::Info {
                message: "noise-3".into(),
            });
        });

        let turn_id: String = sink
            .wait_for_map(std::time::Duration::from_secs(5), |e| match e {
                EngineEvent::TurnEnd { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .await
            .expect("TurnEnd should arrive");

        assert_eq!(turn_id, "t-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_map_returns_err_on_timeout() {
        // No matching event will ever arrive — helper must respect the
        // bounded timeout instead of hanging the test runner.
        let sink = TestSink::new();
        sink.emit(EngineEvent::Info {
            message: "unrelated".into(),
        });

        let result: Result<(), _> = sink
            .wait_for_map(std::time::Duration::from_millis(50), |e| match e {
                EngineEvent::TurnEnd { .. } => Some(()),
                _ => None,
            })
            .await;

        assert!(result.is_err(), "expected timeout error, got {result:?}");
    }

    // ── wait_for (backfill) ────────────────────────────────────────────
    //
    // The boolean-predicate sister was added before this test module
    // existed; pin the same race-free contract for symmetry.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_returns_event_for_already_emitted_match() {
        use crate::engine::event::TurnEndReason;
        let sink = TestSink::new();
        sink.emit(EngineEvent::TurnEnd {
            turn_id: "t-1".into(),
            reason: TurnEndReason::Complete,
        });

        let ev = sink
            .wait_for(std::time::Duration::from_secs(1), |e| {
                matches!(e, EngineEvent::TurnEnd { .. })
            })
            .await
            .expect("already-emitted event should resolve immediately");

        assert!(matches!(ev, EngineEvent::TurnEnd { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_returns_err_on_timeout() {
        let sink = TestSink::new();
        let result = sink
            .wait_for(std::time::Duration::from_millis(50), |e| {
                matches!(e, EngineEvent::TurnEnd { .. })
            })
            .await;
        assert!(result.is_err());
    }

    // ── classify_for_persist (8a PR-2) ──────────────────────────────
    //
    // The pre-#1265-8a-PR-2 negative-assertion tests for PersistingSink
    // (in `tests/persisting_sink_test.rs`) sent a non-allowlisted event,
    // slept 50ms hoping any erroneous fire-and-forget DB insert would
    // land, then asserted the table was empty. That's a fundamentally
    // racy negative assertion: "absence after a guess."
    //
    // The classifier extraction makes the routing decision a pure
    // function. We can now assert exactly the right thing — the
    // *decision*, not the side effect — with no async, no DB, no sleep.

    use crate::child_agent::AgentStatus;
    use crate::persistence::session_event_kind as sek;

    fn child_task_update_event() -> EngineEvent {
        EngineEvent::ChildTaskUpdate {
            task_id: 42,
            spawner: Some(7),
            is_background: true,
            status: AgentStatus::Pending,
        }
    }

    #[test]
    fn classify_top_level_persists_info_with_message_payload() {
        let event = EngineEvent::Info {
            message: "hello".into(),
        };
        let decision = classify_for_persist(&event, None);
        assert_eq!(decision, Some((sek::INFO, "hello".to_string())));
    }

    #[test]
    fn classify_top_level_persists_child_task_update_with_json_payload() {
        let event = child_task_update_event();
        let (kind, payload) =
            classify_for_persist(&event, None).expect("ChildTaskUpdate must persist top-level");
        assert_eq!(kind, sek::BG_TASK_UPDATE);
        // Payload is a JSON-serialized EngineEvent; sanity-check a
        // couple of fields rather than pinning the full schema.
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("payload must parse as JSON");
        assert_eq!(parsed["task_id"], 42);
        assert_eq!(parsed["is_background"], true);
    }

    #[test]
    fn classify_top_level_skips_non_allowlisted_events() {
        // Anything not Info / ChildTaskUpdate must skip on the
        // top-level path — these events are already in `messages.*`.
        // Replaces the racy `top_level_passes_through_non_persistable_
        // events_without_db_writes` integration sleep test.
        let cases = [
            EngineEvent::ResponseStart,
            EngineEvent::TextDelta { text: "a".into() },
            EngineEvent::TextDone,
            EngineEvent::ToolCallStart {
                id: "call-1".into(),
                name: "Read".into(),
                args: serde_json::json!({"path": "f.txt"}),
                is_sub_agent: false,
            },
        ];
        for event in cases {
            assert_eq!(
                classify_for_persist(&event, None),
                None,
                "top-level must skip {event:?}"
            );
        }
    }

    #[test]
    fn classify_sub_agent_persists_info_with_sub_agent_kind() {
        let event = EngineEvent::Info {
            message: "sub said hi".into(),
        };
        let decision = classify_for_persist(&event, Some("parent-call"));
        assert_eq!(
            decision,
            Some((sek::SUB_AGENT_EVENT, "sub said hi".to_string()))
        );
    }

    #[test]
    fn classify_sub_agent_persists_tool_call_start_as_rendered_line() {
        let event = EngineEvent::ToolCallStart {
            id: "c-1".into(),
            name: "Read".into(),
            args: serde_json::json!({}),
            is_sub_agent: false,
        };
        let (kind, payload) = classify_for_persist(&event, Some("parent-call"))
            .expect("ToolCallStart must persist on sub-agent path");
        assert_eq!(kind, sek::SUB_AGENT_EVENT);
        assert_eq!(payload, "  \u{1f527} Read");
    }

    #[test]
    fn classify_sub_agent_persists_approval_request_as_auto_reject_line() {
        let event = EngineEvent::ApprovalRequest {
            id: "a-1".into(),
            tool_name: "WriteFile".into(),
            detail: "write 1 file".into(),
            preview: None,
            effect: crate::tools::ToolEffect::LocalMutation,
        };
        let (kind, payload) = classify_for_persist(&event, Some("parent-call"))
            .expect("ApprovalRequest must persist on sub-agent path");
        assert_eq!(kind, sek::SUB_AGENT_EVENT);
        assert!(
            payload.contains("approval auto-rejected for WriteFile"),
            "payload should explain the auto-rejection: {payload:?}"
        );
    }

    #[test]
    fn classify_sub_agent_truncates_ask_user_question_to_80_chars() {
        let long_question = "q".repeat(200);
        let event = EngineEvent::AskUserRequest {
            id: "q-1".into(),
            question: long_question,
            options: Vec::new(),
        };
        let (kind, payload) = classify_for_persist(&event, Some("parent-call"))
            .expect("AskUserRequest must persist on sub-agent path");
        assert_eq!(kind, sek::SUB_AGENT_EVENT);
        // Prefix is fixed; the question slice must be exactly 80 chars
        // (truncation contract). 8 chars of prefix + 80 of question = 88.
        let prefix = "  \u{2398} ask-user auto-skipped: ";
        let question_part = payload.strip_prefix(prefix).expect("prefix should match");
        assert_eq!(
            question_part.chars().count(),
            80,
            "question must truncate to 80 chars, got {} in {payload:?}",
            question_part.chars().count()
        );
    }

    #[test]
    fn classify_sub_agent_skips_child_task_update() {
        // ChildTaskUpdate is a top-level-only signal (parent transcript
        // shows sub-agent activity through InvokeAgent's tool result).
        // Replaces the racy `sub_agent_does_not_persist_bg_task_update`
        // integration sleep test.
        let event = child_task_update_event();
        assert_eq!(classify_for_persist(&event, Some("parent-call")), None);
    }

    #[test]
    fn classify_sub_agent_skips_other_non_allowlisted_events() {
        let cases = [
            EngineEvent::ResponseStart,
            EngineEvent::TextDelta { text: "a".into() },
            EngineEvent::TextDone,
        ];
        for event in cases {
            assert_eq!(
                classify_for_persist(&event, Some("parent-call")),
                None,
                "sub-agent must skip {event:?}"
            );
        }
    }
}
