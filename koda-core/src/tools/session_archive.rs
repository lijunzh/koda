//! `SearchSessions` and `ReadSession`: the model's way to what was said in
//! the user's other sessions, as Claude Code's desktop app gives its model
//! `search_session_transcripts` and Codex's TUI gives its model
//! `read_thread`.
//!
//! The tools hold no database handle. The host that opened the session
//! gives its registry a
//! [`SessionArchive`](crate::tools::session_archive::SessionArchive)
//! ([`crate::tools::ToolRegistry::set_session_archive`]) with the calling
//! session fixed inside it, and that decides which sessions the caller may
//! see; this module asks, bounds what comes back and writes it as text.
//!
//! An old conversation can hold text from the web, so everything read from
//! one goes to the model as external content ([`crate::untrusted::wrap`]),
//! as a fetched page does.

use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult, wrap_result};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

/// How many sessions a search returns without a `limit`, as Claude Code's
/// `search_session_transcripts`, and at most.
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 50;

/// `ReadSession`'s bounds, Codex's for `read_thread`: how many turns a call
/// reads without a `turn_limit` and at most, how many of a turn's items,
/// and how much of each text.
const DEFAULT_TURN_LIMIT: usize = 1;
const MAX_TURN_LIMIT: usize = 10;
const MAX_ITEMS_PER_TURN: usize = 20;
const MAX_TEXT_CHARS: usize = 2_000;
/// How much conversation one `ReadSession` returns, in characters.
const MAX_READ_CHARS: usize = 20_000;

/// How much of a session's title a line shows, in characters.
const MAX_TITLE_CHARS: usize = 80;

// ── What a host provides ────────────────────────────────────────────────

/// The other sessions, as the calling session may see them. A host
/// implements it for each session it opens.
///
/// An error is a refusal in words the model can act on: it is the tool's
/// result.
#[async_trait]
pub trait SessionArchive: Send + Sync {
    /// The sessions that say `search.query`, the most recently active
    /// first, each with its first match.
    async fn search(&self, search: &SessionSearch) -> Result<SessionHits>;
    /// Some of one session's turns, as text.
    async fn read(&self, read: &SessionRead) -> Result<SessionReading>;
}

/// Who said a piece of a session's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// The user typed it.
    User,
    /// The model answered it.
    Assistant,
    /// A tool call's name and arguments.
    ToolCall,
    /// A tool call's output.
    ToolOutput,
}

impl Speaker {
    fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool call",
            Self::ToolOutput => "tool output",
        }
    }
}

/// A stored session, as the tools name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedSession {
    /// Its full id.
    pub id: String,
    /// Its name, or the start of its first prompt.
    pub title: Option<String>,
    /// The folder it works in.
    pub folder: Option<String>,
    /// When it was last written, in Unix seconds.
    pub last_active_unix: i64,
    /// Whether the user archived it.
    pub archived: bool,
}

/// What `SearchSessions` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearch {
    /// The text to find: not empty.
    pub query: String,
    /// How many sessions at most.
    pub limit: usize,
    /// Archived sessions too.
    pub include_archived: bool,
    /// What tools printed too.
    pub include_tool_output: bool,
}

/// One session that says the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHit {
    /// The session.
    pub session: ArchivedSession,
    /// Who said the matched text.
    pub speaker: Speaker,
    /// The text around the match.
    pub snippet: String,
    /// The turn it is in.
    pub turn_id: String,
}

/// A search's answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHits {
    /// The sessions, the most recently active first.
    pub hits: Vec<SessionHit>,
    /// Whether more sessions match than `limit` let through.
    pub more: bool,
}

/// What `ReadSession` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRead {
    /// The session's id, or the start of it.
    pub session_id: String,
    /// The turn to start from; without one, the latest turns.
    pub turn_id: Option<String>,
    /// How many turns.
    pub turn_limit: usize,
    /// Tool output too.
    pub include_outputs: bool,
}

