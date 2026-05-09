//! Frame draw scheduling — coalesces redraw requests to bound CPU/redraw cost.
//!
//! ## Why this exists (#1138)
//!
//! Before this module, the inference loop called `terminal.draw()` synchronously
//! at the top of every iteration. Under heavy event throughput (sub-agent
//! fan-out, fast streaming, parallel tools) that's ~one full redraw per engine
//! event. Wasted CPU + amplified executor pressure that contributed to #1137.
//!
//! With this module, callers fire-and-forget `frame_requester.schedule_frame()`
//! whenever they think a redraw might be useful. A dedicated tokio task
//! ([`FrameScheduler`]) coalesces multiple requests into a **single** notification
//! on an mpsc channel, rate-limited to a maximum frame rate (default ~120 FPS).
//!
//! The TUI event loop then has a `Draw` arm in its `tokio::select!` that pops
//! one notification and performs the actual `terminal.draw()`.
//!
//! ## Design
//!
//! Actor-style: handles ([`FrameRequester`]) are cheap to clone and send across
//! tasks. They forward `Instant` deadlines to the scheduler over an unbounded
//! mpsc. The scheduler folds multiple deadlines into the **earliest** one,
//! sleeps until it expires, then emits exactly one `()` notification.
//!
//! Inspired by — but not copied from — codex's `tui/frame_requester.rs` (the
//! codex codebase is Apache-2.0; this is an independent MIT implementation
//! using the same well-known coalescing pattern).

use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;

/// 120 FPS minimum interval between emitted draw notifications.
///
/// Public so tests can reason about rate limiting without magic numbers.
pub(crate) const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(8_333_334);

/// Cheap, cloneable handle for requesting a future redraw of the TUI.
///
/// Drop the last clone to shut down the associated [`FrameScheduler`] task.
#[derive(Clone, Debug)]
pub(crate) struct FrameRequester {
    tx: mpsc::UnboundedSender<Instant>,
}

impl FrameRequester {
    /// Request a redraw as soon as the rate limiter allows.
    ///
    /// Cheap and infallible from the caller's perspective: if the scheduler
    /// has already exited (e.g. during shutdown), the request is silently
    /// dropped. That is the correct behavior — there is nothing to draw on.
    pub(crate) fn schedule_frame(&self) {
        let _ = self.tx.send(Instant::now());
    }

    /// Request a redraw at least `dur` from now.
    ///
    /// Used by the bg-activity overlay's self-perpetuating draw loop
    /// (#1354): each draw schedules the next one ~1 s out as long as
    /// there's a non-terminal bg agent or process running, so the age
    /// column ticks and the pill stays live without depending on the
    /// user pressing keys. Stops automatically when `total == 0`.
    pub(crate) fn schedule_frame_in(&self, dur: Duration) {
        let _ = self.tx.send(Instant::now() + dur);
    }
}

/// Receiver side of the frame notification channel.
///
/// The TUI event loop holds this and consumes from it inside its
/// `tokio::select!` to know when to actually call `terminal.draw()`.
pub(crate) type DrawNotifyRx = mpsc::Receiver<()>;

/// Build a connected `(FrameRequester, DrawNotifyRx)` pair and spawn the
/// background scheduler that connects them.
///
/// Drop the requester (and any clones) to shut down the scheduler.
pub(crate) fn spawn_frame_scheduler() -> (FrameRequester, DrawNotifyRx) {
    // bounded channel of size 1: extra notifications coalesce naturally
    // because the consumer just calls `terminal.draw()` once per `recv()`.
    let (notify_tx, notify_rx) = mpsc::channel::<()>(1);
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Instant>();
    let scheduler = FrameScheduler {
        deadlines: req_rx,
        notify_tx,
        rate_limiter: FrameRateLimiter::default(),
    };
    tokio::spawn(scheduler.run());
    (FrameRequester { tx: req_tx }, notify_rx)
}

/// Internal task that coalesces deadline requests and emits notifications.
struct FrameScheduler {
    deadlines: mpsc::UnboundedReceiver<Instant>,
    notify_tx: mpsc::Sender<()>,
    rate_limiter: FrameRateLimiter,
}

