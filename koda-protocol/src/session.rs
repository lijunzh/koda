//! Sessions: one conversation with its model, mode and tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::TrustMode;

/// A session: one stored conversation. Codex's `Thread`. What it's running
/// with (`model` through `instruction_sources`) is there only while it is
/// open, as from `session/start`; `session/list` gives stored sessions
/// without it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The session's id, for resuming it.
    pub id: String,
    /// Its title, set from the first prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The first user message; empty before there is one.
    #[serde(default)]
    pub preview: String,
    /// When it was created, in Unix seconds.
    pub created_at: i64,
    /// When its last message was stored, in Unix seconds.
    pub updated_at: i64,
    /// The project it works in; absent for sessions too old to record it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The model id, such as `claude-sonnet-5-5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider, such as `lm-studio` or `anthropic`. Codex's
    /// `modelProvider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// What Koda may do without asking: the mode an open session runs in,
    /// and for a listed one the mode it last ran in, which resuming it
    /// keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TrustMode>,
    /// The tools the model is offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The instruction files (`AGENTS.md` and the like) its prompt carries,
    /// in that order: the global one, then the project's from the git root
    /// down. Codex's `instructionSources`, as of the session's opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_sources: Option<Vec<String>>,
    /// How it came to be, as Codex's `Thread.source`: started in the
    /// terminal UI, by `koda exec`, by another client of the server, or by a
    /// session for a sub-agent. Absent for a session from before this was
    /// recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SessionSourceKind>,
    /// The client that started it, by the name it gave in `initialize`, as
    /// Codex's `originator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originator: Option<String>,
    /// The session it was forked from, as Codex's `forkedFromId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<String>,
    /// The session whose sub-agent it is, as Codex's `parentThreadId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// A side conversation or a review, started with `ephemeral`: its
    /// client deletes it, and `session/list` leaves it out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    /// What it is doing now, as Codex's `Thread.status`;
    /// `session/status/changed` follows each change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    /// What an open session's turns run with: the model and the alias it
    /// was chosen by, the provider and its endpoint, the reasoning effort
    /// and the mode, as Codex's `thread/start` answers with them.
    /// `session/settings/updated` follows each change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Box<SessionSettings>>,
    /// Its turns, oldest first, each with its items. Filled only by
    /// `session/resume` and by `session/read` with
    /// `includeTurns`, as Codex's `Thread.turns` is.
    ///
    /// Each turn is the one the client saw: its id, status, usage and
    /// completed items, a sub-agent's tool calls under its `InvokeAgent`
    /// call. A turn that never ended, Koda having stopped mid-way, reads as
    /// interrupted.
    ///
    /// Sessions from before Koda stored turns (0.6.4 and earlier) are
    /// rebuilt from the messages the model read, which keep less: a turn's id
    /// is its first message's, an interrupted turn followed by a new prompt
    /// reads as one turn, a tool call's status is read from its output, its
    /// `detail` and `changes` are missing, a sub-agent's tool calls and
    /// compactions aren't listed, and turns have no usage or duration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<crate::turn::Turn>,
}

/// `session/start`: a new session, as Codex's `thread/start`. An earlier
/// one is opened with `session/resume`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionStartParams {
    /// Who answers the session's requests. Without one, the client does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    /// The provider, by id (`anthropic`, `lm-studio`, …, as
    /// `modelProvider/list` gives them). A new session without one uses the
    /// host's; a resumed one keeps the provider it was started with, as
    /// Codex's `thread/resume` does. A session's provider never changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The provider's endpoint, for a local server or a gateway. Without
    /// one, the provider's default, or a resumed session's own. Given
    /// without `modelProvider` for a new session, it names the provider
    /// too, by its host or local port, as `koda --base-url` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The model: an id, or an alias such as `claude-opus`, which follows
    /// its line's newest model. Without one, the provider's default, or a
    /// resumed session's own choice, an alias resolved again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The folder the session works in, as Codex's `thread/start` takes a
    /// `cwd`: an absolute path to an existing folder. Without one, the
    /// host's. One shared server serves every project this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Config values for this session only, over the config file and the
    /// host's own, as `-c key=value` gives them and as Codex's
    /// `thread/start` takes a `config` map: dotted keys, JSON values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// A session not meant to be kept, such as a review: `session/list`
    /// leaves it out and its client deletes it when done. Codex's
    /// `ephemeral` thread isn't written to disk at all; Koda's is stored
    /// like any other while it lasts, so it can be resumed after a restart.
    /// Ignored when resuming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
}

