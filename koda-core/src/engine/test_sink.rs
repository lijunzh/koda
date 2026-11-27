//! [`TestSink`]: a sink that collects a turn's events, for tests.
//!
//! **Compiled by two crates from this one file:** koda-core's own unit
//! tests (`#[cfg(test)] mod test_sink` in `sink.rs`) and `koda-test-utils`
//! (`#[path]`) for the integration tests. A sink from another crate can't
//! stand in for koda-core's own tests, which would see a second koda-core,
//! and a public test sink would be public API on crates.io. So one file,
//! naming what it needs through `super`, and nothing to keep in sync.

use super::{EngineEvent, EngineSink};

/// A sink that collects events into a Vec for testing.
///
/// Optionally also broadcasts each event to subscribers (#1109 F3) so
/// tests can wait deterministically for a specific event (e.g.
/// `ToolCallStart`) instead of guessing wall-clock delays.
#[derive(Debug, Default)]
pub struct TestSink {
    events: std::sync::Mutex<Vec<EngineEvent>>,
    /// `Some` after [`Self::subscribe`] is called; broadcasts every emit().
    /// Lazy so tests that don't need it pay no allocation.
    broadcaster: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<EngineEvent>>>,
    /// Set by [`Self::cancel_when`]: cancels its token from inside `emit`.
    cancel_on: std::sync::Mutex<Option<CancelOn>>,
}

/// A token to cancel when an event matches, before the engine goes on.
struct CancelOn {
    cancel: tokio_util::sync::CancellationToken,
    pred: Box<dyn Fn(&EngineEvent) -> bool + Send + Sync>,
}

impl std::fmt::Debug for CancelOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelOn").finish_non_exhaustive()
    }
}

impl TestSink {
    /// Create an empty test sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all collected events.
    pub fn events(&self) -> Vec<EngineEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Get the count of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Check if no events were collected.
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }

    /// Cancel `cancel` when an event matching `pred` is emitted, from
    /// inside `emit`: the engine hasn't gone on past the event when the
    /// token is cancelled. A task that watches [`Self::subscribe`] cancels
    /// later, so a short wait after the event (a retry's) could run out
    /// first on a slow runner.
    pub fn cancel_when(
        &self,
        cancel: tokio_util::sync::CancellationToken,
        pred: impl Fn(&EngineEvent) -> bool + Send + Sync + 'static,
    ) {
        *self.cancel_on.lock().unwrap() = Some(CancelOn {
            cancel,
            pred: Box::new(pred),
        });
    }

    /// Subscribe to a live broadcast of events as they're emitted.
    ///
    /// **#1109 F3**: replaces `loop { sleep; check sink.events() }`
    /// patterns with `recv().await`. The broadcaster is created lazily
    /// on first call — emits before subscription are still captured
    /// in [`Self::events`] but won't appear in the receiver stream.
    ///
    /// Channel capacity is 256, more than enough for any test
    /// scenario; lagging receivers will see
    /// [`tokio::sync::broadcast::error::RecvError::Lagged`].
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        let mut guard = self.broadcaster.lock().unwrap();
        let sender = guard.get_or_insert_with(|| {
            let (tx, _) = tokio::sync::broadcast::channel(256);
            tx
        });
        sender.subscribe()
    }

    /// Wait for the first event matching `pred` or until `timeout`.
    /// Returns `Ok(event)` on match, `Err` on timeout or channel close.
    ///
    /// Convenience wrapper around [`Self::subscribe`]: handles the
    /// already-emitted-before-subscribe case by scanning [`Self::events`]
    /// once, then waits on the live channel for fresh events.
    pub async fn wait_for<F>(
        &self,
        timeout: std::time::Duration,
        pred: F,
    ) -> Result<EngineEvent, &'static str>
    where
        F: Fn(&EngineEvent) -> bool,
    {
        // Subscribe BEFORE the historical scan so we don't miss events
        // emitted between the scan and subscribe (the classic
        // "check-then-wait" race).
        let mut rx = self.subscribe();
        // Scan history first — maybe the event has already fired.
        if let Some(ev) = self.events().into_iter().find(|e| pred(e)) {
            return Ok(ev);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for predicate");
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return Ok(ev),
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err("sink closed");
                }
                Err(_) => return Err("timeout waiting for predicate"),
            }
        }
    }

    /// Wait for the first event for which `extract` returns `Some(T)`,
    /// or until `timeout`. Returns the extracted `T` on match, `Err`
    /// on timeout or channel close.
    ///
    /// This is the [`Iterator::find_map`] sister of [`Self::wait_for`]:
    /// useful when the test needs a *field* of the awaited event
    /// (e.g. the `id` of an `AskUserRequest`) rather than just
    /// confirmation that one was emitted. Eliminates the
    /// `wait_for(...).await + events().find_map(...)` two-step that
    /// previously required a fixed sleep before the historical scan.
    ///
    /// Same race-free subscribe-before-scan ordering as [`Self::wait_for`].
    pub async fn wait_for_map<F, T>(
        &self,
        timeout: std::time::Duration,
        mut extract: F,
    ) -> Result<T, &'static str>
    where
        F: FnMut(&EngineEvent) -> Option<T>,
    {
        let mut rx = self.subscribe();
        if let Some(value) = self.events().iter().find_map(&mut extract) {
            return Ok(value);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timeout waiting for predicate");
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if let Some(value) = extract(&ev) {
                        return Ok(value);
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return Err("sink closed");
                }
                Err(_) => return Err("timeout waiting for predicate"),
            }
        }
    }
}

