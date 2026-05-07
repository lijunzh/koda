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
//! | `session.has_pending_mailbox_items().await` (fast path) | omitted — the watch's `borrow_and_update` does the equivalent |
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

use crate::agent::AgentPath;
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
             turn cleanly. Use it after `SendMessage` if you expect a peer to reply.",
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

        // Phase 3: caller is always /root. Phase 4 reads ctx.caller_path
        // (or equivalent) to support child agents.
        let own_mailbox = match registry.get(&AgentPath::root()) {
            Some(mb) => mb,
            None => {
                // Substrate invariant violation — KodaSession::new
                // pre-registers /root unconditionally. If we hit this,
                // either the registry was rebuilt without /root or the
                // session-construction invariant is broken.
                return ToolResult {
                    output: "WaitForMail: own mailbox (/root) is not registered. \
                             This is a session-construction invariant violation."
                        .to_string(),
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
        let payload = json!({
            "message": if timed_out { "Wait timed out." } else { "Wait completed." },
            "timed_out": timed_out,
        });
        ToolResult {
            output: payload.to_string(),
            success: true,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{InterAgentCommunication, Mailbox, MailboxRegistry};
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
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
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
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
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
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
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
    async fn pre_subscribe_mail_does_not_falsely_complete() {
        // Pin the borrow_and_update semantics: mail that arrived
        // BEFORE the wait started is the responsibility of the
        // previous turn's drain (or the next turn's drain). It must
        // NOT cause the wait to falsely return "completed" —
        // otherwise an LLM that sends-then-waits would always see
        // its own past send as a "new" arrival, defeating the point.
        let (reg, mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
        );

        // Pre-existing mail.
        mb.send(sample_mail());
        mb.send(sample_mail());

        // Tiny timeout — if the wait fires on pre-existing mail,
        // it'll return completed=true. Correct behavior is timed_out=true.
        let result = WaitForMailTool
            .execute(&ctx, &json!({"timeout_ms": 30}))
            .await;

        assert!(result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            payload["timed_out"], true,
            "pre-existing mail must NOT wake a new wait; got: {}",
            result.output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_zero_or_negative_returns_codex_error() {
        let (reg, _mb) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
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
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg,
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
}
