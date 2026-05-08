//! `wait_for_mail` peer tool — Phase 3 of #1325.
//!
//! Lets the LLM block its current turn for up to `timeout_ms`
//! waiting for new mail to arrive in its own mailbox. Returns
//! immediately if mail arrives, returns "timed out" otherwise.
//!
//! The mail itself is **not** drained by this tool — it surfaces as
//! a user-role message at the top of the **next** turn (via
//! `KodaSession::drain_mail_to_db`). This tool only signals "stuff
//! arrived, you can now end your turn so the next one picks it up".
//!
//! # Mapping to codex
//!
//! Patterned on
//! `codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs` minus
//! the codex plumbing koda doesn't have yet:
//!
//! | codex | koda |
//! |---|---|
//! | `session.subscribe_mailbox_seq()` | `MailboxRegistry::get(/root)` then `Mailbox::subscribe` |
//! | `session.has_pending_mailbox_items().await` (fast path) | `Mailbox::has_pending()` — sender-side check via shared drained-count |
//! | `session.send_event(CollabWaitingBegin/End)` | dropped — koda has no peer-event channel yet |
//! | `turn.config.multi_agent_v2.min_wait_timeout_ms` | hard-coded to 1ms (no per-config override yet) |
//! | per-agent status snapshots in result | dropped — no agent-status registry yet |
//!
//! # Why no explicit cancel-token plumbing
//!
//! Both `tokio::time::sleep` and `watch::Receiver::changed` are
//! cancel-safe — when the dispatch task is dropped (TUI Esc /
//! Ctrl+C / session shutdown), the `select!` future is dropped and
//! the wait ends cleanly. No state to clean up. Codex's `wait_agent`
//! similarly relies on dispatch-level abort rather than threading a
//! cancel token through.
//!
//! # Why "own mailbox" = `/root`
//!
//! Phase 3 has no per-spawner caller identity (no `caller_path` on
//! `ToolExecCtx`). Every call to `wait_for_mail` is from the root
//! session, so it waits on the root's mailbox. Phase 4 (`spawn_agent`)
//! will add a `caller_path` field and child agents will wait on
//! their own mailbox via the same lookup.

use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

/// Default wait timeout when the model omits `timeout_ms`. Matches
/// codex's `DEFAULT_WAIT_TIMEOUT_MS = 30_000` so cross-codebase
/// behavior pins.
pub const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Maximum allowed `timeout_ms` — guards against a runaway model
/// blocking a turn for hours. Matches codex's
/// `MAX_WAIT_TIMEOUT_MS = 600_000` (10 minutes).
pub const MAX_WAIT_TIMEOUT_MS: u64 = 600_000;

