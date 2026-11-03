//! Regression test for #1136: project memory loaded twice on startup.
//!
//! Before the fix, `KodaAgent::new()` loaded `CLAUDE.md` and then
//! `rebuild_system_prompt()` loaded it AGAIN on the very next line of the
//! TUI/headless/server boot path — wasted disk I/O and duplicated log lines.
//!
//! After the fix, the loaded memory is cached in `KodaAgent::semantic_memory`
//! and `rebuild_system_prompt()` doesn't touch disk.
//!
//! Memory now opens the first user message instead of the system prompt, so
//! the test checks both: the cache keeps the memory after `CLAUDE.md` is
//! deleted, and neither prompt carries it.

use koda_core::agent::KodaAgent;
use koda_core::config::KodaConfig;
use koda_core::provider_catalog::ProviderType;
use std::fs;
use tempfile::TempDir;

const MEMORY_MARKER: &str = "PROJECT_MEMORY_CONTENT_FOR_TEST_1136";

#[tokio::test]
async fn rebuild_system_prompt_uses_cached_memory_not_disk() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().to_owned();

    // Seed a project memory file the agent will load on construction.
    let memory_path = project_root.join("CLAUDE.md");
    fs::write(
        &memory_path,
        format!("# Project memory\n\n{MEMORY_MARKER}\n"),
    )
    .expect("write memory");

    let config = KodaConfig::default_for_testing(ProviderType::Mock);
    let mut agent = KodaAgent::new(&config, project_root, &[])
        .await
        .expect("build agent");

    assert!(agent.semantic_memory.contains(MEMORY_MARKER));
    assert!(
        !agent.system_prompt.contains(MEMORY_MARKER),
        "memory goes in the first user message, not the system prompt"
    );

    fs::remove_file(&memory_path).expect("remove memory file");
    agent.rebuild_system_prompt(&config, &[]);

    assert!(
        agent.semantic_memory.contains(MEMORY_MARKER),
        "memory stays cached, not re-read from disk (regression: #1136)"
    );
    assert!(!agent.system_prompt.contains(MEMORY_MARKER));
}