/// One text of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedRow {
    /// Who said it.
    pub speaker: Speaker,
    /// The item it is from: a tool call's two rows share one.
    pub item_id: String,
    /// The text.
    pub text: String,
}

/// One turn's texts, in the order they were said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedTurn {
    /// The turn's id.
    pub id: String,
    /// Its texts.
    pub rows: Vec<ArchivedRow>,
}

/// A read's answer: the turns asked for, and where they stand in the
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReading {
    /// The session.
    pub session: ArchivedSession,
    /// The turns, oldest first.
    pub turns: Vec<ArchivedTurn>,
    /// The place of the first of `turns` among the session's, from 1.
    pub first_turn: usize,
    /// How many turns the session has.
    pub total_turns: usize,
    /// The turn before `turns`, if there is one.
    pub previous_turn_id: Option<String>,
    /// The turn after `turns`, if there is one.
    pub next_turn_id: Option<String>,
}

// ── The tools ───────────────────────────────────────────────────────────

/// `SearchSessions` and `ReadSession`, as the model is told of them. The
/// descriptions are prompt, paid on every call: when to use each, and that
/// what they return is old.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "SearchSessions".to_string(),
            description: "Search the user's other Koda sessions, in every folder, for a word \
                or phrase. Use when the user refers to earlier work, a past decision or an \
                error seen before. Returns one match per session, newest first; old sessions \
                may be out of date."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to find, in either case"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Sessions to return (default 20, max 50)"
                    },
                    "include_archived": {
                        "type": "boolean",
                        "description": "Also search archived sessions"
                    },
                    "include_tool_output": {
                        "type": "boolean",
                        "description": "Also search tool output"
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "ReadSession".to_string(),
            description: "Read another Koda session's conversation: the turn `turn_id` and \
                those after it, or its latest turns. Use after SearchSessions to see what was \
                said or decided; an old session may be out of date."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session's id, or its first characters"
                    },
                    "turn_id": {
                        "type": "string",
                        "description": "Turn to start from (default: the latest turns)"
                    },
                    "turn_limit": {
                        "type": "integer",
                        "description": "Turns to read (default 1, max 10)"
                    },
                    "include_outputs": {
                        "type": "boolean",
                        "description": "Also show tool output"
                    }
                },
                "required": ["session_id"]
            }),
        },
    ]
}

/// `SearchSessions`: the other sessions that say a term.
pub struct SearchSessionsTool;

#[async_trait]
impl Tool for SearchSessionsTool {
    fn name(&self) -> &'static str {
        "SearchSessions"
    }
    fn definition(&self) -> ToolDefinition {
        definition_of(self.name())
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        wrap_result(search_sessions(ctx.session_archive.as_deref(), args).await)
    }
}

/// `ReadSession`: some of another session's turns.
pub struct ReadSessionTool;

#[async_trait]
impl Tool for ReadSessionTool {
    fn name(&self) -> &'static str {
        "ReadSession"
    }
    fn definition(&self) -> ToolDefinition {
        definition_of(self.name())
    }
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        wrap_result(read_session(ctx.session_archive.as_deref(), args).await)
    }
}

fn definition_of(name: &str) -> ToolDefinition {
    definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .expect("both tools are defined above")
}

/// The archive, or why there is none: a sub-agent's registry and a session
/// no host opened have no other sessions to reach.
fn archive_for<'a>(
    archive: Option<&'a dyn SessionArchive>,
    tool: &str,
) -> Result<&'a dyn SessionArchive> {
    match archive {
        Some(archive) => Ok(archive),
        None => bail!(
            "{tool} works only in a top-level session; other sessions can't be reached from here."
        ),
    }
}

/// A count the model gave, within `1..=max`; `default` without one.
fn count_arg(args: &Value, name: &str, default: usize, max: usize) -> usize {
    args[name]
        .as_u64()
        .map_or(default, |asked| (asked as usize).clamp(1, max))
}