/// Minimum allowed `timeout_ms`. Codex makes this configurable per
/// session; Phase 3 hard-codes it to 1ms (the lowest value that's
/// still meaningfully "wait" rather than "noop"). Phase 4 can grow
/// a config knob when there's a use case.
pub const MIN_WAIT_TIMEOUT_MS: u64 = 1;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "WaitForMail".to_string(),
        description: format!(
            "Block the current turn for up to `timeout_ms` waiting for new mail to \
             arrive in your mailbox. Returns immediately when any mail arrives. \
             Returns `timed_out: true` if no mail arrives before the timeout. \
             \n\nDefault timeout: {}ms ({}s). Max: {}ms ({}s). Min: {}ms.\
             \n\nThe mail itself surfaces as user-role messages at the top of your \
             NEXT turn — this tool only signals arrival so you can end your current \
             turn cleanly.\
             \n\nWHEN TO USE\
             \n- After `SendMessage` if you expect a peer to reply.\
             \n- After `InvokeAgent` / `SpawnAgent` ONLY if (1) you have genuinely run \
             out of useful concurrent work AND (2) the next step strictly depends on \
             the sub-agent's output. Every bg-agent exit sends a completion mail \
             via the `notify_parent_mailbox` bridge, so this tool will unblock the \
             instant the first sub-agent finishes.\
             \n\nWHEN NOT TO USE (anti-pattern)\
             \n- Immediately after spawning a sub-agent with no other work queued. This \
             defeats the purpose of background execution — you serialize what was \
             supposed to run in parallel and burn real wall-clock waiting on a \
             timeout when you could have been doing follow-up reads, searches, or \
             edits. See `InvokeAgent`'s description for the full guidance.\
             \n- As a general 'pause and think' mechanism. There is no mail-less wakeup \
             — if no mail arrives this tool blocks for the full `timeout_ms`.\
             \n\nRETURN PAYLOAD\
             \n- On mail arrival: `{{\"message\": \"Wait completed.\", \"timed_out\": false}}`. \
             End your turn so the next iteration drains the mailbox.\
             \n- On timeout: `{{\"message\": \"Wait timed out.\", \"timed_out\": true, \
             \"bg_agents_in_flight\": <N>, \"bg_agents\": [...], \"hint\": \"...\"}}`. \
             Use `bg_agents_in_flight` and the per-task `bg_agents` summary \
             (task_id, agent_name, status, age_secs) to decide what to do next: \
             if N > 0, sub-agents are still working — do useful concurrent work \
             and re-check; if N = 0, no work was queued — proceed yourself. The \
             `hint` string spells out the recommendation.\
             \n\nResults still inject automatically on a future iteration via auto-drain; \
             you do NOT need to call `WaitForMail` to receive them. Use it only when \
             you cannot make forward progress without the reply.",
            DEFAULT_WAIT_TIMEOUT_MS,
            DEFAULT_WAIT_TIMEOUT_MS / 1000,
            MAX_WAIT_TIMEOUT_MS,
            MAX_WAIT_TIMEOUT_MS / 1000,
            MIN_WAIT_TIMEOUT_MS,
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "timeout_ms": {
                    "type": "integer",
                    "description": format!(
                        "Maximum milliseconds to wait. Clamped to [{MIN_WAIT_TIMEOUT_MS}, \
                         {MAX_WAIT_TIMEOUT_MS}]. Defaults to {DEFAULT_WAIT_TIMEOUT_MS} when omitted."
                    ),
                    "minimum": MIN_WAIT_TIMEOUT_MS,
                    "maximum": MAX_WAIT_TIMEOUT_MS,
                }
            },
            "required": []
        }),
    }]
}

/// `WaitForMail` — block on the caller's mailbox until new mail or timeout.
pub struct WaitForMailTool;

