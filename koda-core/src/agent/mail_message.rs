//! Convert `InterAgentCommunication` mailbox items into the form
//! koda's inference loop consumes — `(Role, String)` ready for
//! [`crate::persistence::Persistence::insert_message`].
//!
//! # Why this exists (vs. codex's `to_response_input_item`)
//!
//! Codex serializes the full `InterAgentCommunication` as JSON inside
//! a `ResponseInputItem::Message { role: "assistant", phase:
//! Some(MessagePhase::Commentary) }`. The `Commentary` phase tag tells
//! the model "this is side-channel mail, not your own prior output" —
//! that's what makes role-assistant safe.
//!
//! Koda has no `MessagePhase` field on its [`crate::persistence::Message`]
//! type, and adding one is a DB-schema migration that touches every
//! provider adapter + every persistence call site. That migration is
//! its own PR (deferred until a real consumer needs it). For now we
//! use [`crate::persistence::Role::User`] with a clear, structured
//! prefix that preserves every wire field (author, recipient,
//! trigger_turn, content) so:
//!
//! - The model sees mail as user-side input it should react to —
//!   semantically correct given we have no commentary-phase signal.
//! - All wire fields are preserved verbatim, so a future phase-column
//!   migration can re-derive structured form from the prefix without
//!   information loss.
//! - The format is human-readable in the transcript export — useful
//!   for debugging multi-agent traces.
//!
//! # Format
//!
//! ```text
//! [mail from /root/agent_a → /root/agent_b (trigger_turn=true)]
//! <content>
//! ```
//!
//! When `other_recipients` is non-empty, the header gains a `cc`
//! clause matching email convention:
//!
//! ```text
//! [mail from /root/a → /root/b cc: /root/c, /root/d (trigger_turn=true)]
//! <content>
//! ```
//!
//! The arrow + parenthetical header is one line; the content body
//! follows on subsequent lines verbatim. Authorship is unambiguous;
//! the parser is trivial (split on first `\n`).
//!
//! # Phase 3+ migration note
//!
//! When `MessagePhase` (or equivalent) is added to koda's
//! [`crate::persistence::Message`], replace this converter with one
//! that emits `Role::Assistant + Phase::Commentary` with a
//! JSON-serialized body matching codex's wire shape. At that point
//! pin the change to the upstream `to_response_input_item` SHA and
//! delete the deferral note in `inter_agent.rs`'s `Vendor-sync skips`
//! section.

use crate::agent::inter_agent::InterAgentCommunication;
use crate::persistence::Role;

