//! Which session an id typed for `--session` / `--resume` names, as
//! `codex exec resume` works it out: a full id goes to the host as it is
//! (`session/resume` takes only full ids, as Codex's `thread/resume` does),
//! and anything shorter is matched against this project's sessions, which
//! `session/list` gives page by page, as Codex looks a name up over
//! `thread/list`.

use koda_client::InProcessClient;
use koda_client::protocol::methods;
use koda_client::protocol::session::{
    Session, SessionListCwd, SessionListParams, SessionSourceKind,
};

/// How many sessions one `session/list` page asks for.
const PAGE: u32 = 100;

/// Which earlier session to open: one named by id (or the start of one),
/// or the most recent, as `codex resume --last` picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    Id(String),
    Last,
}

/// The full id `resume` names.
pub async fn resolve_target(client: &InProcessClient, resume: &Resume) -> anyhow::Result<String> {
    match resume {
        Resume::Id(typed) => resolve(client, typed).await,
        Resume::Last => last(all_sessions(client).await?)
            .ok_or_else(|| anyhow::anyhow!("this project has no earlier session to resume")),
    }
}

/// The session with the latest activity, as `codex resume --last` sorts
/// by update time; one where nothing was ever sent (a TUI opened and
/// closed) doesn't count. On a tie, the newer one: `sessions` is newest
/// first.
fn last(sessions: Vec<Session>) -> Option<String> {
    sessions
        .into_iter()
        .enumerate()
        .filter(|(_, s)| !s.preview.is_empty())
        .max_by_key(|(at, s)| (s.updated_at, std::cmp::Reverse(*at)))
        .map(|(_, s)| s.id)
}

/// The full id `typed` names.
pub(crate) async fn resolve(client: &InProcessClient, typed: &str) -> anyhow::Result<String> {
    let typed = typed.trim();
    anyhow::ensure!(!typed.is_empty(), "no session id given");
    if is_full_id(typed) {
        return Ok(typed.to_string());
    }
    let sessions = all_sessions(client).await?;
    pick(typed, sessions.iter().map(|s| s.id.as_str())).map(str::to_string)
}

/// The full id `typed` names for `koda archive` or, with `archived`, for
/// `koda unarchive`: a full id as it is, and anything shorter among this
/// project's sessions that aren't archived, or among its archived ones, as
/// Codex looks a session up in its active or its archived collection.
pub async fn resolve_for_archive(
    client: &InProcessClient,
    typed: &str,
    archived: bool,
) -> anyhow::Result<String> {
    let typed = typed.trim();
    anyhow::ensure!(!typed.is_empty(), "no session id given");
    if is_full_id(typed) {
        return Ok(typed.to_string());
    }
    let sessions = sessions(client, archived).await?;
    let matches: Vec<&str> = sessions
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| id.starts_with(typed))
        .collect();
    match matches[..] {
        [only] => Ok(only.to_string()),
        [] => {
            let scope = if archived { "archived" } else { "active" };
            anyhow::bail!("No {scope} session found matching '{typed}'.")
        }
        _ => anyhow::bail!("'{typed}' matches more than one session; give more of the id."),
    }
}

/// Every session of this project that isn't archived, most recent first.
pub(crate) async fn all_sessions(client: &InProcessClient) -> anyhow::Result<Vec<Session>> {
    sessions(client, false).await
}

/// This project's sessions, most recent first, page by page: the archived
/// ones, or the ones that aren't.
async fn sessions(client: &InProcessClient, archived: bool) -> anyhow::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            // This client's project: on the shared server, the server's own
            // folder is another's.
            .request::<methods::SessionList>(SessionListParams {
                cwd: Some(SessionListCwd::One(
                    client.project_root().display().to_string(),
                )),
                limit: Some(PAGE),
                cursor,
                // Every kind: an id may name a `koda exec` run or a
                // sub-agent's session, which the list leaves out by default.
                source_kinds: Some(vec![
                    SessionSourceKind::Cli,
                    SessionSourceKind::Exec,
                    SessionSourceKind::AppServer,
                    SessionSourceKind::SubAgent,
                    SessionSourceKind::Unknown,
                ]),
                archived: archived.then_some(true),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        sessions.extend(page.data);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(sessions),
        }
    }
}

/// A session id: a UUID, as Koda makes them, which Codex also passes
/// through without a lookup.
pub fn is_full_id(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The one id among `ids` that `typed` is, or starts.
pub fn pick<'a>(typed: &str, ids: impl Iterator<Item = &'a str>) -> anyhow::Result<&'a str> {
    let matches: Vec<&str> = ids.filter(|id| id.starts_with(typed)).collect();
    if let Some(exact) = matches.iter().find(|id| **id == typed) {
        return Ok(exact);
    }
    match matches[..] {
        [only] => Ok(only),
        [] => anyhow::bail!("no session matches '{typed}'. Run `koda resume` to pick one."),
        _ => anyhow::bail!("'{typed}' matches more than one session; give more of the id."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "28545aaa-0000-4000-8000-000000000001";
    const B: &str = "28545bbb-0000-4000-8000-000000000002";

    #[test]
    fn last_is_the_latest_activity_and_skips_empty_sessions() {
        let session = |id: &str, updated_at: i64, preview: &str| {
            serde_json::from_value::<Session>(serde_json::json!({
                "id": id, "preview": preview, "createdAt": 0, "updatedAt": updated_at,
            }))
            .unwrap()
        };
        // Newest first, as `session/list` gives them.
        let sessions = vec![
            session("empty", 9, ""),
            session("newer", 5, "hi"),
            session("busier", 7, "hi"),
            session("older", 7, "hi"),
        ];
        assert_eq!(last(sessions).as_deref(), Some("busier"));
        assert_eq!(last(vec![session("empty", 9, "")]), None);
    }

    #[test]
    fn a_full_id_needs_no_lookup() {
        assert!(is_full_id(A));
        assert!(!is_full_id("28545aaa"));
        assert!(!is_full_id(&A.replace('-', "x")));
    }

    #[test]
    fn a_prefix_names_the_one_session_it_starts() {
        let ids = [A, B];
        assert_eq!(pick("28545a", ids.into_iter()).unwrap(), A);
        assert_eq!(pick(B, ids.into_iter()).unwrap(), B);
        let both = pick("28545", ids.into_iter()).unwrap_err().to_string();
        assert!(both.contains("more than one session"), "{both}");
        // A pid or a typo isn't a session: say so instead of running without one.
        let none = pick("9999", ids.into_iter()).unwrap_err().to_string();
        assert!(none.contains("no session matches '9999'"), "{none}");
    }
}
