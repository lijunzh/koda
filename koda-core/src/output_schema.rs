//! A turn's final reply held to a JSON Schema, for `koda exec
//! --output-schema` and `turn/start`'s `outputSchema`, as Codex's
//! `--output-schema` and Claude Code's `--json-schema` hold one.
//!
//! Codex sends the schema on every model call; the Responses API applies it
//! to the text reply and still lets the model call tools. Koda can't: a
//! Chat Completions server built on llama.cpp turns the schema into a
//! grammar and refuses one alongside tools. So the turn runs as usual, with
//! the schema in the prompt, and its last reply is checked:
//!
//! - **It matches:** nothing more happens.
//! - **It doesn't:** one more model call, the reply's own request with a
//!   message saying what was wrong after it, may not call a tool and has the
//!   schema enforced by the API (`ModelSettings::output_schema`). Its reply,
//!   if it matches, is the turn's last.
//! - **That doesn't match either:** a warning, and the reply stands. `koda
//!   exec` checks the last reply itself and exits 1.

use koda_state::db::Database;
use tokio_util::sync::CancellationToken;

use crate::engine::{EngineEvent, EngineSink};
use crate::providers::{ChatMessage, LlmProvider, StreamChunk, ToolDefinition};
use koda_state::db::Role;
use koda_state::session_store::SessionStore;

/// The most the reformatting call may write.
const MAX_ANSWER_TOKENS: u32 = 16_384;

/// Why `text` doesn't match `schema`, or `None` when it does. The text must
/// be the JSON value alone: a code fence or a sentence around it doesn't
/// match, as a script reading the reply couldn't parse it.
pub fn mismatch(schema: &serde_json::Value, text: &str) -> Option<String> {
    let value: serde_json::Value = match serde_json::from_str(text.trim()) {
        Ok(value) => value,
        Err(e) => return Some(format!("the reply isn't JSON ({e})")),
    };
    let validator = match jsonschema::validator_for(schema) {
        Ok(validator) => validator,
        Err(e) => return Some(format!("the schema is invalid ({e})")),
    };
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .take(5)
        .map(|e| {
            let at = e.instance_path().to_string();
            if at.is_empty() {
                e.to_string()
            } else {
                format!("{at}: {e}")
            }
        })
        .collect();
    (!errors.is_empty()).then(|| errors.join("; "))
}

/// Why `schema` isn't a schema Koda can check against, or `None`.
pub fn invalid_schema(schema: &serde_json::Value) -> Option<String> {
    jsonschema::validator_for(schema)
        .err()
        .map(|e| e.to_string())
}

/// What the prompt says when its turn's last reply must match `schema`.
pub fn instruction(schema: &serde_json::Value) -> String {
    let schema = serde_json::to_string_pretty(schema).unwrap_or_default();
    format!(
        "When you have finished, give your final reply as a single JSON value that \
         matches this JSON Schema, with nothing before or after it and no code fence:\n\
         ```json\n{schema}\n```"
    )
}

/// The turn's request, for the reformatting call.
pub(crate) struct TurnRequest<'a> {
    pub provider: &'a dyn LlmProvider,
    /// The turn's history, its last reply included.
    pub messages: Vec<ChatMessage>,
    pub tool_defs: &'a [ToolDefinition],
    pub settings: &'a crate::config::ModelSettings,
}

