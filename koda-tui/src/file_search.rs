//! The files an `@` mention offers, found by the server's
//! `fuzzyFileSearch`, so the TUI never walks the project itself and any
//! other client gets the same matches.
//!
//! Tab waits for the answer ([`FileSearch::search`]). The dropdown can't
//! wait, so [`FileSearch::matches`] asks in the background and shows the
//! latest answer meanwhile, as Codex's `@` popup does; when the answer comes,
//! it wakes the TUI, which shows it ([`FileSearch::take_fresh`]). The
//! dropdown's requests share a cancellation token, so the server stops a
//! search the user has typed past; Tab's carries none, so a dropdown request
//! never cancels what Tab waits for.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use koda_client::RequestHandle;
use koda_client::protocol::methods;
use koda_client::protocol::search::{FuzzyFileSearchMatchType, FuzzyFileSearchParams};

use crate::frame_requester::FrameRequester;

/// The token the dropdown's searches carry: a new one cancels the one before.
const CANCELLATION_TOKEN: &str = "koda-tui-mention";

/// The project's file search, through the server. Cheap to clone.
#[derive(Clone)]
pub(crate) struct FileSearch {
    requests: RequestHandle,
    /// The project, absolute, as `fuzzyFileSearch` takes its roots.
    root: String,
    state: Arc<Mutex<State>>,
    /// Woken when an answer comes, so the dropdown shows it.
    wake: Option<FrameRequester>,
}

#[derive(Default)]
struct State {
    /// The query of the latest request.
    asked: Option<String>,
    /// The latest request's number; an answer to an earlier one is stale.
    sequence: u64,
    /// The latest answer: its query and its mentions.
    answer: Option<(String, Vec<String>)>,
    /// Whether an answer came that the dropdown hasn't shown.
    fresh: bool,
}

impl FileSearch {
    /// Search `project_root` through `requests`.
    pub(crate) fn new(requests: RequestHandle, project_root: &Path) -> Self {
        let root = std::path::absolute(project_root).unwrap_or_else(|_| project_root.into());
        Self {
            requests,
            root: root.display().to_string(),
            state: Arc::default(),
            wake: None,
        }
    }

    /// Wake the TUI through `frames` when an answer comes.
    pub(crate) fn waking(mut self, frames: FrameRequester) -> Self {
        self.wake = Some(frames);
        self
    }

    /// What the dropdown offers for `query`: the answer for it if there is
    /// one; otherwise it asks, and offers the latest answer meanwhile.
    pub(crate) fn matches(&self, query: &str) -> Vec<String> {
        let mut state = self.lock();
        if let Some(found) = state.answer_for(query) {
            return found;
        }
        if query.is_empty() {
            return Vec::new();
        }
        if state.asked.as_deref() != Some(query) {
            let sequence = state.ask(query);
            let this = self.clone();
            let query = query.to_string();
            tokio::spawn(async move {
                let found = this.ask(&query, true).await;
                if this.lock().answered(sequence, query, found)
                    && let Some(wake) = &this.wake
                {
                    wake.schedule_frame();
                }
            });
        }
        state
            .answer
            .as_ref()
            .map(|(_, found)| found.clone())
            .unwrap_or_default()
    }

    /// The mentions for `query`, waiting for the server if it hasn't
    /// answered it yet. Not cancellable: Tab waits for it.
    pub(crate) async fn search(&self, query: &str) -> Vec<String> {
        let sequence = {
            let mut state = self.lock();
            if let Some(found) = state.answer_for(query) {
                return found;
            }
            state.ask(query)
        };
        let found = self.ask(query, false).await;
        self.lock()
            .answered(sequence, query.to_string(), found.clone());
        found
    }

