//! Session compaction — summarize old messages to reclaim context.
//!
//! When the conversation grows long, compaction replaces older messages with
//! a concise summary, freeing context window space for new work.
//!
//! ## How it works
//!
//! 1. **Trigger**: user types `/compact`, or auto-compact fires at ~80% context usage
//! 2. **Summarization**: a cheap model (Standard tier) generates a summary of old messages
//! 3. **Replacement**: old messages are archived in the DB, replaced by the summary
//! 4. **Result**: context usage drops, conversation continues with full history awareness
//!
//! ## Auto-compaction
//!
//! When context usage exceeds the threshold (configurable, default ~80%),
//! compaction runs automatically before the next inference call. The user
//! sees a brief "⚡ Compacting..." indicator.
//!
//! ## What's preserved
//!
//! - Summary of all prior conversation and decisions
//! - Progress tracking entries (survive compaction via DB metadata)
//! - Memory facts (injected from `MEMORY.md`, not from conversation)
//! - File ownership state (tracked in SQLite, not in messages)
//!
//! Pure logic, zero UI dependencies. Returns structured results
//! for the caller (TUI or headless) to render however it likes.
//!
//! Compaction uses a cheap model (Standard tier) when available,
//! falling back to the main model. Summarization is a simple task
//! that doesn't need frontier-class reasoning.

use crate::config::ModelSettings;
use crate::db::{Database, Role};
use crate::persistence::Persistence;
use crate::providers::{ChatMessage, LlmProvider};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::RwLock;

/// Minimum number of recent messages to keep verbatim during compaction.
pub const COMPACT_PRESERVE_COUNT: usize = 4;

/// Fraction of history to compact in partial mode (compact oldest half).
const PARTIAL_COMPACT_FRACTION: f64 = 0.5;

/// Below this message count, always do full compaction (partial is overhead).
const PARTIAL_COMPACT_THRESHOLD: usize = 12;

/// Stop auto-compacting after this many consecutive failures.
/// Prevents wasting an API call every turn when compaction is stuck
/// (e.g. history too large for the model, persistent API errors).
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Global consecutive failure counter. Shared across the session.
static CONSECUTIVE_FAILURES: AtomicU32 = AtomicU32::new(0);

/// Reset the failure counter (call after successful compaction or new session).
pub fn reset_compact_failures() {
    CONSECUTIVE_FAILURES.store(0, Ordering::Relaxed);
}

/// Check if the circuit breaker is tripped.
pub fn is_compact_circuit_broken() -> bool {
    CONSECUTIVE_FAILURES.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_FAILURES
}

/// Record a compaction failure. Returns true if the circuit breaker just tripped.
pub fn record_compact_failure() -> bool {
    let prev = CONSECUTIVE_FAILURES.fetch_add(1, Ordering::Relaxed);
    prev + 1 >= MAX_CONSECUTIVE_FAILURES
}

/// Record a compaction success — resets the failure counter.
fn record_compact_success() {
    reset_compact_failures();
}

/// Most output tokens the summary call may use, thinking included.
///
/// `max_tokens` caps the whole reply, and a reasoning model spends part of it
/// thinking before it writes the summary. A fixed 4096 let Flint think for
/// 4,087 tokens and return nothing; at 16,384 it still ran out on three of
/// eleven summary calls (16,339 reasoning tokens before a word of summary),
/// and each miss cost a retry of several minutes. Flint's card asks for a
/// 32K output budget on hard problems, so that is the first cap when the
/// window has the room.
const COMPACT_MAX_OUTPUT_TOKENS: usize = 32_768;

/// Smallest output cap worth sending, however full the window is.
const COMPACT_MIN_OUTPUT_TOKENS: usize = 1_024;

/// Stored in place of a summary the model could not produce, so the replaced
/// messages still leave the context. Same fallback as Codex.
const NO_SUMMARY_MARKER: &str = "(no summary available)";

/// Heading on a stored compaction summary.
const SUMMARY_HEADING: &str = "[Compacted conversation summary]";

/// Heading on what a marker carries from the messages it replaces.
const CARRIED_HEADING: &str = "Carried over from the earlier conversation:";

/// Most text a marker carries from the messages it replaces. A compaction has
/// to shrink the context, so this stays well under a small window.
const MAX_CARRIED_CHARS: usize = 4_000;

/// Maximum number of head-truncation retries when history is too large.
const MAX_TRUNCATION_RETRIES: usize = 3;

/// A reply shorter than this can't describe a conversation. Storing it would
/// lose the task the way an empty summary would, so it counts as none.
const MIN_SUMMARY_CHARS: usize = 8;

/// Fraction of messages to drop on each truncation attempt.
const TRUNCATION_DROP_FRACTION: f64 = 0.2;