impl FrameScheduler {
    async fn run(mut self) {
        // Effectively "infinity" — a one-year sleep we cancel as soon as
        // the first request arrives. Arms the select branch unconditionally
        // so the tokio::select! always has two pollable arms.
        const NEVER: Duration = Duration::from_secs(60 * 60 * 24 * 365);

        let mut next_deadline: Option<Instant> = None;

        loop {
            let target = next_deadline.unwrap_or_else(|| Instant::now() + NEVER);
            let sleep = tokio::time::sleep_until(target.into());
            tokio::pin!(sleep);

            tokio::select! {
                maybe_req = self.deadlines.recv() => {
                    let Some(requested) = maybe_req else {
                        // All requesters dropped → shut down cleanly.
                        return;
                    };
                    let clamped = self.rate_limiter.clamp_deadline(requested);
                    next_deadline = Some(
                        next_deadline.map_or(clamped, |cur| cur.min(clamped)),
                    );
                    // Loop continues; we recompute `target` and re-arm the
                    // sleep so multiple requests fold into one fire.
                }
                _ = &mut sleep => {
                    if next_deadline.take().is_some() {
                        self.rate_limiter.mark_emitted(target);
                        // try_send so a slow consumer can't deadlock the
                        // scheduler. If the channel is already full, the
                        // consumer hasn't drained the previous notification
                        // yet — they'll see "draw needed" on next recv()
                        // anyway, so dropping the duplicate is correct.
                        let _ = self.notify_tx.try_send(());
                    }
                }
            }
        }
    }
}

/// Tracks the last-emitted timestamp and clamps deadlines forward to
/// enforce a minimum interval between frame notifications.
#[derive(Debug, Default)]
struct FrameRateLimiter {
    last_emitted_at: Option<Instant>,
}

impl FrameRateLimiter {
    /// Clamp `requested` forward to honor [`MIN_FRAME_INTERVAL`].
    fn clamp_deadline(&self, requested: Instant) -> Instant {
        let Some(prev) = self.last_emitted_at else {
            return requested;
        };
        let earliest_allowed = prev.checked_add(MIN_FRAME_INTERVAL).unwrap_or(prev);
        requested.max(earliest_allowed)
    }

    fn mark_emitted(&mut self, at: Instant) {
        self.last_emitted_at = Some(at);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time;

    /// Helper: wait for one notification with a timeout. Returns true if a
    /// notification arrived in time, false on timeout.
    async fn wait_for_draw(rx: &mut DrawNotifyRx, timeout: Duration) -> bool {
        tokio::select! {
            res = rx.recv() => res.is_some(),
            _ = time::sleep(timeout) => false,
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rate_limiter_does_not_clamp_first_request() {
        let limiter = FrameRateLimiter::default();
        let now = Instant::now();
        assert_eq!(limiter.clamp_deadline(now), now);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rate_limiter_clamps_to_min_interval() {
        let mut limiter = FrameRateLimiter::default();
        let t0 = Instant::now();
        limiter.mark_emitted(t0);
        let too_soon = t0 + Duration::from_micros(100);
        assert_eq!(limiter.clamp_deadline(too_soon), t0 + MIN_FRAME_INTERVAL);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn schedule_frame_emits_one_notification() {
        let (req, mut rx) = spawn_frame_scheduler();
        req.schedule_frame();
        time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn three_requests_coalesce_into_one_notification() {
        // The whole point of the module: three rapid requests should fire
        // a single notification, not three.
        let (req, mut rx) = spawn_frame_scheduler();
        req.schedule_frame();
        req.schedule_frame();
        req.schedule_frame();

        time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);
        // No second notification for the same coalesced batch.
        assert!(!wait_for_draw(&mut rx, Duration::from_millis(20)).await);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn schedule_frame_in_delays_notification() {
        let (req, mut rx) = spawn_frame_scheduler();
        req.schedule_frame_in(Duration::from_millis(50));

        // Before the deadline: nothing yet.
        time::advance(Duration::from_millis(30)).await;
        assert!(!wait_for_draw(&mut rx, Duration::from_millis(5)).await);

        // After the deadline: one notification.
        time::advance(Duration::from_millis(25)).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rate_limit_caps_consecutive_notifications() {
        // After one draw, a second `schedule_frame()` must wait at least
        // MIN_FRAME_INTERVAL before the notification fires.
        let (req, mut rx) = spawn_frame_scheduler();

        req.schedule_frame();
        time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);

        req.schedule_frame();
        // Less than MIN_FRAME_INTERVAL — must not fire yet.
        time::advance(Duration::from_micros(100)).await;
        assert!(!wait_for_draw(&mut rx, Duration::from_micros(100)).await);

        // Past MIN_FRAME_INTERVAL — must now fire.
        time::advance(MIN_FRAME_INTERVAL).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dropping_all_requesters_shuts_down_scheduler() {
        let (req, mut rx) = spawn_frame_scheduler();
        drop(req);
        // Receiver should observe channel closure (recv returns None).
        let res = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(matches!(res, Ok(None)), "scheduler should drop notify_tx");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn mixed_immediate_and_delayed_coalesce_to_earliest() {
        let (req, mut rx) = spawn_frame_scheduler();
        req.schedule_frame_in(Duration::from_millis(100));
        req.schedule_frame(); // immediate; should win

        time::advance(Duration::from_millis(1)).await;
        assert!(wait_for_draw(&mut rx, Duration::from_millis(50)).await);
        // The 100ms-delayed request was coalesced — no second draw.
        assert!(!wait_for_draw(&mut rx, Duration::from_millis(120)).await);
    }
}