/// `session/settings/update`: what the session's next turns run with, as
/// Codex's `thread/settings/update`. Only what's given changes. A running
/// turn keeps its own; `session/settings/updated` follows once applied.
/// The provider can't change: a model of another provider is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSettingsUpdateParams {
    /// The session.
    pub session_id: String,
    /// The model: an id, or an alias of the session's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort, such as `high`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The trust mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TrustMode>,
}

/// The answer to `session/settings/update`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSettingsUpdateResponse {}

/// What a session's turns run with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSettings {
    /// The model's id.
    pub model: String,
    /// The alias the model was chosen by, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// The provider's id.
    pub model_provider: String,
    /// The provider's endpoint.
    pub base_url: String,
    /// The context window, in tokens.
    pub context_window: u64,
    /// The reasoning effort sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The trust mode.
    pub mode: TrustMode,
}

/// `session/settings/updated`: a session's settings changed. Codex's
/// `thread/settings/updated`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSettingsUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// All of its settings, as they are now. Codex's `threadSettings`.
    pub session_settings: SessionSettings,
}

/// Who answers a session's approvals, questions and loop-cap decisions.
/// Codex's `approvalPolicy` names the same choice, spelled as Codex's
/// `AskForApproval` (`on-request`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    /// The client answers each request.
    #[default]
    OnRequest,
    /// Nobody is there to ask, as in `koda exec`: Koda approves what the
    /// mode allows, refuses destructive actions and anything outside the
    /// sandbox, answers a question with nothing and goes on at the loop
    /// cap. Each request is still sent, followed at once by
    /// `serverRequest/resolved`, so a reader sees what was decided.
    Never,
}

/// The answer to `session/start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartResponse {
    /// The session, ready for a turn.
    pub session: Session,
}

/// `session/resume`: open an earlier session, as Codex's `thread/resume`,
/// answered with it and its turns. One this server has open is joined: the
/// answer has its running turn as it stands, and its notifications follow
/// from there. A session another Koda process has open is refused
/// (`-32600`, "session … already has an active writer"), as Codex refuses a
/// thread with another writer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionResumeParams {
    /// The session, by its full id; a client matches a shorter one over
    /// `session/list`.
    pub session_id: String,
    /// Who answers the session's requests. Without one, the client does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    /// Ignored: a session keeps the provider it was started with, as
    /// Codex's `thread/resume` does. Taken for Codex's shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The provider's endpoint. Without one, the session's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The model: an id, or an alias. Without one, the session's own
    /// choice, an alias resolved again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The folder to open it in. Without one, the folder it was started in,
    /// as Codex's `thread/resume` keeps the thread's (the host's for a session
    /// too old to have recorded one); a recorded folder that no longer exists
    /// is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Config values for this session only, as on `session/start`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// The answer to `session/resume`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionResumeResponse {
    /// The session, with its turns, ready for a turn.
    pub session: Session,
}

/// `session/started`: a session is ready for its first turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartedNotification {
    /// The session.
    pub session: Session,
    /// The Koda release, such as `0.8.0`, which says which protocol the
    /// stream speaks: a reader that can't read it should stop.
    pub koda_version: String,
}

