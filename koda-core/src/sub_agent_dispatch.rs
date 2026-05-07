//! Sub-agent invocation and lifecycle management.
//!
//! Extracted from `tool_dispatch.rs` — handles `InvokeAgent` execution,
//! background agent spawning, worktree provisioning, and sub-agent caching.
//! Each sub-agent gets its own session, provider, and (optionally) worktree
//! for isolation. Results are cached by `(agent_name, prompt_hash)`.

use crate::approval_flow::request_approval;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::memory;
use crate::persistence::Persistence;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::{ChatMessage, ToolCall};
use crate::sub_agent_cache::SubAgentCache;
use crate::tool_dispatch::execute_one_tool;
use crate::tools::{self, ToolRegistry};
use crate::trust::{self, ToolApproval, TrustMode, derive_child_trust};
use crate::turn_context::ToolExecutionContext;

use anyhow::{Context, Result};
use koda_sandbox::{CwdProvider, GitWorktreeProvider, WorkspaceProvider};

#[cfg(target_os = "macos")]
use koda_sandbox::ClonefileProvider;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Process-wide allocator for sub-agent invocation IDs.
///
/// Phase E of #996. Each `execute_sub_agent` call (foreground or
/// background) draws a fresh id; that id becomes the **spawner tag**
/// for any background work this sub-agent registers, and is the key
/// used to cancel that work when the sub-agent exits.
///
/// Top-level inference uses `None` (no invocation id). Sub-agents at
/// any nesting depth use `Some(N)`.
///
/// `u32::MAX` invocations is comfortably more than any single Code
/// Puppy session needs; we don't bother with wrap-around handling.
/// Starts at 1 so `0` can stay reserved for "unset" should the type
/// ever change.
static NEXT_INVOCATION_ID: AtomicU32 = AtomicU32::new(1);

/// Default per-sub-agent turn cap when the agent JSON does not set
/// `max_iterations` explicitly (#1135).
///
/// Matches the `DEFAULT_MAX_TURNS = 30` used by `gemini-cli`'s agent
/// runtime — see `packages/core/src/agents/types.ts:51`. Codex does
/// not cap sub-agents at all ("trust the model"), which is what koda
/// did between #1110 and this change. The bug from #1135 (read-only
/// explorer agents spinning for 100+s on broad prompts) is exactly
/// the failure mode the no-cap regime can't catch: non-identical but
/// non-progressing tool calls slip past `LoopDetector` and only stop
/// when the user gives up and hits Ctrl-C.
///
/// 30 turns is enough headroom for any reasonable read-only
/// exploration on a moderate codebase (the offending session in
/// #1135 used ~26 calls); long-running write agents that need more
/// can opt up via `"max_iterations": N` in their JSON. Mirrors
/// gemini's per-agent overrides (cli-help: 10, generalist: 20,
/// browser: 50, codebase-investigator: 50).
///
/// **NOTE**: this only applies to sub-agents. The top-level koda
/// agent's cap (currently `KodaConfig::max_iterations = 200` per
/// `config.rs:247`) is unchanged — that path runs interactively and
/// already has the `EngineEvent::LoopCapReached` extension UX.
pub(crate) const DEFAULT_SUB_AGENT_MAX_TURNS: u32 = 30;

/// Allocate a fresh invocation id. See [`NEXT_INVOCATION_ID`].
pub(crate) fn next_invocation_id() -> u32 {
    NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Build a unique child [`AgentPath`] under `parent` for a sub-agent
/// invocation (#1325 Phase 4 — used by commits 2 and 3).
///
/// **Why a helper:** the same composition (sanitize the user-supplied
/// `agent_name`, append a unique id) is needed at both spawn sites
/// (the `tokio::spawn(run_bg_agent(...))` block, and the inline
/// path inside `execute_sub_agent`). Keeping the rule in one place
/// means the bg/inline paths can never drift on uniqueness or
/// validation semantics — and it stays unit-testable as a pure fn.
///
/// **Sanitization rules** match [`crate::agent::path::AgentPath`]'s
/// `[a-z0-9_]+` segment grammar:
///
/// 1. Lowercase every ASCII letter.
/// 2. Replace any other char (uppercase letters that didn't ASCII-
///    lowercase, digits-non-ASCII, punctuation, whitespace, unicode)
///    with `_`.
/// 3. Collapse runs of `_` and trim leading/trailing `_`.
/// 4. If the result is empty (e.g. `agent_name = "!!!"` — pathological
///    but `parse_agent_name_required` would have accepted it), fall
///    back to `agent`.
/// 5. Append `_<unique_id>` so concurrent invocations of the same
///    agent don't collide (`/root/explore_42`, `/root/explore_43`).
///
/// **Why include `unique_id` even when callers pass distinct names:**
/// Phase 4e of #1325 introduces multi-element `InvokeAgent([...])`
/// where the same agent name is fanned out in parallel. The id
/// suffix is the only thing that keeps their mailbox identities
/// distinct, and adding it unconditionally is cheaper than branching
/// on "is this the parallel case."
///
/// **Future-proofing:** `parent` is taken as a borrow rather than
/// hard-coded to [`AgentPath::root`] so when nested spawn lands
/// (Phase 6+) the same helper builds `/root/explore_4/researcher_7`
/// without a touch — caller passes `parent_tx.turn.agent_path`
/// today (always `/root`) and the future nested call passes the
/// in-flight sub-agent's path (also already plumbed by 4a).
pub(crate) fn child_agent_path(
    parent: &crate::agent::AgentPath,
    agent_name: &str,
    unique_id: u32,
) -> anyhow::Result<crate::agent::AgentPath> {
    let mut sanitized = String::with_capacity(agent_name.len());
    let mut prev_underscore = false;
    for ch in agent_name.chars() {
        let mapped = if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            ch
        } else if ch.is_ascii_uppercase() {
            ch.to_ascii_lowercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        sanitized.push(mapped);
    }
    let trimmed = sanitized.trim_matches('_');
    let base = if trimmed.is_empty() { "agent" } else { trimmed };
    let segment = format!("{base}_{unique_id}");
    parent
        .join(&segment)
        .map_err(|e| anyhow::anyhow!("failed to build child AgentPath from {agent_name:?}: {e}"))
}

/// RAII unregister-on-drop guard for a sub-agent's mailbox entry
/// (#1325 Phase 4 — commit 3 of 3).
///
/// Holds an [`Arc`](std::sync::Arc) clone of the registry plus the
/// path it registered itself at. On drop, calls `unregister` so the
/// path slot becomes available for re-use (Phase 4e fan-out is the
/// motivating case: one `explore` agent finishes and its slot opens
/// up for the next).
///
/// **Why RAII rather than an explicit unregister at the bottom of
/// `execute_sub_agent`:** every `?` inside the body would leak a
/// dangling registry entry without it. The same reasoning drives
/// the existing [`InvocationCleanup`] guard in this module.
///
/// **Idempotent:** if `unregister` returns `false` (entry already
/// gone), drop logs at trace level and continues. Lets future code
/// paths explicitly unregister (e.g. for hot-replace) without this
/// guard double-warning.
struct MailboxRegistration {
    registry: std::sync::Arc<crate::agent::MailboxRegistry>,
    path: crate::agent::AgentPath,
}

impl Drop for MailboxRegistration {
    fn drop(&mut self) {
        if !self.registry.unregister(&self.path) {
            tracing::trace!(
                path = %self.path,
                "MailboxRegistration::drop: entry already gone (idempotent)"
            );
        }
    }
}

/// Register a fresh mailbox at `path` in `registry` (#1325 Phase 4 —
/// commit 3 of 3).
///
/// Returns the [`MailboxReceiver`] (the sub-agent drains this into
/// its own DB session at the top of each iter) plus an RAII guard
/// that unregisters on drop. The sender side stays in the registry
/// so peers calling `SendMessage{target: <path>}` find a live inbox.
///
/// Returns `None` if the path is already registered. Because
/// [`child_agent_path`] appends a unique `_<id>` suffix per
/// invocation, this collision should not happen in practice; if it
/// does, the caller is supposed to fall back to the pre-Phase-4
/// "everything routes through `/root`" behaviour rather than
/// silently shadowing another sub-agent's mailbox.
fn register_sub_agent_mailbox(
    registry: &std::sync::Arc<crate::agent::MailboxRegistry>,
    path: &crate::agent::AgentPath,
) -> Option<(crate::agent::MailboxReceiver, MailboxRegistration)> {
    let (mailbox, receiver) = crate::agent::Mailbox::new();
    let mailbox = std::sync::Arc::new(mailbox);
    match registry.register(path.clone(), mailbox) {
        crate::agent::RegisterOutcome::Inserted => Some((
            receiver,
            MailboxRegistration {
                registry: std::sync::Arc::clone(registry),
                path: path.clone(),
            },
        )),
        crate::agent::RegisterOutcome::AlreadyRegistered => {
            // `child_agent_path` includes a unique id, so this should
            // never fire. Treat as a programmer error — don't shadow.
            tracing::error!(
                path = %path,
                "sub-agent mailbox path collision \u{2014} registry already has an entry. \
                 child_agent_path uniquification must be broken."
            );
            None
        }
    }
}

/// Drain a sub-agent's mailbox into its DB session as `Role::User`
/// rows (#1325 Phase 4 — commit 3 of 3).
///
/// Mirrors [`crate::session::KodaSession::drain_mail_to_db`] but
/// scoped to the per-sub-agent receiver and `session_id`. Called at
/// the top of each iter inside the sub-agent's inline inference loop
/// so any mail that arrived since the last iter shows up in the
/// next LLM call's history.
///
/// Empty mailbox is a no-op (returns `Ok(())` without writing
/// anything) — cheaper than guarding at every call site.
async fn drain_sub_mailbox_into_session(
    rx: &mut crate::agent::MailboxReceiver,
    db: &crate::db::Database,
    session_id: &str,
) -> anyhow::Result<()> {
    let items = rx.drain();
    if items.is_empty() {
        return Ok(());
    }
    for mail in items {
        let (role, content) = crate::agent::mail_to_user_message(&mail);
        db.insert_message(session_id, &role, Some(&content), None, None, None)
            .await?;
    }
    Ok(())
}

/// RAII cleanup hook for #996 Phase E.
///
/// On drop, cancels every bg-agent registry entry tagged with this
/// sub-agent's invocation id. That covers the way a sub-agent can
/// exit and leave orphans:
///
///   1. **Error return** (`Err(...)` from any `?` inside the loop) —
///      e.g. provider failure, persistence failure, `?`-propagated
///      cancellation.
///   2. **`LoopDetector` hard stop** — model ignored feedback after
///      consecutive identical tool calls; surfaced via the inference
///      helpers, not as a marker string here.
///
/// On the cancel-token path we'd reap anyway via the parent's cascade,
/// but the spawner-scoped cancel is cheap and idempotent so we just
/// always run it. `cancel_for_spawner` is `O(n)` over the registry,
/// which is fine for the < 100-entry registries we expect.
///
/// Background shell processes (`Bash{background:true}`) are *not*
/// covered here — each sub-agent constructs its own `ToolRegistry`
/// with its own `BgRegistry`, which `Drop`-SIGTERMs everything when
/// the registry goes out of scope. That handles shell orphans for
/// free; this struct only needs to deal with the *shared*
/// `ChildAgentRegistry`.
struct InvocationCleanup<'a> {
    bg: &'a std::sync::Arc<crate::child_agent::ChildAgentRegistry>,
    invocation_id: u32,
}

impl Drop for InvocationCleanup<'_> {
    fn drop(&mut self) {
        let cancelled = self.bg.cancel_for_spawner(self.invocation_id);
        if cancelled > 0 {
            tracing::debug!(
                spawner = self.invocation_id,
                cancelled,
                "execute_sub_agent exit: cancelled orphaned bg agents",
            );
        }
    }
}

