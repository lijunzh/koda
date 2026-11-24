//! `session/backgroundTerminals/*` and `agent/list`: what a session's agent
//! runs beside its turns. Background processes outlive the turn that started
//! them, so stopping a turn leaves them running, as Codex's do.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koda_core::agent::KodaAgent;
use koda_core::tools::bg_process::BgProcessStatus;
use koda_protocol::agent::{AgentInfo, AgentListResponse, AgentSource};
use koda_protocol::background::{
    BackgroundAgent, BackgroundTerminal, BackgroundTerminalsCleanResponse,
    BackgroundTerminalsListResponse, BackgroundTerminalsTerminateResponse,
};
use koda_protocol::rpc::ErrorObject;

use crate::session::invalid;

/// When something `age` old started, in Unix milliseconds.
fn started_at(age: Duration) -> i64 {
    let started = SystemTime::now()
        .checked_sub(age)
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(started.as_millis()).unwrap_or(i64::MAX)
}

/// The session's running background processes, oldest first.
pub(crate) fn list(agent: &KodaAgent) -> BackgroundTerminalsListResponse {
    let mut data: Vec<(Duration, BackgroundTerminal)> = agent
        .tools
        .bg_registry
        .snapshot()
        .into_iter()
        .filter(|p| p.status == BgProcessStatus::Running)
        .map(|p| {
            (
                p.age,
                BackgroundTerminal {
                    process_id: p.pid.to_string(),
                    command: p.command,
                    // Every background command starts in the project.
                    cwd: agent.project_root.display().to_string(),
                    os_pid: p.pid,
                    started_at: started_at(p.age),
                },
            )
        })
        .collect();
    data.sort_by_key(|(age, _)| std::cmp::Reverse(*age));
    let registry = &agent.tools.bg_registry;
    let mut agents: Vec<BackgroundAgent> = registry
        .agents()
        .snapshot()
        .into_iter()
        .filter(|a| a.state == koda_core::tools::bg_agents::AgentState::Running)
        .map(|a| BackgroundAgent {
            waiting_for_approval: a.waiting,
            agent_id: a.id,
            agent_name: a.agent_name,
            prompt: a.prompt,
            started_at: a
                .started
                .duration_since(UNIX_EPOCH)
                .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0),
        })
        .collect();
    agents.sort_by_key(|a| a.started_at);
    BackgroundTerminalsListResponse {
        data: data.into_iter().map(|(_, terminal)| terminal).collect(),
        waiting: registry.any_working() || registry.has_notices(),
        agents,
    }
}

/// Stop the running process `process_id`, and its group.
pub(crate) fn terminate(
    agent: &KodaAgent,
    process_id: &str,
) -> Result<BackgroundTerminalsTerminateResponse, ErrorObject> {
    if koda_core::tools::bg_agents::BgAgents::is_agent_id(process_id) {
        let stopped = agent.tools.bg_registry.agents().stop(process_id);
        return Ok(BackgroundTerminalsTerminateResponse {
            terminated: stopped == koda_core::tools::bg_agents::Stop::Stopped,
        });
    }
    let pid: u32 = process_id
        .parse()
        .map_err(|_| invalid(format!("no background process {process_id:?}")))?;
    let registry = &agent.tools.bg_registry;
    let running = registry
        .snapshot()
        .iter()
        .any(|p| p.pid == pid && p.status == BgProcessStatus::Running);
    Ok(BackgroundTerminalsTerminateResponse {
        terminated: running && registry.kill(pid),
    })
}

/// Stop every running one.
pub(crate) fn clean(agent: &KodaAgent) -> BackgroundTerminalsCleanResponse {
    let registry = &agent.tools.bg_registry;
    let terminated = registry
        .snapshot()
        .iter()
        .filter(|p| p.status == BgProcessStatus::Running)
        .filter(|p| registry.kill(p.pid))
        .count()
        + registry.agents().stop_all();
    BackgroundTerminalsCleanResponse {
        terminated: u32::try_from(terminated).unwrap_or(u32::MAX),
    }
}

/// The sub-agents the session's model can call, by name.
pub(crate) fn list_agents(agent: &KodaAgent) -> AgentListResponse {
    let data = koda_core::tools::agent::discover_all_agents(&agent.project_root)
        .into_iter()
        .map(|info| AgentInfo {
            source: match info.source {
                "user" => AgentSource::User,
                "project" => AgentSource::Project,
                _ => AgentSource::Builtin,
            },
            name: info.name,
            description: info.description,
        })
        .collect();
    AgentListResponse { data }
}