/// `session/unsubscribe`: the client is done with a session, as Codex's
/// `thread/unsubscribe`. On the shared server (`koda serve`) only this
/// client stops hearing it: a running turn goes on, and the session closes
/// after a minute with no client and no turn. On a host of one client
/// (`koda serve --stdio`, in-process) it is the last client, so the session
/// closes now and its running turn stops. `session/resume` with its id opens
/// it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUnsubscribeParams {
    /// The session.
    pub session_id: String,
}

/// The answer to `session/unsubscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnsubscribeResponse {
    /// What happened.
    pub status: SessionUnsubscribeStatus,
}

/// What `session/unsubscribe` did. Codex also has `notSubscribed`, for a
/// session other clients still hold; a Koda host has one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionUnsubscribeStatus {
    /// The session was open, and now isn't.
    Unsubscribed,
    /// No session with that id was open.
    NotLoaded,
}

/// `session/list`: stored sessions, newest first. Codex's `thread/list`.
/// A session started with `ephemeral` is never listed, and an archived one
/// only with `archived`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionListParams {
    /// Only the sessions of this folder, or of these; without one, every
    /// folder's, as Codex's `thread/list` takes `cwd`. `~` is the home
    /// folder, a relative path is taken from the host's folder, and `.` and
    /// `..` are resolved by the path's text, as Codex's does; symlinks are
    /// not followed, so list a folder by the `cwd` its sessions report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<SessionListCwd>,
    /// How many at most; without one, 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Where to go on from: an earlier answer's `nextCursor`, as Codex's
    /// `thread/list` pages. It names the last session of that page, so a
    /// session made meanwhile doesn't shift the next; send the same
    /// filters and order with it. Without one, the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// What to order by; without one, when each was created, as Codex's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<SessionSortKey>,
    /// Which way; without one, newest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_direction: Option<SortDirection>,
    /// Only sessions that came to be one of these ways. Without any, as in
    /// Codex's `thread/list`, the ones a person started: `cli`, `appServer`
    /// and `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kinds: Option<Vec<SessionSourceKind>>,
    /// Only sessions of these providers, by id; without any, every
    /// provider's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_providers: Option<Vec<String>>,
    /// Optional archived filter; when set to true, only archived sessions
    /// are returned. If false or null, only non-archived sessions are
    /// returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Only sessions whose name or preview contains this text, as Codex's
    /// `thread/list` takes `searchTerm`. ASCII letters match in either
    /// case, other letters only as written (Codex's match case exactly).
    /// Spaces around it are dropped; empty is as without one. What was
    /// said in a session is searched with `session/search`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_term: Option<String>,
}

/// `session/list`'s `cwd`: one folder or several. Codex's
/// `ThreadListCwdFilter`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SessionListCwd {
    /// One folder.
    One(String),
    /// Several.
    Many(Vec<String>),
}

impl From<String> for SessionListCwd {
    fn from(folder: String) -> Self {
        Self::One(folder)
    }
}

impl From<&str> for SessionListCwd {
    fn from(folder: &str) -> Self {
        Self::One(folder.to_string())
    }
}

/// What `session/list` orders by. Codex's `ThreadSortKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionSortKey {
    /// When the session was created.
    CreatedAt,
    /// When it was last written.
    UpdatedAt,
}

/// How a session came to be. Codex's `ThreadSourceKind`, without its
/// editor and its kinds of sub-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionSourceKind {
    /// Started in the terminal UI.
    Cli,
    /// Started by `koda exec`.
    Exec,
    /// Started by another client of the server, such as a desktop app.
    AppServer,
    /// Started by a session, for one of its sub-agents.
    SubAgent,
    /// From before a session's source was recorded.
    Unknown,
}

/// The answer to `session/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponse {
    /// The sessions, without what open ones are running with.
    pub data: Vec<Session>,
    /// Opaque; pass it as `cursor` for the next page. Absent when there are
    /// no more, as Codex's `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `session/search`: the stored sessions in which something was said, one