/// Run a sub-agent in the background. Owns all data (no borrows).
///
/// **Phase 2 of #1022 (B5 complete):** uses the multi-thread runtime
/// via `tokio::spawn`. This requires `execute_sub_agent`'s future to
/// be `Send`, which we enforce explicitly via the `+ Send` bound on
/// its return type — see the function's signature for the bound and
/// `koda-sandbox/src/ipc.rs` for the matching `Send` bounds on the
/// generic IPC helpers that previously hid a non-Send transitive.
#[allow(clippy::too_many_arguments)]
async fn run_bg_agent(
    project_root: std::path::PathBuf,
    parent_config: KodaConfig,
    db: Database,
    arguments: String,
    sub_agent_cache: SubAgentCache,
    parent_session: String,
    tx: tokio::sync::oneshot::Sender<
        Result<crate::child_agent::BgPayload, crate::child_agent::BgPayload>,
    >,
    // B2 of #1022: parent's cancel token, threaded as a `child_token()`
    // so a Ctrl-C in the parent loop cancels the bg agent.
    cancel: CancellationToken,
    // B1 of #1022: parent's trust mode — used both as the approval
    // mode for tool calls inside the bg agent and (via the recursive
    // `execute_sub_agent` call) as the clamp ceiling for the
    // sub-agent's own declared trust.
    parent_trust: TrustMode,
    // B4 of #1022: parent's effective sandbox policy at spawn time.
    // The recursive `execute_sub_agent` composes the child policy
    // onto this so the bg agent inherits any parent narrowing.
    parent_sandbox_policy: koda_sandbox::SandboxPolicy,
    // Layer 0 of #996 + #1076: status fan-out helper. Drives the
    // per-task `watch::Sender<AgentStatus>` (read by `/agents` and
    // the status-bar pill via `snapshot()`) AND queues an
    // `EngineEvent::ChildTaskUpdate` on the registry so the inference
    // loop can forward it to the active `EngineSink`. Pre-#1076 this
    // was a raw `watch::Sender` and only the TUI (which polled the
    // registry directly) saw transitions; now every client surface
    // (TUI / headless / ACP) gets the same event stream.
    //
    // `send` failures on the underlying watch are silently absorbed
    // by the emitter — the only way that fails is if the registry
    // entry was reaped, in which case the queued `ChildTaskUpdate` is
    // harmless extra signal that clients can ignore.
    emitter: crate::child_agent::ChildStatusEmitter,
    // #1325 Phase 4 (commit 2): the bg agent's identity in the spawn
    // tree (e.g. `/root/explore_42`). Constructed by the spawn site
    // BEFORE `tokio::spawn` so we get a path-construction error in
    // the parent's dispatch flow rather than as a panic inside the
    // detached task. Threaded into the bg agent's `TurnContext` so
    // `SendMessage` from inside the bg agent stamps the right
    // `author`, and 4c+ peer-tools route by it.
    bg_agent_path: crate::agent::AgentPath,
    // #1325 Phase 4 (commit 3): the shared mailbox registry. Cloned
    // by the spawn site from the parent's `tools.mailbox_registry()`
    // and threaded down so the bg agent's inner `execute_sub_agent`
    // call sees it on `placeholder_tools.mailbox_registry()` and
    // takes the registration branch (rather than falling back to
    // "no per-child mailbox"). `None` propagates the
    // no-session-context fallback unchanged.
    bg_mailbox_registry: Option<std::sync::Arc<crate::agent::MailboxRegistry>>,
    // #1325 Phase 5a: the parent's `AgentPath`. At task completion we
    // send the result to the parent's mailbox so `WaitForMail` can
    // unblock on child completion. `bg_agent_path` is the author;
    // `parent_agent_path` is the recipient. Separate from
    // `bg_mailbox_registry` (which serves the child's own mailbox
    // registration) so the two concerns don't conflate.
    parent_agent_path: crate::agent::AgentPath,
) {
    // Layer 0 placeholder: immediately flip Pending → Running so `/agents`
    // shows the agent as active before the first LLM call. The loop inside
    // `execute_sub_agent` updates this to `iter: 1..=20` as it progresses
    // (Layer 4, #1058). `iter: 0` is intentional here — it signals
    // "started, first iteration pending".
    emitter.send(crate::child_agent::AgentStatus::Running { iter: 0 });

    let (_, mut cmd_rx) = mpsc::channel(1);
    // #1022 B9: bg agents used to run with `NullSink`, so every
    // event inside them was silently dropped — the user only saw
    // the spawn line and the eventual completion line. Now we use
    // `BufferingSink` to capture a narrative trace (tool calls,
    // info, auto-rejected approvals) that ships back over the
    // result oneshot and gets surfaced to the user at
    // result-injection time. See `engine::sink::BufferingSink` for
    // the capture rules.
    //
    // #1201 B: wrap the buffering sink in a `ForwardingChildSink` so
    // every interesting event is *also* forwarded live as a
    // `ChildAgentActivity` to the parent's sink (via the registry's
    // status-event queue, drained by the inference loop). Pre-this
    // wrapper a 30-second tool inside a bg agent looked identical
    // to a 30-second hang — only the post-completion drain showed
    // what happened.
    let buffering_sink = crate::engine::sink::ForwardingChildSink::new(
        crate::engine::sink::BufferingSink::new(),
        emitter.clone(),
    );
    let nested_bg = crate::child_agent::new_shared();

    // **#1163 (Lean A)**: pre-#1163 we had to inject
    // `sync_args["background"] = false` here so the recursive
    // `execute_sub_agent` call ran the loop inline instead of
    // re-spawning. The `background` field is gone from the schema
    // now (and from `parse_background_required`, also deleted), so
    // the recursion is steered by the `inline_only: true` argument
    // below. The arguments string passes through unchanged.
    let sync_arguments = arguments.clone();

    // We need to inspect `cancel` *after* the call to decide between
    // Cancelled and Errored when `execute_sub_agent` returns Err —
    // a cancelled future typically surfaces as an error from inside
    // the loop, but the user-visible state should be "Cancelled",
    // not "Errored". Clone before the move.
    let cancel_for_status = cancel.clone();

    // #1265 item 4 PR-3: bg agents own their args (`'static` for
    // `tokio::spawn`), so we have to construct the parent's dispatch
    // context locally from owned borrows. The `tools` field is a
    // throwaway empty registry — `execute_sub_agent` ignores the
    // parent's tools for tool *execution* (sub-agents build their
    // own from `sub_config.allowed_tools`) but it DOES read
    // `placeholder_tools.mailbox_registry()` to find the registry
    // it should install on the sub-agent's tools — see commit 3 of
    // #1325 Phase 4. Install the threaded-in registry here so that
    // lookup succeeds (otherwise the bg path silently regresses to
    // "no per-child mailbox").
    let placeholder_tools =
        crate::tools::ToolRegistry::new(project_root.clone(), parent_config.max_context_tokens);
    if let Some(reg) = bg_mailbox_registry.as_ref() {
        placeholder_tools.set_mailbox_registry(std::sync::Arc::clone(reg));
    }
    // #1325 Phase 4 (commit 2): the spawn site built our path AND
    // moved it in via the function parameter. We can't construct it
    // here — we'd need the parent's path AND the registry-allocated
    // `task_id`, neither of which lives in `run_bg_agent`'s
    // owned-data world.
    let bg_turn = crate::turn_context::TurnContext::new(
        &project_root,
        &parent_config,
        &db,
        &parent_session,
        &buffering_sink,
        cancel,
        &sub_agent_cache,
        &nested_bg,
        parent_trust,
        &placeholder_tools,
        &bg_agent_path,
    );
    // Phase E of #996: the bg agent has no in-process parent in
    // the spawner sense — its `nested_bg` registry is fresh, and
    // any bg work it spawns gets tagged with the bg agent's *own*
    // invocation id (allocated inside the recursive call). The
    // parent's cascade-cancel covers cross-registry teardown.
    let bg_tx = crate::turn_context::ToolExecutionContext::new(&bg_turn, None);

    let result = execute_sub_agent(
        bg_tx,
        &sync_arguments,
        &mut cmd_rx,
        None,
        &parent_sandbox_policy,
        // Layer 4 of #996 + #1076: forward the status emitter so the
        // loop can push live `Running { iter }` updates that fan out
        // to BOTH the watch channel (for `/agents` snapshots) and
        // the registry's event queue (for the inference-loop → sink
        // path that ACP / headless / TUI all read from). Cloned so
        // the terminal sends below still have access after
        // `execute_sub_agent` returns.
        Some(emitter.clone()),
        // #1108 P2a: nested bg-agent spawns inside this bg agent
        // have no parent `InvokeAgent` tool call in our session
        // (their parent is the bg agent itself, whose tool calls
        // live in the sub-session). Pass `None` — their trace will
        // be visible only via the bg agent's own surfaced output.
        None,
        // **#1163 (Lean A)**: `inline_only=true` is the recursion
        // guard. We're already inside `tokio::spawn(run_bg_agent(...))`,
        // so `execute_sub_agent`'s bg-spawn block must NOT fire again
        // — otherwise every InvokeAgent would spawn an infinite chain
        // of bg tasks. The inline path runs the actual inference loop
        // and writes its result back via the oneshot `tx`.
        true,
    )
    .await;

    // Drain the buffered trace exactly once. The events ship back
    // alongside the output (for the success case) or alongside the
    // error message (for the failure case) so the user can see
    // *what the bg agent attempted* even when it failed.
    let events = buffering_sink.take_lines();

    // Set terminal status *before* sending the result oneshot so a
    // racing `snapshot()` between the `tx.send` and the entry being
    // drained sees the terminal state, not stale `Running`.
    match &result {
        Ok(output) => {
            emitter.send(crate::child_agent::AgentStatus::Completed {
                // `summary` is currently the full output — truncation
                // is the display layer's job (Codex pattern: see
                // `COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES`).
                summary: output.clone(),
            });
        }
        Err(e) => {
            // Cancellation typically reaches us as an error from
            // somewhere deep in the loop. Disambiguate by checking
            // the token: if it fired, the user-visible reason is
            // "Cancelled", not the inner error string.
            let status = if cancel_for_status.is_cancelled() {
                crate::child_agent::AgentStatus::Cancelled
            } else {
                crate::child_agent::AgentStatus::Errored {
                    error: e.to_string(),
                }
            };
            emitter.send(status);
        }
    }

    // #1325 Phase 5a: notify the parent via its mailbox so any
    // `WaitForMail` call in the parent session can unblock on child
    // completion. This fires *before* the oneshot so the watch
    // sequence increment is visible before the drain-injection
    // path adds the `Role::Tool` row — ordering that avoids the
    // race where `WaitForMail` wakes, calls `drain_completed` and
    // finds nothing yet.
    //
    // Send is best-effort: if the registry has no entry for the
    // parent (e.g. top-level session running without a registry,
    // or the parent already exited), we silently skip — the
    // existing drain path (`inference.rs` `drain_completed`) still
    // injects the result on the next iteration, so nothing is lost.
    if let Some(registry) = bg_mailbox_registry.as_deref() {
        if let Some(parent_mailbox) = registry.get(&parent_agent_path) {
            let summary = match &result {
                Ok(output) => format!(
                    "Background agent '{}' completed.\n{}",
                    bg_agent_path, output
                ),
                Err(e) => format!("Background agent '{}' failed: {e:#}", bg_agent_path),
            };
            let mail = crate::agent::inter_agent::InterAgentCommunication::new(
                bg_agent_path.clone(),
                parent_agent_path,
                Vec::new(),
                summary,
                // trigger_turn=true: wake the parent immediately so
                // `WaitForMail` unblocks as soon as the child exits
                // rather than waiting for the parent's next natural turn.
                true,
            );
            // `send` returns the sequence number; we don't need it.
            let _seq = parent_mailbox.send(mail);
        }
    }

    let _ = match result {
        Ok(output) => tx.send(Ok((output, events))),
        // **#1232 §4**: `{:#}` walks the full anyhow context chain so the
        // bg-agent's narrative trace records the underlying cause, not
        // just the topmost `.context(...)` label. See the matching
        // comment in `tool_dispatch.rs`'s foreground branch — same fix,
        // same reasoning. Pre-fix the bg path collapsed multi-layer
        // errors to one line of useless top-level text.
        Err(e) => tx.send(Err((format!("Error: {e:#}"), events))),
    };
}

/// Parse the **required** `agent_name` field out of an `InvokeAgent`
/// argument object.
///
/// **#1232 §5**: extracted from `execute_sub_agent` so the missing-
/// field / wrong-type / empty-string / unknown-agent branches can be
/// unit-tested without spinning up the full sub-agent dispatch
/// plumbing. The schema declares `agent_name` required (see
/// `tools/agent.rs::definitions`) but schema compliance is best-
/// effort across LLMs. Pre-fix the field silently defaulted to
/// `"task"` (`unwrap_or("task")`) so every InvokeAgent call routed
/// to the generic worker even when the model's prompt was written
/// for a specialist ("Rust code architect", "security specialist",
/// etc. — see the bug-review session that opened the issue: 10/10
/// calls omitted the field).
///
/// On any failure the error message lists the available agents
/// (discovered via `tools::agent::discover_all_agents`) so the model
/// can self-correct on the next turn. Discovery is project-aware
/// (built-in → user → project precedence), so a project that adds
/// custom agents under `<root>/agents/` sees them in the hint too.
///
/// Accepts only non-empty strings. Missing, null, empty, wrong-type,
/// and unknown-agent cases all bail with actionable text.
pub(crate) fn parse_agent_name_required(
    args: &serde_json::Value,
    project_root: &std::path::Path,
) -> anyhow::Result<String> {
    let raw = match args.get("agent_name") {
        Some(serde_json::Value::String(s)) => s,
        Some(serde_json::Value::Null) | None => {
            anyhow::bail!(
                "InvokeAgent: 'agent_name' is required \u{2014} no default. {hint}",
                hint = available_agents_hint(project_root),
            );
        }
        Some(other) => {
            let kind = match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "a boolean",
                serde_json::Value::Number(_) => "a number",
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Object(_) => "an object",
                serde_json::Value::String(_) => unreachable!("matched above"),
            };
            anyhow::bail!(
                "InvokeAgent: 'agent_name' must be a string, got {kind}. {hint}",
                hint = available_agents_hint(project_root),
            );
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!(
            "InvokeAgent: 'agent_name' must be a non-empty string. {hint}",
            hint = available_agents_hint(project_root),
        );
    }
    Ok(trimmed.to_string())
}

