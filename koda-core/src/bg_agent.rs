//! Background sub-agent registry.
//!
//! Tracks sub-agents spawned with `background: true` in `InvokeAgent`.
//! The inference loop drains completed results and injects them as
//! user-role messages so the model sees them on the next iteration.
//!
//! ## Lifecycle
//!
//! 1. **Spawn**: `InvokeAgent { background: true }` creates a tokio task
//! 2. **Track**: the task handle + metadata are stored in `BgAgentRegistry`
//! 3. **Poll**: before each inference call, the loop calls `drain_completed()`
//! 4. **Inject**: completed results are appended as user messages
//! 5. **Cleanup**: on session end, all pending tasks are cancelled
//!
//! ## Thread safety
//!
//! The registry is wrapped in `Arc<Mutex<>>` and shared between the main
//! inference loop and the background task spawner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// A completed background agent result.
#[derive(Debug)]
pub struct BgAgentResult {
    /// The agent name that produced this result.
    pub agent_name: String,
    /// The original prompt that was delegated.
    pub prompt: String,
    /// The agent's output (or error message).
    pub output: String,
    /// Whether the agent succeeded.
    pub success: bool,
}

/// Handle returned when a background agent is spawned.
struct BgAgentEntry {
    agent_name: String,
    prompt: String,
    rx: oneshot::Receiver<Result<String, String>>,
}

/// Registry of running background sub-agents.
///
/// Shared via `Arc` between the inference loop (which drains results)
/// and the tool dispatch (which spawns agents).
pub struct BgAgentRegistry {
    pending: Mutex<HashMap<u32, BgAgentEntry>>,
    next_id: Mutex<u32>,
}

impl BgAgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Register a background agent task. Returns the task ID and a sender
    /// that the spawned task uses to deliver the result.
    pub fn register(
        &self,
        agent_name: &str,
        prompt: &str,
    ) -> (u32, oneshot::Sender<Result<String, String>>) {
        let (tx, rx) = oneshot::channel();
        let mut id = self.next_id.lock().unwrap();
        let task_id = *id;
        *id += 1;
        drop(id);

        self.pending.lock().unwrap().insert(
            task_id,
            BgAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                rx,
            },
        );

        (task_id, tx)
    }

    /// Drain all completed background agents. Non-blocking — only takes
    /// entries whose oneshot has already resolved.
    pub fn drain_completed(&self) -> Vec<BgAgentResult> {
        let mut guard = self.pending.lock().unwrap();
        let mut completed = Vec::new();
        let mut done_ids = Vec::new();

        for (id, entry) in guard.iter_mut() {
            match entry.rx.try_recv() {
                Ok(Ok(output)) => {
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output,
                        success: true,
                    });
                }
                Ok(Err(err)) => {
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output: err,
                        success: false,
                    });
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Still running
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending — task panicked or was cancelled
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output: "[background agent task was cancelled]".to_string(),
                        success: false,
                    });
                }
            }
        }

        for id in done_ids {
            guard.remove(&id);
        }

        completed
    }

    /// How many background agents are still running.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

impl Default for BgAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap in Arc for sharing between inference loop and tool dispatch.
pub fn new_shared() -> Arc<BgAgentRegistry> {
    Arc::new(BgAgentRegistry::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_complete() {
        let reg = BgAgentRegistry::new();
        let (task_id, tx) = reg.register("explore", "find all tests");
        assert_eq!(task_id, 1);
        assert_eq!(reg.pending_count(), 1);

        // Not yet complete
        assert!(reg.drain_completed().is_empty());

        // Complete it
        tx.send(Ok("found 42 tests".to_string())).unwrap();
        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_name, "explore");
        assert_eq!(results[0].output, "found 42 tests");
        assert!(results[0].success);
        assert_eq!(reg.pending_count(), 0);
    }

    #[test]
    fn drain_only_completed() {
        let reg = BgAgentRegistry::new();
        let (_id1, tx1) = reg.register("task", "build");
        let (_id2, _tx2) = reg.register("explore", "search");

        tx1.send(Ok("done".to_string())).unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_name, "task");
        assert_eq!(reg.pending_count(), 1); // explore still pending
    }

    #[test]
    fn dropped_sender_reports_cancelled() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register("task", "build");
        drop(tx); // simulate task panic/cancel

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(results[0].output.contains("cancelled"));
    }

    #[test]
    fn error_result() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register("verify", "check");
        tx.send(Err("test failures".to_string())).unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].output, "test failures");
    }
}