impl EngineSink for TestSink {
    fn emit(&self, event: EngineEvent) {
        // Record first, then broadcast: a waiter subscribes before it scans
        // the record ([`Self::wait_for`]), so whenever it subscribes it
        // finds the event in one or the other. Broadcasting first left a
        // gap: a waiter subscribing after the broadcast check and scanning
        // before the record missed the event for good.
        self.events.lock().unwrap().push(event.clone());
        if let Some(tx) = self.broadcaster.lock().unwrap().as_ref() {
            // Ignore the SendError on zero subscribers; the record above
            // is authoritative.
            let _ = tx.send(event.clone());
        }
        let cancel = self
            .cancel_on
            .lock()
            .unwrap()
            .as_ref()
            .filter(|on| (on.pred)(&event))
            .map(|on| on.cancel.clone());
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A waiter that starts while an event is being emitted still finds it,
    /// in the record or on the broadcast. `emit` used to check for
    /// subscribers before recording the event, so a waiter that subscribed
    /// in between, then scanned the record before the event reached it,
    /// missed it and timed out (CI, 2026-11-27, under load). `cancel_when`'s
    /// check runs inside `emit`, so a waiter started from it lands in that
    /// gap every time.
    #[test]
    fn a_wait_that_starts_during_an_emit_still_sees_the_event() {
        let sink = std::sync::Arc::new(TestSink::new());
        let weak = std::sync::Arc::downgrade(&sink);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = std::sync::Arc::clone(&seen);
        sink.cancel_when(tokio_util::sync::CancellationToken::new(), move |_| {
            let sink = weak.upgrade().unwrap();
            let found = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .unwrap()
                    .block_on(sink.wait_for(std::time::Duration::from_millis(200), |e| {
                        matches!(e, EngineEvent::Info { .. })
                    }))
                    .is_ok()
            })
            .join()
            .unwrap();
            *record.lock().unwrap() = Some(found);
            false
        });
        sink.emit(EngineEvent::Info {
            message: "now".into(),
        });
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "a waiter that started during the emit missed its event"
        );
    }

    /// The token is cancelled by the time `emit` returns, before the engine
    /// could start the wait that follows the event.
    #[test]
    fn a_matching_event_cancels_before_emit_returns() {
        let sink = TestSink::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        sink.cancel_when(cancel.clone(), |e| matches!(e, EngineEvent::Warn { .. }));
        sink.emit(EngineEvent::Info {
            message: "not this one".into(),
        });
        assert!(!cancel.is_cancelled());
        sink.emit(EngineEvent::Warn {
            message: "this one".into(),
        });
        assert!(cancel.is_cancelled());
    }
}
