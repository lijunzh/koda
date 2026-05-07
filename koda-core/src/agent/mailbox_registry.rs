//! Per-session lookup table mapping [`AgentPath`] → [`Mailbox`].
//!
//! Phase 3 of #1325 — the foundation under the peer tools
//! (`send_message`, `wait_for_mail`). Without it, tools have no way
//! to find the mailbox of an agent given its path; with it, every
//! peer tool reduces to "look up by path, do the thing".
//!
//! # Relationship to codex's `AgentRegistry`
//!
//! Codex's upstream pattern (`codex-rs/core/src/agent/registry.rs`,
//! ~344 lines) is a richer `AgentRegistry` that tracks per-thread
//! metadata (nickname, role, last task, agent_id), reserves spawn
//! slots with depth limits, and pools nicknames — then routes mail
//! indirectly via `agent_id_for_path` → thread → session → mailbox.
//!
//! Koda doesn't have `ThreadId`, `SessionSource`, depth limits, or
//! a thread-keyed session lookup yet. Vendoring the full codex
//! `AgentRegistry` would force vendoring `AgentControl` (1246 lines)
//! and large chunks of session plumbing too — way over-scoped for
//! Phase 3, where the only goal is "give the LLM a way to send and
//! receive mail through real tools".
//!
//! `MailboxRegistry` is the lean koda equivalent: a direct
//! `path → mailbox` shortcut that compresses codex's two-hop
//! resolution into one. Phase 4 (when `spawn_agent` lands) will
//! either grow this type to absorb thread/nickname/depth concerns,
//! or vendor codex's `AgentRegistry` alongside it and demote this
//! one to a thin lookup cache. Either way, the public surface
//! (`get`/`register`/`unregister`/`list`) stays — callers won't
//! notice.
//!
//! # Why not extend `ChildAgentRegistry`
//!
//! Koda's existing `ChildAgentRegistry` is 1300+ lines of bg-task
//! lifecycle (status events, fg/bg bookkeeping, undo coordination,
//! reservation guards). Mailbox lookup is none of those things —
//! it's a flat dictionary. Wedging it in would couple unrelated
//! concerns and make the eventual Phase 4-5 substrate unification
//! (where bg agents become first-class peers with mailboxes)
//! harder to reason about.
//!
//! # Concurrency
//!
//! `RwLock<HashMap<AgentPath, Mailbox>>` (parking_lot, sync) — every
//! operation is O(1) and never awaits, so an async mutex would be
//! ceremony for nothing. Reads (`get`, `list`) are far more common
//! than mutations (`register`, `unregister`), justifying the
//! reader-writer split over a `Mutex`.
//!
//! `Mailbox` is `Clone`-friendly at the channel layer (the
//! underlying `mpsc::UnboundedSender` is `Clone`), so `get` returns
//! an owned `Mailbox` and the caller can `send` without holding the
//! lock.
//!
//! # Phase 3 deferral
//!
//! No caller-spawner scoping yet: any tool with access to the
//! registry can `get` any registered path. Phase 3 has only one
//! registered path (`/root`), so scoping wouldn't bite anyway. When
//! `spawn_agent` lands in Phase 4 and the path tree grows, port
//! codex's Model E discipline (an agent can only mail
//! siblings/children it spawned).

//! `Mailbox` itself isn't `Clone` (the sequence counter would
//! diverge across clones), so the registry stores `Arc<Mailbox>` per
//! the substrate's documented sharing pattern. `get` returns the
//! `Arc` clone so the caller can `send` without holding the registry
//! lock.

use crate::agent::AgentPath;
use crate::agent::Mailbox;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Path → mailbox lookup table. See module docs for design rationale.
#[derive(Default)]
pub struct MailboxRegistry {
    inner: RwLock<HashMap<AgentPath, Arc<Mailbox>>>,
}

/// Outcome of [`MailboxRegistry::register`].
#[derive(Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Path was vacant; the supplied mailbox is now the registry's
    /// entry for that path.
    Inserted,
    /// Path was already registered; the **previous** mailbox stays
    /// in place and the supplied one is dropped.
    ///
    /// We deliberately reject re-registration rather than overwrite:
    /// an overwrite would silently invalidate every cloned `Mailbox`
    /// handle that callers might still hold for the old entry. A
    /// future `unregister`-then-`register` flow makes the re-bind
    /// explicit.
    AlreadyRegistered,
}

impl MailboxRegistry {
    /// Construct an empty registry. The caller (typically
    /// `KodaSession::new`) registers the root path immediately
    /// after.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a mailbox at `path`, **rejecting** double-registration
    /// (returns [`RegisterOutcome::AlreadyRegistered`] without
    /// touching the existing entry).
    pub fn register(&self, path: AgentPath, mailbox: Arc<Mailbox>) -> RegisterOutcome {
        let mut guard = self.inner.write();
        if guard.contains_key(&path) {
            return RegisterOutcome::AlreadyRegistered;
        }
        guard.insert(path, mailbox);
        RegisterOutcome::Inserted
    }

    /// Remove the entry at `path` if present. Returns `true` iff
    /// something was actually removed — useful for distinguishing
    /// "agent went away" from "agent never existed" in caller
    /// telemetry.
    pub fn unregister(&self, path: &AgentPath) -> bool {
        self.inner.write().remove(path).is_some()
    }