/// Build the "Available agents: ..." suffix used by
/// `parse_agent_name_required`'s error messages.
///
/// Lists every discovered agent's name plus `fork` (the special
/// context-inheriting pseudo-agent that doesn't appear in discovery
/// because it isn't a real agent file). Sorted, comma-separated, so
/// the model gets a stable hint string it can copy-paste from.
fn available_agents_hint(project_root: &std::path::Path) -> String {
    let mut names: Vec<String> = crate::tools::agent::discover_all_agents(project_root)
        .into_iter()
        .map(|a| a.name)
        .collect();
    // `fork` is dispatched specially in `execute_sub_agent` and is
    // never a real agent on disk — surface it here so the model knows
    // it's a valid choice for context-inheriting work.
    names.push("fork".to_string());
    names.sort();
    names.dedup();
    if names.len() == 1 {
        // Should be unreachable in practice (built-ins always present)
        // but be defensive.
        format!("Available agent: {}.", names[0])
    } else {
        format!(
            "Available agents (call ListAgents for descriptions): {}.",
            names.join(", "),
        )
    }
}

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
///
/// Results are cached in `sub_agent_cache` keyed by `(agent_name, prompt_hash)`.
/// On cache hit, returns immediately without any LLM calls.
///
/// **#1265 item 4 PR-3**: takes the parent's [`ToolExecutionContext`]
/// instead of 11 individual ambient args. The parent's `tools` field
/// is intentionally unused — sub-agents construct their own
/// `sub_tools` registry from `sub_config.allowed_tools`.
#[tracing::instrument(skip_all, fields(agent_name, cached = false))]
// 8 args: 7 ambient (tx, arguments, cmd_rx, cache, sandbox, registry,
// session) + 1 private `inline_only` recursion guard added in #1163
// (Lean A) so `run_bg_agent` can drive the inference loop body without
// re-entering the spawn path. Bundling into a struct would just push
// the count from "function args" to "struct fields" without making
// any caller's life easier.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_sub_agent<'a>(
    parent_tx: ToolExecutionContext<'a>,
    arguments: &'a str,
    cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    // Phase 5 PR-4 of #934: parent's effective sandbox policy. The
    // child policy is composed onto this so the child can only narrow,
    // never widen — see [`koda_sandbox::SandboxPolicy::compose`] for
    // the per-field rules. Pass `&SandboxPolicy::strict_default()`
    // when there is no meaningful parent (top-level invocation).
    parent_sandbox_policy: &'a koda_sandbox::SandboxPolicy,
    // Layer 4 of #996 + #1076: live iteration heartbeat.  Pass the
    // bg-agent's `ChildStatusEmitter` so each loop iteration can push
    // `Running { iter }` to BOTH the registry's per-task watch
    // channel (`/agents`, status-bar pill) AND the engine event
    // queue (`EngineSink` → TUI / ACP / headless).  Foreground
    // sub-agents pass `None` — they have no status channel because
    // they're not tracked in the registry at all.
    emitter: Option<crate::child_agent::ChildStatusEmitter>,
    // **#1108 P2a**: parent's `InvokeAgent` tool_call_id. Recorded on
    // the bg-agent reservation so the inference loop's drain handler
    // can persist the bg agent's narrative trace to `session_events`
    // with this id as `parent_tool_call_id`. The transcript renderer
    // folds those rows under the parent's `InvokeAgent` tool result.
    // `None` for foreground sub-agents (their events flow inline
    // through the parent's `EngineSink` and don't need correlation).
    parent_tool_call_id: Option<&'a str>,
    // **#1163 (Lean A)**: internal-only marker. `false` is the public
    // dispatch shape — spawn the agent in the background and return
    // immediately with a task_id. `true` is the recursion-guard path
    // used by `run_bg_agent` when it needs to actually drive the
    // inference loop inside the spawned task; that path skips the
    // bg-spawn block and runs the loop inline.
    //
    // Pre-#1163 the public `background:bool` parameter on the
    // InvokeAgent tool served both roles: model-facing choice AND
    // recursion guard for `run_bg_agent`. #1163 deleted the model-
    // facing flag (sub-agents always spawn-and-return), so this
    // parameter is now strictly an internal implementation detail.
    inline_only: bool,
) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
    async move {
        // #1265 item 4 PR-3: rebind context fields to the same names
        // the body has used since pre-refactor, so the rest of this
        // 700-line function reads exactly as before. The parent's
        // `tools` registry is intentionally NOT bound — sub-agents
        // build their own from `sub_config.allowed_tools`.
        let crate::turn_context::TurnContext {
            project_root,
            config: parent_config,
            db,
            session_id: parent_session_id,
            sink,
            sub_agent_cache,
            bg_agents,
            mode,
            ..
        } = *parent_tx.turn;
        let cancel = parent_tx.turn.cancel.clone();
        let parent_spawner = parent_tx.caller_spawner;

        // Phase E of #996: allocate this invocation's id up-front. It
        // becomes the `caller_spawner` for every tool call inside
        // this sub-agent's loop, AND the `spawner` tag the cleanup
        // hook below uses to reap any orphaned bg work.
        let my_invocation_id = next_invocation_id();
        let args: serde_json::Value = serde_json::from_str(arguments)?;
        // **#1232 §5**: `agent_name` is a required field. Pre-fix the
        // dispatcher used `args["agent_name"].as_str().unwrap_or("task")`,
        // which silently routed every missing-field call to the
        // generic worker even when the model's prompt was written for
        // a specialist ("Rust code architect", "security specialist",
        // ...). The bug-review session that opened the issue showed
        // 10/10 InvokeAgent calls hit this path.
        //
        // Validation is extracted into a free function so the
        // missing / wrong-type / empty / bad-name branches can be
        // unit-tested without spinning up the full dispatch
        // plumbing. The error message lists available agents so the
        // model can self-correct on the next turn.
        let agent_name_owned = parse_agent_name_required(&args, project_root)?;
        let agent_name = agent_name_owned.as_str();
        tracing::Span::current().record("agent_name", agent_name);
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
        let is_fork = agent_name == "fork";

        // **#1163 (Lean A)**: emit `SubAgentStart` here — BEFORE the
        // bg-spawn early-return — so the parent's sink observes the
        // dispatch synchronously. Pre-#1163 this fired only on the
        // (now-deleted) inline foreground path, and the bg path leaked
        // through with just an `Info { "\u{1f680} ... launched in
        // background" }` line. ACP / headless clients (and the
        // e2e tests in `e2e_agent_test.rs`) rely on `SubAgentStart`
        // as the canonical "a sub-agent was just dispatched" signal,
        // independent of whether dispatch resolves inline (cache hit)
        // or by spawn. The `inline_only` recursion-guard path is the
        // only place this could double-emit, but that path's `sink`
        // is a `BufferingSink` inside `run_bg_agent`, so the duplicate
        // is silently captured and never reaches a user surface.
        sink.emit(EngineEvent::SubAgentStart {
            agent_name: agent_name.to_string(),
        });

        // **#1163 (Lean A)**: cache lookup MUST happen before the
        // bg-spawn block, not after. Pre-#1163 it lived in the (then
        // foreground-only) inline path, which meant a cache hit on a
        // background dispatch still spawned a `tokio::spawn` task,
        // ate a registry slot, and only short-circuited *inside* the
        // spawned task (where the cache-hit Info event vanished into
        // the BufferingSink). Hoisting it here means cache hits emit
        // their Info on the parent's real sink AND skip the spawn
        // entirely — the test fixture in `test_sub_agent_cache_hit_skips_llm`
        // directly asserts both behaviours. Cheap to retry idempotent
        // tasks; no provider hit, no worktree, no registry churn.
        if let Some(cached) = sub_agent_cache.get(agent_name, prompt) {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{26a1} {agent_name}: cache hit, skipping LLM call"),
            });
            tracing::Span::current().record("cached", true);
            return Ok(cached);
        }
        // **#1163 (Lean A)**: every InvokeAgent dispatch spawns a
        // background task and returns its task_id immediately. The only
        // skip-spawn path is the `inline_only` recursion guard used by
        // `run_bg_agent` to drive the inference loop inside the spawned
        // task itself.
        //
        // Phase 1 of #1022 fixes B1–B4 here:
        //  * **B1 trust:** the recursive `execute_sub_agent` call below
        //    receives `mode` (the parent's trust mode) instead of
        //    hard-coded `TrustMode::Auto`. The clamp inside that call
        //    then guarantees the bg agent can only narrow, never widen.
        //  * **B2 cancellation:** the bg task receives a `child_token()`
        //    of the parent's `cancel`. Ctrl-C in the parent loop now
        //    cascades into every in-flight bg agent.
        //  * **B3 lifecycle:** the spawned `JoinHandle` is held by the
        //    registry as an `AbortOnDropHandle`, so a registry drop
        //    aborts the task and releases its worktree.
        //  * **B4 sandbox:** `parent_sandbox_policy.clone()` is captured
        //    at spawn time, so the recursive call composes the bg
        //    agent's policy onto the parent's effective policy instead
        //    of regressing to `strict_default()`.
        //  * **B5 (Phase 2):** the bg agent now runs on the multi-thread
        //    runtime via `tokio::spawn`. We enforced `Send` on
        //    `execute_sub_agent`'s future by switching its signature to
        //    `fn(...) -> impl Future<Output = ...> + Send + 'a`, which
        //    forces the compiler to *prove* Send (vs. silently degrading
        //    when an `async fn` happens to capture a non-Send temporary).
        //    The transitive offender was `koda-sandbox::ipc::{read,write}_message`
        //    — those generic helpers had no `Send` bound on `R`/`W`/`T`,
        //    so MutexGuards held across their awaits weren't Send. Bounds
        //    have been added there as well.
        if !inline_only {
            // Phase E of #996: tag the bg-sub-agent task with the
            // **parent's** spawner identity. The parent (not the bg
            // sub-agent itself) owns the right to wait/cancel its
            // bg children via WaitTask/CancelTask. The bg sub-agent's
            // own invocation id (`my_invocation_id` allocated above)
            // is unused on this code path — it would matter if the
            // bg sub-agent itself were to be cancelled-on-parent-exit,
            // but the registry's parent->bg cancel-token cascade
            // already handles that case.
            // PR-A0.5 of #1232: pass `is_background: true` here —
            // this dispatch path is the bg-only spawn site (the
            // `tokio::spawn` + auto-drain mechanism). PR-A wires a
            // separate fg registration path that passes `false`.
            let reservation = bg_agents.reserve(&cancel, parent_spawner, true);
            let task_id = reservation.task_id;
            let bg_cancel = reservation.cancel.clone();
            let bg_tx = reservation.tx;
            let bg_rx = reservation.rx;
            let entry_cancel = reservation.cancel;
            // Layer 0 of #996 + #1076: bundle the watch sender, the
            // task id, the spawner id, and an `Arc` on the registry
            // into a `ChildStatusEmitter`.  The emitter fans out every
            // `.send(...)` to BOTH the per-task watch channel (read
            // by `snapshot()` / `/agents`) AND the registry's event
            // queue (drained by the inference loop and forwarded to
            // the active `EngineSink`).  This is what closes the
            // engine/UI boundary leak: the TUI no longer needs to
            // poll the registry directly to render live status, and
            // ACP / headless gain visibility for free.
            let emitter = crate::child_agent::ChildStatusEmitter::new(
                task_id,
                parent_spawner,
                // PR-A0.5 of #1232: bg path — emitter tags every
                // `ChildTaskUpdate` with `is_background: true`.
                true,
                reservation.status_tx,
                bg_agents.clone(),
            );
            let entry_status_rx = reservation.status_rx;

            let project_root_owned = project_root.to_path_buf();
            let parent_config_owned = parent_config.clone();
            let agent_name_owned = agent_name.to_string();
            let prompt_owned = prompt.to_string();
            let arguments_owned = arguments.to_string();
            let sub_agent_cache_owned = sub_agent_cache.clone();
            let parent_session_owned = parent_session_id.to_string();
            let bg_db = db.clone();
            let bg_policy = parent_sandbox_policy.clone();
            let bg_trust = mode;

            // #1325 Phase 4 (commit 2): build the child's `AgentPath`
            // BEFORE spawning so it can be moved into the bg task.
            // Same `parent` as inline (`parent_tx.turn.agent_path`)
            // so when nested spawn lands the path nesting falls out
            // for free. Failures here are programmer errors against
            // `child_agent_path`'s `"agent"` fallback — surface them
            // as the dispatch error rather than panicking inside the
            // spawned task.
            let bg_agent_path = child_agent_path(parent_tx.turn.agent_path, agent_name, task_id)?;

            // #1325 Phase 4 (commit 3): hand the parent's registry
            // (Arc) into the bg task so its inner `execute_sub_agent`
            // recursion can register the bg agent's own mailbox
            // exactly like the inline path does. `None` means the
            // parent is itself running without a session-attached
            // registry (test fixture); pass it through so the bg
            // path matches the inline fallback semantics.
            let bg_mailbox_registry = parent_tx.turn.tools.mailbox_registry();

            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{1f680} {agent_name} launched in background (task {task_id})"
                ),
            });

            // #1325 Phase 5a: clone the parent's path so the bg task
            // can send the completion mail to the parent's mailbox.
            let parent_path_for_bg = parent_tx.turn.agent_path.clone();

            let handle = tokio::spawn(run_bg_agent(
                project_root_owned,
                parent_config_owned,
                bg_db,
                arguments_owned,
                sub_agent_cache_owned,
                parent_session_owned,
                bg_tx,
                bg_cancel,
                bg_trust,
                bg_policy,
                emitter,
                bg_agent_path,
                bg_mailbox_registry,
                parent_path_for_bg,
            ));

            bg_agents.attach(
                task_id,
                &agent_name_owned,
                &prompt_owned,
                bg_rx,
                entry_cancel,
                entry_status_rx,
                parent_spawner,
                // PR-A0.5 of #1232: bg path — stamp the entry as
                // background so snapshots and the `/agents` overlay
                // can keep filtering bg-only when they want.
                true,
                parent_tool_call_id.map(str::to_string),
                handle,
            );

            return Ok(format!(
                "Background agent '{agent_name_owned}' started (agent:{task_id}). \
             Results will be injected when complete."
            ));
        }

        // From this point on, any bg work this invocation spawns is
        // tagged with `my_invocation_id`. Install the RAII cleanup
        // guard *now*, after the bg-spawn early-return — a bg-spawn
        // path's child is intentionally meant to outlive us; the
        // guard would (correctly!) reap it as an orphan, which is
        // exactly the behaviour we want to AVOID for the bg branch.
        let _cleanup = InvocationCleanup {
            bg: bg_agents,
            invocation_id: my_invocation_id,
        };
        // PR-A of #1232 §1: register the foreground sub-agent in the
        // shared `ChildAgentRegistry` so the `/agents` overlay can
        // render its row alongside any concurrent bg sub-agents.
        // Pre-PR-A foreground sub-agents emitted nested
        // `execute_one_tool` spans visible in tracing logs but had
        // ZERO presence in the live overlay — the user-visible
        // "sub-agent ran for 1009s with no progress signal"
        // complaint that opened #1232.
        //
        // The guard is held until function return (success, `?`-error,
        // or panic), at which point its `Drop` impl removes the entry.
        // RAII is critical: every error path below uses `?` to bubble,
        // so manual unregister would leak the registry slot on the
        // first failure (and the overlay would render a phantom row
        // forever).
        //
        // The cache-hit early-return above intentionally skips this
        // — a cache hit returns synchronously in microseconds, far
        // below the overlay's render cadence, so registering would
        // just flash a row that immediately disappears.
        let (fg_emitter, _fg_guard) = bg_agents.register_fg_with_emitter(
            agent_name,
            prompt,
            &cancel,
            parent_spawner,
            parent_tool_call_id.map(str::to_string),
        );
        // Push `Pending` immediately so the overlay row appears
        // before the LLM call — same shape as bg's `reserve()` +
        // initial `Pending` from the `watch::channel`. Sending it
        // explicitly (rather than relying on the channel's initial
        // value) keeps the bg/fg event streams symmetric for any
        // downstream consumer that reads the queue rather than the
        // watch.
        fg_emitter.send(crate::child_agent::AgentStatus::Pending);
        // Wrap the parent's sink so tool/info events fan out to the
        // overlay's `ChildAgentActivity` stream in addition to the
        // normal forwarded path. The wrapper borrows `sink` for the
        // duration of this scope; the original `sink` reference is
        // shadowed below so every existing emit site downstream
        // routes through the wrapper without code changes.
        let fg_sink = crate::engine::sink::FgForwardingSink::new(sink, fg_emitter.clone());
        let sink: &dyn crate::engine::EngineSink = &fg_sink;
        // Now that the registry entry exists, transition to
        // `Running { iter: 0 }` so the overlay shows the agent as
        // active rather than queued. The inference loop below
        // doesn't currently push per-iteration `Running` heartbeats
        // for fg (bg gets them via the registered emitter inside
        // `run_bg_agent`) — a follow-up could add iter ticks here,
        // but the `last activity` line driven by `ToolCallStart`
        // events already gives the user a moving signal.
        fg_emitter.send(crate::child_agent::AgentStatus::Running { iter: 0 });
        // Note on terminal status: we deliberately do NOT emit a
        // `Completed`/`Errored` `ChildTaskUpdate` on return. Reason:
        // the function has ~half a dozen `return Ok(...)` sites and
        // many `?`-bubbled errors; instrumenting each is fragile and
        // refactoring the inference body into an inner async fn just
        // to capture a Result was ruled out as scope creep. Instead
        // we lean on the `_fg_guard`'s `Drop` impl, which removes
        // the entry from the registry at the instant the function
        // returns. The overlay's `build_rows` prunes activity for
        // missing task ids on the very next frame — visually
        // identical to receiving a terminal event. ACP / headless
        // clients that subscribe to the raw `ChildTaskUpdate`
        // stream will see the event flow simply stop for that
        // task_id, which mirrors how a bg drain looks to them once
        // the registry entry is gone. Documented as a known
        // asymmetry in the PR body; can be tightened in a follow-up
        // if downstream consumers grow a hard dependency on the
        // terminal tag.

        sink.emit(EngineEvent::SubAgentStart {
            agent_name: agent_name.to_string(),
        });

        // Fork inherits parent config; named agents load their own persona
        // but fall back to the parent's provider and model for anything not
        // explicitly set in the agent JSON.
        //
        // Inheritance rules (applied only when the agent JSON leaves a field None):
        //
        // provider + base_url — inherited from parent when the agent JSON sets
        //   neither. If the agent sets its own provider or base_url (e.g. a
        //   test-only "mock" agent or a specialist routed to a different endpoint)
        //   we respect that and leave it alone.
        //
        // model — inherited from parent only when (a) the agent JSON left it
        //   unset AND (b) we are also inheriting the provider. Cross-provider
        //   model names are not portable ("gemini-2.0-flash" means nothing on
        //   Anthropic), so if the agent has its own provider we leave the model
        //   resolved from that provider's defaults.
        // #1135: per-agent turn cap. Computed alongside `sub_config`
        // because for non-fork agents we need the **raw** JSON to
        // distinguish "explicit max_iterations" from "defaulted to 200
        // in `KodaConfig::for_agent_invocation`". A tuple return keeps
        // both values in scope without a redundant disk read.
        let (sub_config, max_turns) = if is_fork {
            // Fork inherits the parent config verbatim, *except* for
            // trust — which must come from the **runtime** mode
            // (see `derive_child_trust` doc).
            //
            // **#1022 B17**: was `debug_assert!`, which is *compiled
            // out* in release builds. The fork-trust invariant is a
            // security-relevant property (a future change that
            // accidentally narrowed/widened trust between the clone
            // and use would silently ship). Promoted to `assert!`
            // — the runtime cost is a single enum equality check, so
            // there's no reason to weaken the guarantee for release.
            //
            // **#1022 B19**: pre-fix asserted `cfg.trust ==
            // parent_config.trust` after a clone, which checked the
            // wrong thing — `parent_config.trust` is the *startup*
            // value of the trust mode and ignores `/safe`/`/auto`
            // toggles. The actual invariant is "fork runs at the
            // parent's *runtime* trust", and the runtime mode is
            // `mode`. Now we explicitly write `cfg.trust` from
            // `derive_child_trust(mode, mode)` (= `mode`) and
            // assert against `mode`. The clone-then-overwrite
            // pattern is intentional: keeps the rest of
            // `parent_config` (model, base_url, system prompt
            // overrides, ...) verbatim while making the trust
            // derivation explicit and uniform with the named path.
            let mut cfg = parent_config.clone();
            cfg.trust = derive_child_trust(mode, mode);
            assert!(
                cfg.trust == mode,
                "fork must inherit parent's runtime trust mode exactly"
            );
            // Forks inherit the parent's cap (parent could be top-level
            // koda at 200 or another sub-agent at 30). We don't impose
            // the sub-agent default on a fork because the model is
            // continuing the parent's work, not starting a fresh task.
            let max_turns = cfg.max_iterations;
            (cfg, max_turns)
        } else {
            // Load the raw JSON first to see what the agent explicitly set.
            let raw = crate::config::KodaConfig::load_agent_json(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let mut cfg = crate::config::KodaConfig::load(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();

            if !agent_has_own_provider {
                // Inherit parent's provider, base_url, and (if unset) model.
                // All three travel together: model names are provider-scoped.
                let model_override = raw.model.is_none().then(|| parent_config.model.clone());
                cfg = cfg.with_overrides(
                    Some(parent_config.base_url.clone()),
                    model_override,
                    Some(parent_config.provider_type.to_string()),
                );
            }
            // else: agent opted into its own provider — use its resolved config
            // as-is. The agent JSON is responsible for any model it needs.

            // Inherit trust: child can never exceed parent's *runtime*
            // trust (#845, #1022 B19). Same pattern as Codex's
            // `apply_spawn_agent_runtime_overrides()` which copies the
            // parent's runtime sandbox_policy onto the child.
            //
            // **#1022 B19**: pre-fix used `parent_config.trust` as the
            // ceiling. That field is the *startup* trust value;
            // `cycle_trust`/`set_trust` mutate the SharedTrustMode
            // atomic but never the config field. So a user who
            // started in Auto and hit `/safe` would get sub-agents
            // clamped against the stale Auto, allowing the child to
            // run with broader privileges than the parent's *current*
            // mode. Real escalation. The runtime mode is `mode`,
            // threaded through `execute_one_tool` from the inference
            // loop — that's the only authoritative value. The helper
            // `derive_child_trust` exists to make this antipattern
            // greppable (see its doc).
            //
            // **#1246**: `cfg.trust` here is the agent's **declared**
            // mode, sourced from the optional `"trust"` field in agent
            // JSON (defaults to `Safe` for back-compat with pre-#1246
            // agent files). Built-in `explore` and `plan` agents
            // declare `"trust": "plan"` so they get kernel-enforced
            // read-only via the sandbox — strictly stronger than the
            // soft `disallowed_tools` gate (which a creative model
            // could try to bash around via `cat > foo`).
            //
            // **The parallel-fan-out invariant** (the architectural
            // payoff of #1246): two or more sub-agents that BOTH end
            // up with `effective == Plan` cannot mutate any file by
            // construction. So a parent can fan out N read-only
            // sub-agents simultaneously WITHOUT git worktrees, WITHOUT
            // copy-on-write filesystems, and WITHOUT any conflict-
            // resolution machinery — the absence of writes IS the
            // conflict-prevention. This property is load-bearing for
            // any future bg-dispatch scheduler that wants to widen
            // fan-out aggressively for `explore`/`plan`-style agents.
            // The pinning test for this invariant lives in
            // `config.rs::agent_json_trust_interacts_with_derive_child_trust_correctly`.
            let child_trust = cfg.trust;
            cfg.trust = derive_child_trust(mode, cfg.trust);
            if cfg.trust != child_trust {
                tracing::info!(
                    agent = agent_name,
                    parent = %mode,
                    child = %child_trust,
                    effective = %cfg.trust,
                    "sub-agent trust clamped to match parent",
                );
            }

            // #1135: read from the **raw** JSON so we can tell
            // "explicit `max_iterations: N`" apart from "defaulted
            // to 200 by `KodaConfig::for_agent_invocation`". Without
            // this distinction every sub-agent would inherit the
            // 200 default and the gemini-pattern fix would do
            // nothing for the agents that triggered #1135 (which
            // don't set the field).
            let max_turns = raw.max_iterations.unwrap_or(DEFAULT_SUB_AGENT_MAX_TURNS);
            (cfg, max_turns)
        };

        let sub_session = {
            let sid = db
                .create_session(&sub_config.agent_name, project_root)
                .await?;
            // Fork: copy parent conversation history into the new session.
            //
            // **#1022 B20**: was a per-row loop — N×(`insert_message`
            // + `mark_message_complete` for assistant rows), each
            // call its own fsync, on the synchronous fork hot path.
            // For a 200-message parent that's ~600 round-trips and
            // hundreds of ms of disk wait. Now a single transaction
            // via `copy_messages_into_session` (one fsync at COMMIT,
            // `completed_at` written inline for assistant rows).
            if is_fork {
                let parent_history = db.load_context(parent_session_id).await?;
                db.copy_messages_into_session(&sid, &parent_history).await?
            }
            sid
        };

        db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
            .await?;

        let provider = crate::providers::create_provider(&sub_config);
        // Select workspace provider. Write-capable agents get an isolated
        // workspace; read-only agents share the parent root for free.
        //
        // Per-platform write-isolation choice:
        //
        // - **macOS:** `ClonefileProvider` (APFS clonefile(2)) is
        //   preferred for its ~3-4× provision speedup over git worktree
        //   (Phase 4d / #934). Falls back to git worktree if construction
        //   fails (e.g. `$HOME` unset, project path can't canonicalize).
        // - **Linux + others:** `GitWorktreeProvider`. The Linux CoW
        //   equivalent (4e in #934) is deferred until production
        //   telemetry shows it's worth building.
        //
        // **Documented platform divergence** — see `docs/src/sandbox.md`
        // → "Workspace providers". Both backends provide the same
        // isolation guarantees; only provision speed differs.
        let has_write_tools = !sub_config
            .disallowed_tools
            .iter()
            .any(|t| t == "Write" || t == "Edit");
        // Phase 1 of #1022: explicit `+ Send + Sync` on the trait object.
        // The supertrait bound `WorkspaceProvider: Send + Sync` constrains
        // *implementors*, but Rust trait objects don't auto-inherit those
        // bounds — `Box<dyn WorkspaceProvider>` is `!Send` without the
        // explicit annotation, which makes the whole `execute_sub_agent`
        // future `!Send` and unspawnable.
        let workspace: Box<dyn WorkspaceProvider + Send + Sync> = if has_write_tools {
            pick_write_provider(project_root, agent_name)
        } else {
            Box::new(CwdProvider::new(project_root))
        };
        let effective_root = match workspace.provision(&sub_session).await {
            Ok(path) => {
                if path != project_root {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f333} {agent_name}: isolated in worktree"),
                    });
                }
                path
            }
            Err(e) if has_write_tools => {
                // **#1022 B21**: pre-fix this branch silently fell
                // back to `project_root.to_path_buf()`. With write
                // tools requested, that drops the sub-agent into
                // the parent's unisolated working tree — two
                // parallel sub-agents would race on the same
                // files, exactly the corruption mode the workspace
                // provider exists to prevent. Worse, the only
                // signal was a `tracing::warn!` invisible to most
                // headless / TUI runs.
                //
                // Now: short-circuit with a structural-failure
                // marker (same `[ERROR:` shape as the iteration-cap
                // marker from B18) so the parent agent sees the
                // failure as a sub-agent result and can adapt
                // (retry without write tools, do the work itself,
                // surface to the user). Also cache it so a
                // verbatim re-dispatch with the same prompt
                // doesn't pay the failed-provision cost twice —
                // mirrors the iteration-cap caching policy.
                let reason = e.to_string();
                tracing::warn!("Workspace provision failed for sub-agent '{agent_name}': {reason}");
                sink.emit(EngineEvent::Info {
                    message: format!(
                        "  \u{26a0}\u{fe0f}  {agent_name}: workspace isolation failed, not dispatching ({reason})"
                    ),
                });
                let marker = workspace_provision_failure_marker(agent_name, &reason);
                sub_agent_cache.put(agent_name, prompt, &marker);
                return Ok(marker);
            }
            Err(e) => {
                // Read-only sub-agent (no write tools): isolation
                // wasn't requested, so falling back to project_root
                // is the *intended* behavior — there's no
                // race-on-files corruption mode without write
                // tools. Today this branch is unreachable because
                // `CwdProvider::provision` is infallible, but the
                // explicit arm documents intent and survives a
                // future read-only provider that *can* fail.
                tracing::warn!(
                    "Workspace provision failed (read-only sub-agent '{agent_name}'): {e}"
                );
                project_root.to_path_buf()
            }
        };
        let effective_root_ref = effective_root.as_path();

        let tools = {
            let registry = ToolRegistry::with_trust(
                effective_root.clone(),
                sub_config.max_context_tokens,
                sub_config.trust,
            );
            // Phase 5 PR-4 of #934: compose the parent's effective policy
            // with the child's. Per-field rules in [`SandboxPolicy::compose`]
            // (denies union, allows parent-wins, limits min, trust strictest)
            // ensure the child can never widen the parent's surface — only
            // narrow it. PR-2 installed the child policy verbatim; PR-4
            // makes the install additive over the parent so chains of
            // sub-agents accumulate restrictions monotonically.
            let composed_policy = crate::sandbox::compose_child_policy(
                parent_sandbox_policy,
                sub_config.trust,
                effective_root_ref,
            );
            let registry = registry.with_sandbox_policy(composed_policy);
            match parent_cache {
                Some(cache) => registry.with_shared_cache(cache),
                None => registry,
            }
        };
        let tool_defs = {
            let mut denied = sub_config.disallowed_tools.clone();
            // #1022 B7 (revised): sub-agents cannot spawn sub-agents.
            // Period. Originally only `is_fork` blocked `InvokeAgent`,
            // but that left a sharp edge: named sub-agents could call
            // it, the call fell through to a registry stub returning
            // `"InvokeAgent is handled by the inference loop."` with
            // `success=false`, and the model would hallucinate around
            // the bogus error.
            //
            // Allowing real recursion was the alternative considered,
            // but it requires a depth cap (~hundreds of KB of `async
            // fn` state per level), `Box::pin` on a mutually-recursive
            // future, threading `depth: u32` through five functions,
            // and the resulting design has no use case worth the
            // surface area. Codex matches this stance — their
            // sub-agents can't spawn sub-agents either. The master
            // agent at depth 0 can fire as many parallel/background
            // workers as it wants; workers complete their task and
            // report back. Flat by design.
            //
            // Filtering at the tool-def level keeps the model from
            // ever seeing the tool. The sub-agent dispatch loop also
            // contains a defense-in-depth refusal in case a rogue or
            // scripted model emits `InvokeAgent` regardless.
            if !denied.contains(&"InvokeAgent".to_string()) {
                denied.push("InvokeAgent".to_string());
            }
            // #1022 B8: AskUser requires a live `cmd_rx` connected to the
            // user. Sub-agents have a detached channel (foreground sub-agents
            // get `&mut mpsc::channel(1).1` from the parent dispatch path,
            // bg agents get an even more detached one). Filter the tool out
            // entirely so the model never tries to call it. Without this
            // filter the call falls through to the registry stub and the
            // sub-agent gets `"AskUser is handled by the inference loop."`
            // back as a tool result — which the model then dutifully
            // hallucinates around.
            if !denied.contains(&"AskUser".to_string()) {
                denied.push("AskUser".to_string());
            }
            // Phase E of #996 wired `caller_spawner` through the
            // dispatch layer, so the bg-task tools (ListBackgroundTasks /
            // CancelTask / WaitTask) now correctly scope to the calling
            // sub-agent's own invocation id — a sub-agent only sees
            // bg work *it* spawned. The earlier blanket denylist that
            // hid these tools from sub-agents has been removed.
            tools.get_definitions(&sub_config.allowed_tools, &denied)
        };
        let semantic_memory = if sub_config.skip_memory {
            String::new()
        } else {
            memory::load(project_root)?
        };
        let env = crate::prompt::EnvironmentInfo {
            project_root: effective_root_ref,
            model: &sub_config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = build_system_prompt(
            &sub_config.system_prompt,
            &semantic_memory,
            &env,
            &[], // sub-agents have no REPL commands
            &tools.skill_registry,
        );

        // #1265 item 4 PR-3: build the sub-agent's own dispatch
        // context once. Used by both `execute_one_tool` callsites in
        // the per-iteration loop below — PR-2 had to construct this
        // inline twice with identical args. Hoisted here because
        // `sub_config`, `sub_session`, and `tools` are all bound by
        // this point and don't change across iterations.
        // #1325 Phase 4 (commit 2): build this invocation's path
        // BEFORE constructing the sub-agent's `TurnContext`.
        // `my_invocation_id` (allocated near the top of
        // `execute_sub_agent`) is the unique-per-invocation handle
        // — perfect for path uniquification when multiple inline
        // `InvokeAgent`s of the same agent name run concurrently
        // (Phase 4e's multi-element form).
        let sub_agent_path =
            child_agent_path(parent_tx.turn.agent_path, agent_name, my_invocation_id)?;

        // #1325 Phase 4 (commit 3): give the sub-agent its own
        // mailbox in the shared registry, install the registry on
        // its tools, and keep the receiver around so we can drain
        // it at the top of each iter.
        //
        // `parent_registry == None` means this dispatch path is
        // running without a session (test fixture / no-session
        // context). Fall back to pre-Phase-4 behaviour: no per-
        // child mailbox, no peer-tool wiring inside the child. The
        // child still functions; it just can't `SendMessage` /
        // `WaitForMail` (which is the same as Phase 1-2).
        //
        // The guard MUST live until the end of the `async move` so
        // the registry entry survives every iter. Binding it as
        // `_sub_mailbox_guard` (with a leading underscore) silences
        // the unused-variable warning while still extending the
        // lifetime to the enclosing scope. `let _ = ...;` would
        // drop it immediately; `let _x = ...;` keeps it alive.
        let parent_registry = parent_tx.turn.tools.mailbox_registry();
        let (mut sub_mailbox_rx, _sub_mailbox_guard) = match parent_registry.as_ref() {
            Some(reg) => match register_sub_agent_mailbox(reg, &sub_agent_path) {
                Some((rx, guard)) => (Some(rx), Some(guard)),
                None => (None, None),
            },
            None => (None, None),
        };
        if let Some(reg) = parent_registry.as_ref() {
            // Install the SHARED registry on the sub-agent's tools.
            // `set_mailbox_registry` takes `&self` (interior
            // mutability) so the immutable `tools` binding is fine.
            tools.set_mailbox_registry(std::sync::Arc::clone(reg));
        }

        let sub_turn = crate::turn_context::TurnContext::new(
            project_root,
            &sub_config,
            db,
            &sub_session,
            sink,
            cancel.clone(),
            sub_agent_cache,
            bg_agents,
            mode,
            &tools,
            &sub_agent_path,
        );
        let sub_tx =
            crate::turn_context::ToolExecutionContext::new(&sub_turn, Some(my_invocation_id));

        // #1135: gemini-pattern grace turn. When `iter > max_turns` we
        // append a system reminder to the message list ("you have one
        // final chance — give your best answer NOW, no more tools")
        // and run exactly one more turn. Whatever the model returns on
        // that turn is the final answer (tool calls in the grace
        // response are ignored, not dispatched). Tracked here rather
        // than via `iter`-arithmetic alone because the loop body must
        // know which iteration was "the grace one" to decide whether
        // to dispatch tools or short-circuit to a return.
        //
        // Mirrors gemini-cli's `executeFinalWarningTurn` in
        // `local-executor.ts:436`.
        let mut grace_turn_done = false;

        // #1232 §3a: pre-flight context-budget check.
        //
        // Estimate `system_prompt + tool_defs + user_prompt` size against
        // the resolved `max_context_tokens` for this sub-agent. Bail with
        // an actionable breakdown when over budget instead of letting the
        // user see a raw `400 "Context size has been exceeded"` from
        // upstream (the actual UX in the bug-review session that opened
        // #1232: 4/10 sub-agents failed this way with no usable hint).
        //
        // The estimate is heuristic and conservative — it doesn't account
        // for the response budget or any future tool-call traffic, but it
        // catches the common "baseline already exceeds the window"
        // failure mode that drove the issue. Subsequent in-loop estimates
        // (`estimate_tokens(&messages)` further down) handle the
        // grow-during-the-conversation case.
        let preflight = crate::inference_helpers::estimate_subagent_preflight(
            &system_prompt,
            &tool_defs,
            prompt,
            sub_config.max_context_tokens,
        );
        tracing::debug!(
            agent = agent_name,
            preflight = %preflight.summary(),
            "sub-agent context pre-flight"
        );
        if preflight.is_over_budget() {
            // Surface to the inference loop's normal narrative so the
            // master TUI / ACP / headless clients all see the same
            // structured signal alongside the bubbled error.
            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{1f6d1} {agent_name}: context pre-flight failed ({})",
                    preflight.summary()
                ),
            });
            // Best-effort cleanup mirrors the cancel path below — release
            // any provisioned workspace before bubbling so we don't leak
            // a tempdir on every over-budget sub-agent invocation.
            let _ = workspace.release(&sub_session, &effective_root).await;
            anyhow::bail!(
                "Sub-agent '{agent_name}' context exceeds model window: {summary}. \
                 Reduce the prompt, drop tools (set `disallowed_tools` on the agent), \
                 or pick a model with a larger context window.",
                summary = preflight.summary(),
            );
        }

        for iter in 1u32.. {
            // #1325 Phase 4 (commit 3): drain any peer-agent mail
            // that arrived since the last iter into the sub-agent's
            // DB session BEFORE `db.load_context` reads the
            // conversation. Mirrors `KodaSession::drain_mail_to_db`
            // for the root session. No-op when no mailbox is wired
            // (`sub_mailbox_rx` is `None`) or no mail has arrived.
            //
            // Drain failure is fatal for the iter — a partial drain
            // would silently lose mail, and the alternative
            // (dropping mail on error) is worse than bailing the
            // sub-agent.
            if let Some(rx) = sub_mailbox_rx.as_mut() {
                drain_sub_mailbox_into_session(rx, db, &sub_session).await?;
            }

            // #1110: sub-agents have no hardcoded iteration cap. Termination
            // is driven by the model (clean stop, no tool calls), `LoopDetector`
            // (consecutive identical calls -> feedback -> hard stop), parent
            // cancellation, or context exhaustion. Per DESIGN.md P3:
            // "Let the model drive [...] don't scaffold around weakness."
            //
            // Layer 4 of #996 + #1076: push the live iteration counter
            // so `/agents`, the status-bar pill, AND ACP / headless /
            // TUI clients all reflect real progress instead of the
            // Layer-0 placeholder `iter: 0` that `run_bg_agent` sends
            // on entry.  Fan-out failures inside the emitter are
            // silently absorbed for the same reason as the terminal
            // sends: if the receiver is gone, the user can't see the
            // update and we don't want noise.
            if let Some(ref e) = emitter {
                e.send(crate::child_agent::AgentStatus::Running { iter });
            }
            // Respect parent cancellation (#286)
            if cancel.is_cancelled() {
                // Release workspace on cancellation (best-effort, no user hint).
                let _ = workspace.release(&sub_session, &effective_root).await;
                return Ok("[cancelled by parent]".to_string());
            }
            let history = db.load_context(&sub_session).await?;
            let mut messages = vec![ChatMessage::text("system", &system_prompt)];
            for msg in &history {
                let tool_calls: Option<Vec<ToolCall>> = msg
                    .tool_calls
                    .as_deref()
                    .and_then(|tc| serde_json::from_str(tc).ok());
                messages.push(ChatMessage {
                    role: msg.role.as_str().to_string(),
                    content: msg.content.clone(),
                    tool_calls,
                    tool_call_id: msg.tool_call_id.clone(),
                    images: None,
                });
            }

            sink.emit(EngineEvent::SpinnerStart {
                message: format!("  \u{1f9a5} {agent_name} thinking..."),
            });

            // #1135: append the grace-turn reminder when this iteration
            // is the one that exceeded the cap. The reminder is stuffed
            // into the OUTGOING `messages` only — it is not persisted
            // to the DB, so a future replay won't see it twice. The
            // strict "text only, no tools" framing matches gemini's
            // wording (`local-executor.ts::getFinalWarningMessage`).
            if iter > max_turns && !grace_turn_done {
                grace_turn_done = true;
                let warning = format!(
                    "You have reached the maximum number of turns ({max_turns}) for this \
                     sub-agent invocation. You have ONE final chance to complete the task. \
                     You MUST respond with your best answer NOW as plain text. DO NOT call \
                     any more tools \u{2014} any tool calls in this response will be ignored. \
                     Summarize what you found, what you would do next if you had more turns, \
                     and explain that your investigation was interrupted by the budget."
                );
                messages.push(ChatMessage::text("system", &warning));
                tracing::warn!(
                    agent = agent_name,
                    iter,
                    max_turns,
                    "sub-agent reached max_turns; running grace turn"
                );
                sink.emit(EngineEvent::Info {
                    message: format!(
                        "  \u{23f3} {agent_name}: reached max turns ({max_turns}); requesting final summary"
                    ),
                });
            }

            let response = provider
                .chat(&messages, &tool_defs, &sub_config.model_settings)
                .await?;
            sink.emit(EngineEvent::SpinnerStop);

            let tool_calls_json = if response.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&response.tool_calls)?)
            };

            // **koda#1101**: capture msg_id and mark the assistant
            // message complete. Pre-fix this call discarded the row
            // ID and never marked complete, so `load_context` (which
            // filters out `(role = 'assistant' AND completed_at IS
            // NULL)`) dropped every sub-agent assistant turn from
            // the next iteration's history. The orphan tool-result
            // rows then got pruned by `prune_mismatched_tool_calls`,
            // leaving the sub-agent with `[system, user]` only — it
            // re-issued the same tool call every iteration until the
            // cap. Mirrors the parent inference loop's pattern at
            // `inference.rs::mark_message_complete`. The contract is
            // pinned by `db::tests::load_context_excludes_incomplete_assistant_messages`.
            let assistant_msg_id = db
                .insert_message(
                    &sub_session,
                    &Role::Assistant,
                    response.content.as_deref(),
                    tool_calls_json.as_deref(),
                    None,
                    Some(&response.usage),
                )
                .await?;
            db.mark_message_complete(assistant_msg_id).await?;

            // #1135: if this iteration was the grace turn, return
            // immediately regardless of whether the model called tools.
            // The grace contract is exactly one extra LLM call —
            // dispatching its tool calls would defeat the budget.
            // Falls through to the normal empty-tool-calls return path
            // when the model complied (typical case after the strong
            // "text only" framing); short-circuits with a marker if the
            // model defied the instruction and tried to keep tooling.
            if grace_turn_done {
                let result = match response.content.as_deref() {
                    Some(text) if !text.trim().is_empty() => text.to_string(),
                    _ => format!(
                        "[max_turns reached: agent exceeded the {max_turns}-turn budget \
                         and did not produce a final summary on its grace turn. \
                         {n} pending tool call(s) were dropped.]",
                        n = response.tool_calls.len(),
                    ),
                };
                sub_agent_cache.put(agent_name, prompt, &result);
                if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f335} {agent_name}: {hint}"),
                    });
                }
                return Ok(result);
            }

            if response.tool_calls.is_empty() {
                let result = response
                    .content
                    .unwrap_or_else(|| "(no output)".to_string());
                // Cache the result for future identical calls
                sub_agent_cache.put(agent_name, prompt, &result);
                // Release workspace; surface branch hint if agent left changes.
                if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f335} {agent_name}: {hint}"),
                    });
                }
                return Ok(result);
            }

            for tc in &response.tool_calls {
                sink.emit(EngineEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.function_name.clone(),
                    args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                    is_sub_agent: true,
                });

                // Sub-agents inherit the parent's approval mode
                let parsed_args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_default();

                // #1022 B7 (revised): defense-in-depth refusal of
                // `InvokeAgent` and `AskUser`. Both are filtered from
                // the sub-agent's `tool_defs` above, so a well-behaved
                // model never emits them. A misbehaving or scripted
                // model still might — short-circuit here with a clear
                // refusal message instead of falling through to
                // `execute_one_tool` (which would happily recurse for
                // InvokeAgent) or the registry stub (which returns
                // confusing `success=false` boilerplate).
                if tc.function_name == "InvokeAgent" {
                    let refusal = "InvokeAgent is not available inside a sub-agent. \
                                   Sub-agents are autonomous workers and cannot spawn \
                                   further sub-agents. Complete the task directly with \
                                   the tools you have, or report back what additional \
                                   dispatch the parent agent should perform.";
                    db.insert_message(
                        &sub_session,
                        &Role::Tool,
                        Some(refusal),
                        None,
                        Some(&tc.id),
                        None,
                    )
                    .await?;
                    continue;
                }
                if tc.function_name == "AskUser" {
                    let refusal = "AskUser is not available inside a sub-agent. \
                                   Sub-agents have no channel to the user; the parent \
                                   agent gathers any required input before delegating. \
                                   Proceed with the information you already have or \
                                   report what's missing.";
                    db.insert_message(
                        &sub_session,
                        &Role::Tool,
                        Some(refusal),
                        None,
                        Some(&tc.id),
                        None,
                    )
                    .await?;
                    continue;
                }

                // #1022 B14: pre-flight validation — catch obvious errors
                // (missing path, bad regex, file-cache violations) before
                // we burn an approval prompt or execute. The top-level
                // sequential dispatcher does the same; without this the
                // sub-agent would hit the same class of errors *after*
                // the user had already approved.
                let validation_error = tools::validate::validate_with_registry(
                    &tools,
                    &tc.function_name,
                    &parsed_args,
                    effective_root_ref,
                )
                .await;

                let output = if let Some(error) = validation_error {
                    format!("Validation error: {error}")
                } else {
                    // #1250: sub-agent context. The dead approval channel
                    // (line 173) means `NeedsConfirmation` is meaningless
                    // here. `check_tool_for_sub_agent` resolves it per the
                    // safe-side rule: mutating ops auto-approve (the agent
                    // was invoked to do work), destructive ops block.
                    let approval = trust::check_tool_for_sub_agent(
                        &tc.function_name,
                        &parsed_args,
                        mode,
                        Some(effective_root_ref),
                    );

                    match approval {
                        ToolApproval::AutoApprove => {
                            // #1022 B6 + B7: route through `execute_one_tool`
                            // (instead of calling `tools.execute()` directly)
                            // so that:
                            //   - mutating tool calls invalidate the
                            //     `SubAgentCache` (B6) — otherwise an
                            //     identical follow-up `InvokeAgent` returns
                            //     a stale cached result.
                            //   - nested `InvokeAgent` from inside this
                            //     sub-agent dispatches recursively into
                            //     `execute_sub_agent` (B7), instead of
                            //     hitting the registry stub that returns
                            //     "InvokeAgent is handled by the inference
                            //     loop." with success=false.
                            //   - Bash output streams through the parent
                            //     sink (free visibility win).
                            let (_id, result, _success, _full) = execute_one_tool(tc, sub_tx).await;
                            result
                        }
                        ToolApproval::Blocked => {
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
                            let diff_preview = preview::compute(
                                &tc.function_name,
                                &parsed_args,
                                effective_root_ref,
                            )
                            .await;
                            sink.emit(EngineEvent::ActionBlocked {
                                tool_name: tc.function_name.clone(),
                                detail,
                                preview: diff_preview,
                            });
                            "[safe mode] Action blocked.".to_string()
                        }
                        ToolApproval::NeedsConfirmation => {
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
                            let diff_preview = preview::compute(
                                &tc.function_name,
                                &parsed_args,
                                effective_root_ref,
                            )
                            .await;
                            let effect = crate::trust::resolve_tool_effect_with_registry(
                                &tc.function_name,
                                &parsed_args,
                                &tools,
                            );
                            match request_approval(
                                sink,
                                cmd_rx,
                                &cancel,
                                &tc.function_name,
                                &detail,
                                diff_preview,
                                effect,
                            )
                            .await
                            {
                                Some(ApprovalDecision::Approve) => {
                                    let (_id, result, _success, _full) =
                                        execute_one_tool(tc, sub_tx).await;
                                    result
                                }
                                Some(ApprovalDecision::Reject) => "[rejected by user]".to_string(),
                                Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                                    format!("[rejected: {feedback}]")
                                }
                                Some(ApprovalDecision::RejectAuto { reason }) => {
                                    // #1022 B15: same shape as the existing
                                    // sub-agent auto-reject below, so the model
                                    // sees a uniform "no human, here's why"
                                    // signal regardless of whether the auto-
                                    // rejection came from headless policy or
                                    // from a closed approval channel.
                                    format!("[auto-rejected: {reason}]")
                                }
                                None => {
                                    // #1022 B10: `request_approval` returns `None`
                                    // when the command channel is closed (sub-agents
                                    // don't have a live channel to the user) or
                                    // cancelled. Distinguish the two so the model
                                    // gets actionable signal instead of a generic
                                    // "[cancelled]" that looked like the user
                                    // hit Ctrl+C.
                                    if cancel.is_cancelled() {
                                        "[cancelled]".to_string()
                                    } else {
                                        format!(
                                            "[auto-rejected: '{tool}' requires user \
                                             confirmation but this sub-agent has no \
                                             channel to the user. The parent agent \
                                             must pre-approve destructive operations \
                                             or run the tool itself.]",
                                            tool = tc.function_name,
                                        )
                                    }
                                }
                            }
                        }
                    }
                };

                db.insert_message(
                    &sub_session,
                    &Role::Tool,
                    Some(&output),
                    None,
                    Some(&tc.id),
                    None,
                )
                .await?;
            }
        }
        // #1110 + #1135: unreachable in practice. The `for iter in 1u32..`
        // range is unbounded; exits are:
        //   - `return Ok(content)` on a clean (no-tool-calls) response,
        //   - `return Ok(grace_summary)` after the #1135 grace turn,
        //   - `return Ok("[cancelled by parent]")` on parent cancellation,
        //   - `return Err(...)` on a provider/persistence failure.
        // The grace-turn path guarantees termination within `max_turns + 1`
        // iterations even when the model refuses to stop on its own — the
        // class of bug from #1135.
        unreachable!("sub-agent loop exits via return; the iteration range is unbounded");
    }
}