/// Result of a successful compaction.
#[derive(Debug)]
pub struct CompactResult {
    /// Number of messages deleted from the database.
    pub deleted: usize,
    /// Estimated tokens in the summary.
    pub summary_tokens: usize,
    /// The model returned no summary, so the messages were replaced by a
    /// "(no summary available)" marker and their content is gone from context.
    pub no_summary: bool,
}

/// Why compaction was skipped (not an error, just a precondition).
#[derive(Debug)]
pub enum CompactSkip {
    /// Session has unresolved tool calls — can't compact safely.
    PendingToolCalls,
    /// Session is too short to compact (contains N messages).
    TooShort(usize),
    /// History is too large for the current model to summarize without data loss.
    /// The user should switch to a model with a larger context window or start a new session.
    HistoryTooLarge,
}

/// Attempt to compact a session.
///
/// Returns `Ok(Ok(result))` on success, `Ok(Err(skip))` if a
/// precondition prevented compaction, or `Err(e)` on failure.
pub async fn compact_session(
    db: &Database,
    session_id: &str,
    max_context_tokens: usize,
    model_settings: &crate::config::ModelSettings,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) -> Result<std::result::Result<CompactResult, CompactSkip>> {
    let prov = provider.read().await;
    compact_session_with_provider(db, session_id, max_context_tokens, model_settings, &**prov).await
}

/// Core compaction logic — accepts `&dyn LlmProvider` directly.
///
/// Uses partial compaction for longer sessions (≥12 messages): only the oldest
/// half of messages are summarized and archived, preserving more recent context
/// verbatim. Short sessions fall back to full compaction (keep last 4).
///
/// Used by the inference loop for pre-flight compaction (where we already
/// have a `&dyn LlmProvider` and don't need the Arc<RwLock<>> wrapper).
pub async fn compact_session_with_provider(
    db: &Database,
    session_id: &str,
    max_context_tokens: usize,
    model_settings: &crate::config::ModelSettings,
    provider: &dyn LlmProvider,
) -> Result<std::result::Result<CompactResult, CompactSkip>> {
    let (history, preserve_count) = match compaction_range(db, session_id).await? {
        Ok(range) => range,
        Err(skip) => return Ok(Err(skip)),
    };
    let compact_count = history.len() - preserve_count;

    // Only summarize the messages being compacted, not the ones we're keeping.
    let to_compact = &history[..compact_count];
    let conversation_text = build_conversation_text(to_compact);

    tracing::info!(
        "Compacting {compact_count}/{} messages (preserving {preserve_count})",
        history.len(),
    );

    // Check if the conversation text fits in the current model's context,
    // leaving room for the summary (and any thinking before it).
    let text_tokens = (conversation_text.len() as f64 / crate::inference_helpers::CHARS_PER_TOKEN)
        as usize
        + crate::inference_helpers::SYSTEM_PROMPT_OVERHEAD;
    let available = max_context_tokens
        .saturating_sub((max_context_tokens / 4).clamp(4096, COMPACT_MAX_OUTPUT_TOKENS));

    // If history fits, use it as-is. Otherwise, progressively truncate
    // the oldest messages until it fits (up to MAX_TRUNCATION_RETRIES).
    let final_text = if text_tokens <= available {
        conversation_text
    } else {
        match truncate_until_fits(to_compact, available) {
            Some(text) => text,
            None => return Ok(Err(CompactSkip::HistoryTooLarge)),
        }
    };

    let summary_prompt = build_summary_prompt(&final_text);
    let prompt_tokens = (summary_prompt.len() as f64 / crate::inference_helpers::CHARS_PER_TOKEN)
        as usize
        + crate::inference_helpers::SYSTEM_PROMPT_OVERHEAD;
    // What the window has left for the reply: thinking plus the summary.
    let room = max_context_tokens
        .saturating_sub(prompt_tokens)
        .max(COMPACT_MIN_OUTPUT_TOKENS);
    let first_cap = room.min(COMPACT_MAX_OUTPUT_TOKENS);
    // A reply cut off before the summary is complete gets one retry with
    // twice the room, as long as the window has it. The Claude API's
    // compaction guidance is the same: a `max_tokens` stop means the summary
    // was cut off, resend with a larger cap.
    let retry_cap = room.min(first_cap * 2);

    let messages = vec![ChatMessage::text("user", &summary_prompt)];
    // Same model and the session's own reasoning settings: Claude Code and
    // Codex both summarize that way. Only the output cap and temperature differ.
    let mut cap = first_cap;
    let summary = loop {
        let compact_settings = ModelSettings {
            max_tokens: Some(cap as u32),
            temperature: Some(0.3),
            ..model_settings.clone()
        };
        let response = provider.chat(&messages, &[], &compact_settings).await?;
        let content = response.content.as_deref().unwrap_or("");
        let (mut summary, mut complete) = summary_from_reply(content);
        // A finished reply that never opened <summary> still holds what the
        // model worked out: Gemini sent 6-9K of analysis like this. It beats
        // the marker.
        if summary.is_empty() && response.usage.stop_reason != "max_tokens" {
            tracing::warn!(
                "Compact reply has no usable <summary> block; keeping its {} chars of text",
                content.len()
            );
            summary = analysis_text(content);
            complete = true;
        }
        if summary.chars().count() < MIN_SUMMARY_CHARS {
            summary.clear();
        }
        let cut_off = response.usage.stop_reason == "max_tokens" || !complete;
        if !cut_off && !summary.is_empty() {
            break Some(summary);
        }
        if cut_off && retry_cap > cap {
            tracing::warn!(
                "Compact summary was cut off at the {cap}-token cap ({} chars of text, \
                 reasoning used the rest); retrying with {retry_cap}",
                content.len()
            );
            cap = retry_cap;
            continue;
        }
        if summary.is_empty() {
            tracing::warn!(
                "Compact reply has no usable summary: {} chars, stop reason {:?}",
                content.len(),
                response.usage.stop_reason
            );
            break None;
        }
        // Still cut off with no room left to retry: a partial summary beats
        // the marker, but say so.
        tracing::warn!(
            "Compact summary is still cut off at the {cap}-token cap; keeping the \
             {} chars that came back",
            summary.len()
        );
        break Some(summary);
    };

    let no_summary = summary.is_none();
    let summary = summary.unwrap_or_else(|| {
        tracing::warn!(
            "Compact summary is empty or too short; replacing {compact_count} messages \
             with a \"{NO_SUMMARY_MARKER}\" marker"
        );
        no_summary_text(&history[..compact_count])
    });
    let deleted = replace_with_summary(db, session_id, &summary, history[compact_count].id).await?;

    if no_summary {
        // The history is smaller, but the model is still not summarizing.
        record_compact_failure();
    } else {
        record_compact_success();
    }

    Ok(Ok(CompactResult {
        deleted,
        summary_tokens: summary.len() / 4,
        no_summary,
    }))
}