    /// Look up the mailbox for `path`. Returns an `Arc<Mailbox>`
    /// clone so the caller can `send` without holding the registry
    /// lock — the lock is released when this method returns.
    ///
    /// `None` means "no agent registered at this path" — the tool
    /// surface should translate this to a model-visible error so
    /// the LLM can pick a different recipient.
    pub fn get(&self, path: &AgentPath) -> Option<Arc<Mailbox>> {
        self.inner.read().get(path).cloned()
    }

    /// List every registered path. If `prefix` is `Some`, only paths
    /// whose string form starts with that prefix are returned.
    /// Returns paths in **sorted** order so the LLM-facing
    /// `list_agents` tool (Phase 4) gets deterministic output.
    pub fn list(&self, prefix: Option<&str>) -> Vec<AgentPath> {
        let guard = self.inner.read();
        let mut out: Vec<AgentPath> = guard
            .keys()
            .filter(|p| prefix.is_none_or(|pre| p.as_str().starts_with(pre)))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        out
    }

    /// Number of registered paths. Mostly useful for tests + future
    /// telemetry.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// True iff no paths are registered.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_mailbox() -> Arc<Mailbox> {
        let (mb, _rx) = Mailbox::new();
        Arc::new(mb)
    }

    #[test]
    fn register_then_get_round_trips() {
        // Pin the basic round-trip contract: every peer tool's
        // entire happy path is "register on session start, get on
        // tool call". If this breaks, every peer tool breaks.
        let reg = MailboxRegistry::new();
        let path = AgentPath::root();
        assert_eq!(
            reg.register(path.clone(), fresh_mailbox()),
            RegisterOutcome::Inserted
        );
        assert!(reg.get(&path).is_some(), "registered path must be lookupable");
    }

    #[test]
    fn double_register_rejects_and_preserves_original() {
        // Pin the no-overwrite contract documented on RegisterOutcome.
        // A silent overwrite would invalidate held Mailbox clones for
        // the old entry — exactly the class of bug this rejection
        // policy exists to prevent.
        let reg = MailboxRegistry::new();
        let path = AgentPath::root();
        let mb1 = fresh_mailbox();
        let mb2 = fresh_mailbox();
        assert_eq!(reg.register(path.clone(), mb1), RegisterOutcome::Inserted);
        assert_eq!(
            reg.register(path.clone(), mb2),
            RegisterOutcome::AlreadyRegistered
        );
        assert_eq!(reg.len(), 1, "double-register must not grow the registry");
    }

    #[test]
    fn unregister_removes_entry_and_returns_true() {
        let reg = MailboxRegistry::new();
        let path = AgentPath::root();
        reg.register(path.clone(), fresh_mailbox());
        assert!(reg.unregister(&path), "unregister must return true on hit");
        assert!(reg.get(&path).is_none(), "entry must actually be gone");
        assert!(
            !reg.unregister(&path),
            "unregister of missing entry must return false"
        );
    }

    #[test]
    fn list_returns_sorted_paths() {
        // Pin the sorted-order contract — Phase 4's list_agents tool
        // and any LLM that learns to depend on stable order would
        // both break under a HashMap-iteration-order regression.
        let reg = MailboxRegistry::new();
        for name in ["worker", "alpha", "researcher"] {
            reg.register(
                AgentPath::root().join(name).unwrap(),
                fresh_mailbox(),
            );
        }
        let listed = reg.list(None);
        let strs: Vec<&str> = listed.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            strs,
            vec!["/root/alpha", "/root/researcher", "/root/worker"],
            "list must return paths in sorted order"
        );
    }

    #[test]
    fn list_with_prefix_filters_correctly() {
        // Pin: prefix-filter is plain string startsWith on the path
        // form. Phase 4's list_agents will expose this directly to
        // the LLM.
        let reg = MailboxRegistry::new();
        reg.register(AgentPath::root(), fresh_mailbox());
        reg.register(
            AgentPath::root().join("worker").unwrap(),
            fresh_mailbox(),
        );
        reg.register(
            AgentPath::root().join("worker").unwrap().join("nested").unwrap(),
            fresh_mailbox(),
        );
        let listed = reg.list(Some("/root/worker"));
        let strs: Vec<&str> = listed.iter().map(|p| p.as_str()).collect();
        assert_eq!(
            strs,
            vec!["/root/worker", "/root/worker/nested"],
            "prefix filter must keep only matching paths, sorted"
        );
    }

    #[test]
    fn get_returns_clonable_mailbox_that_outlives_lock() {
        // Pin: get must release the lock before returning. Otherwise
        // a tool that does `let mb = reg.get(p).unwrap(); mb.send(...);`
        // would hold the read lock across the send — fine for now
        // but a future refactor that changes send() to await would
        // deadlock under contention.
        let reg = MailboxRegistry::new();
        let (mb_orig, mut rx) = Mailbox::new();
        reg.register(AgentPath::root(), Arc::new(mb_orig));
        let cloned = reg.get(&AgentPath::root()).unwrap();
        // Verify the clone actually delivers — if get accidentally
        // returned a 'fresh' Mailbox unhooked from the receiver, the
        // send would silently land nowhere.
        let seq = cloned.send(crate::agent::InterAgentCommunication {
            author: AgentPath::root(),
            recipient: AgentPath::root(),
            other_recipients: Vec::new(),
            content: "hi".to_string(),
            trigger_turn: false,
        });
        assert_eq!(seq, 1, "first send must assign sequence 1");
        assert!(rx.has_pending(), "receiver must see the delivery");
    }
}
