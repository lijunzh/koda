//! Asking about a host that isn't on the egress proxy's allow-list, as
//! Codex's managed proxy does (`core/src/tools/network_approval.rs`).
//!
//! A sandboxed command reaches the network only through the session's
//! proxies. A host on the allow-list goes through; for any other, the proxy
//! asks here, and this asks the person through the session's
//! [`Approver`](crate::approval_flow::Approver), with the host (Codex's `networkApprovalContext`). As in
//! Codex:
//!
//! - **"Yes"** lets this connection through. The next one asks again.
//!   Connections waiting on the same question share its answer.
//! - **"Yes, for this session"** lets every later connection to the host,
//!   on that protocol and port, through (`ApprovedForSession`).
//! - **"No"** refuses the connection and stops the command that opened it;
//!   the model is told the user denied the network access.
//! - **Which command asks:** the one running, when exactly one is. With
//!   none running (a background process, say) the question names no
//!   command; the client refuses it when no turn is running. With several,
//!   the connection is refused without asking, since stopping one of them
//!   would be a guess.
//!
//! With nobody to ask (a headless run), the host is refused and the
//! command goes on without it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use koda_sandbox::{HostAsker, NetworkProtocol};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::approval_flow::Approver;
use crate::engine::ApprovalDecision;

/// A request to reach a host that isn't on the allow-list.
#[derive(Debug, Clone)]
pub struct NetworkApproval {
    /// The command's tool call, when one command is running.
    pub tool_call_id: Option<String>,
    /// The command, when one command is running.
    pub command: Option<String>,
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
    /// Cancelled when the command ends: a question still open for it is
    /// withdrawn.
    ended: CancellationToken,
    /// Cancelled when the user refuses a host the command reached for: the
    /// command is stopped. [`Self::refused`] says which host.
    stop: CancellationToken,
    refused: Mutex<Option<String>>,
}

/// The session's answers about hosts off the allow-list, and the commands
/// running now. Shared by the session's proxies (as their [`HostAsker`])
/// and its tool registry, which registers each sandboxed command.
#[derive(Default)]
pub struct NetworkApprovals {
    /// Hosts allowed for the session, by [`host_key`].
    session_hosts: Mutex<HashSet<String>>,
    calls: Mutex<HashMap<u64, Arc<ActiveCall>>>,
    /// Questions being asked, by command and [`host_key`]: a connection
    /// to the same host meanwhile waits for the same answer.
    pending: Mutex<HashMap<String, watch::Receiver<Option<bool>>>>,
    next: AtomicU64,
    approver: RwLock<Option<Arc<dyn Approver>>>,
}

impl std::fmt::Debug for NetworkApprovals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkApprovals").finish_non_exhaustive()
    }
}

/// The key a host is allowed under, as Codex's `HostApprovalKey`: the
/// host, the protocol and the port.
fn host_key(host: &str, port: u16, protocol: NetworkProtocol) -> String {
    let protocol = match protocol {
        NetworkProtocol::Https => "https",
        NetworkProtocol::Socks5Tcp => "socks5-tcp",
    };
    format!("{protocol}://{host}:{port}")
}

/// While held, the command it names is running.
pub struct CallRegistration {
    approvals: Arc<NetworkApprovals>,
    id: u64,
    call: Arc<ActiveCall>,
}

impl CallRegistration {
    /// Resolves when the user refuses a host this command reached for,
    /// with what to tell the model: the command is to stop.
    pub async fn refused(&self) -> String {
        self.call.stop.cancelled().await;
        let target = self
            .call
            .refused
            .lock()
            .ok()
            .and_then(|refused| refused.clone())
            .unwrap_or_default();
        format!(
            "The user denied network access to {target}, so the command was stopped. \
             Don't retry it; do without that host, or ask the user."
        )
    }
}