/// result per session, in `session/list`'s order. Codex's `thread/search`.
///
/// A session matches when `searchTerm` occurs, letters in either case, in
/// one text of its conversation: what the user typed (not the files
/// attached to it), what the model answered, or a tool call's name and
/// arguments; and a tool's output when `includeToolOutput` asks. The
/// model's reasoning isn't searched. A session started with `ephemeral` is
/// never found, and an archived one only with `archived`. Every provider's
/// sessions are searched.
///
/// A session from before Koda stored turns (0.6.4 and earlier) is searched
/// as `session/read` gives it: its user messages are the text the model
/// read, attached files included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSearchParams {
    /// What to look for: one piece of text, taken as written (no
    /// wildcards, no words to combine). Spaces around it are dropped; it
    /// can't be empty, or longer than 1000 characters.
    pub search_term: String,
    /// Where to go on from: an earlier answer's `nextCursor`, as on
    /// `session/list`; send the same term, filters and order with it.
    /// Without one, the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many sessions at most: 25 without one, at most 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// What to order by; without one, when each was created, as on
    /// `session/list`. Results aren't ranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<SessionSortKey>,
    /// Which way; without one, newest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_direction: Option<SortDirection>,
    /// Only sessions that came to be one of these ways. Without any, as on
    /// `session/list`, the ones a person started: `cli`, `appServer` and
    /// `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kinds: Option<Vec<SessionSourceKind>>,
    /// Optional archived filter; when set to true, only archived sessions
    /// are returned. If false or null, only non-archived sessions are
    /// returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Only the sessions of this folder, or of these, as `session/list`
    /// takes `cwd`; without one, every folder's. Koda's own: Codex's
    /// `thread/search` has no folder filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<SessionListCwd>,
    /// Also search what tools returned: file contents, command output.
    /// Off without it, since a term found there is seldom what the session
    /// was about. Koda's own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_tool_output: bool,
}

/// The answer to `session/search`. Codex's `ThreadSearchResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchResponse {
    /// The sessions found, one entry each.
    pub data: Vec<SessionSearchResult>,
    /// Opaque; pass it as `cursor` for the next page. Absent when there are
    /// no more, as Codex's `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A session `session/search` found, with the first place in it the term
/// occurs. Codex's `ThreadSearchResult`, which has the thread and the
/// snippet; where the match is (`turnId`, `itemId`, `matched`) is Koda's
/// own, so a client can open the session at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionSearchResult {
    /// The session, as `session/list` gives it.
    pub session: Session,
    /// The text around the match, as Codex cuts it: up to 48 characters
    /// before it and 96 after, runs of spaces and line breaks as one
    /// space, with `...` at an end that was cut.
    pub snippet: String,
    /// The turn the match is in, as `session/turns/list` names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// The item the match is in: the message, or the tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Which of the item's texts matched.
    pub matched: SessionSearchMatch,
}

/// The text of an item that `session/search` matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionSearchMatch {
    /// What the user typed.
    User,
    /// What the model answered.
    Assistant,
    /// A tool call's name or arguments.
    ToolCall,
    /// A tool call's output.
    ToolOutput,
}

/// `session/delete`: delete a stored session. An open one is closed first,
/// its running turn stopped, as Codex's `thread/delete` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// The session, by its full id.
    pub session_id: String,
}

/// The answer to `session/delete`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleteResponse {}

/// `session/archive`: put a stored session away, as Codex's
/// `thread/archive` does. It stays stored and readable (`session/read`,
/// `session/fork`), but `session/list` leaves it out unless asked for
/// archived ones, and `session/resume` refuses it until
/// `session/unarchive`. An open one is closed first, its running turn
/// stopped and its unanswered requests withdrawn. The sessions of the
/// sub-agents it ran are archived with it. `session/archived` follows, one
/// for each session archived, to every client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionArchiveParams {
    /// The session, by its full id.
    pub session_id: String,
}

/// The answer to `session/archive`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionArchiveResponse {}

