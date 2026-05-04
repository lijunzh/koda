//! Unified trust mode — the single permission knob for Koda.
//!
//! Replaces the old `ApprovalMode × SandboxMode` two-layer system with one
//! enum that controls both sandbox configuration and approval behavior.
//! Users see one concept, one CLI flag (`--mode`), one status bar word.
//!
//! See [`docs/src/approval.md`](https://lijunzh.github.io/koda/approval.html)
//! for the canonical user-facing mental-model doc, and `DESIGN.md §Security
//! Model` for the architectural framing (TrustMode as the canonical instance
//! of P1 — "customization over configuration").
//!
//! ## Trust modes
//!
//! | Mode | Sandbox | Approval | Use case |
//! |------|---------|----------|----------|
//! | **Plan** | project, read-only | deny all non-read | Sub-agent investigation only — see [`coerce_for_top_level()`](crate::trust::coerce_for_top_level) |
//! | **Safe** | project, read+write | confirm side effects | User default |
//! | **Auto** | project, read+write | auto-approve mutations; destructive still confirms | Autonomous coding |
//!
//! Plan is **sub-agent-only** for the top-level session (#1244): the
//! `--mode plan` CLI flag, `/resume` of a Plan-persisted session, and
//! the keyboard toggle all funnel through [`coerce_for_top_level()`](crate::trust::coerce_for_top_level)
//! which coerces `Plan → Safe` with a warning. Sub-agents reach Plan
//! via their JSON `"trust": "plan"` declaration plus parent clamping.
//!
//! ## Top-level (interactive) decision matrix
//!
//! Used by [`check_tool()`](crate::trust::check_tool) / [`check_tool_with_tracker()`](crate::trust::check_tool_with_tracker) when a human
//! is at the keyboard.
//!
//! | Behavior | Plan | Safe | Auto |
//! |---|---|---|---|
//! | ReadOnly tools | ✅ auto | ✅ auto | ✅ auto |
//! | RemoteAction | ❌ deny | ⚠️ confirm | ✅ auto |
//! | LocalMutation | ❌ deny | ⚠️ confirm | ✅ auto |
//! | Destructive | ❌ deny | ⚠️ confirm | ⚠️ confirm |
//! | Outside project | ❌ deny | ⚠️ confirm | ⚠️ confirm |
//!
//! **`Auto × Destructive` confirms, not auto-approves** (#1251). The
//! user said YOLO for normal work, not for `rm -rf` / `git reset --hard`
//! / `git push --force` / `Delete`. Destructive ops by definition
//! can't be undone by the sandbox alone (deleting a tracked file is
//! "legal" inside the project root), so Auto keeps the prompt as a
//! deliberate speed-bump.
//!
//! ## Sub-agent (no-human-channel) decision matrix
//!
//! Used by [`check_tool_for_sub_agent()`](crate::trust::check_tool_for_sub_agent) /
//! [`check_tool_for_sub_agent_with_tracker()`](crate::trust::check_tool_for_sub_agent_with_tracker) when an agent dispatched
//! via `InvokeAgent` is making the call. Sub-agents have no live
//! human approval channel by design (#1022 B10), so the matrix
//! resolves what the top-level matrix would treat as `⚠️ confirm`
//! using a **safe-side rule**: mutating ops auto-approve, destructive
//! ops block.
//!
//! | Behavior | Sub-agent in Plan | Sub-agent in Safe | Sub-agent in Auto |
//! |---|---|---|---|
//! | ReadOnly | ✅ auto | ✅ auto | ✅ auto |
//! | RemoteAction | ❌ deny | ✅ auto | ✅ auto |
//! | LocalMutation | ❌ deny | ✅ auto | ✅ auto |
//! | Destructive | ❌ deny | ❌ block | ❌ block |
//! | Outside project | ❌ deny | ❌ block | ❌ block |
//!
//! See [`check_tool_for_sub_agent()`](crate::trust::check_tool_for_sub_agent) for the rationale and the bug-fix
//! reference (#1249).
//!
//! ## Always-on safety floors
//!
//! These apply regardless of trust mode:
//!
//! - **Outside-project floor** (#218) — writes to paths outside
//!   `project_root` always confirm (Safe + Auto) or deny (Plan), even
//!   if the matrix would otherwise auto-approve. Temp dirs (`/tmp`,
//!   `$TMPDIR`, `/var/folders/*/T`) and `~/.cache/koda` are exempt
//!   per the scratch-zone allowlist (#560, #1236).
//! - **Bash path-escape lint** — commands containing `cd`/path
//!   redirection out of the project tree get downgraded to
//!   NeedsConfirmation.
//! - **Sandbox-unavailable downgrade** (#860) — if the platform
//!   sandbox backend isn't installed, Auto downgrades mutating and
//!   destructive ops to NeedsConfirmation so you never lose both
//!   the sandbox and the prompt.
//! - **Koda-owned file Delete** (#465) — deleting a file Koda created
//!   in this session auto-approves (net-zero effect: Koda created it,
//!   Koda removes it). See [`check_tool_with_tracker()`](crate::trust::check_tool_with_tracker).
//!
//! ## Child trust derivation
//!
//! Sub-agents never widen their parent's trust.
//! [`derive_child_trust()`](crate::trust::derive_child_trust) is the single,
//! authoritative way to compute child trust across all
//! sub-agent dispatch paths (sequential, parallel, background, fork).
//! See its rustdoc for the #1022 B19 cautionary tale on why
//! `parent_runtime` (the live atomic) and not `parent_config.trust`
//! (the startup value) is the correct first argument.
//!
//! ## Design principle
//!
//! The sandbox is the safety boundary, not the approval prompt.
//! Auto trusts the agent for non-destructive work within the project
//! sandbox — the kernel enforces the perimeter. Safe adds approval
//! as a second layer (belt and suspenders). Auto keeps the prompt
//! for destructive ops because the sandbox can't undo them. Plan
//! denies all writes outright and is enforced at the syscall level
//! (kernel-level read-only sandbox), strictly stronger than soft
//! denylisting `Write`/`Edit`/`Delete` in `disallowed_tools`.
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
    /// Plan is **sub-agent-only** (#1244) and never reachable for the
    /// top-level session via any path — not the keyboard toggle, not
    /// the `--mode plan` CLI flag, not `/resume` of a Plan-persisted
    /// session. All non-toggle entry paths funnel through
    /// [`coerce_for_top_level`] which coerces Plan → Safe + warns.
    /// The toggle simply loops Plan → Plan as a defensive no-op for
    /// the (impossible-by-construction) case where a sub-agent's
    /// runtime mode somehow gets cycled.
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Plan, // sub-agent-only; #1244
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

