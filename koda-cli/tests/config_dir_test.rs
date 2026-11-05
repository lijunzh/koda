//! Every per-user path follows `XDG_CONFIG_HOME`, as the database does.
//!
//! The eval harness isolates each run by pointing `XDG_CONFIG_HOME` at a
//! fresh directory. Global memory and user agents used to be built from
//! `$HOME/.config/koda`, so a run could read the user's real files and a
//! global `MemoryWrite` landed in the user's real memory. Here `HOME` and
//! `XDG_CONFIG_HOME` are different temp directories, and nothing may touch
//! the one under `HOME`.

use std::path::Path;
use std::process::Command;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// Run headless koda on the mock provider with separate HOME and config dirs.
fn run(home: &Path, xdg: &Path, project: &Path, responses: &str) -> (String, bool) {
    let output = Command::new(koda_bin())
        .args(["-p", "go", "--provider", "mock", "--mode", "auto"])
        .args(["--output-format", "json", "--project-root"])
        .arg(project)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        .env("KODA_MOCK_RESPONSES", responses)
        .output()
        .expect("failed to run koda");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stderr, output.status.success())
}

#[test]
fn global_memory_is_written_under_xdg_config_home() {
    let (home, xdg, project) = (tempdir(), tempdir(), tempdir());
    let responses = serde_json::json!([
        {"tool": "MemoryWrite", "args": {"content": "prefers tabs", "scope": "global"}},
        {"text": "saved"}
    ])
    .to_string();
    let (stderr, ok) = run(home.path(), xdg.path(), project.path(), &responses);
    assert!(ok, "koda failed:\n{stderr}");

    let written = std::fs::read_to_string(xdg.path().join("koda/memories/MEMORY.md"))
        .unwrap_or_else(|e| panic!("no global memory under XDG_CONFIG_HOME: {e}\n{stderr}"));
    assert!(written.contains("prefers tabs"), "{written}");
    assert!(
        !home.path().join(".config/koda").exists(),
        "global memory was written under HOME"
    );
}

#[test]
fn user_agents_are_read_from_xdg_config_home_only() {
    let (home, xdg, project) = (tempdir(), tempdir(), tempdir());
    let agent = |name: &str| {
        serde_json::json!({
            "name": name,
            "description": format!("{name} probe"),
            "system_prompt": "Probe.",
            "trust": "plan"
        })
        .to_string()
    };
    for (root, name) in [
        (xdg.path().join("koda/agents"), "xdg-probe"),
        (home.path().join(".config/koda/agents"), "home-probe"),
    ] {
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(format!("{name}.json")), agent(name)).unwrap();
    }
    let responses = serde_json::json!([
        {"tool": "ListAgents", "args": {}},
        {"text": "listed"}
    ])
    .to_string();
    let (stderr, ok) = run(home.path(), xdg.path(), project.path(), &responses);
    assert!(ok, "koda failed:\n{stderr}");
    assert!(
        stderr.contains("xdg-probe"),
        "agent under XDG_CONFIG_HOME missing:\n{stderr}"
    );
    assert!(
        !stderr.contains("home-probe"),
        "agent under HOME was read:\n{stderr}"
    );
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
