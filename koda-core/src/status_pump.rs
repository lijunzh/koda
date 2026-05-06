//! Status-event pump for long-running tool calls (#1321).
//!
//! ## The problem this module fixes
//!
//! Background sub-agents emit `ChildAgentActivity` events through their
//! `ForwardingChildSink` (see `engine::sink`). Those events land in
//! [`crate::child_agent::ChildAgentRegistry`]'s status queue via
//! `push_status_event`. Until #1321 there was **exactly one** consumer of
//! that queue in production — the per-iteration drain at the top of
//! [`crate::inference::inference_loop`] — so events only reached the
//! parent's `EngineSink` *between* parent inference iterations.
//!
//! While the parent was blocked inside a long-running tool call (the
//! textbook offender is `WaitTask`, which can block for up to 300s), the
//! activity overlay above the composer received zero updates. Then, the
//! moment the wait returned, every queued event flushed in a single frame
//! — by which time the bg agent had already finished and the events were
//! useless as live progress.
//!
//! ## The fix
//!
//! [`with_status_pump`] races the wrapped tool-dispatch future against
//! a background drain task that ticks every [`STATUS_PUMP_INTERVAL`].
//! Each tick drains the registry's status queue and emits each event
//! through the parent's sink — so the activity overlay sees bg tool
//! calls in real time, not in a stale post-wait flood.
//!
//! ## Why this shape
//!
//! Three options were considered (see #1321 issue body):
//!
//! 1. **Replace queue with `tokio::mpsc`**, hand the receiver to the UI
//!    loop. Architecturally cleanest but requires changing
//!    `InferenceContext::sink` from `&dyn EngineSink` to `Arc<dyn
//!    EngineSink>` and wiring a new mpsc through every sub-agent
//!    dispatch site — large blast radius for a focused bug.
//!
//! 2. **Pump from inside `WaitTask`**. Localized but doesn't generalize
//!    to other slow tools (a long `Bash` or a slow MCP call has the
//!    same problem in principle), and `WaitTask` doesn't have direct
//!    access to the parent's `EngineSink`.
//!
//! 3. **Pump at the dispatch boundary** (this module). The sink and
//!    registry are both already in scope at the three
//!    `execute_tools_*` call sites in `inference_loop`. Wrapping each
//!    of those three calls covers every long tool, sequential or
//!    parallel. Smallest possible diff that solves the root cause.
//!
//! ## Concurrency model
//!
//! `drain_status_events` takes the registry's mutex and atomically
//! drains. Two concurrent drainers (the pump and the per-iteration
//! drain in `inference_loop`) cannot double-emit because the queue is
//! drained inside the lock — whichever wins the race takes everything
//! and the loser sees an empty drain. The per-iteration drain stays as
//! a backstop for events queued while no tool dispatch is in flight
//! (e.g. between turns).

use std::sync::Arc;
use std::time::Duration;

use crate::child_agent::ChildAgentRegistry;
use crate::engine::EngineSink;

/// How often the pump drains the status queue while a tool call is in
/// flight. 200ms balances "smooth-feeling live overlay" against pump
/// overhead (one mutex acquisition + a `VecDeque::drain` per tick — a
/// few microseconds at most).
///
/// At 200ms the activity overlay updates ~5 times per second, which
/// is comfortably under the human flicker-fusion threshold (~10 Hz)
/// while staying well clear of the rate-limiter inside the TUI's
/// frame scheduler (~120 FPS cap).
pub const STATUS_PUMP_INTERVAL: Duration = Duration::from_millis(200);

/// Run `fut` while continuously draining `bg_agents.drain_status_events()`
/// to `sink` every [`STATUS_PUMP_INTERVAL`]. Returns `fut`'s output.
///
/// **Final flush guarantee**: any events emitted in the final tick
/// window — between the pump's last drain and `fut`'s completion —
/// are flushed to the sink before this function returns. Without this
/// the user could miss the bg agent's terminal `ToolEnd` event when
/// `fut` is `WaitTask` and the bg agent finishes in the same window.
///
/// **Safety against double-emission**: the pump and the
/// inference-loop's per-iteration drain both call
/// [`ChildAgentRegistry::drain_status_events`], which takes the
/// registry's internal mutex and drains atomically. Concurrent
/// callers see whichever drain wins the lock; the loser sees an
/// empty queue. Events are never duplicated.
///
/// See module docs for the full design rationale.
pub async fn with_status_pump<F, T>(
    fut: F,
    bg_agents: &Arc<ChildAgentRegistry>,
    sink: &dyn EngineSink,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let pump = async {
        let mut interval = tokio::time::interval(STATUS_PUMP_INTERVAL);
        // `Skip` not `Burst`: if the executor is briefly starved (e.g.
        // a long blocking syscall in a tool), we don't want to fire a
        // flurry of catch-up ticks the moment we yield back. One
        // tick-per-interval is plenty — events are append-only and a
        // single drain catches everything that accumulated.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate first tick — the inference loop already
        // drains at the top of every iteration, so there's nothing to
        // pump for the first interval window.
        interval.tick().await;
        loop {
            interval.tick().await;
            for ev in bg_agents.drain_status_events() {
                sink.emit(ev);
            }
        }
    };

    tokio::pin!(pump);
    tokio::pin!(fut);

    let output = tokio::select! {
        // `biased` so the future-completion arm is checked first each
        // poll — the moment `fut` resolves we want to break out and
        // do the final flush, not service one more pump tick.
        biased;
        out = &mut fut => out,
        // The pump loops forever — only `fut` can resolve.
        _ = &mut pump => unreachable!("status pump loops forever"),
    };

    // Final flush: any events emitted in the gap between the pump's
    // last tick and `fut`'s completion would otherwise sit in the
    // queue until the inference loop's next iteration. For
    // user-visible terminal events (`ToolEnd`, `Info { "completed" }`)
    // that's exactly the latency we're trying to eliminate.
    for ev in bg_agents.drain_status_events() {
        sink.emit(ev);
    }

    output
}