/// Coerce a candidate trust mode to one valid for the **top-level**
/// session, returning `(coerced, warning)`.
///
/// **#1244**: `Plan` is sub-agent-only. Pre-fix, the main session
/// could enter Plan via `--mode plan`, `/resume` of a Plan-persisted
/// session, or any other call that fed `TrustMode::parse(..)` output
/// directly into `KodaConfig::with_trust(..)` / `set_trust(..)`. None
/// of those paths offered a coherent UX — the user would land in a
/// "read-only main session" where every write tool the model called
/// would be silently denied, leading to confused
/// retry-loop-then-give-up behavior.
///
/// This helper is the **single coercion point**: every call site that
/// turns a parsed/persisted/CLI-flag mode into a top-level session
/// trust value must route through here. Sub-agent dispatch uses
/// [`derive_child_trust`] instead and is unaffected — sub-agents can
/// (and should) still be Plan.
///
/// Returns the warning as `Option<&'static str>` (not `String`) because
/// the message is wholly static — callers can `eprintln!`/`warn_msg`
/// without an intermediate allocation.
///
/// # Examples
///
/// ```
/// use koda_core::trust::{TrustMode, coerce_for_top_level};
/// // Plan → Safe with a warning
/// let (m, w) = coerce_for_top_level(TrustMode::Plan);
/// assert_eq!(m, TrustMode::Safe);
/// assert!(w.is_some());
/// // Safe / Auto pass through unchanged with no warning
/// assert_eq!(coerce_for_top_level(TrustMode::Safe), (TrustMode::Safe, None));
/// assert_eq!(coerce_for_top_level(TrustMode::Auto), (TrustMode::Auto, None));
/// ```
pub fn coerce_for_top_level(candidate: TrustMode) -> (TrustMode, Option<&'static str>) {
    match candidate {
        TrustMode::Plan => (
            TrustMode::Safe,
            Some(
                "Plan mode is sub-agent-only (read-only mode for spawned agents). \
                 Falling back to Safe for the main session.",
            ),
        ),
        other => (other, None),
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
///
/// Plan is **not** part of the cycle for the top-level session
/// (#1244) — see [`coerce_for_top_level`] for the rationale. The
/// underlying [`TrustMode::next`] implements the same Safe↔Auto
/// loop as a defensive no-op for the (impossible-by-construction)
/// case where Plan reaches the top-level toggle.
pub fn cycle_trust(shared: &SharedTrustMode) -> TrustMode {
    let current = read_trust(shared);
    let next = current.next();
    set_trust(shared, next);
    next
}

// ── Tool Approval Decision ──────────────────────────────────

/// What the trust system decides for a given tool call.
///
/// Returned by [`check_tool`], [`check_tool_with_tracker`],
/// [`check_tool_for_sub_agent`], and [`check_tool_for_sub_agent_with_tracker`].
/// The engine dispatch loop maps each variant to a concrete action:
///
/// - [`AutoApprove`](Self::AutoApprove) — dispatch the tool immediately.
/// - [`NeedsConfirmation`](Self::NeedsConfirmation) — in the TUI, surface
///   an approval prompt; in headless mode, reject with a structural
///   `RejectAuto` event so the model knows no human is in the loop.
/// - [`Blocked`](Self::Blocked) — refuse outright; the tool result
///   carries an error explaining which floor or matrix cell tripped.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolApproval {
    /// Execute without asking.
    AutoApprove,
    /// Show confirmation dialog (top-level only). In sub-agent contexts
    /// this variant is never returned — [`check_tool_for_sub_agent`]
    /// resolves it to either `AutoApprove` (mutating ops) or `Blocked`
    /// (destructive ops) per the safe-side rule.
    NeedsConfirmation,
    /// Blocked. Reasons include:
    /// - Plan trust mode (denies all non-read effects)
    /// - Sub-agent destructive op in Safe/Auto trust (#1251 — no
    ///   human channel to confirm `rm -rf`)
    /// - Delegation scope violation (sub-agent trying to use a tool
    ///   not in its `allowed_tools`)
    Blocked,
}

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
///
/// **Top-level (interactive) decision matrix:**
///
/// | ToolEffect     | Plan    | Safe          | Auto          |
/// |----------------|---------|---------------|---------------|
/// | ReadOnly       | ✅ auto  | ✅ auto        | ✅ auto        |
/// | RemoteAction   | ❌ block | ⚠️ confirm     | ✅ auto        |
/// | LocalMutation  | ❌ block | ⚠️ confirm     | ✅ auto        |
/// | Destructive    | ❌ block | ⚠️ confirm     | ⚠️ confirm     |
///
/// `Auto × Destructive` confirms (changed in #1251 — was auto pre-#1251).
/// The sandbox can't undo a `Delete` or `git reset --hard` on a tracked
/// file inside the project root, so Auto keeps the prompt as a
/// deliberate speed-bump for ops that would otherwise be irreversible.
///
/// For sub-agent contexts, see [`check_tool_for_sub_agent`].
///
/// Additional hardcoded floors:
/// - Writes outside project root → NeedsConfirmation (even in Auto) (#218)
/// - Bash path escapes → NeedsConfirmation
/// - Delete of Koda-owned file → AutoApprove (#465 — see [`check_tool_with_tracker`])
/// - Sandbox backend unavailable → Auto downgrades mutating/destructive
///   ops to NeedsConfirmation (#860)
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
) -> ToolApproval {
    check_tool_inner(tool_name, args, mode, project_root, None, false)
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
    check_tool_inner(tool_name, args, mode, project_root, file_tracker, false)
}

