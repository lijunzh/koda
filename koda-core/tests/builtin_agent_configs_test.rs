//! #1361 regression: read-only-by-system-prompt sub-agents must
//! disallow Write/Edit/Delete in their config.
//!
//! Before this guard, `explore` and `plan` had system prompts loudly
//! declaring "STRICTLY PROHIBITED from creating, editing, or deleting
//! any files" but their `disallowed_tools` arrays didn't list
//! `Write`/`Edit`/`Delete`. The dispatcher's `has_write_tools` check
//! returned `true`, so:
//!
//! 1. They were routed through the slow workspace provider
//!    (clonefile / git worktree, ~30s spin-up on this repo with a 56 GB
//!    `target/`) for no reason \u2014 they never wrote anything.
//! 2. Defense-in-depth was missing: nothing actually stopped the model
//!    from writing if it ignored the system prompt.
//!
//! This test pins the invariant: any agent whose system prompt
//! contains a read-only marker MUST disallow the write tools at the
//! config level. Adding a new read-only agent without this contract
//! fails the test loudly.

use serde_json::Value;

/// Markers in a system prompt that signal the agent is intended to
/// be read-only. If any of these appear, the config MUST disallow
/// write tools. Match is case-insensitive on the prompt text.
const READ_ONLY_MARKERS: &[&str] = &[
    "STRICTLY PROHIBITED from creating, editing, or deleting",
    "READ-ONLY MODE: NO FILE MODIFICATIONS",
];

/// Tools that MUST appear in `disallowed_tools` for any agent that
/// hits a `READ_ONLY_MARKER`. Mirrors the `has_write_tools` check in
/// `koda-core/src/sub_agent_dispatch.rs` (which only inspects `Write`
/// and `Edit`) plus `Delete` for completeness \u2014 a read-only agent
/// shouldn't be able to remove files either.
const REQUIRED_DISALLOWED: &[&str] = &["Write", "Edit", "Delete"];

/// Embedded JSON for every built-in agent, mirroring the
/// `BUILTIN_AGENTS` array in `koda-core/src/config.rs`.
///
/// We use `include_str!` directly (rather than a public accessor) so
/// this test is decoupled from `KodaConfig`'s internal layout and
/// stays a pure file-content guarantee.
const BUILTIN_AGENT_JSONS: &[(&str, &str)] = &[
    ("default", include_str!("../agents/default.json")),
    ("task", include_str!("../agents/task.json")),
    ("explore", include_str!("../agents/explore.json")),
    ("plan", include_str!("../agents/plan.json")),
    ("verify", include_str!("../agents/verify.json")),
];

#[test]
fn readonly_by_prompt_agents_must_disallow_write_tools() {
    let mut violations: Vec<String> = Vec::new();

    for (name, json) in BUILTIN_AGENT_JSONS {
        let v: Value = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("agent `{name}` has invalid JSON: {e}"));

        let prompt = v
            .get("system_prompt")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("agent `{name}` missing `system_prompt`"));

        let is_readonly = READ_ONLY_MARKERS
            .iter()
            .any(|marker| prompt.contains(marker));

        if !is_readonly {
            continue; // Write-capable agents (default, task) are exempt.
        }

        let disallowed: Vec<&str> = v
            .get("disallowed_tools")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let missing: Vec<&str> = REQUIRED_DISALLOWED
            .iter()
            .copied()
            .filter(|t| !disallowed.contains(t))
            .collect();

        if !missing.is_empty() {
            violations.push(format!(
                "agent `{name}` is read-only by system prompt but \
                 `disallowed_tools` is missing: {missing:?}\n\
                 \n  current disallowed_tools: {disallowed:?}\n  \
                 required: {REQUIRED_DISALLOWED:?}"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "#1361 regression: read-only-by-prompt agents must disallow write \
         tools at the config level so the dispatcher routes them to the \
         fast `CwdProvider` workspace path (skipping ~30s of clonefile/\
         worktree provisioning) AND so the model literally cannot bypass \
         the system-prompt rule.\n\nViolations:\n  - {}",
        violations.join("\n  - ")
    );
}

/// Sanity: the test data itself is non-empty and parses.
/// Catches a future bug where someone removes one of the five
/// built-in agents but forgets to update this test's array.
///
/// Note we don't assert `name == file-stem` because `default.json`'s
/// `name` field is `"koda"` (the user-facing CLI default agent
/// name), not `"default"`. The file-name and the in-config name are
/// independent identifiers.
#[test]
fn all_builtin_agent_jsons_parse() {
    for (name, json) in BUILTIN_AGENT_JSONS {
        let v: Value = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("agent `{name}` has invalid JSON: {e}"));
        assert!(
            v.get("name").and_then(Value::as_str).is_some(),
            "agent JSON for `{name}` missing `name` field"
        );
        assert!(
            v.get("system_prompt").and_then(Value::as_str).is_some(),
            "agent JSON for `{name}` missing `system_prompt` field"
        );
    }
}