#[async_trait]
impl Tool for WaitForMailTool {
    fn name(&self) -> &'static str {
        "WaitForMail"
    }

    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "WaitForMail")
            .expect("wait_for_mail::definitions() must contain WaitForMail")
    }

    fn classify(&self, _args: &Value) -> ToolEffect {
        // Read-only: WaitForMail observes the mailbox sequence
        // counter but doesn't mutate any state. Codex's `wait_agent`
        // is also classified as read-only.
        ToolEffect::ReadOnly
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let registry = match ctx.mailbox_registry {
            Some(reg) => reg,
            None => {
                return ToolResult {
                    output: "WaitForMail requires an active session with a mailbox registry; \
                             no registry was attached to this tool execution context."
                        .to_string(),
                    success: false,
                    full_output: None,
                };
            }
        };

        // #1325 Phase 4: look up the *caller's* mailbox, not always
        // root. `caller_agent_path` is threaded down from
        // `KodaSession::agent_path` so a spawned child blocks on
        // its own inbox (the only one it can ever read from).
        let own_path = ctx.caller_agent_path;
        let own_mailbox = match registry.get(own_path) {
            Some(mb) => mb,
            None => {
                // Substrate invariant violation — KodaSession::new
                // pre-registers `/root` unconditionally, and Phase 4's
                // SpawnAgent registers child paths before yielding
                // control. If we hit this, either the registry was
                // rebuilt without the caller's entry or the session-
                // construction invariant is broken.
                return ToolResult {
                    output: format!(
                        "WaitForMail: own mailbox (`{own_path}`) is not registered. \
                         This is a session-construction invariant violation."
                    ),
                    success: false,
                    full_output: None,
                };
            }
        };

        // Argument parsing + clamping. Extra fields are ignored
        // (matches the lenient-input policy used by other peer tools).
        let raw_timeout = args.get("timeout_ms").and_then(Value::as_i64);
        let timeout_ms = match raw_timeout {
            Some(ms) if ms <= 0 => {
                // Codex returns "timeout_ms must be greater than zero"
                // here; we pin the verbatim string for cross-codebase
                // greppability.
                return ToolResult {
                    output: "timeout_ms must be greater than zero".to_string(),
                    success: false,
                    full_output: None,
                };
            }
            Some(ms) => (ms as u64).clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS),
            None => DEFAULT_WAIT_TIMEOUT_MS,
        };

        // Subscribe BEFORE checking current state. `borrow_and_update`
        // marks the current sequence as "seen" so a subsequent
        // `changed()` only fires for a strictly-later send. Without
        // borrow_and_update, the watch starts in an "unseen" state
        // and `changed()` would return immediately on the first poll
        // even if no new mail had arrived since subscribe.
        //
        // Fast path (codex parity — #1325 Phase 5b follow-up): if mail
        // arrived BEFORE this tool was invoked but AFTER the parent's
        // last drain, the watch's `borrow_and_update` would mark that
        // already-published seq as seen and the subsequent `changed()`
        // would block until the next mail (often timing out at 30s).
        // The fix mirrors codex's `wait_agent`: short-circuit when
        // `Mailbox::has_pending` reports unread mail. Without this,
        // a parent that calls `WaitForMail` immediately after a fast
        // bg-agent (the `test_sub_agent_cache_hit_skips_llm` shape)
        // hangs for the full timeout.
        if own_mailbox.has_pending() {
            let payload = json!({
                "message": "Wait completed.",
                "timed_out": false,
            });
            return ToolResult {
                output: payload.to_string(),
                success: true,
                full_output: None,
            };
        }

        let mut seq_rx = own_mailbox.subscribe();
        let _ = seq_rx.borrow_and_update();

        let timed_out = tokio::select! {
            // `seq_rx.changed()` is cancel-safe — dropping the
            // future on dispatch abort leaves no leaked state.
            res = seq_rx.changed() => {
                // Err means the watch sender was dropped (mailbox
                // gone). Treat the same as "no mail arrived" — the
                // session is shutting down anyway.
                res.is_err()
            }
            // `sleep` is cancel-safe at drop.
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => true,
        };

        // Output structure mirrors codex's `WaitAgentResult`: a
        // human-readable `message` string + a structured `timed_out`
        // bool. Returned as JSON so downstream tooling (and the
        // model) can branch on the bool without parsing prose.
        //
        // #1338 Issue #3: on the timeout path the bare `{message,
        // timed_out}` payload told the model nothing about WHY the
        // wait failed (was a bg-agent still running? was no agent
        // ever spawned?). Without that signal the model's recovery
        // was correct-but-slow: "timeout \u2192 do exploration myself \u2192
        // re-wait \u2192 timeout again". Burning real wall-clock for no
        // good reason.
        //
        // We now enrich the timeout payload with `bg_agents_in_flight`
        // (count) plus a per-task `bg_agents` summary (task_id,
        // agent_name, status, age_secs) and a `hint` string nudging
        // the model toward the right next move. The success payload
        // stays minimal because there's nothing useful to say beyond
        // "go drain your mailbox on the next iteration".
        let payload = if timed_out {
            build_timed_out_payload(ctx)
        } else {
            json!({
                "message": "Wait completed.",
                "timed_out": false,
            })
        };
        ToolResult {
            output: payload.to_string(),
            success: true,
            full_output: None,
        }
    }
}

