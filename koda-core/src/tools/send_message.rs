//! `send_message` peer tool — Phase 3 of #1325.
//!
//! Lets the LLM mail another agent (currently only `/root` —
//! itself — until Phase 4's `spawn_agent` registers child paths).
//! Looks the recipient's mailbox up via the per-session
//! [`MailboxRegistry`](crate::agent::mailbox_registry::MailboxRegistry)
//! and calls `Mailbox::send`.
//!
//! # Mapping to codex
//!
//! Patterned on `codex-rs/core/src/tools/handlers/multi_agents_v2/`
//! `message_tool.rs::handle_message_string_tool` minus the codex
//! plumbing koda doesn't have yet:
//!
//! | codex | koda |
//! |---|---|
//! | `target` parsed via `resolve_agent_target` (path or ThreadId) | `target` parsed as [`AgentPath`] only — no ThreadId yet |
//! | `agent_control.send_inter_agent_communication(thread_id, c)` | `MailboxRegistry::get(&path)` then `Mailbox::send` |
//! | `MessageDeliveryMode::{QueueOnly, TriggerTurn}` | same enum, vendored |
//! | `CollabAgentInteraction{Begin,End}Event` | dropped — koda has no peer-event channel yet |
//! | "Tasks can't be assigned to the root agent" guard | dropped — Phase 3 only has /root, that guard would block every call |
//!
//! The dropped pieces are Phase 4+ work. The kept pieces are
//! exactly the load-bearing core: validate input, resolve recipient,
//! build [`InterAgentCommunication`], deliver.
//!
//! # Why `LocalMutation` and not `ReadOnly`
//!
//! Sending mail mutates the recipient's mailbox state — even though
//! the local FS isn't touched. Classifying as `ReadOnly` would let
//! the tool bypass approval gates that exist precisely to surface
//! peer-effecting operations to the user. `LocalMutation` is the
//! defensive default that keeps the user in the loop until trust
//! mode says otherwise.

use crate::agent::{AgentPath, InterAgentCommunication};
use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Whether the delivery should fold into the recipient's current
/// turn (if any) or sit silently until they explicitly check mail.
///
/// Vendored from codex's `MessageDeliveryMode` (same name, same
/// variants, same semantics) so cross-codebase reasoning stays
/// portable. Phase 3 uses this internally: the `send_message` tool
/// always operates in `TriggerTurn` mode (matching codex's
/// `send_message`); a future `followup_task` tool could use
/// `QueueOnly` (matching codex's tool of the same name).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageDeliveryMode {
    /// Mail lands but does **not** wake an idle recipient. They'll
    /// see it next time they explicitly drain (e.g. `wait_for_mail`
    /// returning, or starting a turn for any other reason).
    QueueOnly,
    /// Mail lands AND sets `trigger_turn = true` so an idle
    /// recipient wakes immediately to process it.
    TriggerTurn,
}

impl MessageDeliveryMode {
    /// Apply this mode to a freshly-built communication. Mirrors
    /// codex's `MessageDeliveryMode::apply` exactly so the wire
    /// format stays bit-for-bit compatible.
    fn apply(self, communication: InterAgentCommunication) -> InterAgentCommunication {
        match self {
            Self::QueueOnly => InterAgentCommunication {
                trigger_turn: false,
                ..communication
            },
            Self::TriggerTurn => InterAgentCommunication {
                trigger_turn: true,
                ..communication
            },
        }
    }
}

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "SendMessage".to_string(),
        description: "Send a message to a peer agent's mailbox. \
            The recipient sees the message as a user-role turn input on their next turn. \
            \n\nPhase 3: only `/root` (yourself) is a valid recipient until child agents \
            are spawnable in Phase 4.\
            \n\nUse this to test the peer-messaging substrate or to leave a note \
            for yourself that surfaces on the next turn."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "The recipient's canonical AgentPath (e.g. `/root`). \
                        Must be a path that's currently registered in this session's \
                        mailbox registry."
                },
                "content": {
                    "type": "string",
                    "description": "The message body. Must not be empty or whitespace-only."
                }
            },
            "required": ["target", "content"]
        }),
    }]
}

