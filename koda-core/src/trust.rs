//! Unified trust mode — the single permission knob for Koda.
//!
//! Replaces the old `ApprovalMode × SandboxMode` two-layer system with one
//! enum that controls both sandbox configuration and approval behavior.
//! Users see one concept, one CLI flag (`--mode`), one status bar word.
//!
//! ## Trust modes
//!
//! | Mode | Sandbox | Approval | Use case |
//! |------|---------|----------|----------|
//! | **Plan** | project, read-only | deny all non-read | Investigation agents |
//! | **Safe** | project, read+write | confirm side effects | User default |
//! | **Auto** | project, read+write | auto-approve all | Autonomous coding |
//!
//! ## Behavior matrix
//!
//! | Behavior | Plan | Safe | Auto |
//! |---|---|---|---|
//! | ReadOnly tools | ✅ auto | ✅ auto | ✅ auto |
//! | RemoteAction | ❌ deny | ⚠️ confirm | ✅ auto |
//! | LocalMutation | ❌ deny | ⚠️ confirm | ✅ auto |
//! | Destructive | ❌ deny | ⚠️ confirm | ✅ auto |
//! | Outside project | ❌ deny | ⚠️ confirm | ⚠️ confirm |
//!
//! ## Design principle
//!
//! The sandbox is the safety boundary, not the approval prompt.
//! Auto trusts the agent within the project sandbox — the kernel enforces the
//! perimeter. Safe adds approval as a second layer (belt and suspenders).
//! Credential dirs are always blocked regardless of mode.

use crate::bash_safety::classify_bash_command;
use crate::file_tracker::FileTracker;
use crate::tools::ToolEffect;
use path_clean::PathClean;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

// ── TrustMode ─────────────────────────────────────────────

/// The unified trust mode: Plan (read-only), Safe (confirm), Auto (autonomous).
///
/// Derives `Ord` so that `std::cmp::min(parent, child)` implements clamping:
/// a child agent can never exceed its parent's trust level.
///
/// # Examples
///
/// ```
/// use koda_core::trust::TrustMode;
///
/// let mode = TrustMode::Safe;
/// assert_eq!(mode.as_str(), "safe");
/// assert_eq!(mode.next(), TrustMode::Auto);
///
/// // Clamping: child can't exceed parent
/// assert_eq!(TrustMode::clamp(TrustMode::Plan, TrustMode::Auto), TrustMode::Plan);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum TrustMode {
    /// Read-only: deny all non-read tool calls. For investigation agents.
    Plan = 0,
    /// Supervised: project-sandboxed writes, confirm all side effects. User default.
    #[default]
    Safe = 1,
    /// Autonomous: project-sandboxed writes, auto-approve all including destructive.
    Auto = 2,
}

impl TrustMode {
    /// Cycle between user-facing modes: Safe ↔ Auto.
    ///
    /// Plan is agent-only and never toggled by the user.
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Plan, // agent-only, no toggle
            Self::Safe => Self::Auto,
            Self::Auto => Self::Safe,
        }
    }

    /// Stable string representation for persistence and wire protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Safe => "safe",
            Self::Auto => "auto",
        }
    }

    /// Short label for display (same as `as_str` for now).
    pub fn label(self) -> &'static str {
        self.as_str()
    }

    /// Human-readable description of this mode.
    pub fn description(self) -> &'static str {
        match self {
            Self::Plan => "read-only, deny all writes",
            Self::Safe => "confirm every side effect",
            Self::Auto => "auto-approve, confirm outside-project only",
        }
    }

    /// Parse a trust mode from a user-provided or config string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" | "yolo" | "accept" => Some(Self::Auto),
            "safe" | "confirm" | "strict" | "normal" => Some(Self::Safe),
            "plan" | "readonly" | "read-only" => Some(Self::Plan),
            _ => None,
        }
    }

    /// Clamp a child's trust mode to never exceed the parent's.
    ///
    /// Since `TrustMode` derives `Ord` with `Plan < Safe < Auto`,
    /// this is simply `std::cmp::min(parent, child)`.
    pub fn clamp(parent: TrustMode, child: TrustMode) -> TrustMode {
        std::cmp::min(parent, child)
    }
}

