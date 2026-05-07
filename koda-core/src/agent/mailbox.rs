// Vendored from openai/codex (Apache-2.0) — see top-level NOTICE.
// Source: codex-rs/core/src/agent/mailbox.rs
// Local modifications:
//   - Updated import path for `InterAgentCommunication` to point at
//     `crate::agent::inter_agent` (koda's vendored location).
//   - `pub(crate)` visibility raised to `pub` so the rest of koda-core
//     and downstream tools can construct/consume mailboxes. Codex
//     keeps this internal because their session module owns it; we'll
//     do the same wiring in Phase 2 of #1325 but expose for now to
//     keep this PR substrate-only.
//   - Test imports adapted (`AgentPath` from koda's path module).
//   - Tokio test attributes use `(flavor = "multi_thread")` to satisfy
//     koda's #1109 F2 guard (any test touching `tokio::spawn` / `watch`
//     / `broadcast` must run on the multi-thread runtime).
//     Codex's upstream uses the bare attribute form.

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

use crate::agent::inter_agent::InterAgentCommunication;
use std::collections::VecDeque;
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
}

/// Receive-side handle for an agent's inbox. Single-consumer (matches
/// `mpsc` semantics); held by the owning agent's session loop.
pub struct MailboxReceiver {
    rx: mpsc::UnboundedReceiver<InterAgentCommunication>,
    pending_mails: VecDeque<InterAgentCommunication>,
}

impl Mailbox {
    /// Creates a paired sender/receiver. Sequence starts at 0; the
    /// first `send` call assigns sequence 1.
    pub fn new() -> (Self, MailboxReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (seq_tx, _) = watch::channel(0);
        (
            Self {
                tx,
                next_seq: AtomicU64::new(0),
                seq_tx,
            },
            MailboxReceiver {
                rx,
                pending_mails: VecDeque::new(),
            },
        )
    }

    /// Subscribe for "anything new" wakeups. Each `changed().await`
    /// returns when `send` next bumps the sequence.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.seq_tx.subscribe()
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
        self.pending_mails.drain(..).collect()
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
}