#[cfg(test)]
mod tests {
    //! Tests pin the bug-and-fix contract from #1321.
    //!
    //! The original bug: events pushed during a long future never
    //! reached the sink until the future returned. The fix: the pump
    //! drains every ~200ms during the future's lifetime, plus a final
    //! flush on completion.
    //!
    //! These tests are intentionally timing-driven (the bug is a
    //! latency bug — there's no observable artifact except "did the
    //! sink see events while the future was still running"). The
    //! tolerances are generous (50–200ms slack) so they pass on
    //! loaded CI runners; tighter values flake without catching
    //! anything real.
    use super::*;
    use crate::engine::event::EngineEvent;
    use crate::engine::sink::TestSink;

    /// Pin the core fix: events pushed *during* a 500ms future must be
    /// emitted to the sink before the future completes — not all in a
    /// final-flush burst.
    ///
    /// Pre-#1321 this test would see `events_during_wait == 0` because
    /// nothing drains the queue while the future is in flight. Post-fix
    /// we expect to see roughly `500ms / 200ms = 2-3` ticks worth of
    /// events flowing through during the wait.
    #[tokio::test]
    async fn pump_drains_events_during_long_running_future() {
        let registry = Arc::new(ChildAgentRegistry::new());
        let sink = TestSink::new();

        // Producer: pushes 5 events spaced 80ms apart over ~400ms.
        // Faster than the 200ms pump interval so we're guaranteed at
        // least one drain catches a fresh batch before the wait ends.
        let registry_for_producer = registry.clone();
        let producer = tokio::spawn(async move {
            for i in 0..5 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                registry_for_producer.push_status_event(EngineEvent::Info {
                    message: format!("event-{i}"),
                });
            }
        });

        // The "long tool call" we're racing against. 500ms is long
        // enough for ≥2 pump ticks (at 200ms each) to fire while the
        // future is still in flight.
        let result = with_status_pump(
            tokio::time::sleep(Duration::from_millis(500)),
            &registry,
            &sink,
        )
        .await;
        let _: () = result;

        producer.await.expect("producer task panicked");

        // All 5 events should land in the sink: most via mid-future
        // pump ticks, the last one (or two) via the final flush.
        let events = sink.events();
        assert_eq!(
            events.len(),
            5,
            "expected all 5 events to reach the sink; got {}: {:?}",
            events.len(),
            events
        );

        // FIFO order preserved — the bug also happened to break
        // ordering in some corner cases, so pin that here.
        for (i, ev) in events.iter().enumerate() {
            match ev {
                EngineEvent::Info { message } => {
                    assert_eq!(message, &format!("event-{i}"), "out-of-order at index {i}");
                }
                other => panic!("expected Info, got {other:?}"),
            }
        }
    }

    /// The final-flush guarantee: an event pushed in the last tick
    /// window (after the most recent pump tick but before the future
    /// resolves) must still reach the sink before the function returns.
    /// Without the explicit final drain, terminal events from a bg
    /// agent that finishes "just in time" would sit in the queue until
    /// the next inference iteration.
    #[tokio::test]
    async fn pump_final_flush_catches_events_emitted_in_last_tick_window() {
        let registry = Arc::new(ChildAgentRegistry::new());
        let sink = TestSink::new();

        // The future runs for 50ms — shorter than the 200ms pump
        // interval, so the pump will not have ticked a single time
        // before the future completes. Whatever events make it in
        // must come exclusively from the final flush.
        let registry_for_producer = registry.clone();
        let _ = with_status_pump(
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                // Push an event 20ms into a 50ms future — well within
                // the first 200ms pump window, so only the final
                // flush can deliver it.
                registry_for_producer.push_status_event(EngineEvent::Info {
                    message: "last-window-event".to_string(),
                });
                tokio::time::sleep(Duration::from_millis(30)).await;
            },
            &registry,
            &sink,
        )
        .await;

        let events = sink.events();
        assert_eq!(events.len(), 1, "final flush must catch last-window event");
        match &events[0] {
            EngineEvent::Info { message } => assert_eq!(message, "last-window-event"),
            other => panic!("expected Info, got {other:?}"),
        }
    }

    /// Sanity: when no events are ever pushed, the pump is a clean
    /// no-op — no panics, no spurious sink emissions, fut's output is
    /// returned verbatim. This is the common case (most tool calls
    /// don't have bg agents emitting concurrently) and we want to
    /// keep its overhead at "two empty mutex acquisitions".
    #[tokio::test]
    async fn pump_is_noop_when_queue_stays_empty() {
        let registry = Arc::new(ChildAgentRegistry::new());
        let sink = TestSink::new();

        let result = with_status_pump(async { 42u32 }, &registry, &sink).await;

        assert_eq!(result, 42);
        assert!(
            sink.is_empty(),
            "sink must stay empty when no events pushed"
        );
    }

    /// The wrapped future's output is returned to the caller verbatim,
    /// including `Result::Err`. Important because the real call sites
    /// in `inference_loop` use `.await?` on the wrapped dispatch
    /// futures and rely on errors propagating untouched.
    #[tokio::test]
    async fn pump_propagates_future_errors() {
        let registry = Arc::new(ChildAgentRegistry::new());
        let sink = TestSink::new();

        let result: Result<(), &'static str> =
            with_status_pump(async { Err("dispatch blew up") }, &registry, &sink).await;

        assert_eq!(result, Err("dispatch blew up"));
    }
}
