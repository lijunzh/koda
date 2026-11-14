//! `koda mcp list|get|add|remove`, run as the binary against a scratch
//! config directory, as `codex mcp` is used.

use std::path::Path;
use std::process::{Command, Output};

fn koda_mcp(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_koda"))
        .arg("mcp")
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .output()
        .expect("failed to run koda mcp")
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn servers_are_added_listed_shown_and_removed() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    assert!(stdout(&koda_mcp(home, &["list"])).contains("No MCP servers configured yet"));

    let added = koda_mcp(
        home,
        &[
            "add",
            "notes",
            "--env",
            "NOTES_KEY=s3cret",
            "--",
            "notes-mcp",
            "--quiet",
        ],
    );
    assert_eq!(stdout(&added).trim(), "Added MCP server 'notes'.");
    let added = koda_mcp(
        home,
        &[
            "add",
            "mail",
            "--url",
            "https://mail.example/mcp",
            "--bearer-token-env-var",
            "MAIL_TOKEN",
        ],
    );
    assert_eq!(stdout(&added).trim(), "Added MCP server 'mail'.");

    let listed = stdout(&koda_mcp(home, &["list"]));
    assert!(
        listed.contains("notes  notes-mcp  --quiet  NOTES_KEY=*****"),
        "{listed}"
    );
    assert!(
        listed.contains("mail  https://mail.example/mcp  MAIL_TOKEN"),
        "{listed}"
    );
    assert!(
        !listed.contains("s3cret"),
        "an env value was shown: {listed}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout(&koda_mcp(home, &["list", "--json"]))).unwrap();
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mail", "notes"]);
    assert_eq!(json[0]["bearer_token_env_var"], "MAIL_TOKEN");

    let shown = stdout(&koda_mcp(home, &["get", "notes"]));
    assert!(shown.contains("  transport: stdio"), "{shown}");
    assert!(shown.contains("  command: notes-mcp"), "{shown}");
    assert!(shown.contains("  env: NOTES_KEY=*****"), "{shown}");

    assert_eq!(
        stdout(&koda_mcp(home, &["remove", "mail"])).trim(),
        "Removed MCP server 'mail'."
    );
    assert_eq!(
        stdout(&koda_mcp(home, &["remove", "mail"])).trim(),
        "No MCP server named 'mail' found."
    );
    let missing = koda_mcp(home, &["get", "mail"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("No MCP server named 'mail'"));
}

#[test]
fn a_bad_server_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // A command and a URL: clap refuses, as Codex's does.
    let both = koda_mcp(
        home,
        &["add", "x", "--url", "https://x.example", "--", "sh"],
    );
    assert_eq!(both.status.code(), Some(2), "{both:?}");
    // The host refuses a name the tool routing can't take.
    let name = koda_mcp(home, &["add", "a__b", "--", "sh"]);
    assert!(!name.status.success(), "{name:?}");
    assert!(stdout(&koda_mcp(home, &["list"])).contains("No MCP servers configured yet"));
}