/// `session/unarchive`: bring an archived session back, as Codex's
/// `thread/unarchive` does, with the sessions of the sub-agents it ran. It
/// is listed again and can be resumed; this doesn't open it.
/// `session/unarchived` follows, one for each session restored, to every
/// client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUnarchiveParams {
    /// The session, by its full id.
    pub session_id: String,
}

/// The answer to `session/unarchive`: the session, as `session/read` gives
/// it without its turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnarchiveResponse {
    /// The restored session.
    pub session: Session,
}

/// `session/compact/start`: compact an open session's history now, as
/// Codex's `thread/compact/start`. Answered at once; the compaction runs as
/// a turn of its own (`turn/started`, a `contextCompaction` item with the
/// `manual` trigger, `turn/completed`). A running turn is stopped first and
/// ends interrupted. The compaction's turn can be interrupted, not steered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCompactStartParams {
    /// The session.
    pub session_id: String,
}

/// The answer to `session/compact/start`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactStartResponse {}

/// `session/revert`: take a turn and every turn after it out of an open,
/// idle session's history, as Codex's `thread/revert` replaces a thread's
/// history with the part before one turn. Files the turns changed stay as
/// they are. Refused while a turn runs, for a turn from before Koda
/// recorded where turns start (0.7.8), and across a compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRevertParams {
    /// The session.
    pub session_id: String,
    /// The first turn taken out, as `session/turns/list` names it.
    pub before_turn_id: String,
}

/// The answer to `session/revert`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRevertResponse {}

/// `session/read`: a stored session, without opening it. Codex's
/// `thread/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReadParams {
    /// The session, by its full id.
    pub session_id: String,
    /// Whether to return its turns and their items.
    #[serde(default)]
    pub include_turns: bool,
}

/// The answer to `session/read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionReadResponse {
    /// The session as `session/list` gives it, with its turns when asked.
    pub session: Session,
}

/// `session/turns/list`: a page of a session's turns, with their items, as
/// Codex's `thread/turns/list` pages a thread's. A client that loads only a
/// session's recent turns (a phone, a long session) fetches older ones as it
/// needs them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTurnsListParams {
    /// The session, by its full id; open or not.
    pub session_id: String,
    /// Where to go on from: an earlier answer's `nextCursor`, or its
    /// `backwardsCursor` with the other `sortDirection`. Without one, the
    /// newest turns (or the oldest, ascending).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many turns at most: 25 without one, at most 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Newest first (`desc`, without one) or oldest first (`asc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_direction: Option<SortDirection>,
}

/// The order a page runs in. Codex's `SortDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SortDirection {
    /// Oldest first.
    Asc,
    /// Newest first.
    Desc,
}

/// The answer to `session/turns/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionTurnsListResponse {
    /// The turns, in the order asked for, each with its items. A turn still
    /// running is `inProgress`, with the items it has so far.
    pub data: Vec<crate::turn::Turn>,
    /// Opaque; pass it as `cursor` for the next page in the same order.
    /// Absent when there are no more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Opaque; pass it as `cursor` with the other `sortDirection` to read
    /// back from this page's first turn, that turn included, to catch what
    /// changed in it (a turn that was running). Absent on an empty page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backwards_cursor: Option<String>,
}

/// `session/tokenUsage/updated`: what an open session's model calls have
/// used, and how full its context is. Codex's `thread/tokenUsage/updated`,
/// sent before each model call and when a turn's last call is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenUsageUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The counts.
    pub token_usage: TokenUsage,
}

/// A session's token counts. Codex's `ThreadTokenUsage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// Every model call since the session was created: its turns,
    /// sub-agents and compactions. A resumed session's count starts from
    /// what its stored calls used, as Codex restores a thread's.
    pub total: TokenUsageBreakdown,
    /// The latest model call of the session's own: not a sub-agent's.
    pub last: TokenUsageBreakdown,
    /// The context window, in tokens.
    pub model_context_window: Option<u64>,
    /// `total` by agent, with its model calls: the session's own under
    /// `main`, each sub-agent's under its name. Koda's own: a Codex
    /// sub-agent is a thread with its own usage, while Koda's run inside
    /// the session's turn, so the split is sent here.
    #[serde(default)]
    pub by_agent: std::collections::BTreeMap<String, crate::turn::Usage>,
}

