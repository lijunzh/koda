//! Built-in skill injection for koda-cli.
//!
//! `koda-cli` owns its documentation — `koda-core` stays generic.
//! The `koda_docs` skill points the agent at the online user manual
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
        "koda_docs",
        "Koda user manual: commands, TUI, configuration, sessions, providers, and more.",
        Some(
            "Use when the user asks how to use Koda: commands, flags, TUI keybindings, \
             configuration, sessions, providers, approval modes, skills, sub-agents, \
             ACP server, or any other feature documented in the user manual. \
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
             - {url}cli-reference.html — full CLI reference",
            url = KODA_DOCS_URL,
        ),
    );
}