/// Format a mailbox item as `(Role::User, content_string)` suitable
/// for [`crate::persistence::Persistence::insert_message`].
///
/// See module docs for format rationale.
///
/// # Examples
///
/// ```
/// use koda_core::agent::{AgentPath, InterAgentCommunication};
/// use koda_core::agent::mail_message::mail_to_user_message;
/// use koda_core::persistence::Role;
///
/// let mail = InterAgentCommunication {
///     author: AgentPath::root(),
///     recipient: "/root/researcher".parse().unwrap(),
///     other_recipients: Vec::new(),
///     content: "please summarize the design doc".to_string(),
///     trigger_turn: true,
/// };
/// let (role, body) = mail_to_user_message(&mail);
/// assert_eq!(role, Role::User);
/// assert_eq!(
///     body,
///     "[mail from /root → /root/researcher (trigger_turn=true)]\n\
///      please summarize the design doc"
/// );
/// ```
pub fn mail_to_user_message(mail: &InterAgentCommunication) -> (Role, String) {
    let cc_clause = if mail.other_recipients.is_empty() {
        String::new()
    } else {
        let cc_list = mail
            .other_recipients
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(" cc: {cc_list}")
    };
    let body = format!(
        "[mail from {} → {}{} (trigger_turn={})]\n{}",
        mail.author, mail.recipient, cc_clause, mail.trigger_turn, mail.content,
    );
    (Role::User, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentPath;

    fn sample_mail() -> InterAgentCommunication {
        InterAgentCommunication {
            author: AgentPath::root(),
            recipient: "/root/worker".parse().unwrap(),
            other_recipients: Vec::new(),
            content: "do the thing".to_string(),
            trigger_turn: true,
        }
    }

    #[test]
    fn role_is_always_user() {
        // Pin: until koda gains a MessagePhase column, mail must land
        // as Role::User. Role::Assistant would confuse the LLM into
        // thinking it produced the mail itself.
        let (role, _) = mail_to_user_message(&sample_mail());
        assert_eq!(role, Role::User);
    }

    #[test]
    fn header_includes_author_recipient_and_trigger_turn() {
        // Pin the wire-field preservation contract: any future
        // refactor that drops a field would break the
        // information-equivalence guarantee documented in module docs.
        let (_, body) = mail_to_user_message(&sample_mail());
        assert!(body.contains("/root"), "header missing author");
        assert!(body.contains("/root/worker"), "header missing recipient");
        assert!(body.contains("trigger_turn=true"), "header missing trigger_turn");
    }

    #[test]
    fn body_separates_header_and_content_with_single_newline() {
        // The parser contract is "split on first \n". If a future
        // refactor uses \n\n or some other delimiter, that contract
        // breaks silently for any consumer relying on it.
        let (_, body) = mail_to_user_message(&sample_mail());
        let (header, content) = body.split_once('\n').expect("must split on \\n");
        assert!(header.starts_with('['));
        assert_eq!(content, "do the thing");
    }

    #[test]
    fn content_is_preserved_verbatim_including_newlines() {
        // Mail content can be multi-line markdown / code / whatever.
        // The format must not mangle it — a future refactor that
        // escapes newlines or trims whitespace would silently corrupt
        // peer-agent communications.
        let mail = InterAgentCommunication {
            author: AgentPath::root(),
            recipient: "/root/worker".parse().unwrap(),
            other_recipients: Vec::new(),
            content: "line one\nline two\n  indented".to_string(),
            trigger_turn: false,
        };
        let (_, body) = mail_to_user_message(&mail);
        let (_, content) = body.split_once('\n').unwrap();
        assert_eq!(content, "line one\nline two\n  indented");
    }

    #[test]
    fn trigger_turn_false_is_distinguishable_from_true() {
        // Pin: trigger_turn must round-trip into the rendered header.
        // Phase 3's wait_agent tool will read this back to decide
        // whether mail "wakes" the recipient or "FYI's" them.
        let mut mail = sample_mail();
        mail.trigger_turn = false;
        let (_, body) = mail_to_user_message(&mail);
        assert!(body.contains("trigger_turn=false"));
        assert!(!body.contains("trigger_turn=true"));
    }

    #[test]
    fn cc_clause_omitted_when_other_recipients_empty() {
        // Pin: zero-cost when no cc — don't pollute the header with
        // an empty `cc:` clause. Codex's JSON-blob format always
        // includes the field; our human-readable format suppresses it
        // when there's no information to convey.
        let (_, body) = mail_to_user_message(&sample_mail());
        assert!(!body.contains("cc"), "empty cc must not render: {body}");
    }

    #[test]
    fn cc_clause_lists_all_other_recipients_in_order() {
        // Pin the cc preservation contract — mirrors the
        // wire-field-equivalence guarantee for primary recipient.
        // Order matters: Phase 3 may build replies that mirror the
        // recipient set, and stable order makes that deterministic.
        let mail = InterAgentCommunication {
            author: AgentPath::root(),
            recipient: "/root/a".parse().unwrap(),
            other_recipients: vec![
                "/root/b".parse().unwrap(),
                "/root/c".parse().unwrap(),
            ],
            content: "hi all".to_string(),
            trigger_turn: false,
        };
        let (_, body) = mail_to_user_message(&mail);
        assert!(
            body.contains("cc: /root/b, /root/c"),
            "cc clause must list both recipients in order: {body}"
        );
    }
}