/// Drop the compactable messages for a marker, with no model call.
///
/// The last resort when summarizing keeps failing and the context is already
/// past the window: sending it as-is would only have the server trim the
/// prompt where the model can't see what went missing.
pub async fn compact_without_summary(
    db: &Database,
    session_id: &str,
) -> Result<std::result::Result<CompactResult, CompactSkip>> {
    let (history, preserve_count) = match compaction_range(db, session_id).await? {
        Ok(range) => range,
        Err(skip) => return Ok(Err(skip)),
    };
    tracing::warn!(
        "Dropping {} messages without a summary to fit the window",
        history.len() - preserve_count
    );
    let compact_count = history.len() - preserve_count;
    let text = no_summary_text(&history[..compact_count]);
    let deleted = replace_with_summary(db, session_id, &text, history[compact_count].id).await?;
    Ok(Ok(CompactResult {
        deleted,
        summary_tokens: text.len() / 4,
        no_summary: true,
    }))
}

/// The marker, plus what the model can't work without: the user's messages and
/// any earlier summary from the part being replaced (#1452).
///
/// Newest first until [`MAX_CARRIED_CHARS`] runs out, the same order Codex
/// keeps user messages through compaction, then back in conversation order.
fn no_summary_text(replaced: &[crate::db::Message]) -> String {
    let mut budget = MAX_CARRIED_CHARS;
    let mut carried = Vec::new();
    for msg in replaced.iter().rev() {
        let Some(text) = msg.content.as_deref().map(str::trim) else {
            continue;
        };
        let keep = match msg.role {
            Role::User => msg.tool_call_id.is_none(),
            Role::System => text.starts_with(SUMMARY_HEADING),
            _ => false,
        };
        if !keep || text.is_empty() {
            continue;
        }
        if budget == 0 {
            break;
        }
        // An earlier marker carries its text forward; don't nest the marker.
        let text = [SUMMARY_HEADING, NO_SUMMARY_MARKER, CARRIED_HEADING]
            .iter()
            .fold(text, |text, prefix| {
                text.strip_prefix(prefix).unwrap_or(text).trim()
            });
        let end = text
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|end| *end <= budget)
            .last()
            .unwrap_or(0);
        budget -= end;
        carried.push(&text[..end]);
    }
    carried.reverse();
    let mut out = NO_SUMMARY_MARKER.to_string();
    if !carried.is_empty() {
        out.push_str("\n\n");
        out.push_str(CARRIED_HEADING);
        out.push_str("\n\n");
        out.push_str(&carried.join("\n\n"));
    }
    out
}

