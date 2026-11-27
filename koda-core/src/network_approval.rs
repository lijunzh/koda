//! Asking about a host that isn't on the egress proxy's allow-list, as
//! Codex's managed proxy does (`core/src/tools/network_approval.rs`).
//!
//! A sandboxed command reaches the network only through the session's
//! proxies. A host on the allow-list goes through; for any other, the proxy
//! asks here, and this asks the person through the session's
//! [`Approver`]: the request is the running command's, with the host
//! (Codex's `networkApprovalContext`).
//!
//! - **"Yes"** lets this command reach the host for as long as it runs.
//!   Codex's covers one connection and asks again on the next; a command
//!   such as `npm install` opens many, so Koda's covers the command.
//! - **"Yes, for this session"** lets every later command reach it, as
//!   Codex's `ApprovedForSession`.
//! - **"No"** refuses this connection and the command's later ones to the
//!   host. Codex also stops the command; Koda lets it see the refusal and
//!   fail, as it does when a host isn't allowed at all.
//!
//! The proxy can't tell which command opened a connection, so, as Codex
//! does when it can't attribute one, a connection while no command or more
//! than one is running is refused without asking. So is one with nobody to
//! ask (a headless run).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use koda_sandbox::{HostAsker, NetworkProtocol};

use crate::approval_flow::Approver;
use crate::engine::ApprovalDecision;

/// A request to reach a host that isn't on the allow-list.
#[derive(Debug, Clone)]
pub struct NetworkApproval {
    /// The command's tool call.
    pub tool_call_id: String,
    /// The command.
    pub command: String,
    /// The host, lowercased, without its port.
    pub host: String,
    /// The port.
    pub port: u16,
    /// How the connection came in.
    pub protocol: NetworkProtocol,
}

/// A running command the proxy's connections may belong to.
struct ActiveCall {
    tool_call_id: String,
    command: String,
    /// This command's answers, by `host:port`. Held across a question, so
    /// two connections to the same host wait for one answer.
    answers: tokio::sync::Mutex<HashMap<String, bool>>,
    /// Cancelled when the command ends: a question still open for it is
    /// withdrawn.
    ended: tokio_util::sync::CancellationToken,
}

/// The session's answers about hosts off the allow-list, and the commands
/// running now. Shared by the session's proxies (as their [`HostAsker`])
/// and its tool registry, which registers each sandboxed command.
#[derive(Default)]
pub struct NetworkApprovals {
    /// Hosts allowed for the session, as `host:port`.
    session_hosts: Mutex<HashSet<String>>,
    calls: Mutex<HashMap<u64, Arc<ActiveCall>>>,
    next: AtomicU64,
    approver: RwLock<Option<Arc<dyn Approver>>>,
}

impl std::fmt::Debug for NetworkApprovals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkApprovals").finish_non_exhaustive()
    }
}

/// While held, the command it names is running.
pub struct CallRegistration {
    approvals: Arc<NetworkApprovals>,
    id: u64,
}

impl Drop for CallRegistration {
    fn drop(&mut self) {
        if let Ok(mut calls) = self.approvals.calls.lock()
            && let Some(call) = calls.remove(&self.id)
        {
            call.ended.cancel();
        }
    }
}

impl NetworkApprovals {
    /// Where questions go: the session's client.
    pub fn set_approver(&self, approver: Arc<dyn Approver>) {
        if let Ok(mut slot) = self.approver.write() {
            *slot = Some(approver);
        }
    }

    /// Note that `command` (tool call `tool_call_id`) runs until the
    /// returned registration drops.
    pub fn register(self: &Arc<Self>, tool_call_id: &str, command: &str) -> CallRegistration {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let call = Arc::new(ActiveCall {
            tool_call_id: tool_call_id.to_string(),
            command: command.to_string(),
            answers: tokio::sync::Mutex::new(HashMap::new()),
            ended: tokio_util::sync::CancellationToken::new(),
        });
        if let Ok(mut calls) = self.calls.lock() {
            calls.insert(id, call);
        }
        CallRegistration {
            approvals: Arc::clone(self),
            id,
        }
    }

    fn allowed_for_session(&self, key: &str) -> bool {
        self.session_hosts
            .lock()
            .is_ok_and(|hosts| hosts.contains(key))
    }

