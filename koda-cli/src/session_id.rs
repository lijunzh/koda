//! Which session an id typed for `--session` / `--resume` names, as
//! `codex exec resume` works it out: a full id goes to the host as it is
//! (`session/start` takes only full ids, as Codex's `thread/resume` does),
//! and anything shorter is matched against this project's sessions, which
//! `session/list` gives page by page, as Codex looks a name up over
//! `thread/list`.

use koda_client::InProcessClient;
use koda_client::protocol::methods;
use koda_client::protocol::session::{Session, SessionListParams};

/// How many sessions one `session/list` page asks for.
const PAGE: u32 = 100;

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

/// Every session of this project, most recent first, page by page.
pub(crate) async fn all_sessions(client: &InProcessClient) -> anyhow::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .request::<methods::SessionList>(SessionListParams {
                cwd: None,
                limit: Some(PAGE),
                cursor,
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
fn is_full_id(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The one id among `ids` that `typed` is, or starts.
fn pick<'a>(typed: &str, ids: impl Iterator<Item = &'a str>) -> anyhow::Result<&'a str> {
    let matches: Vec<&str> = ids.filter(|id| id.starts_with(typed)).collect();
    if let Some(exact) = matches.iter().find(|id| **id == typed) {
        return Ok(exact);
    }
    match matches[..] {
        [only] => Ok(only),
        [] => anyhow::bail!(
            "no session matches '{typed}'. Run `koda` and type /sessions to list them."
        ),
        _ => anyhow::bail!("'{typed}' matches more than one session; give more of the id."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "28545aaa-0000-4000-8000-000000000001";
    const B: &str = "28545bbb-0000-4000-8000-000000000002";

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
