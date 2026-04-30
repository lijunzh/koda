//! Built-in skill injection for koda-cli.
//!
//! `koda-cli` owns its documentation — `koda-core` stays generic.
//! The `koda-docs` skill points the agent at the online user manual
//! so it can answer "how do I use koda?" questions via `WebFetch`.
//!
//! Call [`inject_builtin_skills`] once after [`KodaAgent::new`] in each
//! CLI entry point (TUI, headless, ACP server).

use koda_core::agent::KodaAgent;

/// URL of the published Koda user manual (mdBook on GitHub Pages).
const KODA_DOCS_URL: &str = "https://lijunzh.github.io/koda/";

/// Inject koda-cli's built-in skills into the agent's skill registry.
///
/// This is the only place that knows about the docs URL — keeping
/// `koda-core` completely decoupled from CLI-specific content.
pub fn inject_builtin_skills(agent: &mut KodaAgent) {
    agent.tools.skill_registry.add_builtin(
        "koda-docs",
        "Koda user manual: commands, TUI, configuration, security, sandbox, privacy, sessions, providers, and more.",
        Some(
            "Use when the user asks how to use Koda: commands, flags, TUI keybindings, \
             configuration, sessions, providers, approval modes, trust modes, \
             sandbox, security, safety, privacy, undo, tools, MCP servers, \
             context management, memory, file attachments, skills, sub-agents, \
             ACP server, troubleshooting, panic logs, crash diagnosis, or any \
             other feature documented in the user manual. \
             Fetch the manual from the URL below with WebFetch.",
        ),
        &format!(
            "The Koda user manual is published at: {url}\n\n\
             Use the WebFetch tool to read specific pages. Key pages:\n\
             - {url}commands.html — slash commands and CLI flags\n\
             - {url}tui.html — TUI keybindings and interface\n\
             - {url}configuration.html — config and settings\n\
             - {url}providers.html — LLM provider setup\n\
             - {url}sessions.html — session management\n\
             - {url}agents.html — sub-agents and delegation\n\
             - {url}skills.html — skill system\n\
             - {url}approval.html — approval modes\n\
             - {url}headless.html — headless / CI mode\n\
             - {url}cli-reference.html — full CLI reference\n\
             - {url}sandbox.html — kernel sandbox, write restrictions, credential protection\n\
             - {url}privacy.html — zero telemetry, local-only data, deletion\n\
             - {url}tools.html — tools reference (all available tools)\n\
             - {url}mcp.html — MCP server integration\n\
             - {url}attachments.html — file and image attachments (@file)\n\
             - {url}context.html — context window management\n\
             - {url}memory.html — cross-session memory\n\
             - {url}keybindings.html — full keybinding reference\n\
             - {url}acp.html — ACP server (editor integration)\n\
             - {url}troubleshooting.html — panic logs, crash diagnosis, common issues\n\
             - {url}introduction.html — overview and getting started",
            url = KODA_DOCS_URL,
        ),
    );
}
