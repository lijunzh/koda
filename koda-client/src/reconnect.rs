//! Getting back to the shared server after the connection to it is lost,
//! as Codex's TUI does (`codex-rs/tui/src/app/reconnect.rs`): try right
//! away, then after 1, 2 and 4 seconds, then every 8, for two minutes in
//! all. Each try is the caller's: a fresh connection, `initialize`, and the
//! session opened again, after which the server resends the session's
//! pending approvals.

use std::future::Future;
use std::time::Duration;

use crate::RequestError;

/// How long the tries go on, waits included, as in Codex's TUI.
pub const BUDGET: Duration = Duration::from_secs(120);

/// The waits before the first tries, in seconds; after them, [`LATER_WAIT`].
const FIRST_WAITS: [u64; 4] = [0, 1, 2, 4];
/// The wait before each later try, in seconds.
const LATER_WAIT: u64 = 8;

/// Why a try didn't connect.
#[derive(Debug)]
pub enum Failure<E> {
    /// Worth trying again: the server isn't back yet, or is still closing.
    Retry(E),
    /// Trying again won't help.
    Stop(E),
}

/// Why reconnecting gave up.
#[derive(Debug)]
pub enum GaveUp<E> {
    /// [`BUDGET`] ran out; the last try's failure, if one ended.
    TimedOut(Option<E>),
    /// A try failed in a way trying again won't help.
    Stopped(E),
}

/// Tries `attempt` on Codex's schedule until it connects, fails for good,
/// or `budget` runs out, a try still running included.
pub async fn reconnect<T, E, F, Fut>(budget: Duration, mut attempt: F) -> Result<T, GaveUp<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Failure<E>>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut last = None;
    for wait in FIRST_WAITS.into_iter().chain(std::iter::repeat(LATER_WAIT)) {
        let tried = tokio::time::timeout_at(deadline, async {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            attempt().await
        })
        .await;
        match tried {
            Ok(Ok(connected)) => return Ok(connected),
            Ok(Err(Failure::Retry(error))) => last = Some(error),
            Ok(Err(Failure::Stop(error))) => return Err(GaveUp::Stopped(error)),
            Err(_) => break,
        }
    }
    Err(GaveUp::TimedOut(last))
}

/// Whether a failed try is worth repeating, as Codex's TUI decides: the
/// server can't be reached or hung up, or it refused to open the session
/// because a server that is going away still holds it.
pub fn worth_retrying(error: &RequestError) -> bool {
    match error {
        RequestError::Connect(_) | RequestError::Closed { .. } => true,
        RequestError::Server { error, .. } => {
            koda_state::writer_lock::is_active_writer(&error.message)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The tries' start times, in seconds from the first.
    async fn tried_at(
        budget: Duration,
        outcome: impl Fn(usize) -> Result<(), Failure<usize>>,
    ) -> (Vec<u64>, Result<(), GaveUp<usize>>) {
        let start = tokio::time::Instant::now();
        let times = Arc::new(Mutex::new(Vec::new()));
        let result = reconnect(budget, || {
            let times = Arc::clone(&times);
            let n = {
                let mut times = times.lock().unwrap();
                times.push(start.elapsed().as_secs());
                times.len()
            };
            let result = outcome(n);
            async move { result }
        })
        .await;
        let times = times.lock().unwrap().clone();
        (times, result)
    }

    #[tokio::test(start_paused = true)]
    async fn tries_on_codexs_schedule_for_two_minutes() {
        let (times, result) = tried_at(BUDGET, |n| Err(Failure::Retry(n))).await;
        assert_eq!(
            times,
            [
                0, 1, 3, 7, 15, 23, 31, 39, 47, 55, 63, 71, 79, 87, 95, 103, 111, 119
            ]
        );
        assert!(matches!(result, Err(GaveUp::TimedOut(Some(18)))));
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_the_first_connection() {
        let (times, result) = tried_at(BUDGET, |n| {
            if n < 3 {
                Err(Failure::Retry(n))
            } else {
                Ok(())
            }
        })
        .await;
        assert_eq!(times, [0, 1, 3]);
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_a_failure_trying_again_wont_help() {
        let (times, result) = tried_at(BUDGET, |n| {
            if n < 2 {
                Err(Failure::Retry(n))
            } else {
                Err(Failure::Stop(n))
            }
        })
        .await;
        assert_eq!(times, [0, 1]);
        assert!(matches!(result, Err(GaveUp::Stopped(2))));
    }

    /// A try still running when the budget ends is cut short.
    #[tokio::test(start_paused = true)]
    async fn the_budget_cuts_a_hung_try_short() {
        let result: Result<(), GaveUp<()>> = reconnect(Duration::from_secs(5), || async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(GaveUp::TimedOut(None))));
    }

    #[test]
    fn retries_a_lost_server_and_a_session_still_held() {
        let refused = |message: &str| RequestError::Server {
            method: "session/start",
            error: koda_protocol::rpc::ErrorObject {
                code: -32600,
                message: message.into(),
                data: None,
            },
        };
        assert!(worth_retrying(&RequestError::Connect("gone".into())));
        assert!(worth_retrying(&RequestError::Closed {
            method: "initialize"
        }));
        assert!(worth_retrying(&refused(
            "session s1 already has an active writer"
        )));
        assert!(!worth_retrying(&refused("session s1 not found")));
    }
}
