// Apache-2.0 vendored module — see top-level NOTICE.

//! Per-agent inbox: an unbounded mpsc backing the message buffer plus a
//! `watch::Sender<u64>` carrying a monotonic sequence number for cheap
//! "anything new?" wakeups.
//!
//! The split between `Mailbox` (sender) and `MailboxReceiver` (drain
//! side, not `Clone`) mirrors how `tokio::sync::mpsc` enforces a single
//! consumer. The watch sequence is the public synchronization primitive
//! — `wait_agent`-style tools subscribe to it and park on
//! `changed().await` instead of polling.
//!
//! `pending_mails` on the receiver lets `has_pending` / `drain` give a
//! consistent view of "everything sent up to this point" without a race
//! against a producer that fired between `try_recv` and the next call.
//!
//! # Provenance
//!
//! Ported from `codex-rs/core/src/agent/mailbox.rs` at upstream commit
//! `213756c9ab22b567d426fe1be9757705fb5862c9` (codex `main` as of
//! 2026-05-05; the file's only commit — it was added in
//! `feat: add mailbox concept for wait (#16010)` and has not been
//! touched since).
//!
//! ## Local modifications
//!
//! - Updated import path for `InterAgentCommunication` to point at
//!   `crate::agent::inter_agent` (koda's vendored location).
//! - `pub(crate)` visibility raised to `pub` so the rest of koda-core
//!   and downstream tools can construct/consume mailboxes. Codex
//!   keeps this internal because their session module owns it; we'll
//!   do the same wiring in Phase 2 of #1325 but expose for now to
//!   keep this PR substrate-only.
//! - Test imports adapted (`AgentPath` from koda's path module).
//! - Tokio test attributes use `(flavor = "multi_thread")` to satisfy
//!   koda's #1109 F2 guard (any test touching `tokio::spawn` / `watch`
//!   / `broadcast` must run on the multi-thread runtime).
//!   Codex's upstream uses the bare attribute form.
//! - Added 2 koda-extra tests (`watch_wakes_a_parked_subscriber_on_send`
//!   exercises the parking contract Phase 3 will rely on;
//!   `drain_after_drain_is_empty` is a regression net).
//!
//! ## Vendor-sync skips
//!
//! (None.)

use crate::agent::inter_agent::InterAgentCommunication;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::watch;

/// Send-side handle for an agent's inbox. Cheaply cloneable across
/// tasks (the underlying `mpsc::UnboundedSender` is `Clone`-friendly
/// at the channel layer; wrap in `Arc<Mailbox>` to share).
pub struct Mailbox {
    tx: mpsc::UnboundedSender<InterAgentCommunication>,
    next_seq: AtomicU64,
    seq_tx: watch::Sender<u64>,
    /// Total mails the receiver has ack'd via [`MailboxReceiver::drain`].
    /// Lets sender-side callers (notably [`Self::has_pending`], used by
    /// `WaitForMail`'s fast path) tell whether mail has arrived since
    /// the last drain WITHOUT having to subscribe to the watch and
    /// race the publisher (the bug behind the
    /// `test_sub_agent_cache_hit_skips_llm` flake — #1325 Phase 5b
    /// follow-up).
    ///
    /// Shared `Arc` so the receiver can write to it from the other
    /// half of the pair after `Mailbox::new` splits them.
    drained_count: Arc<AtomicU64>,
}

/// Receive-side handle for an agent's inbox. Single-consumer (matches
/// `mpsc` semantics); held by the owning agent's session loop.
pub struct MailboxReceiver {
    rx: mpsc::UnboundedReceiver<InterAgentCommunication>,
    pending_mails: VecDeque<InterAgentCommunication>,
    /// Shared with [`Mailbox::drained_count`]. Bumped by `drain` to
    /// the cumulative number of mails the receiver has taken, so the
    /// sender-side `has_pending` view stays consistent.
    drained_count: Arc<AtomicU64>,
}

impl Mailbox {
    /// Creates a paired sender/receiver. Sequence starts at 0; the
    /// first `send` call assigns sequence 1.
    pub fn new() -> (Self, MailboxReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (seq_tx, _) = watch::channel(0);
        let drained_count = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                next_seq: AtomicU64::new(0),
                seq_tx,
                drained_count: Arc::clone(&drained_count),
            },
            MailboxReceiver {
                rx,
                pending_mails: VecDeque::new(),
                drained_count,
            },
        )
    }

    /// Subscribe for "anything new" wakeups. Each `changed().await`
    /// returns when `send` next bumps the sequence.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.seq_tx.subscribe()
    }

    /// True iff at least one mail has been sent that the owning
    /// receiver has not yet drained. Sender-side equivalent of
    /// [`MailboxReceiver::has_pending`] — useful when only a clone
    /// of the [`Mailbox`] (e.g. via [`crate::agent::MailboxRegistry`])
    /// is reachable. Used by `WaitForMail`'s fast path to short-
    /// circuit when mail arrived BEFORE the wait was issued (the
    /// codex `has_pending_mailbox_items` path; was incorrectly
    /// dropped during the koda port).
    ///
    /// `Relaxed` ordering is fine: the only invariant is monotonic
    /// growth, and a stale read just falls through to the watch
    /// subscribe path which is itself the source of truth.
    pub fn has_pending(&self) -> bool {
        let sent = self.next_seq.load(Ordering::Relaxed);
        let drained = self.drained_count.load(Ordering::Relaxed);
        sent > drained
    }

    /// Deliver one mail and bump the wakeup sequence. Returns the
    /// monotonic sequence number assigned to this delivery.
    ///
    /// Send-side errors (receiver dropped) are swallowed: at that
    /// point the recipient agent is gone and the message has nowhere
    /// to land. Codex does the same.
    pub fn send(&self, communication: InterAgentCommunication) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(communication);
        self.seq_tx.send_replace(seq);
        seq
    }
}

