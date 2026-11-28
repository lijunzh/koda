//! The project's sessions in which something was said, found by the
//! server's `session/search`, for the `/resume` picker.
//!
//! The picker can't wait for the server while keys come in, so
//! [`SessionSearch::ask`] asks in the background, as the `@` dropdown asks
//! for files ([`crate::file_search`]): when the answer comes it wakes the
//! TUI, which hands it to the picker ([`SessionSearch::take_fresh`]). Each
//! request has a number, and only the latest one's answer is kept, so the
//! answer for a query the user has typed past never shows. A request waits
//! a moment before it goes, so a word typed quickly is one search, not one
//! for each letter.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use koda_client::RequestHandle;
use koda_client::protocol::methods;
use koda_client::protocol::session::{SessionListCwd, SessionSearchParams, SessionSearchResult};

use crate::frame_requester::FrameRequester;

/// How long a query rests before the server is asked for it.
const PAUSE: Duration = Duration::from_millis(150);

/// How many sessions a search asks for: as many as the picker lists.
const LIMIT: u32 = 10;

/// What a search came to: the sessions found, or why there are none.
pub(crate) type Found = Result<Vec<SessionSearchResult>, String>;

/// The project's session search, through the server.
pub(crate) struct SessionSearch {
    requests: Arc<Mutex<RequestHandle>>,
    /// The project's folder, as `session/list` is asked for it.
    cwd: String,
    state: Arc<Mutex<State>>,
    /// Woken when an answer comes, so the picker shows it.
    wake: FrameRequester,
}

#[derive(Default)]
struct State {
    /// The latest request's number; an answer to an earlier one is stale.
    sequence: u64,
    /// The latest request's answer, until the picker takes it.
    fresh: Option<(String, Found)>,
}

impl SessionSearch {
    /// Search the sessions of `project_root` through `requests`, waking
    /// the TUI through `wake` when an answer comes.
    pub(crate) fn new(requests: RequestHandle, project_root: &Path, wake: FrameRequester) -> Self {
        Self {
            requests: Arc::new(Mutex::new(requests)),
            cwd: project_root.display().to_string(),
            state: Arc::default(),
            wake,
        }
    }

    /// Search through `requests` from now on: the connection after a
    /// reconnect.
    pub(crate) fn reconnected(&self, requests: RequestHandle) {
        *self.requests.lock().unwrap_or_else(PoisonError::into_inner) = requests;
    }

    /// Ask which sessions mention `term`, in the background. A request
    /// still on its way is overtaken: its answer is dropped.
    pub(crate) fn ask(&self, term: &str) {
        let sequence = self.lock().ask();
        let requests = Arc::clone(&self.requests);
        let state = Arc::clone(&self.state);
        let wake = self.wake.clone();
        let params = SessionSearchParams {
            search_term: term.to_string(),
            cwd: Some(SessionListCwd::One(self.cwd.clone())),
            limit: Some(LIMIT),
            ..Default::default()
        };
        tokio::spawn(async move {
            tokio::time::sleep(PAUSE).await;
            let latest = |state: &Mutex<State>| {
                state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .sequence
                    == sequence
            };
            // Typed past while it rested: the server isn't asked.
            if !latest(&state) {
                return;
            }
            let requests = requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let term = params.search_term.clone();
            let found = requests
                .request::<methods::SessionSearch>(params)
                .await
                .map(|answer| answer.data)
                .map_err(|e| {
                    tracing::warn!("session/search failed: {e}");
                    e.to_string()
                });
            let kept = state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .answered(sequence, term, found);
            if kept {
                wake.schedule_frame();
            }
        });
    }

    /// Nothing is being searched for any more: a request on its way is
    /// dropped when it answers.
    pub(crate) fn clear(&self) {
        let mut state = self.lock();
        state.ask();
        state.fresh = None;
    }

    /// The answer that came since the last call, with the text it is for.
    pub(crate) fn take_fresh(&self) -> Option<(String, Found)> {
        self.lock().fresh.take()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Note a request; its number.
    fn ask(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// Keep the answer to request `sequence` unless a later request was
    /// made. Whether it was kept.
    fn answered(&mut self, sequence: u64, term: String, found: Found) -> bool {
        if sequence != self.sequence {
            return false;
        }
        self.fresh = Some((term, found));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_overtaken_by_a_later_request_is_dropped() {
        let mut state = State::default();
        let first = state.ask();
        let second = state.ask();
        assert!(!state.answered(first, "ze".into(), Ok(Vec::new())));
        assert!(state.fresh.is_none());
        assert!(state.answered(second, "zebra".into(), Err("gone".into())));
        assert_eq!(state.fresh, Some(("zebra".to_string(), Err("gone".into()))));
    }
}
