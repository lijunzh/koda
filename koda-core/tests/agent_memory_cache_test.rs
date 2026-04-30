//! Regression test for #1136: project memory loaded twice on startup.
//!
//! Before the fix, `KodaAgent::new()` loaded `CLAUDE.md` and then
//! `rebuild_system_prompt()` loaded it AGAIN on the very next line of the
//! TUI/headless/server boot path — wasted disk I/O and duplicated log lines.
//!
//! After the fix, the loaded memory is cached in `KodaAgent::semantic_memory`
//! and `rebuild_system_prompt()` reuses it without touching disk.
//!
//! This test proves the cache works by deleting `CLAUDE.md` between the two
//! calls. If we re-read on rebuild, the prompt would lose the memory content.
//! With the cache, the prompt keeps the original content.

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

    // Sanity: initial prompt contains the memory.
    assert!(
        agent.system_prompt.contains(MEMORY_MARKER),
        "initial prompt should include memory"
    );

    // Delete the file. If `rebuild_system_prompt` re-reads from disk,
    // the marker will disappear from the rebuilt prompt.
    fs::remove_file(&memory_path).expect("remove memory file");

    agent.rebuild_system_prompt(&config, &[]);

    assert!(
        agent.system_prompt.contains(MEMORY_MARKER),
        "rebuilt prompt should still contain memory because it was cached, \
         not re-read from disk (regression: #1136)"
    );
}