impl Drop for CallRegistration {
    fn drop(&mut self) {
        if let Ok(mut calls) = self.approvals.calls.lock() {
            calls.remove(&self.id);
        }
        self.call.ended.cancel();
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
            ended: CancellationToken::new(),
            stop: CancellationToken::new(),
            refused: Mutex::new(None),
        });
        if let Ok(mut calls) = self.calls.lock() {
            calls.insert(id, Arc::clone(&call));
        }
        CallRegistration {
            approvals: Arc::clone(self),
            id,
            call,
        }
    }

    fn allowed_for_session(&self, key: &str) -> bool {
        self.session_hosts
            .lock()
            .is_ok_and(|hosts| hosts.contains(key))
    }

    /// The command a connection belongs to: `Ok(None)` with none running,
    /// `Err` with more than one.
    fn owner(&self) -> Result<Option<Arc<ActiveCall>>, ()> {
        let calls = self.calls.lock().map_err(|_| ())?;
        match calls.len() {
            0 => Ok(None),
            1 => Ok(calls.values().next().cloned()),
            _ => Err(()),
        }
    }

    /// Ask the person, for `owner` if there is one.
    async fn ask(
        &self,
        owner: Option<&Arc<ActiveCall>>,
        key: &str,
        host: &str,
        port: u16,
        protocol: NetworkProtocol,
    ) -> bool {
        let approver = self.approver.read().ok().and_then(|slot| slot.clone());
        let Some(approver) = approver else {
            return false;
        };
        let request = NetworkApproval {
            tool_call_id: owner.map(|call| call.tool_call_id.clone()),
            command: owner.map(|call| call.command.clone()),
            host: host.to_string(),
            port,
            protocol,
        };
        let question = approver.approve_network(request);
        // A command that ends (finished, or its turn stopped) takes its
        // question with it.
        let decision = match owner {
            Some(call) => tokio::select! {
                decision = question => decision,
                () = call.ended.cancelled() => None,
            },
            None => question.await,
        };
        match decision {
            Some(ApprovalDecision::ApproveForSession) => {
                if let Ok(mut hosts) = self.session_hosts.lock() {
                    hosts.insert(key.to_string());
                }
                true
            }
            Some(ApprovalDecision::Approve) => true,
            // Refused by policy (nobody to ask): the connection fails and
            // the command goes on, as Codex blocks it with no approval flow.
            Some(ApprovalDecision::RejectAuto { .. }) => false,
            Some(_) => {
                // The user said no: the command that reached for the host
                // stops, as Codex cancels its execution.
                if let Some(call) = owner {
                    if let Ok(mut refused) = call.refused.lock() {
                        *refused = Some(format!("{host}:{port}"));
                    }
                    call.stop.cancel();
                }
                false
            }
            None => false,
        }
    }
}