impl MailboxReceiver {
    fn sync_pending_mails(&mut self) {
        while let Ok(mail) = self.rx.try_recv() {
            self.pending_mails.push_back(mail);
        }
    }

    /// True iff at least one mail has been sent and not yet drained.
    /// Cheap: drains pending into the buffer first, then peeks.
    pub fn has_pending(&mut self) -> bool {
        self.sync_pending_mails();
        !self.pending_mails.is_empty()
    }

    /// True iff at least one buffered mail has `trigger_turn = true`.
    /// Used by the session loop to decide whether arrival of a mail
    /// should wake an idle turn (vs. just be folded in next time).
    pub fn has_pending_trigger_turn(&mut self) -> bool {
        self.sync_pending_mails();
        self.pending_mails.iter().any(|mail| mail.trigger_turn)
    }

    /// Take everything pending, in delivery order. After this call
    /// `has_pending` is false until new mail arrives.
    pub fn drain(&mut self) -> Vec<InterAgentCommunication> {
        self.sync_pending_mails();
        let drained: Vec<_> = self.pending_mails.drain(..).collect();
        // Publish the drain to the sender-side `Mailbox::has_pending`
        // view. `fetch_add` keeps `drained_count` monotonic even if a
        // future receiver adds non-drain take paths.
        if !drained.is_empty() {
            self.drained_count
                .fetch_add(drained.len() as u64, Ordering::Relaxed);
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    // Tests vendored verbatim from codex (mailbox_assigns_monotonic_sequence_numbers,
    // mailbox_drains_in_delivery_order, mailbox_tracks_pending_trigger_turn_mail).
    // Behavioral lockdown — these are the reference semantics.

    use super::*;
    use crate::agent::path::AgentPath;

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mailbox_assigns_monotonic_sequence_numbers() {
        let (mailbox, _receiver) = Mailbox::new();
        let mut seq_rx = mailbox.subscribe();

        let seq_a = mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        ));
        let seq_b = mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        ));

        seq_rx.changed().await.expect("first seq update");
        assert_eq!(*seq_rx.borrow(), seq_b);
        assert_eq!(seq_a, 1);
        assert_eq!(seq_b, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mailbox_drains_in_delivery_order() {
        let (mailbox, mut receiver) = Mailbox::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ false,
        );

        mailbox.send(mail_one.clone());
        mailbox.send(mail_two.clone());

        assert_eq!(receiver.drain(), vec![mail_one, mail_two]);
        assert!(!receiver.has_pending());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mailbox_tracks_pending_trigger_turn_mail() {
        let (mailbox, mut receiver) = Mailbox::new();

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        ));
        assert!(!receiver.has_pending_trigger_turn());

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        ));
        assert!(receiver.has_pending_trigger_turn());
    }

    // koda-added tests — exercise the parts we'll lean on in Phase 2/3
    // that codex's own tests don't cover directly.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_wakes_a_parked_subscriber_on_send() {
        // Verifies the wakeup contract: `wait_agent`-style tools that
        // park on `changed().await` MUST be woken by the next `send`.
        let (mailbox, _receiver) = Mailbox::new();
        let mut seq_rx = mailbox.subscribe();

        let waker = tokio::spawn(async move {
            // Wait at most 1s for the wakeup; this test should resolve
            // in microseconds in practice.
            tokio::time::timeout(std::time::Duration::from_secs(1), seq_rx.changed())
                .await
                .expect("waker timed out — watch channel did not wake")
                .expect("watch sender dropped");
            *seq_rx.borrow()
        });

        // Tiny yield so the spawned task definitely reaches `changed().await`
        // before we send. Not strictly necessary (watch buffers initial
        // state) but makes the test's intent unambiguous.
        tokio::task::yield_now().await;
        let seq = mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("path"),
            "wakey",
            false,
        ));

        let observed = waker.await.expect("waker task panicked");
        assert_eq!(observed, seq);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_after_drain_is_empty() {
        let (mailbox, mut receiver) = Mailbox::new();
        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/w").expect("path"),
            "x",
            false,
        ));
        assert_eq!(receiver.drain().len(), 1);
        assert_eq!(receiver.drain().len(), 0);
        assert!(!receiver.has_pending());
    }

    /// Sender-side `has_pending` (used by `WaitForMail`'s fast
    /// path — see `tools::wait_for_mail`) must stay in sync with the
    /// receiver's drain. This is the regression net for the
    /// `test_sub_agent_cache_hit_skips_llm` flake (#1325 Phase 5b
    /// follow-up): pre-fix, `Mailbox` had no sender-side view of
    /// pending mail, so `WaitForMail` raced the publisher via the
    /// watch channel and silently lost wakeups.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sender_side_has_pending_tracks_drain() {
        let (mailbox, mut receiver) = Mailbox::new();
        assert!(
            !mailbox.has_pending(),
            "fresh mailbox must report no pending"
        );

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/w").expect("path"),
            "a",
            false,
        ));
        assert!(
            mailbox.has_pending(),
            "after send, sender-side view must report pending"
        );

        let _ = receiver.drain();
        assert!(
            !mailbox.has_pending(),
            "after receiver drain, sender-side view must clear"
        );

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/w").expect("path"),
            "b",
            false,
        ));
        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/w").expect("path"),
            "c",
            false,
        ));
        assert!(
            mailbox.has_pending(),
            "sequence of sends must remain visible until drained"
        );

        assert_eq!(receiver.drain().len(), 2);
        assert!(
            !mailbox.has_pending(),
            "sender-side view clears once all messages drained"
        );
    }
}
