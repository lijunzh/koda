//! Getting back to the shared server after the connection to it is lost,
//! as Codex's TUI does: try on its schedule for two minutes (see
//! [`koda_client::reconnect`]), each try a fresh connection, `initialize`
//! and the session opened again, so the server resends the approvals it is
//! waiting on. Koda's own server is started again when nothing answers, as
//! at start-up. If the server kept running, a turn still running there is
//! followed again; if it restarted, that turn ended with it.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use koda_client::InProcessClient;
use koda_client::protocol::methods;
use koda_client::protocol::session::{SessionResumeParams, SessionResumeResponse};
use koda_client::protocol::turn::TurnStatus;
use koda_client::reconnect::{BUDGET, Failure, GaveUp, reconnect, worth_retrying};

use super::TuiContext;
use crate::tui_output::{dim_msg, err_msg, warn_msg};

/// A try that reached the server and opened the session again.
type Reconnected = (InProcessClient, Option<String>, SessionResumeResponse);

impl TuiContext {
    /// Reconnect to the shared server, saying so; Ctrl+C or Ctrl+D while it
    /// tries quits. Returns the session's turn still running on the server,
    /// for the caller to follow.
    pub(crate) async fn reconnect(&mut self) -> Option<String> {
        // The server stopped for a newer Koda, which this window can't use.
        if let Some(version) = self.updated_to.clone() {
            self.offline = true;
            err_msg(
                &mut self.scroll_buffer,
                updated_away(&version, &self.session.id),
            );
            return None;
        }
        // A host in this process that stopped has nothing to reconnect to.
        if self.backend == crate::backend::Backend::InProcess {
            self.offline = true;
            err_msg(
                &mut self.scroll_buffer,
                format!(
                    "Koda's host stopped. /quit leaves, and `koda --resume {}` picks this \
                     session up later.",
                    self.session.id
                ),
            );
            return None;
        }
        dim_msg(
            &mut self.scroll_buffer,
            "Lost the connection to Koda's server. Reconnecting\u{2026} (Ctrl+C quits)".into(),
        );
        let _ = self.draw();

        // The requests on screen and queued came over the lost connection:
        // the host sends again what it still waits on. So did the messages
        // set aside.
        self.drop_requests();
        self.held.clear();
        // A side conversation or review goes with the connection: the
        // session to get back is the main one, where its turn and its
        // approvals are.
        let left_side = self.side.take().map(|side| {
            let left = std::mem::replace(&mut self.session, side.main);
            self.host_session = self.session.id.clone();
            self.leave_side_requested = false;
            self.side_from_turn = None;
            left.id
        });

        let backend = self.backend.clone();
        let root = self.project_root.clone();
        let session_id = self.session.id.clone();
        let attempt = || {
            let backend = backend.clone();
            let root = root.clone();
            let session_id = session_id.clone();
            async move {
                let (client, note) = crate::backend::reconnect_once(&backend, &root).await?;
                let started = client
                    .request::<methods::SessionResume>(SessionResumeParams {
                        session_id,
                        cwd: Some(root.display().to_string()),
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| {
                        if worth_retrying(&e) {
                            Failure::Retry(e.to_string())
                        } else {
                            Failure::Stop(e.to_string())
                        }
                    })?;
                Ok::<Reconnected, Failure<String>>((client, note, started))
            }
        };
        let reconnecting = reconnect(BUDGET, attempt);
        tokio::pin!(reconnecting);
        let result = loop {
            tokio::select! {
                result = &mut reconnecting => break Some(result),
                Some(Ok(event)) = self.crossterm_events.next() => {
                    if quits(&event) {
                        break None;
                    }
                }
            }
        };

        match result {
            None => {
                self.should_quit = true;
                None
            }
            Some(Ok((client, note, started))) => {
                self.client = client;
                self.offline = false;
                self.file_search.reconnected(self.client.request_handle());
                self.session_search
                    .reconnected(self.client.request_handle());
                self.mcp_stale = true;
                if let Some(side) = left_side {
                    // Best-effort, as leaving it does: it may be gone already.
                    let _ = self
                        .client
                        .request::<methods::SessionDelete>(
                            koda_client::protocol::session::SessionDeleteParams {
                                session_id: side,
                            },
                        )
                        .await;
                    self.redraw_main().await;
                    dim_msg(
                        &mut self.scroll_buffer,
                        "The side conversation ended with the connection. Back in the main \
                         session."
                            .into(),
                    );
                }
                dim_msg(
                    &mut self.scroll_buffer,
                    "Reconnected to Koda's server.".into(),
                );
                if let Some(note) = note {
                    warn_msg(&mut self.scroll_buffer, note);
                }
                let running = started
                    .session
                    .turns
                    .last()
                    .filter(|turn| turn.status == TurnStatus::InProgress)
                    .map(|turn| turn.id.clone());
                if std::mem::take(&mut self.turn_cut) && running.is_none() {
                    warn_msg(
                        &mut self.scroll_buffer,
                        "The turn that was running ended with the server. What it had done is \
                         saved; send a prompt to go on."
                            .into(),
                    );
                }
                running
            }
            Some(Err(gave_up)) => {
                self.offline = true;
                let why = match gave_up {
                    GaveUp::Stopped(error) | GaveUp::TimedOut(Some(error)) => error,
                    GaveUp::TimedOut(None) => "it didn't answer".into(),
                };
                err_msg(
                    &mut self.scroll_buffer,
                    format!(
                        "Couldn't reconnect to Koda's server: {why}. Your next prompt tries \
                         again; /quit leaves, and `koda --resume {}` picks this session up later.",
                        self.session.id
                    ),
                );
                None
            }
        }
    }
}

impl TuiContext {
    /// Take note of `serve/updating`, saying what it means for this window.
    /// True when `message` was it.
    pub(crate) fn note_update(&mut self, message: &koda_client::events::Outgoing) -> bool {
        let koda_client::events::Outgoing::Notification(
            koda_client::protocol::ServerNotification::ServeUpdating(update),
        ) = message
        else {
            return false;
        };
        let when = if update.working == 0 {
            "now".to_string()
        } else {
            format!(
                "once the turns running in {} session(s) finish; new turns are refused until then",
                update.working
            )
        };
        warn_msg(
            &mut self.scroll_buffer,
            format!(
                "Koda was updated to {}. Its server restarts onto it {when}. This window \
                 runs {} and can't go on after that: /quit, then `koda --resume {}`.",
                update.version,
                env!("CARGO_PKG_VERSION"),
                self.session.id
            ),
        );
        self.updated_to = Some(update.version.clone());
        true
    }
}

/// What a window says once its server has stopped for `version`.
fn updated_away(version: &str, session: &str) -> String {
    format!(
        "Koda's server stopped for the update to {version}, which this window ({}) can't \
         use. /quit, then `koda --resume {session}` goes on with this session.",
        env!("CARGO_PKG_VERSION")
    )
}

/// Whether a key press asks to quit while reconnecting.
fn quits(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn ctrl_c_and_ctrl_d_quit_while_reconnecting() {
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        assert!(quits(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(quits(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
        assert!(!quits(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!quits(&key(KeyCode::Enter, KeyModifiers::NONE)));
    }
}