/// Check the turn's last reply `reply` against `schema`; if it doesn't
/// match, ask once more with the schema enforced, store and show the new
/// reply. Returns the reply that stands.
pub(crate) async fn conform(
    db: &Database,
    session_id: &str,
    schema: &serde_json::Value,
    reply: &str,
    request: TurnRequest<'_>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let Some(why) = mismatch(schema, reply) else {
        return Ok(());
    };
    let nudge = format!(
        "Your last reply doesn't match the required JSON Schema: {why}. Reply again with \
         only your final answer, as a single JSON value matching the schema."
    );
    db.insert_message(session_id, &Role::User, Some(&nudge), None, None, None)
        .await?;
    let TurnRequest {
        provider,
        mut messages,
        tool_defs,
        settings,
    } = request;
    messages.push(ChatMessage::text("user", &nudge));
    let settings = crate::config::ModelSettings {
        no_tool_calls: true,
        output_schema: Some(schema.clone()),
        max_tokens: Some(
            settings
                .max_tokens
                .map_or(MAX_ANSWER_TOKENS, |cap| cap.max(MAX_ANSWER_TOKENS)),
        ),
        task_budget: None,
        ..settings.clone()
    };
    sink.emit(EngineEvent::SpinnerStart {
        message: "Formatting the reply...".into(),
    });
    let answer = ask(provider, &messages, tool_defs, &settings, sink, cancel).await;
    sink.emit(EngineEvent::SpinnerStop);
    let Some((text, usage)) = answer? else {
        return Ok(());
    };
    let msg_id = db
        .insert_message(
            session_id,
            &Role::Assistant,
            Some(&text),
            None,
            None,
            Some(&crate::inference_helpers::stored_usage(&usage)),
        )
        .await?;
    db.mark_message_complete(msg_id).await?;
    sink.emit(EngineEvent::ResponseStart);
    sink.emit(EngineEvent::TextDelta { text: text.clone() });
    sink.emit(EngineEvent::TextDone);
    if let Some(why) = mismatch(schema, &text) {
        sink.emit(EngineEvent::Warn {
            message: format!("The final reply doesn't match the output schema: {why}."),
        });
    }
    Ok(())
}

/// One call, streamed to its end; `None` when cancelled.
async fn ask(
    provider: &dyn LlmProvider,
    messages: &[ChatMessage],
    tool_defs: &[ToolDefinition],
    settings: &crate::config::ModelSettings,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<(String, crate::providers::TokenUsage)>> {
    let collector = tokio::select! {
        result = provider.chat_stream(messages, tool_defs, settings) => result?,
        _ = cancel.cancelled() => return Ok(None),
    };
    let mut rx = collector.rx;
    let mut text = String::new();
    let mut usage = None;
    loop {
        let chunk = tokio::select! {
            chunk = rx.recv() => chunk,
            _ = cancel.cancelled() => {
                collector.handle.abort();
                return Ok(None);
            }
        };
        match chunk {
            Some(StreamChunk::TextDelta(delta)) => text.push_str(&delta),
            Some(StreamChunk::Done(done)) => usage = Some(done),
            Some(StreamChunk::NetworkError(error)) => anyhow::bail!("{error}"),
            Some(_) => {}
            None => break,
        }
    }
    let usage = usage.unwrap_or_default();
    sink.emit(EngineEvent::ModelUsage {
        usage: crate::engine::UsageTotals::of_call(provider, &usage),
    });
    Ok(Some((text, usage)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "passed": { "type": "boolean" } },
            "required": ["passed"],
            "additionalProperties": false
        })
    }

    #[test]
    fn a_matching_reply_passes() {
        assert_eq!(mismatch(&schema(), " {\"passed\": true}\n"), None);
    }

    #[test]
    fn prose_or_a_fence_around_the_json_does_not_match() {
        assert!(mismatch(&schema(), "Done: {\"passed\": true}").is_some());
        assert!(
            mismatch(&schema(), "```json\n{\"passed\": true}\n```")
                .unwrap()
                .contains("isn't JSON")
        );
    }

    #[test]
    fn a_wrong_shape_says_where() {
        let why = mismatch(&schema(), "{\"passed\": \"yes\"}").unwrap();
        assert!(why.contains("/passed"), "{why}");
        assert!(mismatch(&schema(), "{}").unwrap().contains("passed"));
    }

    #[test]
    fn an_invalid_schema_is_reported() {
        assert!(invalid_schema(&json!({ "type": "nope" })).is_some());
        assert!(invalid_schema(&schema()).is_none());
    }
}
