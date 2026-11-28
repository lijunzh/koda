//! The other sessions, as one session's model may see them: what the host
//! gives each session's tools for `SearchSessions` and `ReadSession`
//! ([`koda_core::tools::session_archive`]).
//!
//! The model's search is `session/search`'s scan ([`Host::scan_sessions`])
//! and its read is `session/read`'s turns through the same text
//! ([`crate::session_text`]); what differs is who is asking. A client is
//! the user and sees every session. A model sees what [`may_see`] lets its
//! session see, which is the one place that rule is written.

use std::sync::Weak;

use koda_core::tools::session_archive::{
    ArchivedRow, ArchivedSession, ArchivedTurn, SessionArchive, SessionHit, SessionHits,
    SessionRead, SessionReading, SessionSearch, Speaker,
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

/// Whose search a scan is: a session's model's.
pub(super) struct Viewer<'a> {
    /// The session searching.
    caller: &'a str,
    /// Whether its search takes in archived sessions.
    include_archived: bool,
}

impl Viewer<'_> {
    /// Whether the search reports `stored` when it says the term.
    pub(super) fn covers(&self, stored: &SessionInfo) -> bool {
        may_see(self.caller, stored) && (self.include_archived || stored.archived_unix.is_none())
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

    async fn read(&self, read: &SessionRead) -> anyhow::Result<SessionReading> {
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
        let viewer = Viewer {
            caller,
            include_archived: search.include_archived,
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
        })
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
    async fn read_for(&self, caller: &str, read: &SessionRead) -> anyhow::Result<SessionReading> {
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
        Ok(SessionReading {
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
        })
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
    use koda_state::session_store::{ITEM_KIND, SessionOrigin, TurnRecord};
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
        /// Kept while the host runs: its folder, and the other ends of its
        /// channels.
        _dir: tempfile::TempDir,
        _events: mpsc::UnboundedReceiver<crate::events::Outgoing>,
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
            let started = host
                .start_session(None, SessionStartParams::default(), cli())
                .await
                .unwrap();
            Self {
                host,
                db,
                root,
                caller: started.session.id,
                _dir: dir,
                _events: received,
                _ended: ended_rx,
            }
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
            let agent = self.host.agent(&self.caller).unwrap();
            agent
                .tools
                .execute(tool, &args.to_string(), None, None)
                .await
        }

        /// The ids of the sessions a search from the caller returns, in
        /// order.
        async fn found(&self, args: Value) -> Vec<String> {
            let result = self.call("SearchSessions", args).await;
            assert!(result.success, "{}", result.output);
            result
                .output
                .lines()
                .filter_map(|line| line.strip_prefix("session_id: "))
                .map(|rest| rest.split_whitespace().next().unwrap().to_string())
                .collect()
        }
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
}