// ── Workspace provider selection ────────────────────────────────────────────
//
// Two cfg-gated definitions of `pick_write_provider` rather than
// inline `cfg!()` branches because:
//
//  * `ClonefileProvider` is itself `cfg(target_os = "macos")` in
//    koda-sandbox; inline branches would still need cfg gating to
//    avoid "unresolved import" on Linux.
//  * Each platform's selection logic is small but distinct (macOS
//    has a fallback path, Linux doesn't), and side-by-side cfg
//    bodies read more honestly than a tangled inline form.
//
// Behavior is documented for users in `docs/src/sandbox.md`
// → "Workspace providers".

#[cfg(target_os = "macos")]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Try `ClonefileProvider` first — its 3-4× provision speedup
    // (Phase 4d / #934 bench) is durable and the implementation is
    // a thin wrapper over the OS primitive designed for this.
    //
    // Fall back to `GitWorktreeProvider` only if construction itself
    // fails (e.g. `$HOME` unset, project path can't canonicalize).
    // Runtime `clonefile(2)` failures (non-APFS volume etc.) surface
    // through the existing `provision()` error path — **#1022 B21**
    // makes that path short-circuit with a structural-failure marker
    // instead of silently dropping the sub-agent into the parent's
    // unisolated working tree (which would let parallel sub-agents
    // race on the same files).
    match ClonefileProvider::new(project_root) {
        Ok(p) => Box::new(p),
        Err(e) => {
            tracing::warn!("ClonefileProvider unavailable, falling back to git worktree: {e}");
            Box::new(GitWorktreeProvider::new(project_root, agent_name))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Linux + others: GitWorktreeProvider. The Linux CoW equivalent
    // (4e in #934) is parked until production telemetry shows it's
    // worth building — see #934 deferral comments.
    Box::new(GitWorktreeProvider::new(project_root, agent_name))
}

/// isolated sub-agent's workspace provider can't provision.
///
/// Replaces the pre-fix silent fallback to `project_root` — which
/// dropped the sub-agent into the parent's unisolated working
/// tree and let parallel sub-agents race on the same files.
///
/// Same `[ERROR:` prefix convention as other structural-failure
/// markers so the model treats it as failure metadata rather
/// than a sub-agent answer. Includes the failure reason so the
/// parent can adapt (e.g. switch to a read-only sub-agent if
/// the underlying issue is non-APFS volume / no git repo).
fn workspace_provision_failure_marker(agent_name: &str, reason: &str) -> String {
    format!(
        "[ERROR: sub-agent '{agent_name}' could not provision an isolated workspace and was not \
         dispatched, to avoid corrupting the parent project tree (reason: {reason}). Either \
         resolve the workspace setup issue, retry without write tools, or attempt the work \
         directly without delegating.]"
    )
}

#[cfg(test)]
mod child_agent_path_tests {
    //! **#1325 Phase 4 (commit 2)** regression tests for
    //! [`super::child_agent_path`].
    //!
    //! Helper is reachable via two spawn sites buried in 1100+ lines
    //! of `execute_sub_agent`; we'd rather pin its contract here than
    //! through a giant integration test of either site.

    use super::child_agent_path;
    use crate::agent::AgentPath;

    #[test]
    fn passthrough_for_already_valid_lowercase_name() {
        let p = child_agent_path(&AgentPath::root(), "explore", 42).unwrap();
        assert_eq!(p.as_str(), "/root/explore_42");
    }

    #[test]
    fn keeps_underscores_inside_name() {
        let p = child_agent_path(&AgentPath::root(), "code_reviewer", 7).unwrap();
        assert_eq!(p.as_str(), "/root/code_reviewer_7");
    }

    #[test]
    fn lowercases_uppercase_letters() {
        let p = child_agent_path(&AgentPath::root(), "CodeReviewer", 1).unwrap();
        assert_eq!(p.as_str(), "/root/codereviewer_1");
    }

    #[test]
    fn replaces_punctuation_with_underscore_and_collapses_runs() {
        let p = child_agent_path(&AgentPath::root(), "a.b/c-d", 9).unwrap();
        assert_eq!(p.as_str(), "/root/a_b_c_d_9");
    }

    #[test]
    fn collapses_doubled_underscores_inside_name() {
        // "a..b" -> two punctuation chars become two underscores; we
        // collapse to one so the segment stays readable.
        let p = child_agent_path(&AgentPath::root(), "a..b", 3).unwrap();
        assert_eq!(p.as_str(), "/root/a_b_3");
    }

    #[test]
    fn trims_leading_and_trailing_underscores_from_sanitized_name() {
        let p = child_agent_path(&AgentPath::root(), "-explore-", 5).unwrap();
        assert_eq!(p.as_str(), "/root/explore_5");
    }

    #[test]
    fn falls_back_to_agent_when_name_sanitizes_to_empty() {
        // All-punctuation name. `parse_agent_name_required` only
        // checks non-empty, so this is reachable from a real call.
        let p = child_agent_path(&AgentPath::root(), "!!!", 12).unwrap();
        assert_eq!(p.as_str(), "/root/agent_12");
    }

    #[test]
    fn unicode_replaced_with_underscore() {
        let p = child_agent_path(&AgentPath::root(), "\u{1F436}explore", 4).unwrap();
        assert_eq!(p.as_str(), "/root/explore_4");
    }

    #[test]
    fn nests_under_non_root_parent() {
        // Future-proofing: when nested spawn lands, the same helper
        // builds `/root/explore_4/researcher_7` from a parent path
        // of `/root/explore_4`. Pinning today so the property
        // doesn't silently regress.
        let parent = AgentPath::root().join("explore_4").unwrap();
        let p = child_agent_path(&parent, "researcher", 7).unwrap();
        assert_eq!(p.as_str(), "/root/explore_4/researcher_7");
    }

    #[test]
    fn distinct_unique_ids_produce_distinct_paths() {
        // Phase 4e fan-out invariant: two parallel `InvokeAgent`s of
        // the same agent name must end up with different paths so
        // their mailbox identities don't alias.
        let a = child_agent_path(&AgentPath::root(), "explore", 1).unwrap();
        let b = child_agent_path(&AgentPath::root(), "explore", 2).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn digits_in_agent_name_are_preserved() {
        // Numeric suffixes are valid `[a-z0-9_]` chars — keep them.
        let p = child_agent_path(&AgentPath::root(), "agent42", 99).unwrap();
        assert_eq!(p.as_str(), "/root/agent42_99");
    }
}

#[cfg(test)]
mod sub_agent_mailbox_tests {
    //! **#1325 Phase 4 (commit 3)** regression tests for the per-
    //! sub-agent mailbox lifecycle helpers ([`super::register_sub_agent_mailbox`],
    //! [`super::MailboxRegistration`], [`super::drain_sub_mailbox_into_session`]).
    //!
    //! Helpers are wired into `execute_sub_agent`, which is
    //! a 1100+ line `async move` deep behind workspace provisioning,
    //! provider mocking, and sandbox composition. Pure-fn /
    //! pure-RAII tests here pin the contract that the wiring
    //! depends on; the wiring itself is exercised by the existing
    //! sub-agent integration tests.

    use super::{MailboxRegistration, drain_sub_mailbox_into_session, register_sub_agent_mailbox};
    use crate::agent::{AgentPath, InterAgentCommunication, MailboxRegistry};
    use crate::db::{Database, Role};
    use crate::persistence::Persistence;
    use std::sync::Arc;

    fn sample_mail(content: &str) -> InterAgentCommunication {
        InterAgentCommunication {
            author: AgentPath::root(),
            recipient: AgentPath::root().join("explore_42").unwrap(),
            other_recipients: Vec::new(),
            content: content.to_string(),
            trigger_turn: true,
        }
    }

    #[test]
    fn register_returns_some_for_fresh_path() {
        // Pin: a path that's never been registered yields a
        // receiver + guard. Without this, the sub-agent silently
        // falls back to root — the regression we're fixing.
        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let result = register_sub_agent_mailbox(&reg, &path);
        assert!(result.is_some(), "fresh path must register");
        assert_eq!(reg.len(), 1, "registry must contain the new entry");
    }

    #[test]
    fn register_returns_none_on_collision() {
        // Pin: collision means uniquification is broken. The helper
        // refuses to shadow rather than silently masking another
        // sub-agent's mailbox — documented contract on
        // RegisterOutcome.
        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let _first = register_sub_agent_mailbox(&reg, &path).expect("first register");
        let second = register_sub_agent_mailbox(&reg, &path);
        assert!(
            second.is_none(),
            "colliding register must return None, not shadow"
        );
        assert_eq!(reg.len(), 1, "registry must not grow on collision");
    }

    #[test]
    fn drop_unregisters_entry() {
        // Pin the load-bearing RAII contract: every `?` inside
        // `execute_sub_agent` would leak a registry entry without it.
        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (_rx, guard) = register_sub_agent_mailbox(&reg, &path).expect("register");
        assert_eq!(reg.len(), 1);
        drop(guard);
        assert_eq!(reg.len(), 0, "drop must unregister");
    }

    #[test]
    fn drop_is_idempotent_when_entry_already_gone() {
        // Pin: explicit unregister followed by drop must not panic.
        // Future code paths may explicitly unregister (e.g. for
        // hot-replace); this test pins that the RAII guard tolerates it.
        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (_rx, guard) = register_sub_agent_mailbox(&reg, &path).expect("register");
        assert!(reg.unregister(&path), "explicit unregister succeeds");
        // Now drop the guard — entry is already gone. Must not panic.
        drop(guard);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn manual_construction_drops_correctly() {
        // Pin: MailboxRegistration is constructed by
        // `register_sub_agent_mailbox` today, but the Drop impl
        // shouldn't depend on construction path. Verify by hand-
        // constructing one and dropping it.
        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (mb, _rx) = crate::agent::Mailbox::new();
        reg.register(path.clone(), Arc::new(mb));
        let guard = MailboxRegistration {
            registry: Arc::clone(&reg),
            path: path.clone(),
        };
        drop(guard);
        assert!(
            reg.get(&path).is_none(),
            "manually-built guard's Drop must still unregister"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_empty_mailbox_writes_nothing() {
        // Pin: empty drain is a no-op (no rows inserted, no error).
        // Called every iter — must be cheap and side-effect-free
        // when there's no mail.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("drain_empty.db"))
            .await
            .unwrap();
        let session_id = db.create_session("test", dir.path()).await.unwrap();

        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (mut rx, _guard) = register_sub_agent_mailbox(&reg, &path).expect("register");

        drain_sub_mailbox_into_session(&mut rx, &db, &session_id)
            .await
            .unwrap();

        let history = db.load_context(&session_id).await.unwrap();
        assert!(history.is_empty(), "empty drain must not insert any rows");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_writes_one_user_row_per_mail_item() {
        // Pin: each mail item becomes one Role::User row — the same
        // shape `KodaSession::drain_mail_to_db` produces for the
        // root session, so the LLM sees the same prompt structure
        // whether mail lands in the root or in a sub-agent.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("drain_full.db"))
            .await
            .unwrap();
        let session_id = db.create_session("test", dir.path()).await.unwrap();

        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (mut rx, _guard) = register_sub_agent_mailbox(&reg, &path).expect("register");

        // Send via the registry handle so we exercise the same path
        // peer tools use.
        let mb = reg.get(&path).expect("registered mailbox");
        mb.send(sample_mail("hello from peer"));
        mb.send(sample_mail("second message"));

        drain_sub_mailbox_into_session(&mut rx, &db, &session_id)
            .await
            .unwrap();

        let history = db.load_context(&session_id).await.unwrap();
        assert_eq!(history.len(), 2, "two mails -> two rows");
        for msg in &history {
            assert_eq!(
                msg.role,
                Role::User,
                "mail must land as user-role to mirror KodaSession::drain_mail_to_db"
            );
        }
        let bodies: Vec<&str> = history
            .iter()
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert!(
            bodies.iter().any(|b| b.contains("hello from peer")),
            "first mail body must be persisted; got: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|b| b.contains("second message")),
            "second mail body must be persisted; got: {bodies:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_after_unregister_still_drains_buffered_mail() {
        // Pin: dropping the registry entry doesn't drop the receiver.
        // A peer's send + our unregister can race; we must not lose
        // the mail that already landed in the channel.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("drain_after_unreg.db"))
            .await
            .unwrap();
        let session_id = db.create_session("test", dir.path()).await.unwrap();

        let reg = Arc::new(MailboxRegistry::new());
        let path = AgentPath::root().join("explore_42").unwrap();
        let (mut rx, guard) = register_sub_agent_mailbox(&reg, &path).expect("register");
        let mb = reg.get(&path).expect("registered mailbox");

        // Send, then drop the guard (unregistering the path) BEFORE
        // draining. The mail is already in the unbounded channel.
        mb.send(sample_mail("survives unregister"));
        drop(guard);
        drop(mb);

        drain_sub_mailbox_into_session(&mut rx, &db, &session_id)
            .await
            .unwrap();
        let history = db.load_context(&session_id).await.unwrap();
        assert_eq!(history.len(), 1, "buffered mail must drain post-unregister");
    }
}

#[cfg(test)]
mod b21_tests {
    //! **#1022 B21** regression tests for the workspace-provision-
    //! failure marker.
    //!
    //! Pre-fix, an `Err` from `workspace.provision()` silently fell
    //! back to `project_root.to_path_buf()` — dropping the sub-agent
    //! into the parent's unisolated working tree and letting parallel
    //! sub-agents race on the same files. The marker is the
    //! short-circuit that replaces that fallback for write-tool sub-
    //! agents.
    //!
    //! These tests pin the *contract* of the marker (not its exact
    //! wording):
    //!
    //! - `[ERROR:` prefix so the model treats it as structural
    //!   failure metadata, not a sub-agent answer.
    //! - The agent name appears so multi-agent flows can identify
    //!   which sub-agent's workspace failed.
    //! - The failure reason is included so the parent / user can
    //!   diagnose (non-APFS volume, no git repo, etc.).
    //! - A re-strategize hint so the model doesn't just retry the
    //!   same prompt.
    //! - Single-line so it formats cleanly as a tool result.
    //!
    //! End-to-end coverage of the dispatch short-circuit itself
    //! requires injecting a `WorkspaceProvider` into the sub-agent
    //! dispatch path, which is not currently a public seam. Until
    //! that refactor lands, the marker contract here plus a manual
    //! check on a non-APFS macOS volume is the regression net.

    use super::workspace_provision_failure_marker;

    #[test]
    fn marker_has_error_prefix() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.starts_with("[ERROR:"),
            "marker must start with `[ERROR:` so the model treats it as \
             structural failure, not a sub-agent answer; got: {m}"
        );
    }

    #[test]
    fn marker_includes_agent_name() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.contains("'writer'"),
            "marker must name the sub-agent that failed so multi-agent \
             flows can disambiguate; got: {m}"
        );
    }

    #[test]
    fn marker_includes_failure_reason() {
        // The reason is the actionable bit — if it's missing, the
        // user has no way to know whether it's a misconfigured
        // volume, missing git repo, or something else. This
        // guarantees the underlying `e.to_string()` reaches the
        // parent agent intact.
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.contains("clonefile: ENOTSUP"),
            "marker must include the failure reason verbatim so it's \
             diagnosable; got: {m}"
        );
    }

    #[test]
    fn marker_does_not_silently_dispatch() {
        // The whole point of B21: the marker explicitly states the
        // sub-agent was *not* dispatched. If a future refactor
        // re-introduces the silent fallback and forgets this
        // wording, the model loses the signal that nothing ran.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("not dispatched") || lower.contains("was not dispatched"),
            "marker must state the sub-agent was not dispatched, so the \
             parent doesn't assume the work happened; got: {m}"
        );
    }

    #[test]
    fn marker_includes_restrategize_hint() {
        // Without a hint, the model would tend to retry the same
        // prompt and hit the same provision failure. Same rationale
        // as the iteration-cap marker.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("directly") || lower.contains("resolve") || lower.contains("retry"),
            "marker must hint at re-strategizing (e.g. resolve setup, \
             retry without write tools, do directly); got: {m}"
        );
    }

    #[test]
    fn marker_is_single_line() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            !m.contains('\n'),
            "marker must be single-line for clean tool-result formatting; got:\n{m}"
        );
    }
}