/// Load the history and decide how many trailing messages stay verbatim.
async fn compaction_range(
    db: &Database,
    session_id: &str,
) -> Result<std::result::Result<(Vec<crate::db::Message>, usize), CompactSkip>> {
    if db.has_pending_tool_calls(session_id).await.unwrap_or(false) {
        return Ok(Err(CompactSkip::PendingToolCalls));
    }

    let history = db.load_context(session_id).await?;

    if history.len() < 4 {
        return Ok(Err(CompactSkip::TooShort(history.len())));
    }

    // Partial: compact the oldest half, keep the newest half.
    // Full: compact everything except the last COMPACT_PRESERVE_COUNT.
    let preserve_count = compute_preserve_count(history.len());
    if history.len() <= preserve_count {
        return Ok(Err(CompactSkip::TooShort(history.len())));
    }
    let split = tail_start(&history, history.len() - preserve_count);
    if split == 0 {
        return Ok(Err(CompactSkip::TooShort(history.len())));
    }
    let preserve_count = history.len() - split;
    Ok(Ok((history, preserve_count)))
}

async fn replace_with_summary(
    db: &Database,
    session_id: &str,
    summary: &str,
    keep_from_id: i64,
) -> Result<usize> {
    let compact_message = format!("{SUMMARY_HEADING}\n\n{summary}");
    // Debug only: this is conversation text, which the default logs leave out.
    tracing::debug!("Stored compaction summary:\n{summary}");
    db.compact_session_before(session_id, &compact_message, keep_from_id)
        .await
}

/// Move a split off any tool result, so a call and its results stay together.
///
/// A result cut off from its call is dropped at load, which would leave it in
/// neither the summary nor the tail. The batch goes to the summary, unless it
/// runs to the end of the history: then it stays, whole.
fn tail_start(history: &[crate::db::Message], split: usize) -> usize {
    let is_result = |i: &usize| history[*i].role == Role::Tool;
    match (split..history.len()).find(|i| !is_result(i)) {
        Some(start) => start,
        None => (0..split).rev().find(|i| !is_result(i)).unwrap_or(0),
    }
}

/// Compute how many messages to preserve during compaction.
///
/// - Short sessions (<12 messages): full compaction, keep last 4.
/// - Longer sessions: partial compaction, keep the newest ~50%.
///
/// This preserves more recent context verbatim in long sessions,
/// producing a smaller, more focused summary of just the oldest half.
fn compute_preserve_count(total: usize) -> usize {
    if total < PARTIAL_COMPACT_THRESHOLD {
        COMPACT_PRESERVE_COUNT
    } else {
        let keep = (total as f64 * (1.0 - PARTIAL_COMPACT_FRACTION)).ceil() as usize;
        keep.max(COMPACT_PRESERVE_COUNT)
    }
}