    /// Whether an answer came since the last call; the dropdown then shows
    /// it.
    pub(crate) fn take_fresh(&self) -> bool {
        std::mem::take(&mut self.lock().fresh)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ask the server: each match as a mention, a folder with a trailing
    /// `/`, best first. Nothing for an empty query or when the server
    /// can't answer. A `cancellable` request carries the dropdown's token.
    async fn ask(&self, query: &str, cancellable: bool) -> Vec<String> {
        if query.is_empty() {
            return Vec::new();
        }
        let params = FuzzyFileSearchParams {
            query: query.to_string(),
            roots: vec![self.root.clone()],
            cancellation_token: cancellable.then(|| CANCELLATION_TOKEN.to_string()),
        };
        match self
            .requests
            .request::<methods::FuzzyFileSearch>(params)
            .await
        {
            Ok(found) => found
                .files
                .into_iter()
                .map(|file| match file.match_type {
                    FuzzyFileSearchMatchType::Directory => format!("{}/", file.path),
                    FuzzyFileSearchMatchType::File => file.path,
                })
                .collect(),
            Err(e) => {
                tracing::warn!("fuzzyFileSearch failed: {e}");
                Vec::new()
            }
        }
    }
}

impl State {
    fn answer_for(&self, query: &str) -> Option<Vec<String>> {
        match &self.answer {
            Some((answered, found)) if answered == query => Some(found.clone()),
            _ => None,
        }
    }

    /// Note a request for `query`; its number.
    fn ask(&mut self, query: &str) -> u64 {
        self.sequence += 1;
        self.asked = Some(query.to_string());
        self.sequence
    }

    /// Keep the answer to request `sequence` unless a later request was
    /// made (the server cancels an earlier one, which then answers with
    /// nothing). Whether it was kept.
    fn answered(&mut self, sequence: u64, query: String, found: Vec<String>) -> bool {
        if sequence != self.sequence {
            return false;
        }
        self.answer = Some((query, found));
        self.fresh = true;
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    /// A Koda host in this process for a test project; it runs while this
    /// lives.
    pub(crate) struct Host {
        client: koda_client::InProcessClient,
        _home: tempfile::TempDir,
    }

    impl Host {
        pub(crate) async fn start(project_root: &Path) -> Self {
            // Koda's folder apart from the project, so the search can't
            // find its files.
            let home = tempfile::tempdir().unwrap();
            let client = koda_client::InProcessClient::start(koda_client::StartArgs {
                config_file: koda_client::ConfigFile::in_dir(home.path()),
                project_root: project_root.to_path_buf(),
                home: home.path().to_path_buf(),
                client_info: koda_client::protocol::initialize::ClientInfo {
                    name: "file-search-test".into(),
                    version: "0".into(),
                    title: None,
                },
            })
            .await
            .unwrap();
            Self {
                client,
                _home: home,
            }
        }

        pub(crate) fn files(&self, root: &Path) -> FileSearch {
            FileSearch::new(self.client.request_handle(), root)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_dropdown_asks_in_the_background_and_hears_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        let host = Host::start(dir.path()).await;
        let files = host.files(dir.path());

        assert!(files.matches("main").is_empty(), "nothing until it answers");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !files.take_fresh() {
            assert!(tokio::time::Instant::now() < deadline, "no answer");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(files.matches("main"), vec!["src/main.rs"]);
        assert!(!files.take_fresh(), "shown once");
        // A new query offers the last answer while it waits.
        assert_eq!(files.matches("mai"), vec!["src/main.rs"]);
        assert_eq!(files.search("src").await, vec!["src/", "src/main.rs"]);
        assert!(files.matches("").is_empty());
    }

    #[test]
    fn an_answer_overtaken_by_a_later_request_is_dropped() {
        let mut state = State::default();
        let first = state.ask("ma");
        let second = state.ask("mai");
        // The server cancelled the first: it answers with nothing.
        assert!(!state.answered(first, "ma".into(), Vec::new()));
        assert!(state.answer_for("ma").is_none());
        assert!(state.answered(second, "mai".into(), vec!["main.rs".into()]));
        assert_eq!(state.answer_for("mai"), Some(vec!["main.rs".to_string()]));
    }
}