// ── Child trust derivation (#1022 B19) ─────────────────────────────────

/// The single, authoritative way to compute a child agent's trust mode.
///
/// **Per DESIGN.md § "Trust never widens"**: this helper is the *only*
/// way fork, named, and bg sub-agent dispatch paths derive child
/// trust. Three call sites converging on one function ensures the
/// invariant cannot drift between paths.
///
/// # `parent_runtime` MUST be the runtime mode
///
/// **#1022 B19**: pre-fix, `sub_agent_dispatch` clamped against
/// `parent_config.trust` — the *startup* value of the trust mode.
/// `cycle_trust`/`set_trust` mutate the `SharedTrustMode` atomic but
/// **never** the `KodaConfig.trust` field. So a user who started in
/// `Auto` and hit `/safe` would still get sub-agents clamped against
/// the stale `Auto`, allowing the child to run with broader
/// privileges than the parent's *current* mode. Real escalation.
///
/// The runtime trust mode is the `mode: TrustMode` parameter
/// threaded through `execute_one_tool` → `execute_sub_agent`. That
/// value is read from the `SharedTrustMode` atomic at the start of
/// each turn and is the only source of truth for "what trust level
/// is this turn running at".
///
/// Passing `parent_config.trust` here is the antipattern this helper
/// exists to prevent.
///
/// # `declared` is the child's own declaration
///
/// For named sub-agents this is `cfg.trust` loaded from the agent's
/// JSON. For `fork` (which has no separate declaration) the call
/// site passes `parent_runtime` again — the helper then collapses
/// to identity, but the symmetry across all three paths is what
/// makes the invariant easy to audit.
///
/// # Examples
///
/// ```
/// use koda_core::trust::{derive_child_trust, TrustMode};
///
/// // Named child declares Auto, parent runtime is Safe → child
/// // clamps down to Safe (declared narrows toward parent).
/// assert_eq!(
///     derive_child_trust(TrustMode::Safe, TrustMode::Auto),
///     TrustMode::Safe,
/// );
///
/// // Named child declares Plan, parent runtime is Auto → child
/// // stays Plan (declared is already stricter).
/// assert_eq!(
///     derive_child_trust(TrustMode::Auto, TrustMode::Plan),
///     TrustMode::Plan,
/// );
///
/// // Fork case: declared = parent_runtime → identity.
/// assert_eq!(
///     derive_child_trust(TrustMode::Safe, TrustMode::Safe),
///     TrustMode::Safe,
/// );
/// ```
pub fn derive_child_trust(parent_runtime: TrustMode, declared: TrustMode) -> TrustMode {
    // Implementation is `clamp` — the named function exists to make
    // "trust derivation" greppable and to carry the documentation
    // about which `parent` to pass. **Do not inline back into
    // `TrustMode::clamp` at call sites** — that re-opens the door
    // for `parent_config.trust` to creep back in.
    TrustMode::clamp(parent_runtime, declared)
}

impl From<u8> for TrustMode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Plan,
            1 => Self::Safe,
            2 => Self::Auto,
            // Fail-safe: unknown values default to Safe (most restrictive
            // interactive mode) rather than Auto (#860).
            _ => Self::Safe,
        }
    }
}

impl std::fmt::Display for TrustMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Shared trust state ────────────────────────────────────

/// Thread-safe shared trust mode, readable from prompt formatter and input handlers.
pub type SharedTrustMode = Arc<AtomicU8>;

/// Create a new atomic shared trust mode initialized to `mode`.
pub fn new_shared_trust(mode: TrustMode) -> SharedTrustMode {
    Arc::new(AtomicU8::new(mode as u8))
}

/// Read the current trust mode from shared state.
pub fn read_trust(shared: &SharedTrustMode) -> TrustMode {
    TrustMode::from(shared.load(Ordering::Relaxed))
}

/// Atomically set the trust mode.
pub fn set_trust(shared: &SharedTrustMode, mode: TrustMode) {
    shared.store(mode as u8, Ordering::Relaxed);
}

/// Cycle to the next trust mode (Safe ↔ Auto) and return it.
pub fn cycle_trust(shared: &SharedTrustMode) -> TrustMode {
    let current = read_trust(shared);
    let next = current.next();
    set_trust(shared, next);
    next
}