/// Phase E of #996 — RAII cleanup hook tests.
///
/// `InvocationCleanup`'s job: when a sub-agent invocation exits
/// (success, iteration cap, or error), **fire the cancel token** on
/// every bg-agent registry entry tagged with that invocation's
/// spawner id. The actual reaping (removal from `pending`) happens
/// later — either when the bg future observes its cancel token and
/// returns, then gets `drain_completed`'d, or when the registry's
/// own Drop impl aborts the JoinHandle. Either way, the guard's job
/// is just signalling.
///
/// Two contracts to pin:
///
///   1. **Drop fires cancel on matching entries.** Entries tagged with
///      `Some(my_invocation_id)` must observe their cancel token fire
///      after the guard drops.
///   2. **Drop leaves non-matching entries alone.** Entries tagged with
///      a *different* spawner id (sibling sub-agent, top-level, etc.)
///      must NOT observe their cancel token fire.
///
/// We test the guard in isolation rather than driving a full
/// `execute_sub_agent` because the function is too large to set up
/// in a unit test. The guard's behaviour is the single load-bearing
/// piece; everything else is plumbing the compiler already verified.
#[cfg(test)]
mod invocation_cleanup_tests {
    use super::InvocationCleanup;
    use crate::child_agent::ChildAgentRegistry;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_cancels_entries_tagged_with_matching_spawner() {
        let reg = Arc::new(ChildAgentRegistry::new());
        // Tag two bg entries with our invocation id. The 4th tuple
        // element is a clone of the entry's cancel token — we use it
        // as an observer to detect the guard firing the cancel.
        let (_id_a, _tx_a, _status_tx_a, cancel_a) =
            reg.register_test_with_status("scout", "a", Some(7));
        let (_id_b, _tx_b, _status_tx_b, cancel_b) =
            reg.register_test_with_status("scout", "b", Some(7));
        assert!(
            !cancel_a.is_cancelled() && !cancel_b.is_cancelled(),
            "setup"
        );

        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            cancel_a.is_cancelled(),
            "entry A's cancel token must fire when guard drops"
        );
        assert!(
            cancel_b.is_cancelled(),
            "entry B's cancel token must fire when guard drops"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_leaves_entries_with_different_spawner_alone() {
        let reg = Arc::new(ChildAgentRegistry::new());
        // Mix: one tagged with our id, one with a sibling's, one with None.
        let (_id_mine, _tx_m, _status_tx_m, cancel_mine) =
            reg.register_test_with_status("a", "mine", Some(7));
        let (_id_sib, _tx_s, _status_tx_s, cancel_sibling) =
            reg.register_test_with_status("a", "sibling", Some(8));
        let (_id_top, _tx_t, _status_tx_t, cancel_toplevel) =
            reg.register_test_with_status("a", "toplevel", None);

        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            cancel_mine.is_cancelled(),
            "my own (spawner=7) entry must be cancelled"
        );
        assert!(
            !cancel_sibling.is_cancelled(),
            "sibling sub-agent's (spawner=8) entry must NOT be cancelled"
        );
        assert!(
            !cancel_toplevel.is_cancelled(),
            "top-level (spawner=None) entry must NOT be cancelled"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_with_no_matching_entries_is_noop() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let (_id, _tx, _status_tx, cancel) = reg.register_test_with_status("a", "x", Some(99));

        // Cleanup for an invocation that never spawned anything —
        // common case: a sub-agent that did nothing bg-related.
        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            !cancel.is_cancelled(),
            "unrelated entry's cancel token must not fire"
        );
    }
}

