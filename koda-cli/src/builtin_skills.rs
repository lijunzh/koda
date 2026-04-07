//! Built-in skill injection for koda-cli.
//!
//! `koda-cli` owns its documentation — `koda-core` stays generic.
//! The `koda_docs` skill is compiled from `docs/src/` at build time
//! via `build.rs`, then injected into the agent's skill registry here.
//!
//! Call [`inject_builtin_skills`] once after [`KodaAgent::new`] in each
//! CLI entry point (TUI, headless, ACP server).

use koda_core::agent::KodaAgent;

/// The Koda user manual, bundled at compile time from `docs/src/`.
const KODA_DOCS: &str = include_str!(concat!(env!("OUT_DIR"), "/koda_docs.md"));

/// Inject koda-cli's built-in skills into the agent's skill registry.
///
/// This is the only place that knows about `koda_docs` — keeping
/// `koda-core` completely decoupled from CLI-specific content.
pub fn inject_builtin_skills(agent: &mut KodaAgent) {
    agent.tools.skill_registry.add_builtin(
        "koda_docs",
        "Koda user manual: commands, TUI, configuration, sessions, providers, and more.",
        KODA_DOCS,
    );
}