async fn search_sessions(archive: Option<&dyn SessionArchive>, args: &Value) -> Result<String> {
    let archive = archive_for(archive, "SearchSessions")?;
    let query = args["query"].as_str().map(str::trim).unwrap_or_default();
    if query.is_empty() {
        bail!("SearchSessions needs a `query`: the text to find.");
    }
    let search = SessionSearch {
        query: query.to_string(),
        limit: count_arg(args, "limit", DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT),
        include_archived: args["include_archived"].as_bool().unwrap_or(false),
        include_tool_output: args["include_tool_output"].as_bool().unwrap_or(false),
    };
    let found = archive.search(&search).await?;
    if found.hits.is_empty() {
        return Ok(nothing_found(&search));
    }
    Ok(crate::untrusted::wrap(
        "SearchSessions",
        &hits_text(&search, &found),
    ))
}

async fn read_session(archive: Option<&dyn SessionArchive>, args: &Value) -> Result<String> {
    let archive = archive_for(archive, "ReadSession")?;
    let session_id = args["session_id"]
        .as_str()
        .map(str::trim)
        .unwrap_or_default();
    if session_id.is_empty() {
        bail!("ReadSession needs a `session_id`; SearchSessions gives them.");
    }
    let read = SessionRead {
        session_id: session_id.to_string(),
        turn_id: args["turn_id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        turn_limit: count_arg(args, "turn_limit", DEFAULT_TURN_LIMIT, MAX_TURN_LIMIT),
        include_outputs: args["include_outputs"].as_bool().unwrap_or(false),
    };
    let reading = archive.read(&read).await?;
    Ok(crate::untrusted::wrap(
        "ReadSession",
        &reading_text(&reading, read.turn_id.is_some()),
    ))
}

// ── As text ─────────────────────────────────────────────────────────────

/// What a search that found nothing says: also what it left unsearched.
fn nothing_found(search: &SessionSearch) -> String {
    let mut text = format!("No other session mentions {:?}.", search.query);
    let unsearched: Vec<&str> = [
        (!search.include_archived).then_some("archived sessions (include_archived)"),
        (!search.include_tool_output).then_some("tool output (include_tool_output)"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !unsearched.is_empty() {
        text.push_str(&format!(" Not searched: {}.", unsearched.join(", ")));
    }
    text
}

/// A session on one line: its title, folder and when it was last active.
fn session_line(session: &ArchivedSession) -> String {
    let title = session.title.as_deref().unwrap_or_default();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = match title.char_indices().nth(MAX_TITLE_CHARS) {
        Some((cut, _)) => format!("{}…", &title[..cut]),
        None if title.is_empty() => "(untitled)".to_string(),
        None => title,
    };
    let folder = session.folder.as_deref().unwrap_or("an unknown folder");
    let day = time::OffsetDateTime::from_unix_timestamp(session.last_active_unix)
        .map_or_else(|_| "?".to_string(), |at| at.date().to_string());
    let archived = if session.archived { ", archived" } else { "" };
    format!("\"{title}\" in {folder}, last active {day}{archived}")
}

/// A search's hits: three lines each, a blank line between them.
fn hits_text(search: &SessionSearch, found: &SessionHits) -> String {
    let count = found.hits.len();
    let mut text = format!(
        "{count} other session{} mention{} {:?}, newest first. \
         ReadSession with a session_id and its turn_id reads the conversation there.\n",
        if count == 1 { "" } else { "s" },
        if count == 1 { "s" } else { "" },
        search.query,
    );
    for hit in &found.hits {
        text.push_str(&format!(
            "\nsession_id: {}  turn_id: {}\n{}\n{}: {}\n",
            hit.session.id,
            hit.turn_id,
            session_line(&hit.session),
            hit.speaker.label(),
            hit.snippet,
        ));
    }
    if found.more {
        text.push_str(&format!(
            "\n[More sessions match: narrow the query, or raise limit (max {MAX_SEARCH_LIMIT}).]\n"
        ));
    }
    text
}

/// A turn's rows by item, in order: a tool call and its output are one.
fn items_of(turn: &ArchivedTurn) -> Vec<&[ArchivedRow]> {
    turn.rows.chunk_by(|a, b| a.item_id == b.item_id).collect()
}

/// One turn as text: its heading, a note if its first `skipped` items are
/// left out, then each row under who said it, cut at [`MAX_TEXT_CHARS`].
fn turn_text(
    number: usize,
    turn: &ArchivedTurn,
    items: &[&[ArchivedRow]],
    skipped: usize,
) -> String {
    let mut text = format!("--- turn {number} (turn_id {}) ---\n", turn.id);
    if skipped > 0 {
        text.push_str(&format!(
            "[The turn's first {skipped} item{} not shown.]\n",
            if skipped == 1 { " is" } else { "s are" }
        ));
    }
    if items.is_empty() {
        text.push_str("(nothing said)\n");
    }
    for row in items.iter().flat_map(|item| item.iter()) {
        text.push_str(&format!("[{}]\n", row.speaker.label()));
        match row.text.char_indices().nth(MAX_TEXT_CHARS) {
            Some((cut, _)) => {
                let rest = row.text[cut..].chars().count();
                text.push_str(row.text[..cut].trim_end());
                text.push_str(&format!("\n[cut: {rest} more characters]\n"));
            }
            None => {
                text.push_str(row.text.trim_end());
                text.push('\n');
            }
        }
    }
    text
}

/// A turn within `budget` characters: its last [`MAX_ITEMS_PER_TURN`]
/// items, as Codex's `read_thread` keeps a turn's latest. A turn that
/// `must` be shown gives up more of its first items until it fits; any
/// other is `None` when it doesn't.
fn turn_within(number: usize, turn: &ArchivedTurn, budget: usize, must: bool) -> Option<String> {
    let items = items_of(turn);
    let mut skipped = items.len().saturating_sub(MAX_ITEMS_PER_TURN);
    loop {
        let text = turn_text(number, turn, &items[skipped..], skipped);
        if text.chars().count() <= budget {
            return Some(text);
        }
        if !must {
            return None;
        }
        // One item is at most two cut texts, well within a whole read.
        if skipped + 1 >= items.len() {
            return Some(text);
        }
        skipped += 1;
    }
}

/// A read as text: the session, the turns that fit in [`MAX_READ_CHARS`],
/// and the turns to read on from. `forward` says the read began at a turn
/// the model named, which is then the one shown whatever its length, with
/// the turns after it while they fit; without one the latest turn is, with
/// the turns before it.
fn reading_text(reading: &SessionReading, forward: bool) -> String {
    let SessionReading { session, turns, .. } = reading;
    let mut text = format!("Session {} ({})", session.id, session_line(session));
    if turns.is_empty() {
        text.push_str(" has no turns.\n");
        return text;
    }
    // The turns shown: `shown.0..shown.1` of `turns`, grown from the turn
    // that must be shown.
    let count = turns.len();
    let number = |at: usize| reading.first_turn + at;
    let mut blocks = std::collections::VecDeque::new();
    let mut budget = MAX_READ_CHARS;
    let mut shown = if forward { (0, 0) } else { (count, count) };
    while shown.1 - shown.0 < count {
        let at = if forward { shown.1 } else { shown.0 - 1 };
        let Some(block) = turn_within(number(at), &turns[at], budget, blocks.is_empty()) else {
            break;
        };
        budget = budget.saturating_sub(block.chars().count());
        if forward {
            blocks.push_back(block);
            shown.1 += 1;
        } else {
            blocks.push_front(block);
            shown.0 -= 1;
        }
    }

    let (first, last) = (number(shown.0), number(shown.1 - 1));
    let total = reading.total_turns;
    if first == last {
        text.push_str(&format!(": turn {first} of {total}.\n"));
    } else {
        text.push_str(&format!(": turns {first}-{last} of {total}.\n"));
    }
    for block in &blocks {
        text.push('\n');
        text.push_str(block);
    }
    text.push('\n');
    let unshown = count - blocks.len();
    if unshown > 0 {
        text.push_str(&format!(
            "[Stopped at {MAX_READ_CHARS} characters: {unshown} of the {count} turns asked for \
             {} not shown.]\n",
            if unshown == 1 { "is" } else { "are" }
        ));
    }
    let previous = match shown.0 {
        0 => reading.previous_turn_id.as_deref(),
        at => Some(turns[at - 1].id.as_str()),
    };
    let next = match turns.get(shown.1) {
        Some(turn) => Some(turn.id.as_str()),
        None => reading.next_turn_id.as_deref(),
    };
    match (previous, next) {
        (None, None) => text.push_str("These are all of the session's turns.\n"),
        (previous, next) => {
            if let Some(id) = next {
                text.push_str(&format!("To read on: turn_id {id} (the next turn).\n"));
            }
            if let Some(id) = previous {
                text.push_str(&format!("To read back: turn_id {id} (the turn before).\n"));
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn session(id: &str) -> ArchivedSession {
        ArchivedSession {
            id: id.to_string(),
            title: Some("Fix the\n parser".to_string()),
            folder: Some("/work/app".to_string()),
            // 2026-11-20 00:00:00 UTC.
            last_active_unix: 1_795_132_800,
            archived: false,
        }
    }

    fn row(speaker: Speaker, item_id: &str, text: &str) -> ArchivedRow {
        ArchivedRow {
            speaker,
            item_id: item_id.to_string(),
            text: text.to_string(),
        }
    }

    /// A turn of `items` messages, each `chars` long.
    fn turn(id: &str, items: usize, chars: usize) -> ArchivedTurn {
        ArchivedTurn {
            id: id.to_string(),
            rows: (1..=items)
                .map(|n| {
                    row(
                        Speaker::Assistant,
                        &format!("{id}-m{n}"),
                        &"x".repeat(chars),
                    )
                })
                .collect(),
        }
    }

    fn reading(turns: Vec<ArchivedTurn>) -> SessionReading {
        SessionReading {
            session: session("s1"),
            total_turns: turns.len(),
            turns,
            first_turn: 1,
            previous_turn_id: None,
            next_turn_id: None,
        }
    }

    /// An archive that answers with what it was given, and keeps what it
    /// was asked.
    #[derive(Default)]
    struct Canned {
        hits: SessionHits,
        reading: Option<SessionReading>,
        searches: Mutex<Vec<SessionSearch>>,
        reads: Mutex<Vec<SessionRead>>,
    }

    #[async_trait]
    impl SessionArchive for Canned {
        async fn search(&self, search: &SessionSearch) -> Result<SessionHits> {
            self.searches.lock().unwrap().push(search.clone());
            Ok(self.hits.clone())
        }
        async fn read(&self, read: &SessionRead) -> Result<SessionReading> {
            self.reads.lock().unwrap().push(read.clone());
            match &self.reading {
                Some(reading) => Ok(reading.clone()),
                None => bail!("No session has the id {:?}.", read.session_id),
            }
        }
    }

    /// The wrapped content of a tool's result, which must be wrapped.
    fn unwrapped(output: &str) -> &str {
        assert!(output.starts_with(crate::untrusted::NOTE), "{output}");
        let (_, rest) = output.split_once(">>>\n").expect("a start marker");
        let (content, _) = rest
            .rsplit_once("\n<<<END_EXTERNAL_CONTENT")
            .expect("an end marker");
        content
    }

    /// A search's arguments reach the archive with the defaults and limits
    /// the tool gives them, and its hits come back as external content: a
    /// line of ids, a line for the session and the snippet under who said
    /// it.
    #[tokio::test]
    async fn a_search_asks_within_its_limits_and_lists_its_hits() {
        let archive = Canned {
            hits: SessionHits {
                hits: vec![
                    SessionHit {
                        session: session("s1"),
                        speaker: Speaker::ToolOutput,
                        snippet: "... the okapi test failed".to_string(),
                        turn_id: "t7".to_string(),
                    },
                    SessionHit {
                        session: ArchivedSession {
                            title: None,
                            folder: None,
                            archived: true,
                            ..session("s2")
                        },
                        speaker: Speaker::User,
                        snippet: "find the okapi".to_string(),
                        turn_id: "t1".to_string(),
                    },
                ],
                more: true,
            },
            ..Default::default()
        };
        for (args, limit, archived, tool_output) in [
            (json!({"query": " okapi "}), 20, false, false),
            (json!({"query": "okapi", "limit": 0}), 1, false, false),
            (
                json!({"query": "okapi", "limit": 500, "include_archived": true,
                       "include_tool_output": true}),
                50,
                true,
                true,
            ),
        ] {
            let output = search_sessions(Some(&archive), &args).await.unwrap();
            assert_eq!(
                archive.searches.lock().unwrap().pop(),
                Some(SessionSearch {
                    query: "okapi".to_string(),
                    limit,
                    include_archived: archived,
                    include_tool_output: tool_output,
                }),
                "{args}"
            );
            assert_eq!(
                unwrapped(&output),
                "2 other sessions mention \"okapi\", newest first. ReadSession with a \
                 session_id and its turn_id reads the conversation there.\n\
                 \n\
                 session_id: s1  turn_id: t7\n\
                 \"Fix the parser\" in /work/app, last active 2026-11-20\n\
                 tool output: ... the okapi test failed\n\
                 \n\
                 session_id: s2  turn_id: t1\n\
                 \"(untitled)\" in an unknown folder, last active 2026-11-20, archived\n\
                 user: find the okapi\n\
                 \n\
                 [More sessions match: narrow the query, or raise limit (max 50).]\n"
            );
        }
    }

    /// A search that finds nothing says so plainly, with what it left
    /// unsearched; one without a query, or without an archive, is refused
    /// in words that say what to do.
    #[tokio::test]
    async fn a_search_with_nothing_to_show_says_why() {
        let archive = Canned::default();
        for (args, expected) in [
            (
                json!({"query": "okapi"}),
                "No other session mentions \"okapi\". Not searched: archived sessions \
                 (include_archived), tool output (include_tool_output).",
            ),
            (
                json!({"query": "okapi", "include_archived": true}),
                "No other session mentions \"okapi\". Not searched: tool output \
                 (include_tool_output).",
            ),
            (
                json!({"query": "okapi", "include_archived": true, "include_tool_output": true}),
                "No other session mentions \"okapi\".",
            ),
        ] {
            let output = search_sessions(Some(&archive), &args).await.unwrap();
            assert_eq!(output, expected, "{args}");
        }
        for args in [json!({}), json!({"query": "  "}), json!({"query": 7})] {
            let refused = search_sessions(Some(&archive), &args).await.unwrap_err();
            assert_eq!(
                refused.to_string(),
                "SearchSessions needs a `query`: the text to find.",
                "{args}"
            );
        }
        assert!(archive.searches.lock().unwrap().len() == 3);
        let refused = search_sessions(None, &json!({"query": "okapi"}))
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "SearchSessions works only in a top-level session; other sessions can't be reached \
             from here."
        );
    }

    /// A read's arguments reach the archive within the tool's limits, and
    /// the turn comes back as external content: the session, then each row
    /// under who said it, then where to read on. A refusal by the archive
    /// is the tool's error, as it was worded.
    #[tokio::test]
    async fn a_read_asks_within_its_limits_and_labels_each_row() {
        let archive = Canned {
            reading: Some(SessionReading {
                session: session("s1"),
                turns: vec![ArchivedTurn {
                    id: "t2".to_string(),
                    rows: vec![
                        row(Speaker::User, "u1", "run the tests"),
                        row(Speaker::ToolCall, "c1", "Bash\ncommand\tcargo test"),
                        row(Speaker::ToolOutput, "c1", "1 failed\n"),
                        row(Speaker::Assistant, "m1", "The okapi test failed."),
                    ],
                }],
                first_turn: 2,
                total_turns: 3,
                previous_turn_id: Some("t1".to_string()),
                next_turn_id: Some("t3".to_string()),
            }),
            ..Default::default()
        };
        for (args, turn_id, turn_limit, outputs) in [
            (json!({"session_id": " s1 "}), None, 1, false),
            (
                json!({"session_id": "s1", "turn_id": "t2", "turn_limit": 99,
                       "include_outputs": true}),
                Some("t2"),
                10,
                true,
            ),
            (
                json!({"session_id": "s1", "turn_id": "", "turn_limit": 0}),
                None,
                1,
                false,
            ),
        ] {
            let output = read_session(Some(&archive), &args).await.unwrap();
            assert_eq!(
                archive.reads.lock().unwrap().pop(),
                Some(SessionRead {
                    session_id: "s1".to_string(),
                    turn_id: turn_id.map(str::to_string),
                    turn_limit,
                    include_outputs: outputs,
                }),
                "{args}"
            );
            assert_eq!(
                unwrapped(&output),
                "Session s1 (\"Fix the parser\" in /work/app, last active 2026-11-20): \
                 turn 2 of 3.\n\
                 \n\
                 --- turn 2 (turn_id t2) ---\n\
                 [user]\n\
                 run the tests\n\
                 [tool call]\n\
                 Bash\n\
                 command\tcargo test\n\
                 [tool output]\n\
                 1 failed\n\
                 [assistant]\n\
                 The okapi test failed.\n\
                 \n\
                 To read on: turn_id t3 (the next turn).\n\
                 To read back: turn_id t1 (the turn before).\n"
            );
        }

        let nothing = Canned::default();
        let refused = read_session(Some(&nothing), &json!({"session_id": "nope"}))
            .await
            .unwrap_err();
        assert_eq!(refused.to_string(), "No session has the id \"nope\".");
        let refused = read_session(Some(&nothing), &json!({})).await.unwrap_err();
        assert_eq!(
            refused.to_string(),
            "ReadSession needs a `session_id`; SearchSessions gives them."
        );
        let refused = read_session(None, &json!({"session_id": "s1"}))
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .starts_with("ReadSession works only in a top-level session"),
            "{refused}"
        );
    }

    /// A text longer than 2,000 characters is cut there, counted in
    /// characters, and says how much is left out.
    #[test]
    fn a_long_text_is_cut_and_says_how_much_is_left() {
        let long = ArchivedTurn {
            id: "t1".to_string(),
            rows: vec![
                row(Speaker::User, "u1", &"é".repeat(MAX_TEXT_CHARS)),
                row(Speaker::Assistant, "m1", &"界".repeat(MAX_TEXT_CHARS + 345)),
            ],
        };
        assert_eq!(
            reading_text(&reading(vec![long]), true),
            format!(
                "Session s1 (\"Fix the parser\" in /work/app, last active 2026-11-20): \
                 turn 1 of 1.\n\
                 \n\
                 --- turn 1 (turn_id t1) ---\n\
                 [user]\n\
                 {}\n\
                 [assistant]\n\
                 {}\n\
                 [cut: 345 more characters]\n\
                 \n\
                 These are all of the session's turns.\n",
                "é".repeat(MAX_TEXT_CHARS),
                "界".repeat(MAX_TEXT_CHARS),
            )
        );
    }

    /// A turn shows its last 20 items, a tool call and its output being
    /// one, and says how many came before them.
    #[test]
    fn a_turn_shows_its_last_twenty_items() {
        let mut rows = vec![row(Speaker::User, "u1", "go")];
        for n in 1..=25 {
            let id = format!("c{n}");
            rows.push(row(Speaker::ToolCall, &id, &format!("call {n}")));
            rows.push(row(Speaker::ToolOutput, &id, &format!("output {n}")));
        }
        let text = reading_text(
            &reading(vec![ArchivedTurn {
                id: "t1".to_string(),
                rows,
            }]),
            true,
        );
        // 26 items: the prompt and five calls are left out.
        assert!(
            text.contains(
                "--- turn 1 (turn_id t1) ---\n\
                 [The turn's first 6 items are not shown.]\n\
                 [tool call]\ncall 6\n[tool output]\noutput 6\n"
            ),
            "{text}"
        );
        assert!(!text.contains("call 5\n"), "{text}");
        assert!(text.contains("[tool output]\noutput 25\n"), "{text}");
        assert_eq!(text.matches("[tool call]").count(), MAX_ITEMS_PER_TURN);
    }

    /// The turns of a read fit in 20,000 characters. A read from a turn
    /// shows it and the turns after it that fit whole; a read of the
    /// latest turns shows the last and the turns before it that fit. Either
    /// says how many it left out and names the turns on each side.
    #[test]
    fn a_read_stops_at_its_size_and_says_where_to_read_on() {
        // Three turns of about 8,000 characters each.
        let turns = || {
            vec![
                turn("t4", 4, 2_000),
                turn("t5", 4, 2_000),
                turn("t6", 4, 2_000),
            ]
        };
        let window = SessionReading {
            first_turn: 4,
            total_turns: 9,
            previous_turn_id: Some("t3".to_string()),
            next_turn_id: Some("t7".to_string()),
            ..reading(turns())
        };

        let forward = reading_text(&window, true);
        assert!(forward.contains("): turns 4-5 of 9.\n"), "{forward}");
        assert!(!forward.contains("turn_id t6) ---"), "{forward}");
        assert!(
            forward.ends_with(
                "[Stopped at 20000 characters: 1 of the 3 turns asked for is not shown.]\n\
                 To read on: turn_id t6 (the next turn).\n\
                 To read back: turn_id t3 (the turn before).\n"
            ),
            "{forward}"
        );

        let latest = reading_text(&window, false);
        assert!(latest.contains("): turns 5-6 of 9.\n"), "{latest}");
        assert!(!latest.contains("turn_id t4) ---"), "{latest}");
        assert!(
            latest.ends_with(
                "[Stopped at 20000 characters: 1 of the 3 turns asked for is not shown.]\n\
                 To read on: turn_id t7 (the next turn).\n\
                 To read back: turn_id t4 (the turn before).\n"
            ),
            "{latest}"
        );
        for text in [&forward, &latest] {
            let turns_chars = text
                .split("\n--- turn")
                .skip(1)
                .map(str::len)
                .sum::<usize>();
            assert!(turns_chars <= MAX_READ_CHARS, "{turns_chars}");
        }
    }

    /// One turn longer than a whole read is still shown: its latest items,
    /// as many as fit, with a note of how many were left out.
    #[test]
    fn a_turn_longer_than_a_read_keeps_its_latest_items() {
        let text = reading_text(&reading(vec![turn("t1", 15, 2_000)]), true);
        // Each item is its label, 2,000 characters and a line break.
        assert!(
            text.contains("[The turn's first 6 items are not shown.]\n"),
            "{text}"
        );
        assert_eq!(text.matches("[assistant]").count(), 9);
        assert!(text.contains("): turn 1 of 1.\n"), "{text}");
        assert!(
            text.ends_with("These are all of the session's turns.\n"),
            "{text}"
        );
    }

    /// A session without turns, and a turn in which nothing was said, read
    /// as that.
    #[test]
    fn empty_sessions_and_turns_say_so() {
        assert_eq!(
            reading_text(&reading(Vec::new()), false),
            "Session s1 (\"Fix the parser\" in /work/app, last active 2026-11-20) has no \
             turns.\n"
        );
        let text = reading_text(&reading(vec![turn("t1", 0, 0)]), false);
        assert!(
            text.contains("--- turn 1 (turn_id t1) ---\n(nothing said)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_long_title_is_cut_on_one_line() {
        let long = ArchivedSession {
            title: Some("é".repeat(200)),
            ..session("s1")
        };
        assert_eq!(
            session_line(&long),
            format!(
                "\"{}…\" in /work/app, last active 2026-11-20",
                "é".repeat(MAX_TITLE_CHARS)
            )
        );
    }
}