#[cfg(test)]
mod parse_agent_name_required_tests {
    //! #1232 §5: schema declares `agent_name` required, and this
    //! module's runtime backstop holds the line for models that
    //! ignore the schema. The tests pin every branch so a future
    //! refactor that re-introduces an `unwrap_or("task")` fallback
    //! fails loudly here.

    use super::{available_agents_hint, parse_agent_name_required};
    use serde_json::json;
    use std::path::Path;

    /// Tests use `Path::new(".")` for the project root. Discovery
    /// will return only built-in agents (`explore`, `plan`, `task`,
    /// `verify`) plus any in `<cwd>/agents/` if it exists. The
    /// hint-suffix assertions only check for built-in names that
    /// must always be present.
    fn root() -> &'static Path {
        Path::new(".")
    }

    #[test]
    fn accepts_well_formed_string() {
        let name = parse_agent_name_required(&json!({"agent_name": "explore"}), root())
            .expect("explore must parse");
        assert_eq!(name, "explore");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        // Models occasionally emit `"  explore  "` from sloppy
        // template interpolation. Accept it rather than reject.
        let name = parse_agent_name_required(&json!({"agent_name": "  explore  "}), root())
            .expect("trimmed string must parse");
        assert_eq!(name, "explore");
    }

    #[test]
    fn rejects_missing_field_with_actionable_hint() {
        let err = parse_agent_name_required(&json!({"prompt": "x"}), root())
            .expect_err("missing agent_name must fail")
            .to_string();
        assert!(
            err.contains("'agent_name' is required"),
            "missing-field error must name the field: {err}"
        );
        assert!(
            err.contains("Available agent"),
            "error must list available agents: {err}"
        );
        // Built-ins must be discoverable from any project root.
        assert!(err.contains("task"), "hint must list `task`: {err}");
    }

    #[test]
    fn rejects_explicit_null() {
        let err = parse_agent_name_required(&json!({"agent_name": null}), root())
            .expect_err("null agent_name must fail")
            .to_string();
        // null falls into the same arm as missing — same error message.
        assert!(err.contains("'agent_name' is required"), "got: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        // Empty / whitespace-only strings are useless and silently-
        // routing them anywhere would resurrect the original bug.
        for empty in ["", "   ", "\t\n"] {
            let err = parse_agent_name_required(&json!({"agent_name": empty}), root())
                .expect_err("empty agent_name must fail")
                .to_string();
            assert!(
                err.contains("non-empty"),
                "empty {empty:?} must produce 'non-empty' error: {err}"
            );
        }
    }

    #[test]
    fn rejects_wrong_types() {
        for (value, expected_kind) in [
            (json!({"agent_name": true}), "a boolean"),
            (json!({"agent_name": 42}), "a number"),
            (json!({"agent_name": ["explore"]}), "an array"),
            (json!({"agent_name": {"name": "explore"}}), "an object"),
        ] {
            let err = parse_agent_name_required(&value, root())
                .expect_err("wrong-type agent_name must fail")
                .to_string();
            assert!(
                err.contains(expected_kind),
                "got: {err} — expected to mention {expected_kind:?}"
            );
            assert!(
                err.contains("Available"),
                "wrong-type error must also surface the available list: {err}"
            );
        }
    }

    #[test]
    fn hint_lists_builtins_and_fork() {
        let hint = available_agents_hint(root());
        // The four built-in agents must always be present.
        for name in ["explore", "plan", "task", "verify", "fork"] {
            assert!(hint.contains(name), "hint must list `{name}`: {hint}");
        }
        // Names are sorted + deduplicated; `fork` should appear exactly once
        // even though it's pushed in addition to discovery.
        let fork_count = hint.matches("fork").count();
        assert_eq!(fork_count, 1, "`fork` must not be duplicated: {hint}");
    }
}