/// Construct the rich timeout payload (#1338 Issue #3).
///
/// Reads the bg-agent registry off `ctx.bg_agents` and scopes the
/// snapshot to the caller via `ctx.caller_spawner` (so a sub-agent
/// only sees its own children, not its siblings'). Falls back to
/// the legacy minimal payload when no registry is attached —
/// preserves bytewise behavior for standalone-`ToolRegistry` tests
/// and any future caller that wires `WaitForMail` without the
/// registry.
///
/// Pure function (no async, no I/O beyond cloning snapshots) so it
/// can be tested in isolation.
fn build_timed_out_payload(ctx: &ToolExecCtx<'_>) -> Value {
    let Some(registry) = ctx.bg_agents else {
        // Legacy minimal payload — standalone-`ToolRegistry` tests
        // and any caller that hasn't wired `set_bg_agents`.
        return json!({
            "message": "Wait timed out.",
            "timed_out": true,
        });
    };

    // Scope to caller. A top-level wait sees its own bg-agents;
    // a sub-agent waiter sees only its own children. Mirrors the
    // scoping rules of `ListBackgroundTasks`.
    let mut snapshots = registry.snapshot_for_caller(ctx.caller_spawner);
    // Drop terminal entries — once a bg-agent has Completed/Errored/
    // Cancelled it's no longer "in flight" and reporting it would
    // mislead the model into waiting for something that's already
    // done. Keep `Pending` and `Running { .. }` only.
    snapshots.retain(|s| {
        matches!(
            s.status,
            crate::child_agent::AgentStatus::Pending
                | crate::child_agent::AgentStatus::Running { .. }
        )
    });
    let in_flight = snapshots.len();

    // Per-task summary. `prompt` is intentionally omitted — it can be
    // very long and the model already saw it when it spawned the agent.
    // `age_secs` rounds to whole seconds; sub-second precision would be
    // noise in a payload aimed at human-scale recovery decisions.
    let bg_agents_summary: Vec<Value> = snapshots
        .iter()
        .map(|s| {
            json!({
                "task_id": s.task_id,
                "agent_name": s.agent_name,
                "status": agent_status_label(&s.status),
                "age_secs": s.age.as_secs(),
            })
        })
        .collect();

    // Hint string. Three cases:
    //   1. No bg-agents in flight → the wait timed out for some
    //      OTHER reason (a peer that was supposed to reply didn't,
    //      or the model called WaitForMail speculatively). Tell it
    //      to make forward progress on its own.
    //   2. ≥1 bg-agent in flight → they're still working; nudge
    //      toward useful concurrent work + shorter re-poll.
    //   3. (Implicit) no registry attached → we already returned
    //      the legacy minimal payload above.
    let hint = if in_flight == 0 {
        "No background sub-agents are in flight. The wait timed out \
         because no mail arrived; consider whether the work you were \
         waiting on was actually started, or proceed with the task \
         yourself."
            .to_string()
    } else {
        format!(
            "{in_flight} background sub-agent(s) still running. Consider \
             doing other useful work (reads, searches, edits) and re-checking \
             with a shorter timeout next turn — or end your turn now and \
             let auto-drain inject results on a future iteration."
        )
    };

    json!({
        "message": "Wait timed out.",
        "timed_out": true,
        "bg_agents_in_flight": in_flight,
        "bg_agents": bg_agents_summary,
        "hint": hint,
    })
}

