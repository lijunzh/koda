//! The other sessions, as one session's model may see them: what the host
//! gives each session's tools for `SearchSessions` and `ReadSession`
//! ([`koda_core::tools::session_archive`]).
//!
//! The model's search is `session/search`'s scan ([`Host::scan_sessions`])
//! and its read is `session/read`'s turns through the same text
//! ([`crate::session_text`]); what differs is who is asking. A client is
//! the user and sees every session. A model sees what [`may_see`] lets its
//! session see, which is the one place that rule is written.
//!
//! A session the user marked confidential is kept from a model on a remote
//! provider, which [`may_read`] decides from the provider the calling
//! session runs on: its search never reports one, its read gets no text,
//! and it is told only that such sessions exist ([`Host::withheld_from`]).
//! A caller on a local provider, at an endpoint that is local too, reads
//! one like any other, and becomes
//! confidential itself before the text is given ([`Host::mark_reader`]):
//! its own transcript holds that text from then on. No argument of either
//! tool changes any of this.

use std::sync::Weak;

use koda_core::tools::session_archive::{
    ArchivedRow, ArchivedSession, ArchivedTurn, ConfidentialSession, SessionArchive, SessionHit,
    SessionHits, SessionRead, SessionReadAnswer, SessionReading, SessionSearch, Speaker, Withheld,
};
use koda_protocol::session::SessionSearchMatch;
use koda_state::session_store::{SessionInfo, SessionListQuery, SessionStore, source};

use super::{
    ErrorObject, Host, Listing, MAX_SEARCH_TERM, SEARCH_SCAN_PAGE, SessionSortKey, history_error,
    internal,
};
use crate::session_search::Found;
use crate::session_text::{Row, RowKind};

/// How far up a chain of forks a search looks for where a fork's text came
/// from. A fork's source is always an older session, so a chain ends; this
/// bounds one a damaged store might not end.
const MAX_FORK_DEPTH: usize = 32;

/// How many sessions a shortened id is matched against.
const MAX_ID_MATCHES: i64 = 16;

/// How many of its folder's confidential sessions a caller on a remote
/// provider is told of, one line each.
const MAX_WITHHELD_LISTED: usize = 5;

/// Whether session `caller`'s model may see `stored` at all: any session a
/// person started but its own. Not a sub-agent's session or a `koda exec`
/// run, whose words are their parent's or a script's, nor an ephemeral one,
/// which its client deletes.
fn may_see(caller: &str, stored: &SessionInfo) -> bool {
    stored.id != caller
        && !stored.ephemeral
        && stored.parent_session_id.is_none()
        && !matches!(
            stored.source.as_deref(),
            Some(source::EXEC | source::SUB_AGENT)
        )
}

/// Whether the session stored as `caller` runs on a local model: a local
/// kind of provider (LM Studio, Ollama, vLLM, llama.cpp) at an endpoint
/// on the user's own machine or network
/// ([`koda_core::config::ProviderType::runs_locally`]), by the provider
/// and endpoint its row records, which its opening wrote. A session with
/// no endpoint recorded runs at its kind's own. A provider Koda doesn't
/// know, a session with none recorded and a session that isn't stored all
/// count as remote.
fn runs_locally(caller: Option<&SessionInfo>) -> bool {
    caller.is_some_and(|caller| {
        let provider = caller.provider.as_deref();
        provider
            .and_then(|id| crate::settings::provider(id).ok())
            .is_some_and(|provider| provider.runs_locally(caller.base_url.as_deref()))
    })
}

/// Whether a model may be given `stored`'s text: on a local provider any
/// session's, on a remote one only a session that isn't confidential.
fn may_read(local: bool, stored: &SessionInfo) -> bool {
    local || !stored.confidential
}

/// A confidential session as a caller that may not read it is told of it:
/// its id and when it was last written, nothing that was said in it.
fn withheld(stored: &SessionInfo) -> ConfidentialSession {
    ConfidentialSession {
        id: stored.id.clone(),
        last_active_unix: stored.updated_unix,
    }
}

/// Whose search a scan is: a session's model's.
pub(super) struct Viewer<'a> {
    /// The session searching.
    caller: &'a str,
    /// Whether its search takes in archived sessions.
    include_archived: bool,
    /// Whether it runs on a local provider, and so reads confidential
    /// sessions.
    local: bool,
}

impl Viewer<'_> {
    /// Whether the search reports `stored` when it says the term.
    pub(super) fn covers(&self, stored: &SessionInfo) -> bool {
        may_see(self.caller, stored)
            && may_read(self.local, stored)
            && (self.include_archived || stored.archived_unix.is_none())
    }
}

/// [`SessionArchive`] for one session: the host, asked as that session.
pub(super) struct HostArchive {
    host: Weak<Host>,
    /// The session whose tools hold this.
    caller: String,
}

impl HostArchive {
    pub(super) fn new(host: Weak<Host>, caller: String) -> Self {
        Self { host, caller }
    }

    fn host(&self) -> anyhow::Result<std::sync::Arc<Host>> {
        self.host
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Koda is shutting down."))
    }
}

#[async_trait::async_trait]
impl SessionArchive for HostArchive {
    async fn search(&self, search: &SessionSearch) -> anyhow::Result<SessionHits> {
        self.host()?.search_for(&self.caller, search).await
    }

    async fn read(&self, read: &SessionRead) -> anyhow::Result<SessionReadAnswer> {
        self.host()?.read_for(&self.caller, read).await
    }
}

/// A host's failure, as a tool's result words it.
fn told(error: ErrorObject) -> anyhow::Error {
    anyhow::anyhow!(error.message)
}

fn archived(stored: &SessionInfo) -> ArchivedSession {
    ArchivedSession {
        id: stored.id.clone(),
        title: stored.title.clone().or_else(|| stored.preview.clone()),
        folder: stored.project_root.clone(),
        last_active_unix: stored.updated_unix,
        archived: stored.archived_unix.is_some(),
    }
}

fn speaker(matched: SessionSearchMatch) -> Speaker {
    match matched {
        SessionSearchMatch::User => Speaker::User,
        SessionSearchMatch::Assistant => Speaker::Assistant,
        SessionSearchMatch::ToolCall => Speaker::ToolCall,
        SessionSearchMatch::ToolOutput => Speaker::ToolOutput,
    }
}

impl Host {
    /// `SearchSessions` from session `caller`: the sessions its model may
    /// see that say the query, in every folder, the most recently active
    /// first, each with its first match. Archived sessions come with the
    /// others when asked for, not in place of them, as Claude Code's
    /// `search_session_transcripts` takes `include_archived`.
    ///
    /// A caller on a remote provider gets no confidential session, and
    /// with its answer the notice of the ones left out, which is the same
    /// whatever it searched for. A caller on a local provider that gets
    /// one is marked confidential first.
    async fn search_for(
        &self,
        caller: &str,
        search: &SessionSearch,
    ) -> anyhow::Result<SessionHits> {
        let term = search.query.trim();
        anyhow::ensure!(!term.is_empty(), "The query is empty.");
        anyhow::ensure!(
            term.chars().count() <= MAX_SEARCH_TERM,
            "The query is longer than {MAX_SEARCH_TERM} characters; search for a part of it."
        );
        let read = self.context.db.get_session(caller).await;
        let me = read.map_err(|e| anyhow::anyhow!("Couldn't read the sessions: {e:#}"))?;
        let local = runs_locally(me.as_ref());
        let viewer = Viewer {
            caller,
            include_archived: search.include_archived,
            local,
        };
        // What the other open sessions have said so far is searched too.
        // Not the caller's: it isn't searched, and it is in a turn.
        self.store_open_sessions(Some(caller)).await;

        let mut found = Vec::new();
        let mut more = false;
        // The store lists archived sessions apart from the others.
        for archived in [false, true] {
            if archived && !search.include_archived {
                break;
            }
            let query = SessionListQuery {
                limit: SEARCH_SCAN_PAGE,
                ..self
                    .listing_query(Listing {
                        method: "SearchSessions",
                        cwd: None,
                        source_kinds: None,
                        archived: Some(archived),
                        sort_key: Some(SessionSortKey::UpdatedAt),
                        sort_direction: None,
                        cursor: None,
                    })
                    .map_err(told)?
            };
            let scanned = self
                .scan_sessions(
                    query,
                    term,
                    search.include_tool_output,
                    search.limit,
                    Some(&viewer),
                )
                .await
                .map_err(told)?;
            more |= scanned.more;
            found.extend(scanned.found);
        }
        let mut found = self.without_copies(found).await;
        found.sort_by_key(|(stored, _)| std::cmp::Reverse((stored.updated_unix, stored.sequence)));
        more |= found.len() > search.limit;
        found.truncate(search.limit);
        // Before the text goes: the caller's transcript holds it from here.
        let caller_marked = if found.iter().any(|(stored, _)| stored.confidential) {
            self.mark_reader(caller).await?
        } else {
            false
        };
        let withheld = if local {
            None
        } else {
            self.withheld_from(caller, me.as_ref()).await?
        };
        Ok(SessionHits {
            hits: found
                .into_iter()
                .map(|(stored, found)| SessionHit {
                    session: archived(&stored),
                    speaker: speaker(found.matched),
                    snippet: found.snippet,
                    turn_id: found.turn_id,
                })
                .collect(),
            more,
            withheld,
            caller_marked,
        })
    }