/// Build the 9-section summarization prompt.
///
/// Adapted from CC's compaction prompt. Uses an `<analysis>` scratchpad block
/// (stripped before storing) that demonstrably improves summary quality.
fn build_summary_prompt(conversation_text: &str) -> String {
    format!(
        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\
         Tool calls will be REJECTED and will waste your only turn.\n\
         Your entire response must be plain text: an <analysis> block followed by a <summary> block.\n\
         \n\
         Your task is to create a detailed summary of the conversation so far, paying close \n\
         attention to the user's explicit requests and your previous actions.\n\
         This summary should be thorough in capturing technical details, code patterns, and \n\
         architectural decisions that would be essential for continuing development work \n\
         without losing context.\n\
         \n\
         Before providing your final summary, wrap your analysis in <analysis> tags to \n\
         organize your thoughts and ensure you've covered all necessary points. In your analysis:\n\
         \n\
         1. Chronologically analyze each message. For each section thoroughly identify:\n\
            - The user's explicit requests and intents\n\
            - Your approach to addressing them\n\
            - Key decisions, technical concepts and code patterns\n\
            - Specific details: file names, code snippets, function signatures, file edits\n\
            - Errors encountered and how they were fixed\n\
            - Specific user feedback, especially corrections\n\
         2. Double-check for technical accuracy and completeness.\n\
         \n\
         Your summary should include these sections:\n\
         \n\
         1. **Primary Request and Intent**: Capture ALL of the user's explicit requests in detail.\n\
         2. **Key Technical Concepts**: List all important technologies and frameworks discussed.\n\
         3. **Files and Code Sections**: Enumerate specific files examined, modified, or created. \n\
            Include code snippets where applicable and a summary of why each file matters.\n\
            **Be exhaustive about file paths** — once compaction runs, the only record of\n\
            files touched in the compacted range is this summary, so missing a path means\n\
            losing it for the rest of the session. Group as: created / modified / deleted.\n\
         4. **Errors and Fixes**: List all errors and how they were resolved. Note user feedback.\n\
         5. **Problem Solving**: Document problems solved and ongoing troubleshooting.\n\
         6. **All User Messages**: List ALL user messages (not tool results). Critical for \n\
            preserving feedback and changing intent.\n\
         7. **Pending Tasks**: Outline anything unfinished or deferred.\n\
            **Preserve every outstanding TodoWrite item verbatim** with its current status\n\
            (pending / in_progress). Compaction is the only mechanism that defends plan\n\
            continuity across context-window pressure (`DESIGN.md § Progress Tracking:\n\
            Model-Owned, History-Persisted, Engine-Surfaced`) — the system prompt does NOT\n\
            re-inject the todo list, so anything dropped here is gone.\n\
         8. **Current Work**: Describe precisely what was being worked on immediately before \n\
            this summary. Include file names and code snippets.\n\
         9. **Optional Next Step**: Only if directly in line with the user's most recent \n\
            explicit request. Include direct quotes from the conversation to prevent drift.\n\
         \n\
         Format your response as:\n\
         \n\
         <analysis>\n\
         [Your thought process ensuring all points are covered]\n\
         </analysis>\n\
         \n\
         <summary>\n\
         1. Primary Request and Intent:\n\
            [Detailed description]\n\
         ...\n\
         </summary>\n\
         \n\
         REMINDER: Do NOT call any tools. Respond with plain text only.\n\
         \n\
         ---\n\n{conversation_text}"
    )
}

/// The summary text in a reply, and whether the reply was complete.
///
/// The prompt asks for an `<analysis>` block followed by a `<summary>` block.
/// A reply that opens `<analysis>` and never reaches `<summary>` is the
/// model's preamble, cut off before the summary: it carries no summary at
/// all, so it comes back empty and incomplete. A `<summary>` that is never
/// closed is a summary cut off part-way: its text is kept, flagged
/// incomplete, so the caller can retry with more room and keep the partial
/// text only when it can't. Plain text with no tags, from a model that
/// ignores the format, is accepted as it is.
fn summary_from_reply(content: &str) -> (String, bool) {
    let has_analysis = content.contains("<analysis>");
    let has_summary = content.contains("<summary>");
    if has_analysis && !has_summary {
        return (String::new(), false);
    }
    if has_summary && !content.contains("</summary>") {
        // `strip_analysis_block` leaves an unclosed `<summary>` tag in place;
        // take what follows it.
        let stripped = strip_analysis_block(content);
        let text = stripped
            .split_once("<summary>")
            .map_or(stripped.as_str(), |(_, rest)| rest)
            .trim()
            .to_string();
        return (text, false);
    }
    (strip_analysis_block(content), true)
}

/// A reply's text with the summary prompt's tags removed.
fn analysis_text(content: &str) -> String {
    ["<analysis>", "</analysis>", "<summary>", "</summary>"]
        .iter()
        .fold(content.to_string(), |text, tag| text.replace(tag, ""))
        .trim()
        .to_string()
}

/// Strip the `<analysis>` scratchpad block from the summary.
///
/// The analysis block improves summary quality (model "thinks" before writing)
/// but has no informational value once the summary is written. Stripping it
/// saves tokens in the ongoing context.
///
/// # Examples
///
/// ```
/// use koda_core::compact::strip_analysis_block;
///
/// let input = "<analysis>\nthinking...\n</analysis>\n\n<summary>\nThe result.\n</summary>";
/// let result = strip_analysis_block(input);
/// assert_eq!(result, "The result.");
///
/// // Plain text without tags passes through unchanged:
/// assert_eq!(strip_analysis_block("just text"), "just text");
/// ```
pub fn strip_analysis_block(summary: &str) -> String {
    // Find the block before touching the analysis: removing that first loses
    // a summary nested inside it, or cuts at a tag it only quotes.
    if let Some(end) = summary.rfind("</summary>")
        && let Some(start) = summary_open_tag(&summary[..end])
    {
        let text = summary[start + SUMMARY_OPEN.len()..end].trim();
        if !text.is_empty() {
            return collapse_blank_lines(text);
        }
    }

    // Remove <analysis>...</analysis> including the tags
    let stripped = if let Some(start) = summary.find("<analysis>") {
        if let Some(end) = summary.find("</analysis>") {
            let after = end + "</analysis>".len();
            format!("{}{}", &summary[..start], &summary[after..])
        } else {
            summary.to_string()
        }
    } else {
        summary.to_string()
    };

    // Extract content from <summary> tags if present
    let stripped = if let Some(start) = stripped.find("<summary>") {
        if let Some(end) = stripped.find("</summary>") {
            let content_start = start + "<summary>".len();
            stripped[content_start..end].trim().to_string()
        } else {
            stripped
        }
    } else {
        stripped
    };

    collapse_blank_lines(&stripped)
}