/// Like [`check_tool`] but for sub-agent contexts (no human at the keyboard).
///
/// **Sub-agent decision matrix:**
///
/// | ToolEffect     | Plan    | Safe          | Auto          |
/// |----------------|---------|---------------|---------------|
/// | ReadOnly       | ✅ auto  | ✅ auto        | ✅ auto        |
/// | RemoteAction   | ❌ block | ✅ auto        | ✅ auto        |
/// | LocalMutation  | ❌ block | ✅ auto        | ✅ auto        |
/// | Destructive    | ❌ block | ❌ block       | ❌ block       |
///
/// **Why the matrix differs from top-level**: sub-agents have no live approval
/// channel to a human (`sub_agent_dispatch.rs:173` creates a dead `cmd_rx` by
/// design — #1022 B10). So `NeedsConfirmation` is meaningless: there's no one
/// to confirm. The single rule is: **"ask" means "default to safe-side."**
///
/// - Top-level *"ask"* (Safe × mutating): prompt the human.
/// - Sub-agent *"ask"* (Safe × mutating): approve — the agent was invoked to
///   do work; only ask because a human is watching.
/// - Sub-agent *"ask"* (Auto × destructive): block — user said YOLO but with
///   no human to confirm `rm -rf /`, refuse on safety grounds.
///
/// This resolves the #1249 dead-channel discovery: today, sub-agents at Safe
/// trust auto-reject every Write/Edit/Delete with *"requires user confirmation
/// but this sub-agent has no channel to the user."* After this change, `task`
/// and `default` sub-agents become functional writers.
pub fn check_tool_for_sub_agent(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
) -> ToolApproval {
    check_tool_inner(tool_name, args, mode, project_root, None, true)
}