/// Tokens of one model call, or added up. Codex's `TokenUsageBreakdown`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    /// Input and output tokens.
    pub total_tokens: u64,
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens read from the provider's cache.
    pub cached_input_tokens: u64,
    /// Input tokens written to the provider's cache.
    pub cache_write_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Output tokens spent reasoning.
    pub reasoning_output_tokens: u64,
}

/// `session/fork`: a new session with a copy of a stored one's history,
/// opened and ready for a turn, as Codex's `thread/fork` does. The copy
/// is named "Fork of" the source's name and keeps its provider, endpoint,
/// model and mode unless others are named, as `session/resume` opens a
/// session; the source is left as it was. Whatever the source has stored
/// is copied, so a turn still running there is copied as far as it has
/// got.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionForkParams {
    /// The session to fork, by its full id.
    pub session_id: String,
    /// Who answers the fork's requests. Without one, the client does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    /// The provider, by id, as `session/start` takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    /// The provider's endpoint, as `session/start` takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The model: an id or an alias, as `session/start` takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The folder the copy works in, as `session/start` takes it; the copy
    /// belongs to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Config values for the copy only, as `session/start` takes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// A copy not meant to be kept, such as a side conversation: see
    /// `session/start`'s `ephemeral`, and Codex's on `thread/fork`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
}

/// The answer to `session/fork`: the fork, opened, with its turns, as
/// `session/resume` returns a resumed session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkResponse {
    /// The fork, ready for a turn.
    pub session: Session,
}

/// `session/name/set`: name a stored session, as Codex's `thread/name/set`
/// does. The name replaces the title Koda takes from the first prompt, and
/// a later prompt doesn't replace it. `session/name/updated` follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionNameSetParams {
    /// The session, by its full id.
    pub session_id: String,
    /// The new name: not empty, at most 100 characters.
    pub name: String,
}

/// The answer to `session/name/set`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionNameSetResponse {}

/// `session/name/updated`: a session was named. Codex's
/// `thread/name/updated`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionNameUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// Its name now. Codex's `threadName`.
    pub session_name: String,
}

/// What a session is doing. Codex's `ThreadStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionStatus {
    /// Stored, and not open on the host.
    NotLoaded,
    /// Open, with nothing running and nothing asked.
    Idle,
    /// A turn is running, or the session is waiting on an answer.
    #[serde(rename_all = "camelCase")]
    Active {
        /// What it is waiting on, if anything.
        active_flags: Vec<SessionActiveFlag>,
    },
}

/// What an active session is waiting on. Codex's `ThreadActiveFlag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionActiveFlag {
    /// A yes or no: a tool call's approval, the loop cap, or an MCP
    /// server's request.
    WaitingOnApproval,
    /// An answer to the model's question.
    WaitingOnUserInput,
}

/// `session/status/changed`: what a session is doing changed, for every
/// client, as Codex's `thread/status/changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusChangedNotification {
    /// The session.
    pub session_id: String,
    /// What it is doing now.
    pub status: SessionStatus,
}

/// `session/deleted`: a session was deleted. Codex's `thread/deleted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeletedNotification {
    /// The session.
    pub session_id: String,
}

/// `session/archived`: a session was archived, for every client. Codex's
/// `thread/archived`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionArchivedNotification {
    /// The session.
    pub session_id: String,
}

/// `session/unarchived`: an archived session was brought back, for every
/// client. Codex's `thread/unarchived`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnarchivedNotification {
    /// The session.
    pub session_id: String,
}