// ── Tool Approval Decision ──────────────────────────────────

/// What the trust system decides for a given tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolApproval {
    /// Execute without asking.
    AutoApprove,
    /// Show confirmation dialog.
    NeedsConfirmation,
    /// Blocked (Plan mode or delegation scope violation).
    Blocked,
}

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
///
/// Decision matrix:
///
/// | ToolEffect     | Plan    | Safe          | Auto          |
/// |----------------|---------|---------------|---------------|
/// | ReadOnly       | ✅ auto  | ✅ auto        | ✅ auto        |
/// | RemoteAction   | ❌ block | ⚠️ confirm     | ✅ auto        |
/// | LocalMutation  | ❌ block | ⚠️ confirm     | ✅ auto        |
/// | Destructive    | ❌ block | ⚠️ confirm     | ✅ auto        |
///
/// Additional hardcoded floors:
/// - Writes outside project root → NeedsConfirmation (even in Auto) (#218)
/// - Bash path escapes → NeedsConfirmation
/// - Delete of Koda-owned file → AutoApprove (#465)
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
) -> ToolApproval {
    check_tool_with_tracker(tool_name, args, mode, project_root, None)
}

/// Like [`check_tool`] but with an optional file tracker for ownership checks.
///
/// When a `FileTracker` is provided and the tool is `Delete` targeting a file
/// that Koda created in this session, the destructive classification is
/// downgraded to auto-approve (net-zero effect: Koda created it, Koda removes it).
pub fn check_tool_with_tracker(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
    file_tracker: Option<&FileTracker>,
) -> ToolApproval {
    let effect = resolve_tool_effect(tool_name, args);

    // Read-only tools always auto-approve in every mode
    if effect == ToolEffect::ReadOnly {
        return ToolApproval::AutoApprove;
    }

    // Plan mode: deny everything except read-only
    if mode == TrustMode::Plan {
        return ToolApproval::Blocked;
    }

    // Hardcoded floor: writes outside project root always need confirmation (#218)
    if let Some(root) = project_root {
        if is_outside_project(tool_name, args, root) {
            return ToolApproval::NeedsConfirmation;
        }
        // Bash path lint: check for cd/path escapes
        if tool_name == "Bash" {
            let command = args
                .get("command")
                .or(args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lint = crate::bash_path_lint::lint_bash_paths(command, root);
            if lint.has_warnings() {
                return ToolApproval::NeedsConfirmation;
            }
        }
    }

    // File lifecycle: Koda-owned files bypass destructive gate (#465)
    if tool_name == "Delete"
        && let Some(tracker) = file_tracker
        && let Some(root) = project_root
        && let Some(abs_path) = crate::file_tracker::resolve_file_path_from_args(args, root)
        && tracker.is_owned(&abs_path)
    {
        return ToolApproval::AutoApprove;
    }

    // Apply the ToolEffect × TrustMode matrix
    match mode {
        TrustMode::Plan => unreachable!(), // handled above
        TrustMode::Safe => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction | ToolEffect::LocalMutation | ToolEffect::Destructive => {
                ToolApproval::NeedsConfirmation
            }
        },
        TrustMode::Auto => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction | ToolEffect::LocalMutation | ToolEffect::Destructive => {
                // Safety net: if the kernel sandbox is unavailable, Auto mode
                // loses its perimeter. Downgrade destructive/mutation ops to
                // NeedsConfirmation so the user still gets a prompt (#860).
                if crate::sandbox::is_available() {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::NeedsConfirmation
                }
            }
        },
    }
}

/// Resolve the effective [`ToolEffect`] for a tool call.
///
/// For Bash, refines the generic `LocalMutation` classification by
/// parsing the actual command string.
///
/// For MCP tools, falls back to `RemoteAction` unless a ToolRegistry
/// is provided via [`resolve_tool_effect_with_registry`].
pub fn resolve_tool_effect(tool_name: &str, args: &serde_json::Value) -> ToolEffect {
    resolve_tool_effect_inner(tool_name, args, None)
}