/// Stable, lowercase string label for an [`AgentStatus`].
///
/// Lives here (rather than as `Display` on `AgentStatus`) because
/// the canonical `Display` impl on `AgentStatus` doesn't exist and
/// adding one feels like API-surface bloat for a single payload
/// consumer. The labels match the JSON serde representation
/// (`#[serde(tag = "kind", rename_all = "snake_case")]`) so consumers
/// can correlate with the `ChildTaskUpdate` engine event stream.
fn agent_status_label(status: &crate::child_agent::AgentStatus) -> &'static str {
    use crate::child_agent::AgentStatus::*;
    match status {
        Pending => "pending",
        Running { .. } => "running",
        Cancelled => "cancelled",
        Completed { .. } => "completed",
        Errored { .. } => "errored",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentPath, InterAgentCommunication, Mailbox, MailboxRegistry};
    use std::sync::Arc;
    use std::time::Instant;

    fn fresh_registry_with_root() -> (Arc<MailboxRegistry>, Arc<Mailbox>) {
        let (mb, _rx) = Mailbox::new();
        let mb = Arc::new(mb);
        let reg = Arc::new(MailboxRegistry::new());
        reg.register(AgentPath::root(), Arc::clone(&mb));
        (reg, mb)
    }

    fn make_test_fixtures() -> (
        std::path::PathBuf,
        crate::tools::FileReadCache,
        koda_sandbox::fs::LocalFileSystem,
        crate::output_caps::OutputCaps,
        crate::tools::bg_process::BgRegistry,
        crate::trust::TrustMode,
        koda_sandbox::SandboxPolicy,
        crate::skills::SkillRegistry,
    ) {
        (
            std::path::PathBuf::from("/tmp"),
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            koda_sandbox::fs::LocalFileSystem::new(),
            crate::output_caps::OutputCaps::for_context(100_000),
            crate::tools::bg_process::BgRegistry::new(),
            crate::trust::TrustMode::Safe,
            koda_sandbox::SandboxPolicy::default(),
            crate::skills::SkillRegistry::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn ctx_with_registry<'a>(
        root: &'a std::path::Path,
        cache: &'a crate::tools::FileReadCache,
        fs: &'a dyn koda_sandbox::fs::FileSystem,
        caps: &'a crate::output_caps::OutputCaps,
        bg: &'a crate::tools::bg_process::BgRegistry,
        trust: &'a crate::trust::TrustMode,
        policy: &'a koda_sandbox::SandboxPolicy,
        skills: &'a crate::skills::SkillRegistry,
        registry: &'a Arc<MailboxRegistry>,
        agent_path: &'a AgentPath,
    ) -> ToolExecCtx<'a> {
        ToolExecCtx {
            project_root: root,
            read_cache: cache,
            fs,
            caps,
            sink: None,
            caller_spawner: None,
            bg_registry: bg,
            trust,
            sandbox_policy: policy,
            proxy_port: None,
            socks5_port: None,
            session: None,
            skill_registry: skills,
            mailbox_registry: Some(registry),
            bg_agents: None,
            caller_agent_path: agent_path,
        }
    }

    fn sample_mail() -> InterAgentCommunication {
        InterAgentCommunication {
            author: AgentPath::root(),
            recipient: AgentPath::root(),
            other_recipients: Vec::new(),
            content: "wakey wakey".to_string(),
            trigger_turn: true,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_completed_when_mail_arrives_during_wait() {
        // Pin the load-bearing happy path: a `send` while wait is
        // blocked must wake the wait promptly.
        let (reg, mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        // Spawn a sender that waits 50ms then delivers.
        let mb_for_send = Arc::clone(&mb);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            mb_for_send.send(sample_mail());
        });

        let started = Instant::now();
        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 5000}))
            .await;
        let elapsed = started.elapsed();
        sender.await.unwrap();

        assert!(result.success, "expected success, got: {}", result.output);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            payload["timed_out"], false,
            "must report not-timed-out when mail arrived"
        );
        assert_eq!(payload["message"], "Wait completed.");
        assert!(
            elapsed < Duration::from_millis(2000),
            "wait must wake on mail arrival, not run to timeout; took {:?}",
            elapsed
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn returns_timed_out_when_no_mail_arrives() {
        // Pin: with no sender, wait must return timed_out=true after
        // (approximately) the requested duration.
        let (reg, _mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        let started = Instant::now();
        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let elapsed = started.elapsed();

        assert!(result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["message"], "Wait timed out.");
        assert!(
            elapsed >= Duration::from_millis(50),
            "wait must run at least the requested duration; took {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "wait must not significantly overshoot the timeout; took {:?}",
            elapsed
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn omitted_timeout_uses_default() {
        // Pin: argument-less call uses DEFAULT_WAIT_TIMEOUT_MS. We
        // can't easily wait 30s in a unit test, so verify by sending
        // mail immediately — the tool should still return promptly.
        let (reg, mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        // Send before the wait starts. The wait still needs to
        // observe the change after subscribing — borrow_and_update
        // marks the current seq as seen, so this delivery (which
        // happened pre-subscribe) is the LAST seen value, not a
        // pending change. We therefore need to send AFTER subscribing.
        let mb_for_send = Arc::clone(&mb);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            mb_for_send.send(sample_mail());
        });

        // No timeout_ms arg — takes the default. Should still
        // complete fast because mail arrives at ~20ms.
        let result = WaitForMailTool.execute(&ctx, &json!({})).await;
        sender.await.unwrap();

        assert!(result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_subscribe_mail_completes_immediately() {
        // Codex parity (was inverted in the original koda port —
        // see #1325 Phase 5b follow-up): mail that arrived BEFORE
        // the wait started but AFTER the parent's last drain MUST
        // trigger immediate completion. Otherwise an LLM that
        // dispatches a fast bg-agent and then calls WaitForMail
        // races the publisher via the watch channel and silently
        // loses the wakeup, hanging for the full timeout (30s
        // default). The fast path uses `Mailbox::has_pending`
        // (sender-side count delta vs. receiver's `drain`) so it
        // stays correct even after the receiver drains.
        let (reg, mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        // Pre-existing, undrained mail.
        mb.send(sample_mail());
        mb.send(sample_mail());

        let start = Instant::now();
        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 30_000}))
            .await;
        let elapsed = start.elapsed();

        assert!(result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            payload["timed_out"], false,
            "pre-existing mail must complete immediately (codex parity); got: {}",
            result.output
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "fast-path check must short-circuit — elapsed {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_zero_or_negative_returns_codex_error() {
        let (reg, _mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 0}))
            .await;
        assert!(!result.success);
        assert_eq!(
            result.output, "timeout_ms must be greater than zero",
            "must match codex's verbatim error string"
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": -50}))
            .await;
        assert!(!result.success);
        assert_eq!(result.output, "timeout_ms must be greater than zero");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_above_max_is_clamped_not_rejected() {
        // Pin the clamping policy: values above MAX are silently
        // clamped (matching codex). The model gets what it asked for
        // up to the cap, no error. Verify by passing a huge value
        // and observing the wait completes promptly when mail arrives.
        let (reg, mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        let mb_for_send = Arc::clone(&mb);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            mb_for_send.send(sample_mail());
        });

        let result = WaitForMailTool
            .execute(
                &ctx,
                &json!({"timeout_ms": (MAX_WAIT_TIMEOUT_MS as i64) * 100}),
            )
            .await;

        assert!(result.success, "huge timeout must be clamped, not rejected");
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_registry_returns_session_required_error() {
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ToolExecCtx {
            project_root: &root,
            read_cache: &cache,
            fs: &fs,
            caps: &caps,
            sink: None,
            caller_spawner: None,
            bg_registry: &bg,
            trust: &trust,
            sandbox_policy: &policy,
            proxy_port: None,
            socks5_port: None,
            session: None,
            skill_registry: &skills,
            mailbox_registry: None,
            bg_agents: None,
            caller_agent_path: &agent_path,
        };

        let result = WaitForMailTool.execute(&ctx, &json!({})).await;
        assert!(!result.success);
        assert!(
            result.output.contains("requires an active session"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn classify_is_read_only() {
        // Pin: see module docs. WaitForMail observes the mailbox seq
        // counter but doesn't mutate state, matching codex's
        // wait_agent classification.
        assert_eq!(WaitForMailTool.classify(&json!({})), ToolEffect::ReadOnly);
    }

    #[test]
    fn definition_documents_default_max_min_in_description() {
        // Pin: the LLM relies on the description text to know the
        // bounds. A regression that drops these constants from the
        // description would silently leave the LLM guessing.
        let def = &definitions()[0];
        let desc = &def.description;
        assert!(desc.contains(&DEFAULT_WAIT_TIMEOUT_MS.to_string()));
        assert!(desc.contains(&MAX_WAIT_TIMEOUT_MS.to_string()));
        assert!(desc.contains(&MIN_WAIT_TIMEOUT_MS.to_string()));
    }

    #[test]
    fn definition_reinforces_anti_pattern_and_use_cases() {
        // #1338 Issue #2: defense-in-depth. `InvokeAgent`'s description
        // already warns against spawn-then-immediately-wait. The
        // contract has two endpoints though, and the model may be
        // looking at `WaitForMail`'s description (not `InvokeAgent`'s)
        // when it decides to call this tool. So `WaitForMail` must
        // *also* surface:
        //
        //   1. The bg-agent-completion mail use case (post-Phase 5b
        //      this is one of the two main reasons to call this tool).
        //   2. A pointer to the `notify_parent_mailbox` bridge so
        //      the model knows mail will arrive on bg-agent exit.
        //   3. The spawn-then-immediately-wait anti-pattern (mirroring
        //      `InvokeAgent`'s own warning).
        //   4. The original `SendMessage`-reply use case (must NOT
        //      regress — the previous description's only stated use
        //      case must still be present).
        let def = &definitions()[0];
        let desc = &def.description;

        // (1) bg-agent completion mail surfaced.
        assert!(
            desc.contains("InvokeAgent") || desc.contains("SpawnAgent"),
            "description must mention the spawn tool(s) so the model knows \
             bg-agent completion is a wait trigger; got:\n{desc}"
        );
        assert!(
            desc.contains("sub-agent") || desc.contains("bg-agent"),
            "description must reference sub-agents as a mail source; got:\n{desc}"
        );

        // (2) bridge pointer.
        assert!(
            desc.contains("notify_parent_mailbox") || desc.contains("completion mail"),
            "description must reference the bridge or completion-mail mechanism so \
             the model knows bg-agent exit unblocks this tool; got:\n{desc}"
        );

        // (3) anti-pattern explicit.
        assert!(
            desc.to_lowercase().contains("anti-pattern") || desc.contains("defeats the purpose"),
            "description must explicitly call out the spawn-then-immediately-wait \
             anti-pattern; got:\n{desc}"
        );

        // (4) inverted regression: original `SendMessage` use case
        //     must NOT be lost in the rewrite.
        assert!(
            desc.contains("SendMessage"),
            "description must still reference SendMessage — the original \
             peer-reply use case predates the bg-agent guidance and remains \
             valid. Do not regress it; got:\n{desc}"
        );
    }

    // ── #1338 Issue #3: rich timeout payload ──────────────────────
    //
    // Tests for the bg_agents-aware timeout payload. They use the
    // same `ctx_with_registry` helper as the existing tests but
    // attach a real `ChildAgentRegistry` so we can exercise the
    // snapshot path. Each test pins exactly ONE behavior so failure
    // messages stay focused.

    /// Build a `ToolExecCtx` with both mailbox and bg-agent
    /// registries attached. Callers that need to scope to a specific
    /// caller (sub-agent test) override `caller_spawner` after.
    #[allow(clippy::too_many_arguments)]
    fn ctx_with_bg_agents<'a>(
        root: &'a std::path::Path,
        cache: &'a crate::tools::FileReadCache,
        fs: &'a dyn koda_sandbox::fs::FileSystem,
        caps: &'a crate::output_caps::OutputCaps,
        bg: &'a crate::tools::bg_process::BgRegistry,
        trust: &'a crate::trust::TrustMode,
        policy: &'a koda_sandbox::SandboxPolicy,
        skills: &'a crate::skills::SkillRegistry,
        registry: &'a Arc<MailboxRegistry>,
        bg_agents: &'a Arc<crate::child_agent::ChildAgentRegistry>,
        agent_path: &'a AgentPath,
    ) -> ToolExecCtx<'a> {
        ToolExecCtx {
            project_root: root,
            read_cache: cache,
            fs,
            caps,
            sink: None,
            caller_spawner: None,
            bg_registry: bg,
            trust,
            sandbox_policy: policy,
            proxy_port: None,
            socks5_port: None,
            session: None,
            skill_registry: skills,
            mailbox_registry: Some(registry),
            bg_agents: Some(bg_agents),
            caller_agent_path: agent_path,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_payload_includes_bg_agents_in_flight() {
        // Pin: when bg-agents are registered, the timeout payload
        // must include `bg_agents_in_flight` (count) plus a
        // `bg_agents` array with one entry per non-terminal task.
        // Without these the model has no signal to differentiate
        // "timed out, work in progress" from "timed out, nothing
        // running" — see #1338 Issue #3 for the diagnosis.
        let (reg, _mb) = fresh_registry_with_root();
        let bg_agents = crate::child_agent::new_shared();
        // Two in-flight bg-agents at the top-level scope. We don't
        // care about the senders here — letting them drop is fine,
        // the registry entries persist until explicitly drained.
        let (_id_a, _tx_a) = bg_agents.register_test("explore", "find usages of Foo");
        let (_id_b, _tx_b) = bg_agents.register_test("verify", "check tests pass");
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_bg_agents(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &bg_agents,
            &agent_path,
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        assert!(result.success);
        let payload: Value = serde_json::from_str(&result.output).unwrap();

        assert_eq!(payload["timed_out"], true);
        assert_eq!(
            payload["bg_agents_in_flight"], 2,
            "in-flight count must reflect the two registered tasks; got: {payload}"
        );
        let entries = payload["bg_agents"].as_array().expect("bg_agents array");
        assert_eq!(
            entries.len(),
            2,
            "per-task summary length mismatch: {payload}"
        );
        // Pin the per-entry shape so a future renamer doesn't
        // silently change the contract the model relies on.
        for entry in entries {
            assert!(entry.get("task_id").and_then(|v| v.as_u64()).is_some());
            assert!(entry.get("agent_name").and_then(|v| v.as_str()).is_some());
            assert!(entry.get("status").and_then(|v| v.as_str()).is_some());
            assert!(entry.get("age_secs").and_then(|v| v.as_u64()).is_some());
        }
        // Hint must reference the in-flight count (so the model
        // sees a concrete number, not just a generic nudge).
        let hint = payload["hint"].as_str().expect("hint string");
        assert!(
            hint.contains("2"),
            "hint must mention the in-flight count; got: {hint}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_payload_zero_in_flight_when_no_bg_agents() {
        // Pin: registry attached but empty → in_flight=0, empty
        // array, hint suggests proceeding solo. Inverted form of
        // the previous test; both shapes are part of the contract.
        let (reg, _mb) = fresh_registry_with_root();
        let bg_agents = crate::child_agent::new_shared();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_bg_agents(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &bg_agents,
            &agent_path,
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["bg_agents_in_flight"], 0);
        assert!(payload["bg_agents"].as_array().unwrap().is_empty());
        let hint = payload["hint"].as_str().unwrap();
        assert!(
            hint.to_lowercase().contains("no background") || hint.to_lowercase().contains("no bg"),
            "hint must call out the empty-queue case so the model proceeds solo; got: {hint}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_payload_falls_back_when_no_registry_attached() {
        // Pin: when `ctx.bg_agents` is `None` (standalone-`ToolRegistry`
        // tests, or any caller that hasn't wired `set_bg_agents`),
        // the payload must fall back to the legacy minimal
        // `{message, timed_out}` shape — keys for the rich payload
        // must not appear at all (so the model can rely on their
        // absence to detect 'no signal available').
        let (reg, _mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_registry(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &agent_path,
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], true);
        assert_eq!(payload["message"], "Wait timed out.");
        assert!(
            payload.get("bg_agents_in_flight").is_none(),
            "fallback payload must not include rich-payload keys; got: {payload}"
        );
        assert!(payload.get("bg_agents").is_none());
        assert!(payload.get("hint").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_payload_terminal_bg_agents_excluded() {
        // Pin: a bg-agent that has Completed/Errored is no longer
        // 'in flight' and must not appear in the timeout payload.
        // Reporting it would mislead the model into waiting for
        // something already done.
        let (reg, _mb) = fresh_registry_with_root();
        let bg_agents = crate::child_agent::new_shared();
        let (_id_running, _tx_r) = bg_agents.register_test("explore", "still working");
        let (_id_done, _tx_d, status_tx, _cancel) =
            bg_agents.register_test_with_status("verify", "already done", None);
        // Drive the second task to a terminal state.
        status_tx
            .send(crate::child_agent::AgentStatus::Completed {
                summary: "all good".to_string(),
            })
            .unwrap();

        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_bg_agents(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &bg_agents,
            &agent_path,
        );

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            payload["bg_agents_in_flight"], 1,
            "terminal bg-agents must be excluded; got: {payload}"
        );
        let names: Vec<&str> = payload["bg_agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["agent_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["explore"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_payload_scoped_to_caller_spawner() {
        // Pin: a sub-agent that calls WaitForMail must only see ITS
        // OWN child bg-agents — not its siblings'. Mirrors the
        // scoping rules of `ListBackgroundTasks` / Model E in #996.
        // Without this a sub-agent would learn about other agents'
        // work, which is both a leak and a source of confusion.
        let (reg, _mb) = fresh_registry_with_root();
        let bg_agents = crate::child_agent::new_shared();
        // Caller is sub-agent with id=42. Its child has spawner=Some(42).
        // A sibling agent's child has spawner=Some(99) — must be hidden.
        let (_id_mine, _tx_mine, _, _) =
            bg_agents.register_test_with_status("mine", "my work", Some(42));
        let (_id_sibling, _tx_sib, _, _) =
            bg_agents.register_test_with_status("sibling", "not mine", Some(99));
        let (_id_top, _tx_top, _, _) =
            bg_agents.register_test_with_status("top", "top-level", None);

        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let mut ctx = ctx_with_bg_agents(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &bg_agents,
            &agent_path,
        );
        ctx.caller_spawner = Some(42);

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        let names: Vec<&str> = payload["bg_agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["agent_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["mine"],
            "sub-agent caller must only see its own children; got: {payload}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn success_payload_remains_minimal_on_mail_arrival() {
        // Pin: the rich payload is timeout-path-only. The success
        // path stays as `{message, timed_out: false}` because there's
        // nothing useful to add (the next iteration drains the
        // mailbox; bg-agent state is irrelevant). Inverted regression
        // for the timeout enrichment.
        let (reg, mb) = fresh_registry_with_root();
        let bg_agents = crate::child_agent::new_shared();
        let (_id, _tx) = bg_agents.register_test("explore", "work");
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let agent_path = AgentPath::root();
        let ctx = ctx_with_bg_agents(
            &root,
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
            &reg,
            &bg_agents,
            &agent_path,
        );

        // Pre-publish mail so the wait completes immediately.
        mb.send(sample_mail());

        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 50}))
            .await;
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["timed_out"], false);
        assert_eq!(payload["message"], "Wait completed.");
        assert!(
            payload.get("bg_agents_in_flight").is_none(),
            "success payload must NOT include rich-payload keys; got: {payload}"
        );
        assert!(payload.get("bg_agents").is_none());
        assert!(payload.get("hint").is_none());
    }
}