    /// The confidential sessions kept from session `caller`, which runs on
    /// a remote provider: those of its own folder, the most recently
    /// active first and [`MAX_WITHHELD_LISTED`] at most, and how many
    /// there are in every folder, archived ones too. `None` when there are
    /// none. It reads only which sessions are marked, so it is the same
    /// for every query and every argument, and says nothing of a session
    /// in another folder but that it is counted.
    async fn withheld_from(
        &self,
        caller: &str,
        me: Option<&SessionInfo>,
    ) -> anyhow::Result<Option<Withheld>> {
        let confidential: Vec<SessionInfo> = self
            .context
            .db
            .confidential_sessions()
            .await
            .map_err(|e| anyhow::anyhow!("Couldn't read the sessions: {e:#}"))?
            .into_iter()
            .filter(|stored| may_see(caller, stored))
            .collect();
        if confidential.is_empty() {
            return Ok(None);
        }
        let folder = me.and_then(|me| me.project_root.as_deref());
        let listed = confidential
            .iter()
            .filter(|stored| folder.is_some() && stored.project_root.as_deref() == folder)
            .take(MAX_WITHHELD_LISTED)
            .map(withheld)
            .collect();
        Ok(Some(Withheld {
            listed,
            total: confidential.len(),
        }))
    }