#[cfg(test)]
mod error_chain_format_tests {
    //! #1232 §4: pin the contract that sub-agent dispatch error
    //! strings include the **entire** anyhow context chain, not just
    //! the topmost label.
    //!
    //! The bug-review session that opened #1232 logged:
    //!   * msg #8–11: `Error invoking sub-agent: LLM API returned 400
    //!     Bad Request: {"error":"Context size has been exceeded."}`
    //!     — useful, the upstream HTTP body is a single `anyhow::bail!`
    //!     string so default `{e}` already shows everything.
    //!   * msg #19–20: `Error invoking sub-agent: Failed to call LLM
    //!     API` — useless, the underlying `reqwest::send()` cause
    //!     (network error, timeout, connection refused, ...) was added
    //!     by `.context("Failed to call LLM API")` in the provider but
    //!     `format!("{e}")` strips every layer except the top.
    //!
    //! Fix: switch every error-stringification site to `{e:#}` (anyhow's
    //! "alternate" Display flag) which walks the chain joined with ": ".
    //! These tests pin the format so a future "let's clean up the
    //! formatting" refactor can't silently regress to `{e}`.
    use anyhow::{Context, Error};
    #[test]
    fn alt_display_walks_chain_in_order_root_first_topmost_last() {
        // Build the exact shape `openai_compat.rs::chat` produces on
        // a network failure: an inner reqwest-style cause wrapped by
        // `.context("Failed to call LLM API")`.
        let e: Error = Err::<(), _>(anyhow::anyhow!("connection refused"))
            .context("error sending request for url")
            .context("Failed to call LLM API")
            .unwrap_err();
        // Default `{}` is the regression case the bug report flagged:
        // only the topmost label survives. Pin this so anyone reading
        // the test sees WHY we use `{:#}` everywhere.
        assert_eq!(format!("{e}"), "Failed to call LLM API");
        // `{:#}` walks the chain top-to-bottom, joined with ": ".
        // This is the contract every sub-agent error site relies on.
        assert_eq!(
            format!("{e:#}"),
            "Failed to call LLM API: error sending request for url: connection refused"
        );
    }
    #[test]
    fn fg_dispatch_error_string_format_contract() {
        // Mirror the literal `format!` in `tool_dispatch.rs`'s
        // foreground branch. If this assertion ever fails it means
        // someone reverted `{e:#}` → `{e}` and re-broke msg #19/20.
        let e: Error = Err::<(), _>(anyhow::anyhow!("timed out"))
            .context("Failed to call LLM API")
            .unwrap_err();
        let formatted = format!("Error invoking sub-agent: {e:#}");
        assert_eq!(
            formatted,
            "Error invoking sub-agent: Failed to call LLM API: timed out"
        );
    }
    #[test]
    fn bg_dispatch_error_string_format_contract() {
        // Mirror the literal `format!` in this module's bg-task branch
        // (the `Err(e) => tx.send(Err((format!("Error: {e:#}"), ...)))`
        // arm above). Same regression protection as the foreground test.
        let e: Error = Err::<(), _>(anyhow::anyhow!("dns lookup failed"))
            .context("error sending request for url")
            .context("Failed to call LLM API (stream)")
            .unwrap_err();
        let formatted = format!("Error: {e:#}");
        assert_eq!(
            formatted,
            "Error: Failed to call LLM API (stream): error sending request for url: dns lookup failed"
        );
    }
    #[test]
    fn single_layer_error_unchanged_by_alt_format() {
        // Negative regression: for the working sibling case (`anyhow::bail!`
        // already produces a rich message with no chain), `{:#}` and
        // `{}` must produce IDENTICAL output. This prevents a future
        // refactor that switches to `{:#}` from accidentally adding
        // a trailing colon or any other formatting noise to errors
        // that were already fine.
        let e: Error = anyhow::anyhow!(
            "LLM API returned 400 Bad Request: {{\"error\":\"Context size has been exceeded.\"}}"
        );
        assert_eq!(format!("{e}"), format!("{e:#}"));
    }
}