    /// The one command running, if exactly one is.
    fn only_call(&self) -> Option<Arc<ActiveCall>> {
        let calls = self.calls.lock().ok()?;
        match calls.len() {
            1 => calls.values().next().cloned(),
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl HostAsker for NetworkApprovals {
    async fn allow(&self, host: &str, port: u16, protocol: NetworkProtocol) -> bool {
        let key = format!("{host}:{port}");
        if self.allowed_for_session(&key) {
            return true;
        }
        let Some(call) = self.only_call() else {
            tracing::debug!("network: {key} refused: no single command to ask for");
            return false;
        };
        let mut answers = call.answers.lock().await;
        if let Some(answer) = answers.get(&key) {
            return *answer;
        }
        // Another command may have allowed it for the session while this
        // one waited.
        if self.allowed_for_session(&key) {
            return true;
        }
        let approver = self.approver.read().ok().and_then(|slot| slot.clone());
        let Some(approver) = approver else {
            return false;
        };
        let request = NetworkApproval {
            tool_call_id: call.tool_call_id.clone(),
            command: call.command.clone(),
            host: host.to_string(),
            port,
            protocol,
        };
        // A command that ends (finished, or its turn stopped) takes its
        // question with it.
        let decision = tokio::select! {
            decision = approver.approve_network(request) => decision,
            () = call.ended.cancelled() => None,
        };
        let allowed = match decision {
            Some(ApprovalDecision::ApproveForSession) => {
                if let Ok(mut hosts) = self.session_hosts.lock() {
                    hosts.insert(key.clone());
                }
                true
            }
            Some(ApprovalDecision::Approve) => true,
            _ => false,
        };
        answers.insert(key, allowed);
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_flow::AgentApproval;

    /// Answers every network question with `answer`, counting them.
    struct Answers {
        answer: ApprovalDecision,
        asked: Mutex<Vec<NetworkApproval>>,
    }

    #[async_trait::async_trait]
    impl Approver for Answers {
        async fn approve(&self, _: AgentApproval) -> Option<ApprovalDecision> {
            None
        }
        async fn approve_network(&self, request: NetworkApproval) -> Option<ApprovalDecision> {
            self.asked.lock().unwrap().push(request);
            Some(self.answer.clone())
        }
    }

    fn approvals(answer: ApprovalDecision) -> (Arc<NetworkApprovals>, Arc<Answers>) {
        let approvals = Arc::new(NetworkApprovals::default());
        let answers = Arc::new(Answers {
            answer,
            asked: Mutex::new(Vec::new()),
        });
        approvals.set_approver(answers.clone());
        (approvals, answers)
    }

    const HTTPS: NetworkProtocol = NetworkProtocol::Https;

    #[tokio::test]
    async fn a_yes_covers_the_command_and_no_other() {
        let (approvals, answers) = approvals(ApprovalDecision::Approve);
        let call = approvals.register("call_1", "curl https://example.com");
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        assert_eq!(answers.asked.lock().unwrap().len(), 1, "asked once");
        let asked = answers.asked.lock().unwrap()[0].clone();
        assert_eq!(asked.tool_call_id, "call_1");
        assert_eq!(asked.command, "curl https://example.com");
        drop(call);

        let _next = approvals.register("call_2", "curl https://example.com");
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        assert_eq!(answers.asked.lock().unwrap().len(), 2, "a new command asks");
    }

    #[tokio::test]
    async fn a_yes_for_the_session_covers_later_commands_on_that_port() {
        let (approvals, answers) = approvals(ApprovalDecision::ApproveForSession);
        {
            let _call = approvals.register("call_1", "curl https://example.com");
            assert!(approvals.allow("example.com", 443, HTTPS).await);
        }
        let _later = approvals.register("call_2", "git fetch");
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        assert_eq!(answers.asked.lock().unwrap().len(), 1);
        assert!(
            approvals
                .allow("example.com", 22, NetworkProtocol::Socks5Tcp)
                .await,
            "another port asks again (and this approver says yes)"
        );
        assert_eq!(answers.asked.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_no_refuses_the_commands_later_connections_without_asking() {
        let (approvals, answers) = approvals(ApprovalDecision::Reject);
        let _call = approvals.register("call_1", "curl https://example.com");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
        assert_eq!(answers.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn with_no_single_command_running_nothing_is_asked() {
        let (approvals, answers) = approvals(ApprovalDecision::Approve);
        assert!(!approvals.allow("example.com", 443, HTTPS).await, "none");
        let _a = approvals.register("call_1", "curl a");
        let _b = approvals.register("call_2", "curl b");
        assert!(!approvals.allow("example.com", 443, HTTPS).await, "two");
        assert!(answers.asked.lock().unwrap().is_empty());
    }

    /// Never answers, as a person who hasn't yet.
    struct Silent;

    #[async_trait::async_trait]
    impl Approver for Silent {
        async fn approve(&self, _: AgentApproval) -> Option<ApprovalDecision> {
            None
        }
        async fn approve_network(&self, _: NetworkApproval) -> Option<ApprovalDecision> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_question_ends_with_its_command() {
        let approvals = Arc::new(NetworkApprovals::default());
        approvals.set_approver(Arc::new(Silent));
        let call = approvals.register("call_1", "curl https://example.com");
        let asking = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move { approvals.allow("example.com", 443, HTTPS).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(call);
        let allowed = tokio::time::timeout(std::time::Duration::from_secs(5), asking)
            .await
            .expect("the question ended with the command")
            .unwrap();
        assert!(!allowed);
    }

    #[tokio::test]
    async fn with_nobody_to_ask_a_host_is_refused() {
        let approvals = Arc::new(NetworkApprovals::default());
        let _call = approvals.register("call_1", "curl https://example.com");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
    }
}