/// `SendMessage` — deliver an [`InterAgentCommunication`] to the
/// recipient identified by [`AgentPath`].
pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &'static str {
        "SendMessage"
    }

    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "SendMessage")
            .expect("send_message::definitions() must contain SendMessage")
    }

    fn classify(&self, _args: &Value) -> ToolEffect {
        // See module docs — peer-mailbox state is mutation even if
        // the local FS isn't touched.
        ToolEffect::LocalMutation
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        let registry = match ctx.mailbox_registry {
            Some(reg) => reg,
            None => {
                return crate::tools::ToolResult {
                    output: "SendMessage requires an active session with a mailbox registry; \
                             no registry was attached to this tool execution context."
                        .to_string(),
                    success: false,
                    full_output: None,
                };
            }
        };

        // Argument parsing — keep error messages model-readable so
        // the LLM can self-correct on the next attempt. Codex uses
        // serde + deny_unknown_fields; we use plain Value access so
        // we can return clean per-field errors without dragging in
        // a serde derive for two strings.
        let target_str = match args.get("target").and_then(Value::as_str) {
            Some(s) => s.trim(),
            None => {
                return crate::tools::ToolResult {
                    output: "missing required field `target` (string AgentPath like `/root`)"
                        .to_string(),
                    success: false,
                    full_output: None,
                };
            }
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(s) => s,
            None => {
                return crate::tools::ToolResult {
                    output: "missing required field `content` (string message body)".to_string(),
                    success: false,
                    full_output: None,
                };
            }
        };
        if content.trim().is_empty() {
            // Mirrors codex's `message_content` empty-check verbatim.
            return crate::tools::ToolResult {
                output: "Empty message can't be sent to an agent".to_string(),
                success: false,
                full_output: None,
            };
        }

        // Parse the path. Bad input is the LLM's fault — RespondToModel.
        let target_path = match AgentPath::try_from(target_str) {
            Ok(p) => p,
            Err(e) => {
                return crate::tools::ToolResult {
                    output: format!(
                        "invalid `target` path `{target_str}`: {e}. \
                         Expected a canonical AgentPath like `/root`."
                    ),
                    success: false,
                    full_output: None,
                };
            }
        };

        // Resolve the recipient. Missing path is also model's fault
        // (they passed a path that isn't registered) — list what IS
        // registered so they can pick one.
        let recipient = match registry.get(&target_path) {
            Some(mb) => mb,
            None => {
                let available = registry.list(None);
                let available_str = available
                    .iter()
                    .map(AgentPath::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                return crate::tools::ToolResult {
                    output: format!(
                        "no agent registered at `{target_path}`. \
                         Currently registered paths: [{available_str}]"
                    ),
                    success: false,
                    full_output: None,
                };
            }
        };

        // #1325 Phase 4: stamp the real caller's identity as `author`.
        // Pre-Phase-4 this was hardcoded to `/root` because every
        // session pretended to be the root. Now `caller_agent_path`
        // carries the spawn-tree position threaded down from
        // `KodaSession::agent_path` → `TurnContext.agent_path`
        // → `ToolExecCtx.caller_agent_path`, so spawned children
        // produce attributable mail without colliding with root's
        // identity (the bug Phase 4 of #1325 exists to close).
        let comm = InterAgentCommunication {
            author: ctx.caller_agent_path.clone(),
            recipient: target_path.clone(),
            other_recipients: Vec::new(),
            content: content.to_string(),
            trigger_turn: true, // overridden by `apply` below
        };
        // SendMessage is the codex `send_message` analogue — always
        // TriggerTurn. A future `FollowupTask` tool would call
        // `QueueOnly` against the same delivery primitive.
        let seq = recipient.send(MessageDeliveryMode::TriggerTurn.apply(comm));

        crate::tools::ToolResult {
            output: format!("Delivered message to `{target_path}` (sequence {seq})."),
            success: true,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Mailbox, MailboxRegistry};
    use std::sync::Arc;

    fn fresh_registry_with_root() -> (
        Arc<MailboxRegistry>,
        Arc<Mailbox>,
        crate::agent::MailboxReceiver,
    ) {
        let (mb, rx) = Mailbox::new();
        let mb = Arc::new(mb);
        let reg = Arc::new(MailboxRegistry::new());
        reg.register(AgentPath::root(), Arc::clone(&mb));
        (reg, mb, rx)
    }

    /// Build a ToolExecCtx with a real mailbox registry attached.
    /// `for_test` defaults registry to None, so this is the manual
    /// path the doc on `for_test` warns peer-tool tests need.
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

    #[tokio::test]
    async fn send_to_root_delivers_to_registered_mailbox() {
        // Pin the happy path: registered path resolves and Mailbox::send
        // assigns a sequence number we can observe.
        let (reg, _mb, mut rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"target": "/root", "content": "hi self"}))
            .await;

        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("sequence 1"),
            "first send must report seq 1; got: {}",
            result.output
        );
        let drained = rx.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].content, "hi self");
        assert_eq!(drained[0].recipient.as_str(), "/root");
        assert_eq!(drained[0].author.as_str(), "/root");
        assert!(
            drained[0].trigger_turn,
            "SendMessage maps to TriggerTurn; recipient must wake"
        );
    }

    /// #1325 Phase 4: when a child agent (non-root caller) sends mail,
    /// the delivered envelope's `author` field MUST equal the child's
    /// path, not a hard-coded `/root`. Regression guard for the bug
    /// the whole phase exists to fix — without this, peer agents
    /// can't tell who's talking and the spawn-tree degenerates back
    /// into a flat namespace.
    #[tokio::test]
    async fn child_agent_send_stamps_author_with_child_path() {
        let (reg, _mb, mut rx) = fresh_registry_with_root();
        let (root, cache, fs, caps, bg, trust, policy, skills) = make_test_fixtures();
        // Simulate dispatch from a child agent: `/root/researcher`.
        let child = "/root/researcher".parse::<AgentPath>().unwrap();
        let ctx = ctx_with_registry(
            &root, &cache, &fs, &caps, &bg, &trust, &policy, &skills, &reg, &child,
        );

        let result = SendMessageTool
            .execute(
                &ctx,
                &json!({"target": "/root", "content": "status update"}),
            )
            .await;

        assert!(result.success, "expected success, got: {}", result.output);
        let drained = rx.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].author.as_str(),
            "/root/researcher",
            "author must reflect the caller's spawn-tree path, not /root"
        );
        assert_eq!(drained[0].recipient.as_str(), "/root");
    }

    #[tokio::test]
    async fn missing_target_path_returns_helpful_error() {
        // Pin: model-visible error includes the list of available
        // paths so the LLM can self-correct without re-trying blind.
        let (reg, _mb, _rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(
                &ctx,
                &json!({"target": "/root/ghost", "content": "anyone home?"}),
            )
            .await;

        assert!(!result.success);
        assert!(
            result.output.contains("/root/ghost"),
            "error must echo the bad path; got: {}",
            result.output
        );
        assert!(
            result.output.contains("/root"),
            "error must list available paths so model can self-correct; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn empty_content_rejected_with_codex_message() {
        // Pin the verbatim codex error string — keeps cross-codebase
        // grep-and-find working for anyone debugging from upstream.
        let (reg, _mb, _rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"target": "/root", "content": "   "}))
            .await;

        assert!(!result.success);
        assert_eq!(
            result.output, "Empty message can't be sent to an agent",
            "must match codex's verbatim error string"
        );
    }

    #[tokio::test]
    async fn invalid_path_string_returns_parse_error() {
        let (reg, _mb, _rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"target": "not-a-path", "content": "hello"}))
            .await;

        assert!(!result.success);
        assert!(
            result.output.contains("invalid `target` path"),
            "error must flag the malformed input; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn missing_registry_returns_session_required_error() {
        // Pin: tool gracefully degrades when used outside a session
        // (standalone-ToolRegistry tests, or future contexts where
        // peer-messaging is not configured).
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"target": "/root", "content": "anyone?"}))
            .await;

        assert!(!result.success);
        assert!(
            result.output.contains("requires an active session"),
            "error must mention session requirement; got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn missing_target_field_returns_validation_error() {
        let (reg, _mb, _rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"content": "no target"}))
            .await;
        assert!(!result.success);
        assert!(result.output.contains("missing required field `target`"));
    }

    #[tokio::test]
    async fn missing_content_field_returns_validation_error() {
        let (reg, _mb, _rx) = fresh_registry_with_root();
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

        let result = SendMessageTool
            .execute(&ctx, &json!({"target": "/root"}))
            .await;
        assert!(!result.success);
        assert!(result.output.contains("missing required field `content`"));
    }

    #[test]
    fn classify_is_local_mutation_not_read_only() {
        // Pin: see module docs. ReadOnly would let the tool bypass
        // approval gates that surface peer-effecting operations.
        assert_eq!(
            SendMessageTool.classify(&json!({})),
            ToolEffect::LocalMutation
        );
    }

    #[test]
    fn delivery_mode_apply_overwrites_trigger_turn() {
        // Pin: codex semantics. apply() replaces trigger_turn
        // regardless of incoming value, so the caller doesn't have
        // to remember to set it.
        let base = InterAgentCommunication {
            author: AgentPath::root(),
            recipient: AgentPath::root(),
            other_recipients: Vec::new(),
            content: "x".to_string(),
            trigger_turn: true,
        };
        let queued = MessageDeliveryMode::QueueOnly.apply(base.clone());
        assert!(!queued.trigger_turn, "QueueOnly must clear trigger_turn");
        let triggered = MessageDeliveryMode::TriggerTurn.apply(InterAgentCommunication {
            trigger_turn: false,
            ..base
        });
        assert!(triggered.trigger_turn, "TriggerTurn must set trigger_turn");
    }
}