/// Like [`check_tool_for_sub_agent`] but with an optional file tracker for
/// ownership checks (Delete of Koda-owned files auto-approves regardless of mode).
pub fn check_tool_for_sub_agent_with_tracker(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
    file_tracker: Option<&FileTracker>,
) -> ToolApproval {
    check_tool_inner(tool_name, args, mode, project_root, file_tracker, true)
}

/// Shared implementation for [`check_tool`], [`check_tool_with_tracker`],
/// [`check_tool_for_sub_agent`], and [`check_tool_for_sub_agent_with_tracker`].
///
/// The `is_sub_agent` flag controls how `NeedsConfirmation` decisions get
/// resolved when no human is in the loop:
/// - Mutating ops (LocalMutation/RemoteAction) → AutoApprove (do the work)
/// - Destructive ops → Blocked (refuse to act without consent)
fn check_tool_inner(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
    file_tracker: Option<&FileTracker>,
    is_sub_agent: bool,
) -> ToolApproval {
    let effect = resolve_tool_effect(tool_name, args);

    // Read-only tools always auto-approve in every mode and context
    if effect == ToolEffect::ReadOnly {
        return ToolApproval::AutoApprove;
    }

    // Plan mode: deny everything except read-only (in both contexts)
    if mode == TrustMode::Plan {
        return ToolApproval::Blocked;
    }

    // Hardcoded floor: writes outside project root always need confirmation (#218).
    // For sub-agents, this becomes Blocked via the resolution at the bottom.
    if let Some(root) = project_root {
        if is_outside_project(tool_name, args, root) {
            return resolve_confirmation(effect, is_sub_agent);
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
                return resolve_confirmation(effect, is_sub_agent);
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

    // Apply the ToolEffect × TrustMode × Context matrix
    let raw_decision = match mode {
        TrustMode::Plan => unreachable!(), // handled above
        TrustMode::Safe => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::RemoteAction | ToolEffect::LocalMutation | ToolEffect::Destructive => {
                ToolApproval::NeedsConfirmation
            }
        },
        TrustMode::Auto => match effect {
            ToolEffect::ReadOnly => ToolApproval::AutoApprove,
            ToolEffect::Destructive => {
                // #1250: Destructive operations always require human consent,
                // even in Auto mode. The user said YOLO for normal work, not
                // for `rm -rf /`. For sub-agents this becomes Blocked below.
                ToolApproval::NeedsConfirmation
            }
            ToolEffect::RemoteAction | ToolEffect::LocalMutation => {
                // Safety net: if the kernel sandbox is unavailable, Auto mode
                // loses its perimeter. Downgrade mutating ops to
                // NeedsConfirmation so the user still gets a prompt (#860).
                if crate::sandbox::is_available() {
                    ToolApproval::AutoApprove
                } else {
                    ToolApproval::NeedsConfirmation
                }
            }
        },
    };

    // For sub-agents, resolve any NeedsConfirmation per the safe-side rule.
    // For top-level, pass through to the human.
    if is_sub_agent && raw_decision == ToolApproval::NeedsConfirmation {
        resolve_confirmation(effect, true)
    } else {
        raw_decision
    }
}

/// Resolve a `NeedsConfirmation` decision for sub-agents (where there's no
/// human in the loop). The rule: **"ask" means "default to safe-side."**
///
/// - Destructive ops → Blocked (refuse to do dangerous things without consent)
/// - Everything else → AutoApprove (the agent was invoked to do work)
///
/// For top-level callers (`is_sub_agent == false`), this returns
/// `NeedsConfirmation` unchanged so the human can decide.
fn resolve_confirmation(effect: ToolEffect, is_sub_agent: bool) -> ToolApproval {
    if !is_sub_agent {
        return ToolApproval::NeedsConfirmation;
    }
    match effect {
        ToolEffect::Destructive => ToolApproval::Blocked,
        _ => ToolApproval::AutoApprove,
    }
}

/// Resolve the effective [`ToolEffect`] for a tool call.
///
/// For Bash, refines the generic `LocalMutation` classification by
/// parsing the actual command string.
///
/// For MCP tools, falls back to `RemoteAction` unless a ToolRegistry
/// is provided via [`resolve_tool_effect_with_registry`].
///
/// Typically called immediately before [`check_tool`] /
/// [`check_tool_for_sub_agent`] in the engine dispatch loop — the
/// resolved effect is the matrix row, the trust mode is the matrix
/// column.
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
        // TodoWrite mutates Koda-owned session state only — no FS impact —
        // so it must auto-approve in every trust mode (#1212).
        "TodoWrite",
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
        for tool in ["Write", "Edit", "Delete", "MemoryWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), TrustMode::Plan, None),
                ToolApproval::Blocked,
                "{tool} should be blocked in Plan mode"
            );
        }
    }

    #[test]
    fn test_safe_confirms_writes() {
        for tool in ["Write", "Edit", "Delete", "MemoryWrite"] {
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
        for tool in ["Write", "Edit"] {
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
    fn test_auto_destructive_needs_confirmation() {
        // #1250: In Auto mode, destructive ops require human confirmation
        // even though normal mutating ops auto-approve. Rationale: the user
        // said YOLO for normal work, not for `rm -rf /`.
        //
        // Pre-#1250 this returned AutoApprove (when sandbox available); the
        // tightening here matches the principle that destructive operations
        // (`rm -rf`, `git reset --hard`, `git push --force`, ...) deserve a
        // second look regardless of trust mode.
        assert_eq!(
            check_tool("Delete", &serde_json::json!({}), TrustMode::Auto, None),
            ToolApproval::NeedsConfirmation,
        );
    }

    #[test]
    fn test_auto_local_mutation_auto_approved_with_sandbox() {
        // Non-destructive mutations still auto-approve in Auto when sandbox
        // is available. Without sandbox, downgrade to NeedsConfirmation (#860).
        let expected = if crate::sandbox::is_available() {
            ToolApproval::AutoApprove
        } else {
            ToolApproval::NeedsConfirmation
        };
        assert_eq!(
            check_tool(
                "Write",
                &serde_json::json!({"path": "foo.rs", "content": "x"}),
                TrustMode::Auto,
                None
            ),
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

    /// #1232 §8b: writing to `/var/folders/.../T/...` (macOS scratch
    /// root) must auto-approve in Auto mode even without `$TMPDIR`
    /// in the env. The pre-PR repro was a sub-process inheriting a
    /// stripped env, losing `$TMPDIR`, and prompting on every
    /// scratchpad write — the env-independent `/var/folders/`
    /// prefix in `is_safe_external_path` is the safety belt.
    #[test]
    fn test_write_to_var_folders_auto_approved() {
        let root = Path::new("/home/user/project");
        let args = serde_json::json!({"path": "/var/folders/xx/yy/T/scratch.md"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "/var/folders/* writes should auto-approve in Auto (#1232 §8b) \
             — it's macOS's scratch root, env-independent"
        );
    }

    /// #1232 §8b: writing to `~/.cache/koda/` (koda's own cache
    /// zone) must auto-approve in Auto mode. Without this the model
    /// would get a confirm prompt every time it touched its own
    /// cache, which is absurd.
    #[test]
    fn test_write_to_koda_cache_auto_approved() {
        let root = Path::new("/home/user/project");
        // Use the real $HOME so the test mirrors production resolution.
        let home = std::env::var("HOME").expect("HOME must be set in test env");
        let path = format!("{home}/.cache/koda/test-scratch.json");
        let args = serde_json::json!({ "path": path });
        assert_eq!(
            check_tool("Write", &args, TrustMode::Auto, Some(root)),
            ToolApproval::AutoApprove,
            "~/.cache/koda/ writes should auto-approve in Auto (#1232 §8b)"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    // ── #1244: Plan is sub-agent-only for the top-level session ────────────────
    //
    // The bug-review session that opened #1232 surfaced that a user
    // can land in `TrustMode::Plan` for the **main session** via:
    //   * `koda --mode plan` CLI flag
    //   * `/resume` of a session that was previously persisted in Plan
    // …which makes no sense — Plan denies all write tools, so the
    // top-level user gets a model that can read but never act.
    // `coerce_for_top_level` is the single funnel every non-sub-agent
    // entry point goes through; sub-agent dispatch (`derive_child_trust`)
    // is unaffected and continues to produce Plan freely.

    #[test]
    fn coerce_top_level_plan_falls_back_to_safe_with_warning() {
        // The whole point of the fix: Plan must NOT survive as a
        // top-level mode. Pin the coercion target (Safe — the most
        // conservative writable mode) and the presence of a warning
        // (callers depend on the `Some(_)` to know they should emit
        // a banner / eprintln).
        let (coerced, warning) = coerce_for_top_level(TrustMode::Plan);
        assert_eq!(coerced, TrustMode::Safe, "Plan must coerce to Safe");
        assert!(
            warning.is_some(),
            "Plan coercion must yield a warning so the user sees what happened"
        );
        // Sniff for the keywords callers will look for in the message;
        // a wholesale rephrasing is fine but it must still self-explain.
        let msg = warning.unwrap();
        assert!(
            msg.contains("Plan") && msg.contains("Safe"),
            "warning must mention both Plan and Safe so the user knows what changed; got: {msg}"
        );
    }

    #[test]
    fn coerce_top_level_safe_passes_through_unchanged_no_warning() {
        // Negative regression: only Plan should ever produce a
        // warning. Safe — the existing default — must round-trip
        // exactly so users who explicitly `--mode safe` don't get
        // a spurious banner.
        let (coerced, warning) = coerce_for_top_level(TrustMode::Safe);
        assert_eq!(coerced, TrustMode::Safe);
        assert!(
            warning.is_none(),
            "Safe must not warn (it's the coerce target, not the source)"
        );
    }

    #[test]
    fn coerce_top_level_auto_passes_through_unchanged_no_warning() {
        // Same negative regression for Auto. After #1241 flips the
        // default to Auto, this test is the load-bearing assertion
        // that the new default doesn't get caught by some future
        // "coerce dangerous modes" change — only Plan ever coerces.
        let (coerced, warning) = coerce_for_top_level(TrustMode::Auto);
        assert_eq!(coerced, TrustMode::Auto);
        assert!(
            warning.is_none(),
            "Auto is a valid top-level mode; no warning"
        );
    }

    #[test]
    fn coerce_top_level_is_idempotent() {
        // Coercing the output of `coerce_for_top_level` again must
        // never produce a warning — the result of a coerce IS valid
        // by construction. Defends against a future change that
        // accidentally widens what coerces (e.g. "also coerce Auto").
        for mode in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
            let (first, _) = coerce_for_top_level(mode);
            let (second, second_warn) = coerce_for_top_level(first);
            assert_eq!(
                first, second,
                "coerce must be idempotent for input {mode:?}"
            );
            assert!(
                second_warn.is_none(),
                "coerce of an already-coerced value must not warn (input {mode:?})"
            );
        }
    }

    #[test]
    fn coerce_top_level_does_not_affect_derive_child_trust() {
        // Sanity: the sub-agent dispatch path (`derive_child_trust`)
        // is the LEGITIMATE source of Plan and must remain untouched
        // by the top-level coercion. If a parent in Plan spawns a
        // child, the child stays in Plan — coercion never enters
        // the picture. This test pins that separation so a future
        // "unify all trust-routing" refactor can't accidentally
        // funnel sub-agents through `coerce_for_top_level`.
        let child_inherits = derive_child_trust(TrustMode::Plan, TrustMode::Auto);
        assert_eq!(
            child_inherits,
            TrustMode::Plan,
            "sub-agent must keep Plan; coercion is top-level-only"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // #1250: Sub-agent context (no human in the loop)
    //
    // The dead approval channel (`sub_agent_dispatch.rs:173`) means
    // `NeedsConfirmation` is meaningless for sub-agents. The single rule
    // applied by `check_tool_for_sub_agent` is: "ask" → default to safe-side.
    //   - mutating ops → AutoApprove (the agent was invoked to do work)
    //   - destructive ops → Blocked (refuse without consent)
    //
    // Critical bug fix: pre-#1250, `task` and `default` sub-agents at Safe
    // trust auto-rejected every Write/Edit/Delete. After #1250 they work.
    // ─────────────────────────────────────────────────────────────────

    /// Sub-agent at Plan: same as top-level Plan. Read auto, everything else block.
    #[test]
    fn sub_agent_plan_blocks_writes() {
        for tool in ["Write", "Edit", "Delete", "Bash"] {
            let args = if tool == "Bash" {
                serde_json::json!({"command": "cargo build"})
            } else {
                serde_json::json!({"path": "foo.rs", "content": "x"})
            };
            assert_eq!(
                check_tool_for_sub_agent(tool, &args, TrustMode::Plan, None),
                ToolApproval::Blocked,
                "sub-agent at Plan must block {tool}"
            );
        }
    }

    #[test]
    fn sub_agent_plan_allows_reads() {
        // Read-only tools and read-only Bash always work, even at Plan.
        for tool in ["Read", "Grep", "Glob"] {
            assert_eq!(
                check_tool_for_sub_agent(tool, &serde_json::json!({}), TrustMode::Plan, None),
                ToolApproval::AutoApprove,
                "sub-agent at Plan must allow read tool {tool}"
            );
        }
        assert_eq!(
            check_tool_for_sub_agent(
                "Bash",
                &serde_json::json!({"command": "git status"}),
                TrustMode::Plan,
                None
            ),
            ToolApproval::AutoApprove,
            "sub-agent at Plan must allow read-only Bash"
        );
    }

    /// THE BUG FIX: sub-agent at Safe can now write. Pre-#1250 these all
    /// auto-rejected because `NeedsConfirmation` had no human to consult.
    #[test]
    fn sub_agent_safe_auto_approves_writes() {
        let args = serde_json::json!({"path": "foo.rs", "content": "x"});
        for tool in ["Write", "Edit"] {
            assert_eq!(
                check_tool_for_sub_agent(tool, &args, TrustMode::Safe, None),
                ToolApproval::AutoApprove,
                "sub-agent at Safe must auto-approve {tool} (was the #1250 bug)"
            );
        }
    }

    #[test]
    fn sub_agent_safe_auto_approves_mutating_bash() {
        // `cargo build` etc. — the bread and butter of worker agents.
        for cmd in ["cargo build", "npm install", "pytest", "git commit -m wip"] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool_for_sub_agent("Bash", &args, TrustMode::Safe, None),
                ToolApproval::AutoApprove,
                "sub-agent at Safe must auto-approve mutating Bash: {cmd}"
            );
        }
    }

    #[test]
    fn sub_agent_safe_blocks_destructive_bash() {
        // Even a worker agent must not silently `rm -rf /`. With no human
        // to confirm, refuse on safety grounds.
        for cmd in [
            "rm -rf /",
            "git reset --hard origin/main",
            "git push --force",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool_for_sub_agent("Bash", &args, TrustMode::Safe, None),
                ToolApproval::Blocked,
                "sub-agent at Safe must BLOCK destructive Bash: {cmd}"
            );
        }
    }

    #[test]
    fn sub_agent_auto_auto_approves_writes() {
        // Non-destructive mutations work in Auto, same as Safe (sub-agent context).
        let args = serde_json::json!({"path": "foo.rs", "content": "x"});
        for tool in ["Write", "Edit"] {
            // Sub-agent dispatch always provides project_root; pass /tmp
            // to satisfy the outside-project guardrail (path is inside).
            assert_eq!(
                check_tool_for_sub_agent(tool, &args, TrustMode::Auto, None),
                ToolApproval::AutoApprove,
                "sub-agent at Auto must auto-approve {tool}"
            );
        }
    }

    #[test]
    fn sub_agent_auto_blocks_destructive() {
        // The user said YOLO at top-level, but the sub-agent has no human
        // to escalate the destructive prompt to. Block.
        for cmd in [
            "rm -rf /",
            "git reset --hard origin/main",
            "git push --force",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool_for_sub_agent("Bash", &args, TrustMode::Auto, None),
                ToolApproval::Blocked,
                "sub-agent at Auto must STILL BLOCK destructive Bash: {cmd}"
            );
        }
    }

    #[test]
    fn sub_agent_safe_auto_approves_remote_action() {
        // curl, WebFetch, gh CLI etc. — work at Safe in sub-agents.
        for cmd in ["curl https://example.com", "gh pr create -t x -b y"] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool_for_sub_agent("Bash", &args, TrustMode::Safe, None),
                ToolApproval::AutoApprove,
                "sub-agent at Safe must auto-approve remote action: {cmd}"
            );
        }
    }

    /// Top-level vs sub-agent divergence: same tool call, same mode, different
    /// approval based on context. This is the core of the #1250 design.
    #[test]
    fn top_level_vs_sub_agent_safe_write_diverges() {
        let args = serde_json::json!({"path": "foo.rs", "content": "x"});
        assert_eq!(
            check_tool("Write", &args, TrustMode::Safe, None),
            ToolApproval::NeedsConfirmation,
            "top-level Safe × Write must prompt human"
        );
        assert_eq!(
            check_tool_for_sub_agent("Write", &args, TrustMode::Safe, None),
            ToolApproval::AutoApprove,
            "sub-agent Safe × Write must auto-approve (no human to ask)"
        );
    }

    #[test]
    fn top_level_vs_sub_agent_auto_destructive_diverges() {
        let args = serde_json::json!({"command": "rm -rf /"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, None),
            ToolApproval::NeedsConfirmation,
            "top-level Auto × destructive Bash must prompt human (#1250 tightening)"
        );
        assert_eq!(
            check_tool_for_sub_agent("Bash", &args, TrustMode::Auto, None),
            ToolApproval::Blocked,
            "sub-agent Auto × destructive Bash must BLOCK (no human to ask)"
        );
    }

    #[test]
    fn sub_agent_read_tools_always_approved() {
        // Sanity floor: read tools work at every mode and context.
        for mode in [TrustMode::Plan, TrustMode::Safe, TrustMode::Auto] {
            for tool in ["Read", "Grep", "Glob"] {
                assert_eq!(
                    check_tool_for_sub_agent(tool, &serde_json::json!({}), mode, None),
                    ToolApproval::AutoApprove,
                    "sub-agent {mode:?} must allow read tool {tool}"
                );
            }
        }
    }
}
