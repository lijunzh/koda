use agent_client_protocol_schema as acp;
use koda_core::engine::EngineEvent;
use koda_core::engine::sink::EngineSink;
use tokio::sync::mpsc;

/// Translates an internal `EngineEvent` to an ACP `SessionNotification`.
pub fn engine_event_to_acp(
    event: &EngineEvent,
    session_id: &str,
) -> Option<acp::SessionNotification> {
    match event {
        EngineEvent::TextDelta { text } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(text.clone()));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::TextDone => None,
        EngineEvent::ThinkingStart => None,
        EngineEvent::ThinkingDelta { text } => {
            let cb = acp::ContentBlock::Text(acp::TextContent::new(text.clone()));
            Some(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(cb)),
            ))
        }
        EngineEvent::ThinkingDone => None,
        EngineEvent::ResponseStart => None,
        EngineEvent::ToolCallStart { .. } => None, // Not implementing complex tool calls yet to keep compilation clean
        EngineEvent::ToolCallResult { .. } => None,
        EngineEvent::SubAgentStart { .. } => None,
        EngineEvent::SubAgentEnd { .. } => None,
        EngineEvent::ApprovalRequest { .. } => None,
        EngineEvent::ActionBlocked { .. } => None,
        EngineEvent::StatusUpdate { .. } => None,
        EngineEvent::Footer { .. } => None,
        EngineEvent::SpinnerStart { .. } => None,
        EngineEvent::SpinnerStop => None,
        EngineEvent::Info { .. } => None,
        EngineEvent::Warn { .. } => None,
        EngineEvent::Error { .. } => None,
    }
}

pub struct AcpSink {
    session_id: String,
    tx: mpsc::Sender<acp::SessionNotification>,
}

impl AcpSink {
    pub fn new(session_id: String, tx: mpsc::Sender<acp::SessionNotification>) -> Self {
        Self { session_id, tx }
    }
}

impl EngineSink for AcpSink {
    fn emit(&self, event: EngineEvent) {
        if let Some(notification) = engine_event_to_acp(&event, &self.session_id) {
            let _ = self.tx.try_send(notification);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_delta() {
        let event = EngineEvent::TextDelta {
            text: "hello".into(),
        };
        let acp = engine_event_to_acp(&event, "session-1").unwrap();

        assert_eq!(acp.session_id, "session-1".to_string().into());
        match acp.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let block = chunk.content;
                match block {
                    acp::ContentBlock::Text(text_content) => {
                        assert_eq!(text_content.text, "hello");
                    }
                    _ => panic!("Expected text block"),
                }
            }
            _ => panic!("Expected AgentMessageChunk"),
        }
    }
}
