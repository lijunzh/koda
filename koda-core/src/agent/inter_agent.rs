// Vendored from openai/codex (Apache-2.0) — see top-level NOTICE.
// Source: codex-rs/protocol/src/protocol.rs (`InterAgentCommunication`)
// Local modifications:
//   - Dropped `to_response_input_item` / `from_message_content` /
//     `is_message_content` helpers — those depend on Codex's
//     `ResponseInputItem` / `ContentItem` / `MessagePhase` types.
//     The equivalent koda mapping (mailbox → next-turn user message)
//     is Phase 2 of #1325; until then this is just the wire shape.
//   - Dropped `JsonSchema`/`TS` derives.

//! Wire format for messages exchanged between agents over their mailboxes.
//!
//! Author/recipient are typed [`AgentPath`]s — see [`crate::agent::path`].
//! `trigger_turn = true` tells the recipient's session to wake an idle
//! turn (used for "I have something for you to act on" semantics);
//! `false` is "FYI, fold this into your next turn whenever it happens".

use serde::Deserialize;
use serde::Serialize;

use crate::agent::path::AgentPath;

/// One unit of inter-agent communication. See module docs for the
/// `trigger_turn` semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterAgentCommunication {
    /// Path of the agent sending this message.
    pub author: AgentPath,
    /// Primary recipient — the mailbox this message lands in.
    pub recipient: AgentPath,
    /// Other agents copied on the message (informational; does not
    /// itself cause delivery to their mailboxes).
    #[serde(default)]
    pub other_recipients: Vec<AgentPath>,
    /// Free-form text payload.
    pub content: String,
    /// If true, the recipient should wake an idle turn to act on this
    /// message; if false, fold into the next turn whenever it happens.
    pub trigger_turn: bool,
}

impl InterAgentCommunication {
    /// Builds a message with the given fields.
    pub fn new(
        author: AgentPath,
        recipient: AgentPath,
        other_recipients: Vec<AgentPath>,
        content: String,
        trigger_turn: bool,
    ) -> Self {
        Self {
            author,
            recipient,
            other_recipients,
            content,
            trigger_turn,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_minimal() {
        let mail = InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("path"),
            Vec::new(),
            "hello".to_string(),
            false,
        );
        let json = serde_json::to_string(&mail).expect("serialize");
        let back: InterAgentCommunication = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, mail);
    }

    #[test]
    fn serde_roundtrip_with_cc_and_trigger() {
        let mail = InterAgentCommunication::new(
            AgentPath::try_from("/root/planner").expect("path"),
            AgentPath::try_from("/root/worker").expect("path"),
            vec![AgentPath::try_from("/root/reviewer").expect("path")],
            "please review the diff in /tmp/x.patch".to_string(),
            true,
        );
        let json = serde_json::to_string(&mail).expect("serialize");
        let back: InterAgentCommunication = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, mail);
        assert_eq!(back.other_recipients.len(), 1);
        assert!(back.trigger_turn);
    }

    #[test]
    fn other_recipients_default_empty_when_omitted() {
        let json = r#"{
            "author": "/root",
            "recipient": "/root/worker",
            "content": "hi",
            "trigger_turn": false
        }"#;
        let mail: InterAgentCommunication = serde_json::from_str(json).expect("deserialize");
        assert!(mail.other_recipients.is_empty());
    }
}