const SUMMARY_OPEN: &str = "<summary>";

/// Where the real summary opens, in a reply cut at its last `</summary>`.
///
/// Models quote this tag, in the analysis and inside the summary (the prompt
/// names it, and section 6 repeats the user's messages), so neither the first
/// nor the last one can be trusted. The real one follows the analysis, and
/// usually starts a line.
fn summary_open_tag(reply: &str) -> Option<usize> {
    let starts_line = |at: usize| {
        reply[..at]
            .rfind('\n')
            .is_none_or(|nl| reply[nl + 1..at].trim().is_empty())
    };
    let opens_from = |from: usize| {
        reply[from..]
            .match_indices(SUMMARY_OPEN)
            .map(move |(at, _)| from + at)
    };
    let after_analysis = reply
        .match_indices("</analysis>")
        .find(|(at, _)| starts_line(*at))
        .or(reply.rmatch_indices("</analysis>").next())
        .map_or(0, |(at, tag)| at + tag.len());

    opens_from(after_analysis)
        .find(|at| starts_line(*at))
        .or_else(|| opens_from(after_analysis).next())
        .or_else(|| opens_from(0).find(|at| starts_line(*at)))
        .or_else(|| opens_from(0).next())
}

/// Clean up extra whitespace.
fn collapse_blank_lines(stripped: &str) -> String {
    let mut result = String::new();
    let mut prev_empty = false;
    for line in stripped.lines() {
        let is_empty = line.trim().is_empty();
        if is_empty && prev_empty {
            continue;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
        prev_empty = is_empty;
    }
    result.trim().to_string()
}

/// Progressively drop oldest messages until the conversation text fits
/// in the available token budget. Keeps at least `COMPACT_PRESERVE_COUNT`
/// recent messages. Returns `None` if it can't fit after max retries.
fn truncate_until_fits(history: &[crate::db::Message], available_tokens: usize) -> Option<String> {
    let total = history.len();
    // Minimum messages to keep: the preserved tail + at least 1 to summarize
    let min_keep = COMPACT_PRESERVE_COUNT + 1;
    if total <= min_keep {
        return None;
    }

    let mut drop_count = 0usize;
    for attempt in 0..MAX_TRUNCATION_RETRIES {
        // Drop 20% of remaining summarizable messages each attempt
        let summarizable = total.saturating_sub(drop_count);
        let to_drop = (summarizable as f64 * TRUNCATION_DROP_FRACTION).ceil() as usize;
        drop_count += to_drop.max(1); // always drop at least 1

        // Never drop so many that we have fewer than min_keep
        if total.saturating_sub(drop_count) < min_keep {
            drop_count = total - min_keep;
        }

        let truncated = &history[drop_count..];
        let text = build_conversation_text(truncated);
        let text_tokens = (text.len() as f64 / crate::inference_helpers::CHARS_PER_TOKEN) as usize
            + crate::inference_helpers::SYSTEM_PROMPT_OVERHEAD;

        tracing::info!(
            "Truncation attempt {}: dropped {drop_count}/{total} messages, \
             ~{text_tokens} tokens (budget: {available_tokens})",
            attempt + 1,
        );

        if text_tokens <= available_tokens {
            return Some(text);
        }
    }

    None
}

/// Format conversation history into a single string for the summarizer.
///
/// Per-message content is truncated to 2000 chars (individual tool outputs
/// can be huge but add little summarization value beyond a preview).
/// No total cap — the capacity check in `compact_session_with_provider`
/// guarantees the result fits in the model's context window.
fn build_conversation_text(history: &[crate::db::Message]) -> String {
    let mut text = String::new();
    for msg in history {
        let role = msg.role.as_str();
        if let Some(ref content) = msg.content {
            let truncated: String = content.chars().take(2000).collect();
            text.push_str(&format!("[{role}]: {truncated}\n\n"));
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            let truncated: String = tool_calls.chars().take(500).collect();
            text.push_str(&format!("[{role} tool_calls]: {truncated}\n\n"));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Message;

    fn make_msg(role: &str, content: Option<&str>, tool_calls: Option<&str>) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.parse().unwrap_or(Role::User),
            content: content.map(String::from),
            full_content: None,
            tool_calls: tool_calls.map(String::from),
            tool_call_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    #[test]
    fn test_circuit_breaker() {
        reset_compact_failures();
        assert!(!is_compact_circuit_broken());

        assert!(!record_compact_failure()); // 1st
        assert!(!is_compact_circuit_broken());

        assert!(!record_compact_failure()); // 2nd
        assert!(!is_compact_circuit_broken());

        assert!(record_compact_failure()); // 3rd — trips
        assert!(is_compact_circuit_broken());

        // Reset should untrip
        reset_compact_failures();
        assert!(!is_compact_circuit_broken());
    }

    #[test]
    fn test_empty_history() {
        assert_eq!(build_conversation_text(&[]), "");
    }

    #[test]
    fn test_basic_conversation() {
        let msgs = vec![
            make_msg("user", Some("hello"), None),
            make_msg("assistant", Some("hi"), None),
        ];
        let text = build_conversation_text(&msgs);
        assert!(text.contains("[user]: hello"));
        assert!(text.contains("[assistant]: hi"));
    }

    #[test]
    fn test_truncates_long_content_per_message() {
        let long = "x".repeat(3000);
        let msgs = vec![make_msg("user", Some(&long), None)];
        let text = build_conversation_text(&msgs);
        // Each msg content capped at 2000 chars
        assert!(text.len() < 2100);
    }

    #[test]
    fn test_no_total_cap() {
        // 50 messages × 500 chars each = 25K chars — no cap applied
        let content = "y".repeat(500);
        let msgs: Vec<_> = (0..50)
            .map(|_| make_msg("user", Some(&content), None))
            .collect();
        let text = build_conversation_text(&msgs);
        // All 50 messages should be included (no 20K cap)
        assert!(text.len() > 20_000);
        assert!(!text.contains("truncated"));
    }

    #[test]
    fn test_multibyte_boundary_safe() {
        // Put emoji right at the 2000-char boundary
        let mut content = "a".repeat(1999);
        content.push('\u{1f43b}'); // bear emoji (4 bytes)
        content.push_str("after");
        let msgs = vec![make_msg("user", Some(&content), None)];
        let text = build_conversation_text(&msgs);
        // Should not panic on char boundary
        assert!(text.contains("\u{1f43b}") || !text.contains("after"));
    }

    #[test]
    fn test_tool_calls_included() {
        let msgs = vec![make_msg("assistant", None, Some("{\"name\": \"Read\"}"))];
        let text = build_conversation_text(&msgs);
        assert!(text.contains("tool_calls"));
        assert!(text.contains("Read"));
    }

    #[test]
    fn test_none_content_skipped() {
        let msgs = vec![make_msg("tool", None, None)];
        let text = build_conversation_text(&msgs);
        assert_eq!(text, "");
    }

    #[test]
    fn test_strip_analysis_block() {
        let input = "<analysis>\nthinking here\n</analysis>\n\n<summary>\n1. Primary Request:\n   Build a thing\n</summary>";
        let result = strip_analysis_block(input);
        assert!(result.contains("Primary Request"));
        assert!(!result.contains("<analysis>"));
        assert!(!result.contains("thinking here"));
        assert!(!result.contains("<summary>"));
    }

    #[test]
    fn test_strip_analysis_no_tags() {
        let input = "Just a plain summary";
        assert_eq!(strip_analysis_block(input), "Just a plain summary");
    }

    #[test]
    fn test_strip_analysis_only_summary_tags() {
        let input = "<summary>\nThe good stuff\n</summary>";
        let result = strip_analysis_block(input);
        assert_eq!(result, "The good stuff");
    }

    #[test]
    fn test_truncate_until_fits_drops_oldest() {
        // 20 messages, each ~50 chars
        let msgs: Vec<_> = (0..20)
            .map(|i| {
                make_msg(
                    "user",
                    Some(&format!("Message number {i} with some padding text here")),
                    None,
                )
            })
            .collect();

        // Budget: fits ~half the messages but not all 20
        // 20 msgs × ~50 chars / 3.5 ≈ 286 tokens + 100 overhead ≈ 386
        // Want to force truncation: set budget to ~250 tokens
        let result = truncate_until_fits(&msgs, 250);
        assert!(result.is_some(), "should succeed after truncation");
        let text = result.unwrap();
        // Should contain the last messages but not the first
        assert!(text.contains("Message number 19"));
        assert!(!text.contains("Message number 0"));
    }

    #[test]
    fn test_truncate_until_fits_too_few_messages() {
        // Only COMPACT_PRESERVE_COUNT + 1 = 5 messages, can't drop any
        let msgs: Vec<_> = (0..5)
            .map(|_| make_msg("user", Some(&"x".repeat(10_000)), None))
            .collect();
        // Tiny budget that can't fit even 5 messages
        let result = truncate_until_fits(&msgs, 10);
        assert!(result.is_none());
    }

    #[test]
    fn test_truncate_until_fits_already_fits() {
        let msgs: Vec<_> = (0..10)
            .map(|i| make_msg("user", Some(&format!("Short {i}")), None))
            .collect();
        // Huge budget
        let result = truncate_until_fits(&msgs, 100_000);
        assert!(result.is_some());
        let text = result.unwrap();
        // First attempt drops 20% but still fits, so it drops
        // We just check it returns something valid
        assert!(text.contains("Short 9"));
    }

    #[test]
    fn test_compute_preserve_count_short_sessions() {
        // Below threshold: always keep COMPACT_PRESERVE_COUNT (4)
        assert_eq!(compute_preserve_count(4), 4);
        assert_eq!(compute_preserve_count(8), 4);
        assert_eq!(compute_preserve_count(11), 4);
    }

    #[test]
    fn test_compute_preserve_count_partial() {
        // At threshold (12): keep ceil(12 * 0.5) = 6
        assert_eq!(compute_preserve_count(12), 6);
        // 20 messages: keep ceil(20 * 0.5) = 10
        assert_eq!(compute_preserve_count(20), 10);
        // 50 messages: keep 25
        assert_eq!(compute_preserve_count(50), 25);
        // 100 messages: keep 50
        assert_eq!(compute_preserve_count(100), 50);
    }

    #[test]
    fn test_compute_preserve_count_never_below_minimum() {
        // Even at threshold, result must be >= COMPACT_PRESERVE_COUNT
        for n in 0..200 {
            assert!(compute_preserve_count(n) >= COMPACT_PRESERVE_COUNT);
        }
    }

    // ── build_summary_prompt ────────────────────────────────────────────

    #[test]
    fn test_build_summary_prompt_embeds_conversation() {
        let text = build_summary_prompt("[user]: hello\n\n[assistant]: hi\n\n");
        assert!(
            text.contains("[user]: hello"),
            "prompt should embed the conversation text verbatim"
        );
        assert!(text.contains("[assistant]: hi"));
    }

    #[test]
    fn test_build_summary_prompt_instructs_no_tool_calls() {
        let text = build_summary_prompt("some conversation");
        assert!(
            text.contains("Do NOT call any tools"),
            "prompt must forbid tool calls"
        );
        assert!(text.contains("CRITICAL"));
    }

    #[test]
    fn test_build_summary_prompt_requests_analysis_and_summary_tags() {
        let text = build_summary_prompt("some conversation");
        assert!(
            text.contains("<analysis>"),
            "prompt should ask for <analysis> block"
        );
        assert!(
            text.contains("<summary>"),
            "prompt should ask for <summary> block"
        );
    }

    // ── build_conversation_text edge cases ────────────────────────────

    #[test]
    fn test_build_conversation_text_tool_calls_truncated_at_500() {
        let long_tc = "T".repeat(600);
        let msgs = vec![make_msg("assistant", None, Some(&long_tc))];
        let text = build_conversation_text(&msgs);
        // 500 char cap on tool_calls
        assert!(
            text.len() <= 550,
            "tool_calls should be capped at 500 chars"
        );
    }

    #[test]
    fn test_build_conversation_text_both_content_and_tool_calls() {
        let msgs = vec![make_msg(
            "assistant",
            Some("I will read the file"),
            Some("{\"name\": \"Read\"}"),
        )];
        let text = build_conversation_text(&msgs);
        assert!(text.contains("I will read the file"));
        assert!(text.contains("tool_calls"));
    }

    // ── strip_analysis_block edge cases ──────────────────────────────

    #[test]
    fn test_strip_analysis_unclosed_tag_passthrough() {
        // If <analysis> has no closing tag, leave the text alone.
        let input = "<analysis>\nthinking...\n1. Primary Request: build a thing";
        let result = strip_analysis_block(input);
        assert!(
            result.contains("thinking"),
            "unclosed analysis tag should leave text intact"
        );
    }

    #[test]
    fn test_strip_analysis_trims_extra_whitespace() {
        let input = "<analysis>\nthink\n</analysis>\n\n\n\n<summary>\nClean content\n</summary>";
        let result = strip_analysis_block(input);
        // Collapsed blank lines, no leading/trailing whitespace
        assert!(!result.starts_with('\n'));
        assert!(!result.ends_with('\n'));
        assert_eq!(result, "Clean content");
    }

    // ── circuit breaker ───────────────────────────────────────────────────────
    // Covered by test_circuit_breaker above. Removed duplicate tests
    // that raced on the global CONSECUTIVE_FAILURES AtomicU32 when
    // running in parallel.
}