#[async_trait::async_trait]
impl HostAsker for NetworkApprovals {
    async fn allow(&self, host: &str, port: u16, protocol: NetworkProtocol) -> bool {
        let key = host_key(host, port, protocol);
        if self.allowed_for_session(&key) {
            return true;
        }
        let Ok(owner) = self.owner() else {
            tracing::debug!("network: {key} refused: more than one command running");
            return false;
        };
        let pending_key = format!(
            "{}\0{key}",
            owner.as_ref().map_or("", |call| call.tool_call_id.as_str())
        );
        // Join a question already open for this command and host.
        let (answer, asking) = {
            let Ok(mut pending) = self.pending.lock() else {
                return false;
            };
            match pending.get(&pending_key) {
                Some(waiting) => (Some(waiting.clone()), None),
                None => {
                    let (tx, rx) = watch::channel(None);
                    pending.insert(pending_key.clone(), rx);
                    (None, Some(tx))
                }
            }
        };
        if let Some(mut waiting) = answer {
            return match waiting.wait_for(Option::is_some).await {
                Ok(answer) => answer.unwrap_or(false),
                Err(_) => false,
            };
        }
        // Allowed for the session while this connection came in.
        let allowed = self.allowed_for_session(&key)
            || self.ask(owner.as_ref(), &key, host, port, protocol).await;
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&pending_key);
        }
        if let Some(tx) = asking {
            let _ = tx.send(Some(allowed));
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_flow::AgentApproval;

    /// Answers every network question with `answer` after `delay`,
    /// recording them.
    struct Answers {
        answer: ApprovalDecision,
        delay: std::time::Duration,
        asked: Mutex<Vec<NetworkApproval>>,
    }

    #[async_trait::async_trait]
    impl Approver for Answers {
        async fn approve(&self, _: AgentApproval) -> Option<ApprovalDecision> {
            None
        }
        async fn approve_network(&self, request: NetworkApproval) -> Option<ApprovalDecision> {
            self.asked.lock().unwrap().push(request);
            tokio::time::sleep(self.delay).await;
            Some(self.answer.clone())
        }
    }

    fn approvals(answer: ApprovalDecision) -> (Arc<NetworkApprovals>, Arc<Answers>) {
        let approvals = Arc::new(NetworkApprovals::default());
        let answers = Arc::new(Answers {
            answer,
            delay: std::time::Duration::ZERO,
            asked: Mutex::new(Vec::new()),
        });
        approvals.set_approver(answers.clone());
        (approvals, answers)
    }

    const HTTPS: NetworkProtocol = NetworkProtocol::Https;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_yes_lets_one_connection_through_and_the_next_asks_again() {
        let (approvals, answers) = approvals(ApprovalDecision::Approve);
        let _call = approvals.register("call_1", "curl https://example.com");
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        let asked = answers.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 2, "each connection asks");
        assert_eq!(asked[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            asked[0].command.as_deref(),
            Some("curl https://example.com")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_waiting_on_a_question_share_its_answer() {
        let approvals = Arc::new(NetworkApprovals::default());
        let answers = Arc::new(Answers {
            answer: ApprovalDecision::Approve,
            delay: std::time::Duration::from_millis(100),
            asked: Mutex::new(Vec::new()),
        });
        approvals.set_approver(answers.clone());
        let _call = approvals.register("call_1", "npm install");
        let (a, b) = tokio::join!(
            approvals.allow("registry.example", 443, HTTPS),
            approvals.allow("registry.example", 443, HTTPS),
        );
        assert!(a && b);
        assert_eq!(answers.asked.lock().unwrap().len(), 1, "one question");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_yes_for_the_session_covers_that_host_protocol_and_port() {
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
                .allow("example.com", 443, NetworkProtocol::Socks5Tcp)
                .await,
            "another protocol asks again (and this approver says yes)"
        );
        assert!(approvals.allow("example.com", 8443, HTTPS).await);
        assert_eq!(answers.asked.lock().unwrap().len(), 3, "and another port");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_no_refuses_and_stops_the_command() {
        let (approvals, _) = approvals(ApprovalDecision::Reject);
        let call = approvals.register("call_1", "curl https://example.com");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
        let told = tokio::time::timeout(std::time::Duration::from_secs(1), call.refused())
            .await
            .expect("the command is stopped");
        assert!(
            told.contains("denied network access to example.com:443"),
            "{told}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_no_command_running_the_question_names_none() {
        let (approvals, answers) = approvals(ApprovalDecision::Approve);
        assert!(approvals.allow("example.com", 443, HTTPS).await);
        let asked = answers.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].tool_call_id.is_none() && asked[0].command.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_two_commands_running_nothing_is_asked() {
        let (approvals, answers) = approvals(ApprovalDecision::Approve);
        let _a = approvals.register("call_1", "curl a");
        let _b = approvals.register("call_2", "curl b");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
        assert!(answers.asked.lock().unwrap().is_empty());
    }

    /// Never answers, as a person who hasn't yet; says when it is asked.
    #[derive(Default)]
    struct Silent {
        asked: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Approver for Silent {
        async fn approve(&self, _: AgentApproval) -> Option<ApprovalDecision> {
            None
        }
        async fn approve_network(&self, _: NetworkApproval) -> Option<ApprovalDecision> {
            self.asked.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_question_ends_with_its_command() {
        let approvals = Arc::new(NetworkApprovals::default());
        let silent = Arc::new(Silent::default());
        approvals.set_approver(silent.clone());
        let call = approvals.register("call_1", "curl https://example.com");
        let asking = {
            let approvals = Arc::clone(&approvals);
            tokio::spawn(async move { approvals.allow("example.com", 443, HTTPS).await })
        };
        // The command ends once it has been asked about, not after a guess
        // at how long asking takes.
        silent.asked.notified().await;
        drop(call);
        let allowed = tokio::time::timeout(std::time::Duration::from_secs(5), asking)
            .await
            .expect("the question ended with the command")
            .unwrap();
        assert!(!allowed);
    }

    /// A refusal by policy (a headless run) isn't a person's no: the
    /// connection fails and the command goes on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_policy_refusal_leaves_the_command_running() {
        let (approvals, _) = approvals(ApprovalDecision::RejectAuto {
            reason: "headless".into(),
        });
        let call = approvals.register("call_1", "curl https://example.com");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
        // A refusal stops the command before `allow` returns, so it would
        // show now; no wait that a slow runner could outlast.
        assert!(!call.call.stop.is_cancelled(), "the command was stopped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_nobody_to_ask_a_host_is_refused() {
        let approvals = Arc::new(NetworkApprovals::default());
        let _call = approvals.register("call_1", "curl https://example.com");
        assert!(!approvals.allow("example.com", 443, HTTPS).await);
    }
}