    /// Session `caller`, on a local provider, is about to be given a
    /// confidential session's text: it becomes confidential itself, with
    /// its sub-agents' sessions, and every client is told. Otherwise a
    /// session on a remote provider could find that text later in this
    /// one's transcript. Whether this changed it; a failure is the tool's,
    /// and no text is given.
    async fn mark_reader(&self, caller: &str) -> anyhow::Result<bool> {
        let changed = self
            .context
            .db
            .set_session_confidential(caller, true)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Couldn't mark this session confidential, so the confidential session \
                     wasn't read: {e:#}"
                )
            })?;
        let became = changed.iter().any(|id| id == caller);
        self.tell_confidential(changed, true);
        Ok(became)
    }

    /// How many of `rows`, the first of fork `stored`'s, were copied from
    /// a session the search reports itself or from the caller: text said
    /// there, not in the fork, so the fork's match is looked for after
    /// them. None for a session that isn't a fork.
    ///
    /// A fork starts as a copy of its source's turns, so the rows the two
    /// begin with alike are the copied ones. They are compared as text,
    /// not by id: a fork of a session stored before turns were recorded
    /// has its turns rebuilt under ids of its own. The source is the
    /// nearest session up the chain of forks that the search covers; one
    /// it doesn't cover (deleted, archived and not asked for) leaves the
    /// text to the fork.
    pub(super) async fn copied_rows(
        &self,
        viewer: &Viewer<'_>,
        stored: &SessionInfo,
        rows: &[Row<'_>],
    ) -> Result<usize, ErrorObject> {
        let db = &self.context.db;
        let mut source = stored.forked_from_id.clone();
        for _ in 0..MAX_FORK_DEPTH {
            let Some(id) = source else { break };
            let read = db.get_session(&id).await;
            let Some(ancestor) =
                read.map_err(|e| internal(format!("couldn't read the session: {e:#}")))?
            else {
                break;
            };
            if ancestor.id == viewer.caller || viewer.covers(&ancestor) {
                let turns = crate::history::turns(db, &ancestor.id, false)
                    .await
                    .map_err(history_error)?;
                let theirs = crate::session_text::rows(&turns);
                return Ok(rows
                    .iter()
                    .zip(&theirs)
                    .take_while(|(ours, theirs)| {
                        ours.kind == theirs.kind && ours.text == theirs.text
                    })
                    .count());
            }
            source = ancestor.forked_from_id;
        }
        Ok(0)
    }

    /// `found` without a second hit for one text. Forks whose common source
    /// the search doesn't cover each hold its text as their own
    /// ([`Self::copied_rows`]): where two sessions of one family of forks
    /// match on the same words from the same speaker, the older session
    /// keeps the hit.
    async fn without_copies(&self, found: Vec<(SessionInfo, Found)>) -> Vec<(SessionInfo, Found)> {
        let mut kept: Vec<(String, SessionInfo, Found)> = Vec::with_capacity(found.len());
        for (stored, hit) in found {
            let family = self.fork_root(&stored).await;
            let same = kept.iter_mut().find(|(root, _, other)| {
                *root == family && other.matched == hit.matched && other.snippet == hit.snippet
            });
            match same {
                Some(same) => {
                    let older = |s: &SessionInfo| (s.created_unix, s.sequence);
                    if older(&stored) < older(&same.1) {
                        *same = (family, stored, hit);
                    }
                }
                None => kept.push((family, stored, hit)),
            }
        }
        kept.into_iter()
            .map(|(_, stored, hit)| (stored, hit))
            .collect()
    }

    /// The session a chain of forks began with, by id, whether or not it
    /// is still stored; `stored` itself when it isn't a fork.
    async fn fork_root(&self, stored: &SessionInfo) -> String {
        let mut root = stored.id.clone();
        let mut source = stored.forked_from_id.clone();
        for _ in 0..MAX_FORK_DEPTH {
            let Some(id) = source else { break };
            let read = self.context.db.get_session(&id).await;
            source = read.ok().flatten().and_then(|s| s.forked_from_id);
            root = id;
        }
        root
    }

    /// `ReadSession` from session `caller`: the turn asked for and the
    /// turns after it, or the session's latest, as rows of text. An
    /// archived session reads like any other; the sessions a search leaves
    /// out are refused, in words that say what to do.
    ///
    /// A confidential session gives a caller on a remote provider that it
    /// is one and nothing else, whatever turn was asked for; a caller on a
    /// local provider reads it and is marked confidential first.
    async fn read_for(
        &self,
        caller: &str,
        read: &SessionRead,
    ) -> anyhow::Result<SessionReadAnswer> {
        let stored = self.session_named(caller, &read.session_id).await?;
        let id = &stored.id;
        anyhow::ensure!(
            id != caller,
            "Session {id} is this session. RecallContext reads its earlier turns."
        );
        anyhow::ensure!(
            may_see(caller, &stored),
            "Session {id} is a sub-agent's, a `koda exec` run's or a temporary one, which \
             ReadSession doesn't read."
        );
        let mut caller_marked = false;
        if stored.confidential {
            let me = self.context.db.get_session(caller).await;
            let me = me.map_err(|e| anyhow::anyhow!("Couldn't read the sessions: {e:#}"))?;
            if !may_read(runs_locally(me.as_ref()), &stored) {
                return Ok(SessionReadAnswer::Confidential(withheld(&stored)));
            }
            // Before the text is read: the caller's transcript holds it
            // from here.
            caller_marked = self.mark_reader(caller).await?;
        }
        let turns = self.session_turns(id).await.map_err(told)?;
        let start = match &read.turn_id {
            Some(turn_id) => turns
                .iter()
                .position(|turn| turn.id == *turn_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Session {id} has no turn {turn_id:?}. Leave turn_id out to read its \
                         latest turns."
                    )
                })?,
            None => turns.len().saturating_sub(read.turn_limit),
        };
        let end = (start + read.turn_limit).min(turns.len());
        Ok(SessionReadAnswer::Turns(SessionReading {
            session: archived(&stored),
            turns: turns[start..end]
                .iter()
                .map(|turn| ArchivedTurn {
                    id: turn.id.clone(),
                    rows: crate::session_text::rows(std::slice::from_ref(turn))
                        .into_iter()
                        .filter(|row| read.include_outputs || row.kind != RowKind::ToolOutput)
                        .map(|row| ArchivedRow {
                            speaker: match row.kind {
                                RowKind::User => Speaker::User,
                                RowKind::Assistant => Speaker::Assistant,
                                RowKind::ToolCall => Speaker::ToolCall,
                                RowKind::ToolOutput => Speaker::ToolOutput,
                            },
                            item_id: row.item_id.to_string(),
                            text: row.text.into_owned(),
                        })
                        .collect(),
                })
                .collect(),
            first_turn: start + 1,
            total_turns: turns.len(),
            previous_turn_id: start.checked_sub(1).map(|before| turns[before].id.clone()),
            next_turn_id: turns.get(end).map(|turn| turn.id.clone()),
            caller_marked,
        }))
    }

    /// The stored session `typed` names: by its full id, or by the start
    /// of it when that starts the id of just one session `caller` may see,
    /// as `koda resume` takes a shortened id.
    async fn session_named(&self, caller: &str, typed: &str) -> anyhow::Result<SessionInfo> {
        let db = &self.context.db;
        let failed = |e: anyhow::Error| anyhow::anyhow!("Couldn't read the sessions: {e:#}");
        if let Some(stored) = db.get_session(typed).await.map_err(failed)? {
            return Ok(stored);
        }
        let mut starting: Vec<SessionInfo> = db
            .sessions_with_id_prefix(typed, MAX_ID_MATCHES)
            .await
            .map_err(failed)?
            .into_iter()
            .filter(|stored| may_see(caller, stored))
            .collect();
        match starting.len() {
            1 => Ok(starting.remove(0)),
            0 => anyhow::bail!(
                "No session has the id {typed:?}. SearchSessions gives the ids of the sessions \
                 that mention a term."
            ),
            _ => anyhow::bail!(
                "{typed:?} starts the id of more than one session; give more of the id."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostArgs;
    use koda_core::config::{KodaConfig, ProviderType};
    use koda_core::tools::ToolResult;
    use koda_protocol::session::SessionStartParams;
    use koda_state::db::Database;
    use koda_state::session_store::{ITEM_KIND, SessionListQuery, SessionOrigin, TurnRecord};
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// A host on a scratch database, with one session open: the caller.
    struct Bench {
        host: Arc<Host>,
        db: Database,
        root: PathBuf,
        caller: String,
        /// What the host tells its clients.
        events: mpsc::UnboundedReceiver<crate::events::Outgoing>,
        /// Kept while the host runs: its folder, and the other end of its
        /// channel.
        _dir: tempfile::TempDir,
        _ended: mpsc::UnboundedReceiver<(String, Option<String>)>,
    }

    impl Bench {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let db = Database::init(&root).await.unwrap();
            let (events, received) = mpsc::unbounded_channel();
            let (ended, ended_rx) = mpsc::unbounded_channel();
            let host = Host::new(
                HostArgs {
                    config: KodaConfig::default_for_testing(ProviderType::LMStudio),
                    config_file: koda_config::ConfigFile::in_dir(&root),
                    config_warnings: Vec::new(),
                    project_root: root.clone(),
                    db: db.clone(),
                    sandbox: koda_protocol::state::SandboxState {
                        backend: "seatbelt".to_string(),
                        available: true,
                        reason: None,
                        setup_hint: None,
                    },
                    events,
                },
                ended,
            );
            // The terminal UI, so a session a request starts is a person's.
            host.initialize(koda_protocol::initialize::ClientInfo {
                name: "koda-tui".to_string(),
                version: "0".to_string(),
                title: None,
            })
            .unwrap();
            let started = host
                .start_session(None, SessionStartParams::default(), cli())
                .await
                .unwrap();
            Self {
                host,
                db,
                root,
                caller: started.session.id,
                events: received,
                _dir: dir,
                _ended: ended_rx,
            }
        }

        /// Open a session on `provider` in `folder` and return its id. Its
        /// endpoint is an address nothing listens at, so a session on a
        /// remote provider's kind reaches nobody.
        async fn open_on(&self, provider: &str, folder: &std::path::Path) -> String {
            let params = SessionStartParams {
                model_provider: Some(provider.to_string()),
                base_url: Some(NOWHERE.to_string()),
                model: Some("test-model".to_string()),
                cwd: Some(folder.display().to_string()),
                ..Default::default()
            };
            let started = self.host.start_session(None, params, cli()).await;
            started.unwrap().session.id
        }

        /// A client's request, decoded and answered as the host answers
        /// any.
        async fn request(&self, method: &str, params: Value) -> crate::host::Answer {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let request = koda_protocol::rpc::Request::new(
                koda_protocol::rpc::RequestId::Integer(1),
                method,
                Some(params),
            );
            self.host.handle(request, reply, None).await;
            answer.await.expect("the host answers")
        }

        /// `session/confidential/set` for session `id`, and its answer.
        async fn set_confidential(&self, id: &str, confidential: bool) -> crate::host::Answer {
            let params = json!({"sessionId": id, "confidential": confidential});
            self.request("session/confidential/set", params).await
        }

        /// Mark session `id`, which has no forks, confidential, or unmark
        /// it, as a client does.
        async fn mark(&self, id: &str, confidential: bool) {
            let set = self.set_confidential(id, confidential).await;
            assert_eq!(set, Ok(json!({})), "{id}");
        }

        /// The next `session/confidential/updated` the host sends: the
        /// session and whether it is confidential now.
        async fn told_confidential(&mut self) -> (String, bool) {
            let next = async {
                loop {
                    let message = self.events.recv().await.expect("the host is running");
                    if let crate::events::Outgoing::Notification(
                        koda_protocol::ServerNotification::SessionConfidentialUpdated(told),
                    ) = message
                    {
                        return (told.session_id, told.confidential);
                    }
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(30), next)
                .await
                .expect("session/confidential/updated within 30 s")
        }

        /// Whether session `id` is stored as confidential.
        async fn is_confidential(&self, id: &str) -> bool {
            self.db.get_session(id).await.unwrap().unwrap().confidential
        }

        /// A stored session from `origin` whose turns are `turns`: each a
        /// turn id and its items.
        async fn session(&self, origin: SessionOrigin, turns: &[(&str, Vec<Value>)]) -> String {
            let id = self
                .db
                .create_session_from("default", &self.root, &origin)
                .await
                .unwrap();
            self.say(&id, turns).await;
            id
        }

        /// Add `turns` to session `id`.
        async fn say(&self, id: &str, turns: &[(&str, Vec<Value>)]) {
            let records: Vec<TurnRecord> = turns
                .iter()
                .flat_map(|(turn_id, items)| {
                    items.iter().map(|item| TurnRecord {
                        turn_id: turn_id.to_string(),
                        kind: ITEM_KIND.into(),
                        payload: item.to_string(),
                    })
                })
                .collect();
            self.db.append_turn_records(id, &records).await.unwrap();
        }

        /// What the caller's model gets from a call of `tool`.
        async fn call(&self, tool: &str, args: Value) -> ToolResult {
            self.call_as(&self.caller, tool, args).await
        }

        /// What open session `caller`'s model gets from a call of `tool`,
        /// through its own registry.
        async fn call_as(&self, caller: &str, tool: &str, args: Value) -> ToolResult {
            let agent = self.host.agent(caller).unwrap();
            agent
                .tools
                .execute(tool, &args.to_string(), None, None)
                .await
        }

        /// The ids of the sessions a search from the caller returns, in
        /// order.
        async fn found(&self, args: Value) -> Vec<String> {
            self.found_as(&self.caller, args).await
        }

        /// The ids of the sessions a search from open session `caller`
        /// returns, in order.
        async fn found_as(&self, caller: &str, args: Value) -> Vec<String> {
            let result = self.call_as(caller, "SearchSessions", args).await;
            assert!(result.success, "{}", result.output);
            result
                .output
                .lines()
                .filter_map(|line| line.strip_prefix("session_id: "))
                .map(|rest| rest.split_whitespace().next().unwrap().to_string())
                .collect()
        }
    }

    /// An endpoint nothing listens at.
    const NOWHERE: &str = "http://127.0.0.1:9/v1";

    /// What a caller that may not read confidential sessions is told to do.
    const HOW: &str = "A confidential session can be read only from a session on a local model, \
                       or after the user unmarks it.";

    /// What a result says when it made its caller confidential.
    const MARKED: &str = "\n\nThis session is now confidential too: it holds text from a \
                          confidential session. The user can unmark it.";

    /// The notice a search's output ends with, after its results: from its
    /// first confidential session's line, or its count of them, to what to
    /// do. None without one.
    fn notice(output: &str) -> Option<&str> {
        let (_, after) = output.split_once("\n\n")?;
        let after = after
            .rsplit_once(">>>\n\n")
            .map_or(after, |(_, notice)| notice);
        after.ends_with(HOW).then_some(after)
    }

    /// A confidential session's line for `id`, with whatever day it has.
    fn assert_line(line: &str, id: &str) {
        let day = line
            .strip_prefix(&format!("Confidential session {}, last active ", &id[..8]))
            .unwrap_or_else(|| panic!("not {id}'s line: {line}"));
        let digits = day.replace('-', "");
        assert!(
            day.len() == 10 && digits.len() == 8 && digits.chars().all(|c| c.is_ascii_digit()),
            "a day: {line}"
        );
    }

    fn cli() -> SessionOrigin {
        SessionOrigin {
            source: Some(source::CLI.to_string()),
            ..Default::default()
        }
    }

    fn user(id: &str, text: &str) -> Value {
        json!({
            "type": "userMessage", "id": id, "clientUserMessageIds": [],
            "content": [{"type": "text", "text": text}],
        })
    }

    fn says(id: &str, text: &str) -> Value {
        json!({"type": "agentMessage", "id": id, "text": text})
    }

    fn ran(id: &str, command: &str, output: &str) -> Value {
        json!({
            "type": "toolCall", "id": id, "tool": "Bash",
            "arguments": {"command": command}, "status": "completed", "output": output,
        })
    }

    /// A search from one session finds what another said, as external
    /// content with the session, who said it and the turn to read; never
    /// the searching session, whatever it holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_finds_what_another_session_said_and_never_its_own() {
        let bench = Bench::new().await;
        bench
            .say(&bench.caller, &[("t0", vec![user("u0", "find the okapi")])])
            .await;
        let other = bench
            .session(
                cli(),
                &[
                    ("t1", vec![user("u1", "hello"), says("m1", "Hi.")]),
                    ("t2", vec![says("m2", "An okapi, at last.")]),
                ],
            )
            .await;
        bench
            .db
            .set_session_title(&other, "Okapi hunt")
            .await
            .unwrap();

        let result = bench
            .call("SearchSessions", json!({"query": "OKAPI"}))
            .await;
        assert!(result.success, "{}", result.output);
        let output = &result.output;
        assert!(
            output.starts_with(koda_core::untrusted::NOTE)
                && output.contains("<<<EXTERNAL_CONTENT id=")
                && output.contains("source=\"SearchSessions\">>>\n1 other session mentions"),
            "{output}"
        );
        assert!(
            output.contains(&format!(
                "\nsession_id: {other}  turn_id: t2\n\"Okapi hunt\" in {}, last active ",
                bench.root.display()
            )),
            "{output}"
        );
        assert!(
            output.contains("\nassistant: An okapi, at last.\n"),
            "{output}"
        );
        assert!(!output.contains(&bench.caller), "{output}");
    }

    /// Archived sessions are searched only when asked for, and then with
    /// the others. The sessions of sub-agents and of `koda exec`, and
    /// ephemeral ones, never are.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_covers_the_sessions_a_person_started() {
        let bench = Bench::new().await;
        let okapi = [("t1", vec![says("m1", "An okapi, at last.")])];
        let kept = bench.session(cli(), &okapi).await;
        let from_an_app = SessionOrigin {
            source: Some(source::APP_SERVER.to_string()),
            ..Default::default()
        };
        let put_away = bench.session(from_an_app, &okapi).await;
        bench.db.archive_session(&put_away).await.unwrap();
        for hidden in [
            SessionOrigin {
                source: Some(source::SUB_AGENT.to_string()),
                parent_session_id: Some(kept.clone()),
                ..Default::default()
            },
            SessionOrigin {
                source: Some(source::EXEC.to_string()),
                ..Default::default()
            },
            SessionOrigin {
                ephemeral: true,
                ..cli()
            },
        ] {
            bench.session(hidden, &okapi).await;
        }

        assert_eq!(
            bench.found(json!({"query": "okapi"})).await,
            [kept.as_str()]
        );
        let with_archived = bench
            .found(json!({"query": "okapi", "include_archived": true}))
            .await;
        assert_eq!(with_archived.len(), 2, "{with_archived:?}");
        assert!(with_archived.contains(&kept) && with_archived.contains(&put_away));
        let output = bench
            .call(
                "SearchSessions",
                json!({"query": "okapi", "include_archived": true}),
            )
            .await
            .output;
        assert_eq!(output.matches(", archived\n").count(), 1, "{output}");
    }

    /// Tool output is searched only when asked for, and a match there says
    /// so.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_reads_tool_output_only_when_asked() {
        let bench = Bench::new().await;
        let ran_it = bench
            .session(
                cli(),
                &[("t1", vec![ran("c1", "cargo test", "the okapi test failed")])],
            )
            .await;
        let nothing = bench
            .call("SearchSessions", json!({"query": "okapi"}))
            .await;
        assert_eq!(
            nothing.output,
            "No other session mentions \"okapi\". Not searched: archived sessions \
             (include_archived), tool output (include_tool_output)."
        );
        let asked = json!({"query": "okapi", "include_tool_output": true});
        assert_eq!(bench.found(asked.clone()).await, [ran_it]);
        let output = bench.call("SearchSessions", asked).await.output;
        assert!(
            output.contains("\ntool output: the okapi test failed\n"),
            "{output}"
        );
    }

    /// A fork holds a copy of its source's turns. What was said before the
    /// fork is found in the source alone; the fork is found by what it
    /// said itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fork_is_found_by_its_own_words_not_its_copy() {
        let bench = Bench::new().await;
        let source = bench
            .session(
                cli(),
                &[(
                    "t1",
                    vec![user("u1", "hello"), says("m1", "An okapi, at last.")],
                )],
            )
            .await;
        let fork = bench
            .db
            .fork_session(&source, "Fork of hello", &bench.root, &cli())
            .await
            .unwrap();
        assert_eq!(
            bench.found(json!({"query": "okapi"})).await,
            [source.as_str()]
        );

        // Each goes on its own way.
        bench
            .say(
                &fork,
                &[("t2", vec![says("m2", "The okapi again, and a zebra.")])],
            )
            .await;
        bench
            .say(&source, &[("t3", vec![says("m3", "A quagga.")])])
            .await;
        let both = bench.found(json!({"query": "okapi"})).await;
        assert_eq!(both.len(), 2, "{both:?}");
        let output = bench
            .call("SearchSessions", json!({"query": "okapi"}))
            .await
            .output;
        assert!(
            output.contains(&format!("session_id: {fork}  turn_id: t2\n")),
            "the fork's match is its own turn: {output}"
        );
        assert_eq!(
            bench.found(json!({"query": "zebra"})).await,
            [fork.as_str()]
        );
        assert_eq!(
            bench.found(json!({"query": "quagga"})).await,
            [source.as_str()]
        );

        // A fork of the searching session: what it copied is the caller's.
        bench
            .say(&bench.caller, &[("t0", vec![says("m0", "A tapir.")])])
            .await;
        let of_caller = bench
            .db
            .fork_session(&bench.caller, "Fork", &bench.root, &cli())
            .await
            .unwrap();
        assert!(bench.found(json!({"query": "tapir"})).await.is_empty());
        bench
            .say(&of_caller, &[("t9", vec![says("m9", "Two tapirs.")])])
            .await;
        assert_eq!(bench.found(json!({"query": "tapir"})).await, [of_caller]);
    }

    /// Two forks of a session the search doesn't cover both hold its text,
    /// and one hit comes of it: the older fork's. With the source covered,
    /// the hit is the source's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forks_of_a_session_out_of_reach_give_one_hit_for_its_text() {
        let bench = Bench::new().await;
        let source = bench
            .session(cli(), &[("t1", vec![says("m1", "An okapi, at last.")])])
            .await;
        let fork = |name: &'static str| {
            let (db, root, source) = (bench.db.clone(), bench.root.clone(), source.clone());
            async move { db.fork_session(&source, name, &root, &cli()).await.unwrap() }
        };
        let first = fork("First").await;
        let _second = fork("Second").await;
        bench.db.archive_session(&source).await.unwrap();

        assert_eq!(bench.found(json!({"query": "okapi"})).await, [first]);
        assert_eq!(
            bench
                .found(json!({"query": "okapi", "include_archived": true}))
                .await,
            [source]
        );
    }

    /// A read returns the turn a search's hit names, each row under who
    /// said it, as external content; tool output only when asked for; and
    /// the turns after it up to the limit, with where to read on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_returns_the_turn_a_hit_names() {
        let bench = Bench::new().await;
        let other = bench
            .session(
                cli(),
                &[
                    ("t1", vec![user("u1", "hello"), says("m1", "Hi.")]),
                    (
                        "t2",
                        vec![
                            user("u2", "run the tests"),
                            ran("c1", "cargo test", "the okapi test failed"),
                            says("m2", "One failed."),
                        ],
                    ),
                    ("t3", vec![user("u3", "thanks")]),
                ],
            )
            .await;
        let hit = bench
            .call("SearchSessions", json!({"query": "cargo test"}))
            .await
            .output;
        assert!(
            hit.contains(&format!("session_id: {other}  turn_id: t2\n")),
            "{hit}"
        );

        let read = bench
            .call("ReadSession", json!({"session_id": other, "turn_id": "t2"}))
            .await;
        assert!(read.success, "{}", read.output);
        assert!(
            read.output.starts_with(koda_core::untrusted::NOTE)
                && read.output.contains("source=\"ReadSession\">>>\n"),
            "{}",
            read.output
        );
        assert!(
            read.output.contains(
                ": turn 2 of 3.\n\
                 \n\
                 --- turn 2 (turn_id t2) ---\n\
                 [user]\n\
                 run the tests\n\
                 [tool call]\n\
                 Bash\n\
                 command\tcargo test\n\
                 [assistant]\n\
                 One failed.\n\
                 \n\
                 To read on: turn_id t3 (the next turn).\n\
                 To read back: turn_id t1 (the turn before).\n"
            ),
            "{}",
            read.output
        );

        let with_output = bench
            .call(
                "ReadSession",
                json!({"session_id": other, "turn_id": "t2", "turn_limit": 5,
                       "include_outputs": true}),
            )
            .await
            .output;
        assert!(
            with_output.contains(
                "command\tcargo test\n[tool output]\nthe okapi test failed\n[assistant]\n"
            ),
            "{with_output}"
        );
        assert!(
            with_output.contains(": turns 2-3 of 3.\n")
                && with_output.contains("--- turn 3 (turn_id t3) ---\n[user]\nthanks\n")
                && !with_output.contains("To read on"),
            "{with_output}"
        );

        // Without a turn: the latest, here by the start of the session's id.
        let latest = bench
            .call("ReadSession", json!({"session_id": other[..8]}))
            .await
            .output;
        assert!(
            latest.contains(&format!("Session {other} (")) && latest.contains(": turn 3 of 3.\n"),
            "{latest}"
        );
    }

    /// A read's bounds hold on a stored session: a long text is cut, a
    /// long turn keeps its last items, and a read stops at its size, each
    /// with its note.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_of_a_long_session_is_cut_and_says_so() {
        let bench = Bench::new().await;
        let long = "x".repeat(2_500);
        let many: Vec<Value> = (1..=30)
            .map(|n| says(&format!("m{n}"), &format!("line {n}")))
            .collect();
        let wide: Vec<Value> = (1..=6)
            .map(|n| says(&format!("w{n}"), &"y".repeat(2_000)))
            .collect();
        let other = bench
            .session(
                cli(),
                &[
                    ("t1", vec![says("l1", &long)]),
                    ("t2", many),
                    ("t3", wide.clone()),
                    ("t4", wide),
                ],
            )
            .await;
        let read = |turn_id: &'static str, turn_limit: u32| {
            let args = json!({"session_id": other, "turn_id": turn_id, "turn_limit": turn_limit});
            async { bench.call("ReadSession", args).await.output }
        };

        let cut = read("t1", 1).await;
        assert!(
            cut.contains(&format!(
                "[assistant]\n{}\n[cut: 500 more characters]\n",
                "x".repeat(2_000)
            )),
            "{cut}"
        );
        let items = read("t2", 1).await;
        assert!(
            items.contains("[The turn's first 10 items are not shown.]\n[assistant]\nline 11\n")
                && items.contains("line 30\n"),
            "{items}"
        );
        // Turns three and four are 12,000 characters each: one fits.
        let stopped = read("t3", 10).await;
        assert!(
            stopped.contains(": turn 3 of 4.\n")
                && stopped.contains(
                    "[Stopped at 20000 characters: 1 of the 2 turns asked for is not shown.]\n\
                     To read on: turn_id t4 (the next turn).\n"
                ),
            "{stopped}"
        );
    }

    /// A read the archive can't give is a plain error that says what to do:
    /// an id no session has, a turn the session hasn't, the caller itself,
    /// and the sessions a search leaves out. An archived session reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_that_cant_be_given_says_why() {
        let bench = Bench::new().await;
        let turns = [("t1", vec![says("m1", "An okapi, at last.")])];
        let other = bench.session(cli(), &turns).await;
        let sub_agent = bench
            .session(
                SessionOrigin {
                    source: Some(source::SUB_AGENT.to_string()),
                    parent_session_id: Some(bench.caller.clone()),
                    ..Default::default()
                },
                &turns,
            )
            .await;
        let temporary = bench
            .session(
                SessionOrigin {
                    ephemeral: true,
                    ..cli()
                },
                &turns,
            )
            .await;
        let caller = bench.caller.clone();
        for (args, expected) in [
            (
                json!({"session_id": "no-such-session"}),
                "Error: No session has the id \"no-such-session\". SearchSessions gives the \
                 ids of the sessions that mention a term."
                    .to_string(),
            ),
            (
                json!({"session_id": other, "turn_id": "t9"}),
                format!(
                    "Error: Session {other} has no turn \"t9\". Leave turn_id out to read its \
                     latest turns."
                ),
            ),
            (
                json!({"session_id": caller}),
                format!(
                    "Error: Session {caller} is this session. RecallContext reads its earlier \
                     turns."
                ),
            ),
            (
                json!({"session_id": sub_agent}),
                format!(
                    "Error: Session {sub_agent} is a sub-agent's, a `koda exec` run's or a \
                     temporary one, which ReadSession doesn't read."
                ),
            ),
            (
                json!({"session_id": temporary}),
                format!(
                    "Error: Session {temporary} is a sub-agent's, a `koda exec` run's or a \
                     temporary one, which ReadSession doesn't read."
                ),
            ),
            (
                // A shortened id never names a session the caller may not
                // see.
                json!({"session_id": &sub_agent[..sub_agent.len() - 1]}),
                format!(
                    "Error: No session has the id {:?}. SearchSessions gives the ids of the \
                     sessions that mention a term.",
                    &sub_agent[..sub_agent.len() - 1]
                ),
            ),
        ] {
            let result = bench.call("ReadSession", args.clone()).await;
            assert!(!result.success, "{args}");
            assert_eq!(result.output, expected, "{args}");
        }

        bench.db.archive_session(&other).await.unwrap();
        let read = bench
            .call("ReadSession", json!({"session_id": other}))
            .await;
        assert!(
            read.success && read.output.contains(", archived): turn 1 of 1.\n"),
            "{}",
            read.output
        );
    }

    /// A search from a session on a remote provider never returns a
    /// confidential session, whether or not the query is in it and
    /// whatever its arguments, and says nothing from it: not its title,
    /// its text or its full id. It ends with the same notice for a query
    /// the session matches, one it doesn't and one another session
    /// matches. Before any session is marked there is no notice, and the
    /// session is found.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remote_caller_never_finds_a_confidential_session() {
        let bench = Bench::new().await;
        let remote = bench.open_on("openai", &bench.root).await;
        let secret = bench
            .session(
                cli(),
                &[(
                    "t1",
                    vec![
                        user("u1", "plan the okapi budget"),
                        ran("c1", "cat budget.txt", "okapi: 42"),
                        says("m1", "The okapi budget is 42."),
                    ],
                )],
            )
            .await;
        bench
            .db
            .set_session_title(&secret, "Okapi budget")
            .await
            .unwrap();
        let plain = bench
            .session(cli(), &[("t1", vec![says("m1", "A zebra.")])])
            .await;

        let before = bench
            .call_as(&remote, "SearchSessions", json!({"query": "okapi"}))
            .await
            .output;
        assert!(
            before.contains(&format!("session_id: {secret}  turn_id: t1\n")),
            "{before}"
        );
        assert_eq!(notice(&before), None, "{before}");

        bench.mark(&secret, true).await;
        let expected = |output: &str| {
            let notice = notice(output).unwrap_or_else(|| panic!("no notice: {output}"));
            let (line, how) = notice.split_once('\n').expect("two lines");
            assert_line(line, &secret);
            assert_eq!(how, HOW, "{output}");
            notice.to_string()
        };
        let matching = bench
            .call_as(&remote, "SearchSessions", json!({"query": "okapi"}))
            .await;
        assert!(matching.success, "{}", matching.output);
        let said = expected(&matching.output);
        assert_eq!(
            matching.output,
            format!(
                "No other session mentions \"okapi\". Not searched: archived sessions \
                 (include_archived), tool output (include_tool_output).\n\n{said}"
            )
        );
        // No argument lifts it, real or made up.
        for args in [
            json!({"query": "okapi", "include_archived": true, "include_tool_output": true}),
            json!({"query": "42", "include_tool_output": true, "limit": 50}),
            json!({"query": "okapi", "include_confidential": true, "confidential": true}),
            json!({"query": "Okapi budget"}),
            // One it doesn't match, and one another session matches.
            json!({"query": "quagga"}),
            json!({"query": "zebra"}),
        ] {
            let result = bench.call_as(&remote, "SearchSessions", args.clone()).await;
            assert!(result.success, "{args}: {}", result.output);
            let output = &result.output;
            assert_eq!(expected(output), said, "{args}");
            assert!(
                !output.contains(&secret)
                    && !output.contains("budget is")
                    && !output.contains(": 42"),
                "{args}: {output}"
            );
        }
        assert_eq!(
            bench.found_as(&remote, json!({"query": "zebra"})).await,
            [plain.as_str()]
        );
        assert!(
            bench
                .found_as(&remote, json!({"query": "okapi"}))
                .await
                .is_empty()
        );

        // Unmarked, it is found again and the notice is gone.
        bench.mark(&secret, false).await;
        let after = bench
            .call_as(&remote, "SearchSessions", json!({"query": "okapi"}))
            .await
            .output;
        assert!(
            after.contains(&format!("session_id: {secret}  turn_id: t1\n")),
            "{after}"
        );
        assert_eq!(notice(&after), None, "{after}");
    }

    /// The notice lists the caller's own folder's confidential sessions,
    /// the five most recently active, and counts the rest: the folder's
    /// others, archived ones and other folders', of which it says nothing
    /// else. A sub-agent's session, a `koda exec` run's and the caller
    /// itself aren't counted. A caller with none in its folder gets the
    /// count alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_notice_lists_the_callers_folders_sessions_and_counts_the_rest() {
        let bench = Bench::new().await;
        let apart = bench.root.join("apart");
        let empty = bench.root.join("empty");
        for folder in [&apart, &empty] {
            std::fs::create_dir(folder).unwrap();
        }
        let okapi = [("t1", vec![says("m1", "An okapi, at last.")])];
        // Oldest first: an archived one, then seven more of the folder.
        let put_away = bench.session(cli(), &okapi).await;
        let mut here = Vec::new();
        for _ in 0..7 {
            here.push(bench.session(cli(), &okapi).await);
        }
        let mut elsewhere = Vec::new();
        for _ in 0..2 {
            let id = bench
                .db
                .create_session_from("default", &apart, &cli())
                .await
                .unwrap();
            bench.say(&id, &okapi).await;
            elsewhere.push(id);
        }
        let uncounted = [
            SessionOrigin {
                source: Some(source::SUB_AGENT.to_string()),
                parent_session_id: Some(here[0].clone()),
                ..Default::default()
            },
            SessionOrigin {
                source: Some(source::EXEC.to_string()),
                ..Default::default()
            },
        ];
        let remote = bench.open_on("openai", &bench.root).await;
        let outside = bench.open_on("groq", &empty).await;
        for id in here.iter().chain(&elsewhere).chain([&put_away, &remote]) {
            bench.mark(id, true).await;
        }
        for origin in uncounted {
            let id = bench.session(origin, &okapi).await;
            bench.db.set_session_confidential(&id, true).await.unwrap();
        }
        bench.db.archive_session(&put_away).await.unwrap();

        let output = bench
            .call_as(&remote, "SearchSessions", json!({"query": "okapi"}))
            .await
            .output;
        let said = notice(&output).unwrap_or_else(|| panic!("no notice: {output}"));
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines.len(), 7, "{said}");
        // The five most recently active of the folder, newest first.
        for (line, id) in lines.iter().zip(here.iter().rev().take(5)) {
            assert_line(line, id);
        }
        assert_eq!(lines[5], "5 more confidential sessions are not listed.");
        assert_eq!(lines[6], HOW);
        for id in &elsewhere {
            assert!(!output.contains(&id[..8]), "another folder's: {output}");
        }
        assert!(!output.contains("apart"), "{output}");

        let output = bench
            .call_as(&outside, "SearchSessions", json!({"query": "okapi"}))
            .await
            .output;
        assert_eq!(
            notice(&output),
            // The other caller is one more: it was marked too.
            Some(
                format!("11 confidential sessions in other folders are not listed.\n{HOW}")
                    .as_str()
            ),
            "{output}"
        );
    }

    /// `ReadSession` of a confidential session from a session on a remote
    /// provider is a plain result with the session's line and what to do,
    /// and nothing it says: by its full id or the start of it, for a turn
    /// it has and one it hasn't alike.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_remote_caller_reads_only_that_a_session_is_confidential() {
        let bench = Bench::new().await;
        let remote = bench.open_on("openai", &bench.root).await;
        let secret = bench
            .session(
                cli(),
                &[("t1", vec![says("m1", "The okapi budget is 42.")])],
            )
            .await;
        bench.mark(&secret, true).await;
        let mut outputs = Vec::new();
        for args in [
            json!({"session_id": secret}),
            json!({"session_id": &secret[..8]}),
            json!({"session_id": secret, "turn_id": "t1", "include_outputs": true}),
            json!({"session_id": secret, "turn_id": "t9"}),
        ] {
            let result = bench.call_as(&remote, "ReadSession", args.clone()).await;
            assert!(result.success, "a result, not a failure: {args}");
            outputs.push(result.output);
        }
        let (line, how) = outputs[0].split_once('\n').expect("two lines");
        assert_line(line, &secret);
        assert_eq!(how, HOW);
        assert!(
            outputs.iter().all(|output| *output == outputs[0]),
            "{outputs:?}"
        );
        assert!(!bench.is_confidential(&remote).await);
    }

    /// A session on a local provider finds and reads a confidential
    /// session, and is made confidential by the result that gives it the
    /// text: the result says so once, every client is told, and from then
    /// on a session on a remote provider gets neither session. The user
    /// can unmark the reader like any other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_caller_reads_a_confidential_session_and_becomes_one() {
        let mut bench = Bench::new().await;
        let remote = bench.open_on("openai", &bench.root).await;
        let reader = bench.open_on("ollama", &bench.root).await;
        let secret = bench
            .session(
                cli(),
                &[("t1", vec![says("m1", "The okapi budget is 42.")])],
            )
            .await;
        let caller = bench.caller.clone();
        bench.mark(&secret, true).await;
        assert_eq!(bench.told_confidential().await, (secret.clone(), true));

        // A search that doesn't return it leaves the caller as it was.
        let missed = bench
            .call("SearchSessions", json!({"query": "quagga"}))
            .await;
        assert_eq!(
            missed.output,
            "No other session mentions \"quagga\". Not searched: archived sessions \
             (include_archived), tool output (include_tool_output)."
        );
        assert!(!bench.is_confidential(&caller).await);

        // A search's hit carries its text.
        let found = bench
            .call("SearchSessions", json!({"query": "okapi"}))
            .await;
        assert!(found.success, "{}", found.output);
        assert!(
            found
                .output
                .contains(&format!("session_id: {secret}  turn_id: t1\n"))
                && found
                    .output
                    .contains("\nassistant: The okapi budget is 42.\n"),
            "{}",
            found.output
        );
        assert!(
            found.output.ends_with(&format!(">>>{MARKED}")),
            "{}",
            found.output
        );
        assert_eq!(bench.told_confidential().await, (caller.clone(), true));
        assert!(bench.is_confidential(&caller).await);
        // Said once: it is confidential already.
        let again = bench
            .call("SearchSessions", json!({"query": "okapi"}))
            .await;
        assert!(
            again.output.contains("The okapi budget is 42.") && again.output.ends_with(">>>"),
            "{}",
            again.output
        );

        // So does a read.
        let read = bench
            .call_as(&reader, "ReadSession", json!({"session_id": secret}))
            .await;
        assert!(read.success, "{}", read.output);
        assert!(
            read.output
                .contains("[assistant]\nThe okapi budget is 42.\n")
                && read.output.ends_with(&format!(">>>{MARKED}")),
            "{}",
            read.output
        );
        assert_eq!(bench.told_confidential().await, (reader.clone(), true));
        assert!(bench.is_confidential(&reader).await);

        // What the caller went on to say of it is kept from a remote
        // provider too.
        bench
            .say(&caller, &[("t0", vec![says("m0", "So the budget is 42.")])])
            .await;
        let hidden = bench
            .call_as(&remote, "SearchSessions", json!({"query": "budget"}))
            .await
            .output;
        assert!(
            hidden.starts_with("No other session mentions \"budget\"."),
            "{hidden}"
        );
        let said = notice(&hidden).unwrap_or_else(|| panic!("no notice: {hidden}"));
        assert_eq!(
            said.lines().count(),
            4,
            "three sessions and what to do: {said}"
        );
        for id in [&secret, &caller, &reader] {
            assert!(
                said.contains(&format!("Confidential session {}, ", &id[..8])),
                "{said}"
            );
        }
        let read = bench
            .call_as(&remote, "ReadSession", json!({"session_id": caller}))
            .await;
        assert!(
            read.success && read.output.ends_with(HOW) && !read.output.contains("budget"),
            "{}",
            read.output
        );

        bench.mark(&caller, false).await;
        assert_eq!(bench.told_confidential().await, (caller.clone(), false));
        assert_eq!(
            bench.found_as(&remote, json!({"query": "budget"})).await,
            [caller.as_str()]
        );
    }

    /// Whether a caller reads confidential sessions goes by the provider
    /// and endpoint its session records: a local kind at an endpoint on
    /// the user's own machine or network does. A remote kind wherever it
    /// points, a local kind pointed elsewhere or at no URL at all, a
    /// provider Koda doesn't know and a session with none recorded don't.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_on_an_unknown_provider_counts_as_remote() {
        let bench = Bench::new().await;
        let secret = bench
            .session(cli(), &[("t1", vec![says("m1", "An okapi, at last.")])])
            .await;
        bench.mark(&secret, true).await;
        let caller = bench.caller.clone();
        for (provider, endpoint, reads) in [
            ("mystery", NOWHERE, false),
            ("", NOWHERE, false),
            ("LM Studio", NOWHERE, false),
            ("anthropic", NOWHERE, false),
            ("openrouter", "http://localhost:1234/v1", false),
            ("lm-studio", "https://lm.example.invalid/v1", false),
            ("ollama", "http://203.0.113.7:11434", false),
            ("vllm", "studio.local:8000/v1", false),
            ("llama-cpp", "", false),
            ("ollama", NOWHERE, true),
            ("vllm", "http://192.168.10.50:8000/v1", true),
            ("llama-cpp", "http://studio.local:8080/v1", true),
            ("lm-studio", "http://100.101.102.103:1234/v1", true),
        ] {
            let provider = &format!("{provider} at {endpoint}");
            let (id, _) = provider.split_once(" at ").unwrap();
            bench
                .db
                .set_session_model(&caller, id, endpoint, "test-model")
                .await
                .unwrap();
            let output = bench
                .call("SearchSessions", json!({"query": "okapi"}))
                .await
                .output;
            assert_eq!(
                output.contains("An okapi, at last."),
                reads,
                "{provider:?}: {output}"
            );
            assert_eq!(notice(&output).is_none(), reads, "{provider:?}: {output}");
            let read = bench
                .call("ReadSession", json!({"session_id": secret}))
                .await;
            assert!(read.success, "{provider:?}: {}", read.output);
            assert_eq!(
                read.output.contains("An okapi, at last."),
                reads,
                "{provider:?}"
            );
            assert_eq!(bench.is_confidential(&caller).await, reads, "{provider:?}");
        }

        // A session that never recorded a provider, asked as its model
        // would ask.
        let unrecorded = bench.session(cli(), &[]).await;
        let stored = bench.db.get_session(&unrecorded).await.unwrap().unwrap();
        assert_eq!(stored.provider, None);
        let archive = HostArchive::new(Arc::downgrade(&bench.host), unrecorded.clone());
        let found = archive
            .search(&SessionSearch {
                query: "okapi".to_string(),
                limit: 20,
                include_archived: true,
                include_tool_output: true,
            })
            .await
            .unwrap();
        assert!(found.hits.is_empty() && !found.caller_marked, "{found:?}");
        assert_eq!(found.withheld.map(|withheld| withheld.total), Some(2));
        let read = archive
            .read(&SessionRead {
                session_id: secret.clone(),
                turn_id: None,
                turn_limit: 1,
                include_outputs: false,
            })
            .await
            .unwrap();
        assert!(
            matches!(&read, SessionReadAnswer::Confidential(session) if session.id == secret),
            "{read:?}"
        );
        assert!(!bench.is_confidential(&unrecorded).await);
    }

    /// A confidential session opens on a local provider at a local
    /// endpoint, or on the provider and endpoint it already has, by `session/resume` and by
    /// `session/fork`; another remote provider, or another endpoint of its
    /// own, is refused in words that say to unmark it, and a refused fork
    /// leaves no copy. A fork of it is confidential. The same requests for
    /// a session that isn't marked are not refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_confidential_session_opens_only_where_its_text_may_go() {
        let bench = Bench::new().await;
        let turns = [("t1", vec![says("m1", "An okapi, at last.")])];
        let stored_on_openai = || async {
            let id = bench.session(cli(), &turns).await;
            bench
                .db
                .set_session_model(&id, "openai", NOWHERE, "test-model")
                .await
                .unwrap();
            id
        };
        let secret = stored_on_openai().await;
        let plain = stored_on_openai().await;
        bench.mark(&secret, true).await;
        let forks_of = async |source: String| {
            let query = SessionListQuery {
                limit: 100,
                ..Default::default()
            };
            let listed = bench.db.list_sessions_page(&query).await.unwrap();
            listed
                .into_iter()
                .filter(|stored| stored.forked_from_id.as_deref() == Some(source.as_str()))
                .count()
        };

        let elsewhere = [
            json!({"modelProvider": "groq", "baseUrl": NOWHERE, "model": "test-model"}),
            json!({"modelProvider": "openai", "baseUrl": "http://127.0.0.1:10/v1"}),
            json!({"baseUrl": "http://127.0.0.1:10/v1"}),
            // A local kind of provider, pointed away from the user's network.
            json!({
                "modelProvider": "ollama", "baseUrl": "https://ollama.example.invalid",
                "model": "test-model",
            }),
        ];
        for method in ["session/resume", "session/fork"] {
            for named in &elsewhere {
                let mut params = named.clone();
                params["sessionId"] = json!(secret);
                let refused = bench
                    .request(method, params)
                    .await
                    .expect_err("another remote provider");
                assert!(
                    refused.message.starts_with(&format!(
                        "this session is confidential: it opens only on a local provider or on \
                         its own (openai at {NOWHERE}), not on "
                    )) && refused
                        .message
                        .ends_with("Unmark it first to open it there."),
                    "{method} {named}: {}",
                    refused.message
                );
            }
        }
        assert_eq!(
            forks_of(secret.clone()).await,
            0,
            "a refused fork leaves no copy"
        );

        // A local provider, and its own.
        let local = json!({
            "sessionId": secret, "modelProvider": "ollama", "baseUrl": NOWHERE,
            "model": "test-model",
        });
        for params in [local, json!({"sessionId": secret})] {
            let forked = bench.request("session/fork", params.clone()).await;
            let forked = forked.unwrap_or_else(|e| panic!("{params}: {}", e.message));
            assert_eq!(forked["session"]["confidential"], json!(true), "{params}");
            assert_eq!(forked["session"]["forkedFromId"], json!(secret));
            let id = forked["session"]["id"].as_str().unwrap();
            assert!(bench.is_confidential(id).await, "{params}");
        }
        assert_eq!(forks_of(secret.clone()).await, 2);
        let resumed = bench
            .request("session/resume", json!({"sessionId": secret}))
            .await
            .unwrap();
        assert_eq!(resumed["session"]["confidential"], json!(true));
        assert_eq!(resumed["session"]["modelProvider"], json!("openai"));

        // Not marked: the same requests open it.
        let mut params = elsewhere[0].clone();
        params["sessionId"] = json!(plain);
        let forked = bench.request("session/fork", params.clone()).await.unwrap();
        assert!(forked["session"].get("confidential").is_none(), "{forked}");
        assert_eq!(forked["session"]["modelProvider"], json!("groq"));
        bench.request("session/resume", params).await.unwrap();
    }

    /// The user's own requests aren't affected: a confidential session is
    /// listed, found by its text and read like any other, and says it is
    /// confidential. Marking works on an archived session and on one that
    /// is already marked; a session that isn't stored is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_lists_searches_and_reads_a_confidential_session() {
        let mut bench = Bench::new().await;
        let secret = bench
            .session(cli(), &[("t1", vec![says("m1", "An okapi, at last.")])])
            .await;
        let plain = bench
            .session(cli(), &[("t1", vec![says("m1", "An okapi, again.")])])
            .await;
        bench.mark(&secret, true).await;
        assert_eq!(bench.told_confidential().await, (secret.clone(), true));
        // Again: answered the same, and nothing more is told.
        bench.mark(&secret, true).await;

        let marks = |sessions: &Value| -> Vec<(String, bool)> {
            let sessions = sessions.as_array().unwrap().iter();
            sessions
                .map(|session| session.get("session").unwrap_or(session))
                .map(|session| {
                    (
                        session["id"].as_str().unwrap().to_string(),
                        session["confidential"].as_bool().unwrap_or(false),
                    )
                })
                .filter(|(id, _)| *id != bench.caller)
                .collect()
        };
        let both = [(plain.clone(), false), (secret.clone(), true)];
        let listed = bench.request("session/list", json!({})).await.unwrap();
        assert_eq!(marks(&listed["data"]), both, "{listed}");
        let found = bench
            .request("session/search", json!({"searchTerm": "okapi"}))
            .await
            .unwrap();
        assert_eq!(marks(&found["data"]), both, "{found}");
        assert_eq!(found["data"][1]["snippet"], json!("An okapi, at last."));
        let read = bench
            .request(
                "session/read",
                json!({"sessionId": secret, "includeTurns": true}),
            )
            .await
            .unwrap();
        assert_eq!(read["session"]["confidential"], json!(true));
        assert_eq!(
            read["session"]["turns"][0]["items"][0]["text"],
            json!("An okapi, at last.")
        );
        let turns = bench
            .request("session/turns/list", json!({"sessionId": secret}))
            .await
            .unwrap();
        assert_eq!(
            turns["data"][0]["items"][0]["text"],
            json!("An okapi, at last.")
        );

        // An archived session is unmarked where it is, and the next
        // message is its own, not a second one for the repeat above.
        bench.db.archive_session(&secret).await.unwrap();
        bench.mark(&secret, false).await;
        assert_eq!(bench.told_confidential().await, (secret.clone(), false));
        let stored = bench.db.get_session(&secret).await.unwrap().unwrap();
        assert!(stored.archived_unix.is_some() && !stored.confidential);

        let refused = bench
            .request(
                "session/confidential/set",
                json!({"sessionId": "no-such-session", "confidential": true}),
            )
            .await
            .unwrap_err();
        assert_eq!(refused.message, "no session \"no-such-session\"");
    }

    /// Marking a session reaches the sessions already forked from it: each
    /// is marked and told of, and the answer says how many. Unmarking the
    /// session leaves them marked, each to be unmarked by itself. A
    /// session made confidential by what it read takes no fork with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn marking_a_session_marks_the_sessions_forked_from_it() {
        let mut bench = Bench::new().await;
        let turns = [("t1", vec![says("m1", "An okapi, at last.")])];
        let secret = bench.session(cli(), &turns).await;
        let fork = async |source: &str| {
            let (db, root) = (&bench.db, &bench.root);
            db.fork_session(source, "Fork", root, &cli()).await.unwrap()
        };
        let first = fork(&secret).await;
        let second = fork(&first).await;
        let of_caller = fork(&bench.caller).await;

        assert_eq!(
            bench.set_confidential(&secret, true).await,
            Ok(json!({"forksMarked": 2}))
        );
        let mut told = Vec::new();
        for _ in 0..3 {
            told.push(bench.told_confidential().await);
        }
        assert_eq!(told[0], (secret.clone(), true));
        told.sort();
        let mut expected = [&secret, &first, &second].map(|id| (id.clone(), true));
        expected.sort();
        assert_eq!(told, expected);
        // Again: nothing more to mark, and nothing more is told.
        assert_eq!(bench.set_confidential(&secret, true).await, Ok(json!({})));

        // Unmarking doesn't come back down: the forks hold the text still.
        assert_eq!(bench.set_confidential(&secret, false).await, Ok(json!({})));
        assert_eq!(bench.told_confidential().await, (secret.clone(), false));
        for id in [&first, &second] {
            assert!(bench.is_confidential(id).await, "{id}");
        }
        assert_eq!(bench.set_confidential(&second, false).await, Ok(json!({})));
        assert_eq!(bench.told_confidential().await, (second.clone(), false));
        assert!(bench.is_confidential(&first).await);

        // The caller reads the fork that is still confidential, and
        // becomes confidential alone: its own fork was made before, and
        // holds none of that text.
        let caller = bench.caller.clone();
        let read = bench
            .call("ReadSession", json!({"session_id": first}))
            .await;
        assert!(
            read.output.ends_with(&format!(">>>{MARKED}")),
            "{}",
            read.output
        );
        assert_eq!(bench.told_confidential().await, (caller.clone(), true));
        assert!(!bench.is_confidential(&of_caller).await);
        // The next message is this mark's, not one for the caller's fork.
        assert_eq!(bench.set_confidential(&second, true).await, Ok(json!({})));
        assert_eq!(bench.told_confidential().await, (second, true));
    }
}