/// Like [`resolve_tool_effect`] but uses the ToolRegistry for MCP-aware
/// classification (#662).
pub fn resolve_tool_effect_with_registry(
    tool_name: &str,
    args: &serde_json::Value,
    registry: &crate::tools::ToolRegistry,
) -> ToolEffect {
    resolve_tool_effect_inner(tool_name, args, Some(registry))
}

fn resolve_tool_effect_inner(
    tool_name: &str,
    args: &serde_json::Value,
    registry: Option<&crate::tools::ToolRegistry>,
) -> ToolEffect {
    // MCP tools: use registry annotations when available.
    if crate::mcp::is_mcp_tool_name(tool_name) {
        if let Some(reg) = registry {
            return reg.classify_tool_with_mcp(tool_name);
        }
        return crate::tools::ToolEffect::RemoteAction;
    }

    let base = crate::tools::classify_tool(tool_name);

    if tool_name == "Bash" {
        let command = args
            .get("command")
            .or(args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return classify_bash_command(command);
    }

    base
}

/// Whether a file tool targets a path outside the project root (#218).
/// Hardcoded floor: always NeedsConfirmation regardless of mode.
///
/// Temp directories (`/tmp`, `$TMPDIR`) are explicitly allowed (#560).
fn is_outside_project(tool_name: &str, args: &serde_json::Value, project_root: &Path) -> bool {
    let path_arg = match tool_name {
        "Write" | "Edit" | "Delete" => args
            .get("path")
            .or(args.get("file_path"))
            .and_then(|v| v.as_str()),
        _ => None,
    };
    match path_arg {
        Some(p) => {
            let requested = Path::new(p);
            let abs_path = if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                project_root.join(requested)
            };
            // Canonicalize for symlink resolution (macOS /var → /private/var).
            // For new files, canonicalize the parent dir and append the filename.
            let resolved = abs_path.canonicalize().unwrap_or_else(|_| {
                if let Some(parent) = abs_path.parent()
                    && let Ok(canon_parent) = parent.canonicalize()
                    && let Some(name) = abs_path.file_name()
                {
                    return canon_parent.join(name);
                }
                abs_path.clean()
            });
            let canon_root = project_root
                .canonicalize()
                .unwrap_or_else(|_| project_root.to_path_buf());
            let outside = !resolved.starts_with(&canon_root);
            // Allow temp directories (#560)
            if outside && crate::bash_path_lint::is_safe_external_path(&resolved) {
                return false;
            }
            outside
        }
        None => false,
    }
}

// ── Re-exports for backward compatibility ─────────────────

/// Re-export settings types for provider persistence.
pub use crate::last_provider::LastProvider;

// ── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode tests ──

    #[test]
    fn test_mode_cycle() {
        assert_eq!(TrustMode::Safe.next(), TrustMode::Auto);
        assert_eq!(TrustMode::Auto.next(), TrustMode::Safe);
        assert_eq!(TrustMode::Plan.next(), TrustMode::Plan); // agent-only, no toggle
    }

    #[test]
    fn test_mode_ordering() {
        assert!(TrustMode::Plan < TrustMode::Safe);
        assert!(TrustMode::Safe < TrustMode::Auto);
    }

    #[test]
    fn test_clamp() {
        assert_eq!(
            TrustMode::clamp(TrustMode::Plan, TrustMode::Auto),
            TrustMode::Plan
        );
        assert_eq!(
            TrustMode::clamp(TrustMode::Safe, TrustMode::Auto),
            TrustMode::Safe
        );
        assert_eq!(
            TrustMode::clamp(TrustMode::Auto, TrustMode::Safe),
            TrustMode::Safe
        );
        assert_eq!(
            TrustMode::clamp(TrustMode::Auto, TrustMode::Auto),
            TrustMode::Auto
        );
    }

    // ── #1022 B19: derive_child_trust contract ──
    //
    // The helper *is* `clamp` underneath, so the matrix duplicates
    // the `test_clamp` cases. That duplication is intentional:
    //
    // - `test_clamp` pins the math.
    // - These tests pin the **named contract** — if a future refactor
    //   inlines `clamp` back into call sites, these tests still pass
    //   (clamp didn't break) but the structural lint test in
    //   `koda-cli/tests/regression_test.rs` catches the call-site
    //   regression. Tests target different layers; both must hold.
    //
    // The naming (`fork_identity`, `named_clamps_down`,
    // `child_already_stricter_passes_through`) documents the
    // architectural cases each call site exercises.

    #[test]
    fn derive_child_trust_fork_identity() {
        // Fork passes (mode, mode) — helper collapses to identity.
        // Documents that the symmetry is preserved.
        for parent in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
            assert_eq!(
                derive_child_trust(parent, parent),
                parent,
                "fork (parent==declared) must return parent verbatim"
            );
        }
    }

    #[test]
    fn derive_child_trust_named_clamps_down() {
        // Named child declares Auto; parent runtime is Safe →
        // child must clamp down to Safe. This is the load-bearing
        // case — a child that wanted broader privileges than its
        // parent's runtime mode is forcibly narrowed.
        assert_eq!(
            derive_child_trust(TrustMode::Safe, TrustMode::Auto),
            TrustMode::Safe
        );
        assert_eq!(
            derive_child_trust(TrustMode::Plan, TrustMode::Auto),
            TrustMode::Plan
        );
        assert_eq!(
            derive_child_trust(TrustMode::Plan, TrustMode::Safe),
            TrustMode::Plan
        );
    }

    #[test]
    fn derive_child_trust_child_already_stricter_passes_through() {
        // Named child declares Plan with parent Auto → child stays
        // Plan. Clamp never *widens*, only narrows.
        assert_eq!(
            derive_child_trust(TrustMode::Auto, TrustMode::Plan),
            TrustMode::Plan
        );
        assert_eq!(
            derive_child_trust(TrustMode::Auto, TrustMode::Safe),
            TrustMode::Safe
        );
        assert_eq!(
            derive_child_trust(TrustMode::Safe, TrustMode::Plan),
            TrustMode::Plan
        );
    }

    #[test]
    fn derive_child_trust_is_commutative_in_min_but_not_in_meaning() {
        // Math is symmetric: min(a, b) == min(b, a).
        // But the *contract* is asymmetric: arg 1 is parent runtime,
        // arg 2 is declared. This test pins that the math is
        // commutative (so no path is order-sensitive in result),
        // while the function name and docstring make the semantic
        // asymmetry explicit. Catches a refactor that introduces
        // *non-commutative* behavior (e.g. "if declared is Plan
        // unconditionally allow it") which would silently change
        // dispatch semantics.
        for a in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
            for b in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
                assert_eq!(derive_child_trust(a, b), derive_child_trust(b, a));
            }
        }
    }

    #[test]
    fn test_mode_from_str() {
        assert_eq!(TrustMode::parse("auto"), Some(TrustMode::Auto));
        assert_eq!(TrustMode::parse("safe"), Some(TrustMode::Safe));
        assert_eq!(TrustMode::parse("plan"), Some(TrustMode::Plan));
        // Legacy aliases
        assert_eq!(TrustMode::parse("yolo"), Some(TrustMode::Auto));
        assert_eq!(TrustMode::parse("confirm"), Some(TrustMode::Safe));
        assert_eq!(TrustMode::parse("strict"), Some(TrustMode::Safe));
        assert_eq!(TrustMode::parse("normal"), Some(TrustMode::Safe));
        assert_eq!(TrustMode::parse("readonly"), Some(TrustMode::Plan));
        assert_eq!(TrustMode::parse("read-only"), Some(TrustMode::Plan));
        assert_eq!(TrustMode::parse("accept"), Some(TrustMode::Auto));
        assert_eq!(TrustMode::parse("nope"), None);
    }

    #[test]
    fn test_mode_from_u8() {
        assert_eq!(TrustMode::from(0), TrustMode::Plan);
        assert_eq!(TrustMode::from(1), TrustMode::Safe);
        assert_eq!(TrustMode::from(2), TrustMode::Auto);
        assert_eq!(TrustMode::from(99), TrustMode::Safe); // fail-safe to Safe (#860)
    }

    #[test]
    fn test_shared_trust_cycle() {
        let shared = new_shared_trust(TrustMode::Safe);
        assert_eq!(read_trust(&shared), TrustMode::Safe);
        let next = cycle_trust(&shared);
        assert_eq!(next, TrustMode::Auto);
        assert_eq!(read_trust(&shared), TrustMode::Auto);
    }

    #[test]
    fn test_display() {
        assert_eq!(TrustMode::Plan.to_string(), "plan");
        assert_eq!(TrustMode::Safe.to_string(), "safe");
        assert_eq!(TrustMode::Auto.to_string(), "auto");
    }

    #[test]
    fn test_default() {
        assert_eq!(TrustMode::default(), TrustMode::Safe);
    }

    // ── Tool approval tests ──

    const READ_ONLY_TOOLS: &[&str] = &[
        "Read",
        "List",
        "Grep",
        "Glob",
        "MemoryRead",
        "ListAgents",
        "InvokeAgent",
        "WebFetch",
        "WebSearch",
        "ListSkills",
        "ActivateSkill",
    ];

    #[test]
    fn test_read_tools_always_approved() {
        for tool in READ_ONLY_TOOLS {
            for mode in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
                assert_eq!(
                    check_tool(tool, &serde_json::json!({}), mode, None),
                    ToolApproval::AutoApprove,
                    "{tool} should auto-approve in {mode:?}"
                );
            }
        }
    }

    #[test]
    fn test_plan_blocks_all_writes() {
        for tool in ["Write", "Edit", "Delete", "MemoryWrite", "TodoWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), TrustMode::Plan, None),
                ToolApproval::Blocked,
                "{tool} should be blocked in Plan mode"
            );
        }
    }

    #[test]
    fn test_safe_confirms_writes() {
        for tool in ["Write", "Edit", "Delete", "MemoryWrite", "TodoWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), TrustMode::Safe, None),
                ToolApproval::NeedsConfirmation,
                "{tool} should need confirmation in Safe mode"
            );
        }
    }

    #[test]
    fn test_auto_approves_non_outside() {
        // On platforms with sandbox available, Auto mode auto-approves mutations.
        // If sandbox is unavailable, mutations downgrade to NeedsConfirmation (#860).
        let expected = if crate::sandbox::is_available() {
            ToolApproval::AutoApprove
        } else {
            ToolApproval::NeedsConfirmation
        };
        for tool in ["Write", "Edit", "TodoWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), TrustMode::Auto, None),
                expected,
                "{tool} in Auto mode"
            );
        }
        // Read-only tools always auto-approve regardless of sandbox.
        assert_eq!(
            check_tool("WebFetch", &serde_json::json!({}), TrustMode::Auto, None),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_auto_approves_destructive() {
        // In Auto mode, destructive ops are auto-approved when sandbox is available.
        // Without sandbox, they downgrade to NeedsConfirmation (#860).
        let expected = if crate::sandbox::is_available() {
            ToolApproval::AutoApprove
        } else {
            ToolApproval::NeedsConfirmation
        };
        assert_eq!(
            check_tool("Delete", &serde_json::json!({}), TrustMode::Auto, None),
            expected,
        );
    }

    #[test]
    fn test_safe_bash_read_only_auto_approved() {
        let args = serde_json::json!({"command": "git status"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Safe, None),
            ToolApproval::AutoApprove,
        );
    }

    /// gh read-only commands should auto-approve even in Safe mode (#518).
    #[test]
    fn test_gh_read_only_auto_approved() {
        for cmd in [
            "gh issue view 42",
            "gh pr view 99",
            "gh pr list",
            "gh issue list",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, TrustMode::Safe, None),
                ToolApproval::AutoApprove,
                "{cmd} should auto-approve even in Safe mode"
            );
        }
    }

    /// gh destructive commands need confirmation in both Safe and Auto modes (#518).
    #[test]
    fn test_gh_destructive_needs_confirmation_in_safe() {
        for cmd in [
            "gh pr merge 42 --squash",
            "gh issue delete 42",
            "gh repo delete owner/repo",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, TrustMode::Safe, None),
                ToolApproval::NeedsConfirmation,
                "{cmd} should need confirmation in Safe mode"
            );
        }
    }

    /// gh mutation commands confirm in Safe, auto-approve in Auto (#518).
    #[test]
    fn test_gh_mutation_auto_approved_in_auto() {
        for cmd in [
            "gh issue create --title 'bug'",
            "gh issue edit 42",
            "gh pr create",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, TrustMode::Auto, None),
                ToolApproval::AutoApprove,
                "{cmd} should auto-approve in Auto mode"
            );
            assert_eq!(
                check_tool("Bash", &args, TrustMode::Safe, None),
                ToolApproval::NeedsConfirmation,
                "{cmd} should need confirmation in Safe mode"
            );
        }
    }

    #[test]
    fn test_dev_workflow_bash_needs_confirmation_in_safe() {
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Safe, None),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation_in_safe() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Safe, None),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_plan_blocks_bash() {
        let args = serde_json::json!({"command": "cargo test"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Plan, None),
            ToolApproval::Blocked,
        );
    }

    #[test]
    fn test_invoke_agent_auto_approved() {
        let args = serde_json::json!({"agent_name": "reviewer", "prompt": "review this"});
        for mode in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
            assert_eq!(
                check_tool("InvokeAgent", &args, mode, None),
                ToolApproval::AutoApprove,
            );
        }
    }

    // ── Path scoping tests (#218) ──────────────────────────

    #[test]
    fn test_write_outside_project_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_write_inside_project_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_edit_with_dotdot_escape_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "../../../etc/passwd"});
        assert_eq!(
            check_tool("Edit", &args, TrustMode::Auto, Some(root)),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_outside_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd /etc && ls"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, Some(root)),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_bash_cd_inside_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd src && ls"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
        );
    }

    #[test]
    fn test_no_project_root_skips_path_check() {
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, None),
            ToolApproval::AutoApprove,
        );
    }

    // ── Temp path allowlist (#560) ──

    #[test]
    fn test_write_to_tmp_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/tmp/issue-draft.md"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "/tmp writes should auto-approve"
        );
    }

    #[test]
    fn test_bash_cd_tmp_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"command": "cd /tmp && ls"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "cd /tmp should auto-approve"
        );
    }

    #[test]
    fn test_write_to_etc_still_blocked() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/etc/hosts"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::NeedsConfirmation,
            "/etc writes should still need confirmation"
        );
    }

    // ── File lifecycle (#465) tests ──

    #[tokio::test]
    async fn test_delete_owned_file_auto_approved() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let mut tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");
        let owned_path = root.join("temp_output.md");
        tracker.track_created(owned_path).await;

        let args = serde_json::json!({"path": "temp_output.md"});
        assert_eq!(
            check_tool_with_tracker("Delete", &args, TrustMode::Auto, Some(root), Some(&tracker),),
            ToolApproval::AutoApprove,
            "Delete of Koda-owned file should auto-approve"
        );
    }

    #[tokio::test]
    async fn test_delete_unowned_file_needs_confirmation_in_safe() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");

        let args = serde_json::json!({"path": "user_file.rs"});
        assert_eq!(
            check_tool_with_tracker("Delete", &args, TrustMode::Safe, Some(root), Some(&tracker),),
            ToolApproval::NeedsConfirmation,
            "Delete of unowned file should need confirmation in Safe mode"
        );
    }

    #[tokio::test]
    async fn test_delete_owned_file_safe_mode_auto_approved() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let mut tracker = FileTracker::new("test-sess", db).await;
        let root = Path::new("/home/user/project");
        let owned_path = root.join("scratch.txt");
        tracker.track_created(owned_path).await;

        let args = serde_json::json!({"path": "scratch.txt"});
        assert_eq!(
            check_tool_with_tracker("Delete", &args, TrustMode::Safe, Some(root), Some(&tracker),),
            ToolApproval::AutoApprove,
            "Delete of Koda-owned file should auto-approve even in Safe mode"
        );
    }

    #[test]
    fn test_no_tracker_safe_mode_delete_needs_confirmation() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "some_file.rs"});
        assert_eq!(
            check_tool_with_tracker("Delete", &args, TrustMode::Safe, Some(root), None),
            ToolApproval::NeedsConfirmation,
            "Without tracker, Delete should need confirmation in Safe"
        );
    }
}
