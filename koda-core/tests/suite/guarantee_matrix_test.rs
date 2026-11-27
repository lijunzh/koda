//! Guarantee matrix verification tests (#307, #293 Phase E, #855, the Safe
//! removal).
//!
//! One table: every action (row) × trust mode (column, Auto and Plan), and
//! the [`ToolApproval`] `check_tool()` returns for it. Approvals follow
//! Codex's sandbox: in Auto what the kernel sandbox contains runs without
//! asking, and only leaving the sandbox, a destructive command or a file
//! outside the project asks. Plan runs commands in a read-only sandbox and
//! refuses edits.
//!
//! Without a kernel sandbox, Auto asks before every change and Plan refuses
//! every command (#860); [`Exp::Contained`] and [`Exp::Sandboxed`] name
//! those cells.

use koda_core::trust::{ToolApproval, TrustMode, check_tool, derive_child_trust};
use serde_json::json;
use std::path::Path;

fn root() -> &'static Path {
    Path::new("/home/user/project")
}

/// What a cell of the matrix expects.
#[derive(Debug, Clone, Copy)]
enum Exp {
    /// Runs without asking.
    Run,
    /// Asks first.
    Ask,
    /// Refused.
    Block,
    /// An Auto change the sandbox contains: runs, or asks with no sandbox.
    Contained,
    /// A Plan command in the read-only sandbox: runs, or is refused with no
    /// sandbox.
    Sandboxed,
}

impl Exp {
    fn approval(self) -> ToolApproval {
        let sandbox = koda_core::sandbox::is_available();
        match self {
            Exp::Run => ToolApproval::AutoApprove,
            Exp::Ask => ToolApproval::NeedsConfirmation,
            Exp::Block => ToolApproval::Blocked,
            Exp::Contained if sandbox => ToolApproval::AutoApprove,
            Exp::Contained => ToolApproval::NeedsConfirmation,
            Exp::Sandboxed if sandbox => ToolApproval::AutoApprove,
            Exp::Sandboxed => ToolApproval::Blocked,
        }
    }
}

use Exp::{Ask, Block, Contained, Run, Sandboxed};

/// `(action, tool, args, Auto, Plan)`.
fn matrix() -> Vec<(&'static str, &'static str, serde_json::Value, Exp, Exp)> {
    vec![
        // Read-only tools run everywhere. Path scoping of a read outside the
        // project is enforced at execution time by safe_resolve_path.
        (
            "read inside project",
            "Read",
            json!({"path": "src/main.rs"}),
            Run,
            Run,
        ),
        (
            "read outside project",
            "Read",
            json!({"path": "/etc/passwd"}),
            Run,
            Run,
        ),
        (
            "write inside project",
            "Write",
            json!({"path": "src/main.rs"}),
            Contained,
            Block,
        ),
        (
            "edit inside project",
            "Edit",
            json!({"path": "src/main.rs"}),
            Contained,
            Block,
        ),
        (
            "write outside project",
            "Write",
            json!({"path": "/etc/hosts"}),
            Ask,
            Block,
        ),
        (
            "edit escaping with ..",
            "Edit",
            json!({"path": "../../../etc/passwd"}),
            Ask,
            Block,
        ),
        // Every command is a change to the classifier now; the sandbox holds
        // it, so it runs (read-only in Plan).
        (
            "read-only bash",
            "Bash",
            json!({"command": "git status"}),
            Contained,
            Sandboxed,
        ),
        (
            "bash writing a file",
            "Bash",
            json!({"command": "echo hello > output.txt"}),
            Contained,
            Sandboxed,
        ),
        (
            "plain rm",
            "Bash",
            json!({"command": "rm a.txt"}),
            Contained,
            Sandboxed,
        ),
        (
            "sed -i",
            "Bash",
            json!({"command": "sed -i s/a/b/ a.txt"}),
            Contained,
            Sandboxed,
        ),
        (
            "gh issue create",
            "Bash",
            json!({"command": "gh issue create --title 'bug'"}),
            Contained,
            Sandboxed,
        ),
        // #1250: the sandbox can't undo these, so Auto asks.
        (
            "forced rm",
            "Bash",
            json!({"command": "rm -rf build"}),
            Ask,
            Block,
        ),
        (
            "force push",
            "Bash",
            json!({"command": "git push --force"}),
            Ask,
            Block,
        ),
        (
            "hard reset",
            "Bash",
            json!({"command": "git reset --hard"}),
            Ask,
            Block,
        ),
        // Leaving the sandbox asks whatever the command does.
        (
            "bash outside the sandbox",
            "Bash",
            json!({"command": "git status", "outside_sandbox": true, "reason": "r"}),
            Ask,
            Block,
        ),
        // The outside-project floor (#218) asks in Auto; Plan's read-only
        // sandbox holds the command, so it runs there. /tmp is exempt (#560).
        (
            "bash path escape",
            "Bash",
            json!({"command": "cd /etc && ls"}),
            Ask,
            Sandboxed,
        ),
        (
            "invoke a sub-agent",
            "InvokeAgent",
            json!({"agent_name": "reviewer", "prompt": "review"}),
            Run,
            Run,
        ),
        (
            "memory write",
            "MemoryWrite",
            json!({"content": "remember this"}),
            Contained,
            Block,
        ),
        // A fetch asks in Plan, as Claude Code asks; a preapproved
        // documentation site is read-only.
        (
            "web fetch",
            "WebFetch",
            json!({"url": "https://example.com", "prompt": "q"}),
            Contained,
            Ask,
        ),
        (
            "web fetch, docs site",
            "WebFetch",
            json!({"url": "https://docs.python.org/3/", "prompt": "q"}),
            Run,
            Run,
        ),
        // Every Cargo subcommand runs code (build scripts, proc macros,
        // tests), so it is a change like `cargo build` through Bash.
        (
            "cargo check",
            "Cargo",
            json!({"subcommand": "check"}),
            Contained,
            Block,
        ),
        (
            "cargo clippy",
            "Cargo",
            json!({"subcommand": "clippy"}),
            Contained,
            Block,
        ),
        (
            "cargo test",
            "Cargo",
            json!({"subcommand": "test"}),
            Contained,
            Block,
        ),
        (
            "cargo build",
            "Cargo",
            json!({"subcommand": "build"}),
            Contained,
            Block,
        ),
    ]
}

#[test]
fn guarantee_matrix() {
    let mut wrong = Vec::new();
    for (action, tool, args, auto, plan) in matrix() {
        for (mode, exp) in [(TrustMode::Auto, auto), (TrustMode::Plan, plan)] {
            let got = check_tool(tool, &args, mode, Some(root()));
            if got != exp.approval() {
                wrong.push(format!(
                    "{action} in {mode:?}: got {got:?}, expected {:?} ({exp:?})",
                    exp.approval()
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// ── B19 regression: child trust clamped to parent *runtime* mode (#1022) ──
//
// Before #1022 B19 the dispatch used `parent_config.trust` (startup value)
// as the ceiling. A user who started in Auto and switched to Plan would
// still spawn sub-agents at Auto because the config field was never mutated —
// only the SharedTrustMode atomic was. This test pins the correct contract:
// `derive_child_trust` must receive the live *runtime* mode, and when that
// runtime mode is stricter than what the child declared, the child is clamped.

#[test]
fn b19_child_trust_clamped_to_parent_runtime_not_startup_config() {
    // Simulates: parent started Auto, user switched to Plan → runtime = Plan.
    let parent_startup_trust = TrustMode::Auto; // stale config field (pre-fix ceiling)
    let parent_runtime_mode = TrustMode::Plan; // live atomic (post-fix ceiling)

    // Named child declares Auto (wants broad permissions).
    let child_declared = TrustMode::Auto;

    // Post-fix: runtime mode is the ceiling → child clamped to Plan.
    assert_eq!(
        derive_child_trust(parent_runtime_mode, child_declared),
        TrustMode::Plan,
        "B19: child must be clamped to parent's runtime trust, not startup config trust",
    );

    // Pre-fix behaviour (using startup config as ceiling) would have
    // returned Auto — this assertion documents the bug that was fixed.
    assert_eq!(
        derive_child_trust(parent_startup_trust, child_declared),
        TrustMode::Auto,
        "pre-fix behaviour: startup config (Auto) failed to clamp child — child got Auto",
    );

    // A child declaring Plan stays Plan under an Auto parent.
    assert_eq!(
        derive_child_trust(TrustMode::Auto, TrustMode::Plan),
        TrustMode::Plan,
    );
}
