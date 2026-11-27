//! Unified trust mode — the single permission knob for Koda.
//!
//! One enum, Plan or Auto, controls both the sandbox and what asks first.
//! Users see one concept, one CLI flag (`--mode`), one status bar word.
//! Approvals follow Codex: the sandbox is the boundary, and a command that
//! needs more asks to leave it (`outside_sandbox`, Codex's escalation).
//!
//! See [`docs/src/approval.md`](https://lijunzh.github.io/koda/approval.html)
//! for the user-facing model, and `DESIGN.md §Security Model` for the
//! architectural framing.
//!
//! ## Trust modes
//!
//! | Mode | Sandbox | Asks first | Use case |
//! |------|---------|------------|----------|
//! | **Plan** | project read-only | nothing runs that could change it; edits refused | Planning with the user (`/plan`, Shift+Tab), read-only sub-agents, `koda exec --mode plan` |
//! | **Auto** | project read+write | leaving the sandbox, dangerous commands, files outside the project | Coding (the default) |
//!
//! Plan runs every shell command in its read-only kernel sandbox
//! ([`policy_for_agent`](crate::sandbox::policy_for_agent)), as Codex's
//! read-only mode does, so a command needs no verdict to run there. Its
//! model is told each Plan turn to propose a plan, which the TUI offers to
//! implement in Auto. Sub-agents reach Plan through their JSON
//! `"trust": "plan"` and parent clamping.
//!
//! ## Top-level (interactive) decision matrix
//!
//! Used by [`check_tool()`](crate::trust::check_tool) when a human is at
//! the keyboard.
//!
//! | Behavior | Plan | Auto |
//! |---|---|---|
//! | ReadOnly tools | ✅ auto | ✅ auto |
//! | A shell command | ✅ auto, read-only sandbox | ✅ auto |
//! | Other changes (edits, remote actions) | ❌ deny | ✅ auto |
//! | Destructive | ❌ deny | ⚠️ confirm |
//! | Leaving the sandbox | ❌ deny | ⚠️ confirm |
//! | Outside project | ❌ deny | ⚠️ confirm |
//!
//! **Destructive confirms in Auto** (#1251): the sandbox can't undo an
//! `rm -rf` inside the project or a `git push --force`.
//!
//! ## Sub-agents
//!
//! A sub-agent's calls go through the same matrix, as Claude Code's
//! sub-agents inherit the session's mode and rules. What would ask for the
//! main agent asks for a sub-agent too, in the main session, naming it
//! (`approval_flow::Approver`); with nobody to ask it is refused.
//!
//! ## Always-on safety floors
//!
//! - **Outside-project floor** (#218): writes outside `project_root`
//!   confirm in Auto and are refused in Plan. Temp dirs (`/tmp`,
//!   `$TMPDIR`, `/var/folders/*/T`) and `~/.cache/koda` are exempt
//!   (#560, #1236).
//! - **No sandbox on this machine** (#860): every command and change
//!   asks first in Auto, as Claude Code asks with its sandbox off, and Plan
//!   refuses commands it can't contain.
//!
//! ## Child trust derivation
//!
//! Sub-agents never widen their parent's trust.
//! [`derive_child_trust()`](crate::trust::derive_child_trust) is the single,
//! authoritative way to compute child trust across all sub-agent dispatch
//! paths. See its rustdoc for why `parent_runtime` (the live atomic) and not
//! `parent_config.trust` (the startup value) is the first argument.
//!
//! ## Design principle
//!
//! The sandbox is the safety boundary, not the approval prompt. Auto trusts
//! the agent for work the sandbox contains and asks for what it can't:
//! leaving it, destructive commands, files outside the project. Plan is
//! enforced at the syscall level (a kernel read-only sandbox), strictly
//! stronger than denylisting `Write`/`Edit`. Credential dirs are always
//! blocked regardless of mode.

// `classify_bash_command` no longer imported here — `BashTool::classify`
// owns Bash's per-call effect resolution (#1265 PR-9 cleanup).
use crate::tools::ToolEffect;
use path_clean::PathClean;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

// ── TrustMode ─────────────────────────────────────────────

/// The trust mode, defined once in the protocol crate and used by the
/// engine, the config file and the wire alike.
pub use koda_protocol::state::TrustMode;

/// What the model reads in place of a tool's result when Plan refuses the
/// call: the protocol's [`koda_protocol::item::PLAN_BLOCKED_MESSAGE`].
pub use koda_protocol::item::PLAN_BLOCKED_MESSAGE;

// ── Child trust derivation (#1022 B19) ──────────────────────

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
/// // Named child declares Auto, parent runtime is Plan → child
/// // clamps down to Plan (declared narrows toward parent).
/// assert_eq!(
///     derive_child_trust(TrustMode::Plan, TrustMode::Auto),
///     TrustMode::Plan,
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
///     derive_child_trust(TrustMode::Auto, TrustMode::Auto),
///     TrustMode::Auto,
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

/// Switch to the other trust mode (Auto ↔ Plan) and return it.
pub fn cycle_trust(shared: &SharedTrustMode) -> TrustMode {
    let current = read_trust(shared);
    let next = current.next();
    set_trust(shared, next);
    next
}

// ── Tool Approval Decision ──────────────────────────────────

/// What the trust system decides for a given tool call.
///
/// Returned by [`check_tool`], for the main agent and sub-agents alike.
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
    /// Ask the person; for a sub-agent, in the main session.
    NeedsConfirmation,
    /// Blocked. Reasons include:
    /// - Plan trust mode (denies all non-read effects)
    /// - Delegation scope violation (sub-agent trying to use a tool
    ///   not in its `allowed_tools`)
    Blocked,
}

/// Decide whether a tool call should be auto-approved, confirmed, or blocked.
///
/// **Top-level (interactive) decision matrix**, as Codex decides from its
/// sandbox:
///
/// | ToolEffect     | Plan                       | Auto          |
/// |----------------|----------------------------|---------------|
/// | ReadOnly       | ✅ auto                     | ✅ auto        |
/// | A shell command| ✅ auto, read-only sandbox  | ✅ auto        |
/// | RemoteAction   | ❌ block                    | ✅ auto        |
/// | LocalMutation  | ❌ block                    | ✅ auto        |
/// | Destructive    | ❌ block                    | ⚠️ confirm     |
///
/// A shell command in Plan (`Bash`, `Monitor`) runs in Plan's read-only
/// kernel sandbox ([`crate::sandbox`]), so it needs no verdict of its own,
/// as in Codex's read-only mode; with no sandbox to hold it, it is refused.
///
/// `Auto × Destructive` confirms (changed in #1251 — was auto pre-#1251).
/// The sandbox can't undo an `rm -rf` or `git reset --hard` on a tracked
/// file inside the project root, so Auto keeps the prompt as a
/// deliberate speed-bump for ops that would otherwise be irreversible.
///
/// Additional hardcoded floors:
/// - Writes outside project root → NeedsConfirmation (even in Auto) (#218)
/// - Bash path escapes → NeedsConfirmation
/// - Sandbox backend unavailable → Auto downgrades mutating/destructive
///   ops to NeedsConfirmation (#860)
pub fn check_tool(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
) -> ToolApproval {
    check_tool_inner(tool_name, args, mode, project_root, None)
}

/// [`check_tool`], classifying with `registry`'s catalog, which knows the
/// connected MCP servers: a tool its server marks `readOnlyHint` counts as
/// read-only, so Plan runs it. [`check_tool`] uses a
/// catalog without MCP servers, where every MCP tool counts as a remote
/// action.
pub fn check_tool_with_registry(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
    registry: &crate::tools::ToolRegistry,
) -> ToolApproval {
    check_tool_inner(tool_name, args, mode, project_root, Some(registry))
}

/// Shared implementation of [`check_tool`] and [`check_tool_with_registry`].
fn check_tool_inner(
    tool_name: &str,
    args: &serde_json::Value,
    mode: TrustMode,
    project_root: Option<&Path>,
    registry: Option<&crate::tools::ToolRegistry>,
) -> ToolApproval {
    let effect = resolve_tool_effect_inner(tool_name, args, registry);

    // Read-only tools always auto-approve in every mode and context
    if effect == ToolEffect::ReadOnly {
        return ToolApproval::AutoApprove;
    }

    // Plan mode: a shell command runs in the read-only sandbox, which
    // refuses its writes, as Codex's read-only mode runs commands; a
    // dangerous one, or one asking to leave the sandbox (Destructive), is
    // refused, and so is any command with no sandbox to hold it. Edits are
    // refused. A page from a site that isn't a preapproved documentation
    // site asks first, as in Claude Code: reading the web is part of
    // planning.
    if mode == TrustMode::Plan {
        if matches!(tool_name, "Bash" | "Monitor") {
            let contained = effect != ToolEffect::Destructive && crate::sandbox::is_available();
            return if contained {
                ToolApproval::AutoApprove
            } else {
                ToolApproval::Blocked
            };
        }
        if tool_name == "WebFetch" {
            return ToolApproval::NeedsConfirmation;
        }
        return ToolApproval::Blocked;
    }

    // Hardcoded floor: writes outside project root always need confirmation (#218).
    if let Some(root) = project_root
        && reaches_outside_project(tool_name, args, root)
    {
        return ToolApproval::NeedsConfirmation;
    }

    // Apply the ToolEffect × TrustMode × Context matrix
    match mode {
        TrustMode::Plan => unreachable!(), // handled above
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
/// Typically called immediately before [`check_tool`] in the engine
/// dispatch loop — the
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
    // Single-source-of-truth dispatch (#1265 PR-9): every tool's
    // per-call classification lives on its `Tool` impl, accessed
    // via the catalog's `classify_call`. The MCP-aware variant
    // here uses the registry's catalog (which carries the manager)
    // when available; otherwise it falls back to the process-wide
    // default catalog (no MCP context, but built-ins classify the
    // same).
    //
    // Pre-#1265 this function had a hand-rolled Bash special-case
    // and a MCP-vs-built-in dispatch ladder. Both are now unnecessary
    // because `BashTool::classify(args)` examines the command string
    // and MCP routing lives in the catalog.
    match registry {
        Some(reg) => reg.catalog().classify_call(tool_name, args),
        None => crate::tools::ToolCatalog::default_static().classify_call(tool_name, args),
    }
}

/// Whether a file tool targets a path outside the project root (#218).
/// Hardcoded floor: always NeedsConfirmation regardless of mode.
///
/// Temp directories (`/tmp`, `$TMPDIR`) are explicitly allowed (#560).
fn is_outside_project(tool_name: &str, args: &serde_json::Value, project_root: &Path) -> bool {
    match file_tool_target(tool_name, args, project_root) {
        Some(resolved) => {
            // Koda's own settings ask even inside the project (a session
            // started in `~`): a change there decides what the next
            // session runs.
            if koda_sandbox::is_koda_config(&resolved) {
                return true;
            }
            let outside = !resolved.starts_with(resolve_through_links(project_root));
            // Allow temp directories (#560)
            if outside && crate::bash_path_lint::is_safe_external_path(&resolved) {
                return false;
            }
            outside
        }
        None => false,
    }
}

/// The file a Write or Edit targets, resolved with [`resolve_through_links`].
fn file_tool_target(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
) -> Option<std::path::PathBuf> {
    if !matches!(tool_name, "Write" | "Edit") {
        return None;
    }
    let requested = Path::new(
        args.get("path")
            .or(args.get("file_path"))
            .and_then(|v| v.as_str())?,
    );
    Some(resolve_through_links(&project_root.join(requested)))
}

/// `path` with its deepest existing ancestor canonicalized (symlinks, macOS
/// /var → /private/var) and the rest appended, so a path resolves the same
/// before and after the file or its directories are created.
fn resolve_through_links(path: &Path) -> std::path::PathBuf {
    let clean = path.clean();
    let mut base = clean.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canon) = base.canonicalize() {
            return rest.iter().rev().fold(canon, |path, part| path.join(part));
        }
        match (base.file_name(), base.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                base = parent;
            }
            _ => return clean,
        }
    }
}

/// Whether a call trips the outside-project floor: a file tool aimed outside
/// the project, or a Bash command whose paths or `cd` leave it.
fn reaches_outside_project(tool_name: &str, args: &serde_json::Value, project_root: &Path) -> bool {
    if is_outside_project(tool_name, args, project_root) {
        return true;
    }
    if tool_name == "Bash" {
        let command = args
            .get("command")
            .or(args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return crate::bash_path_lint::lint_bash_paths(command, project_root).has_warnings();
    }
    false
}

// ── Allow for this session ────────────────────────────────

/// What the user allowed "for this session" at an approval prompt.
///
/// Codex's model, not Claude Code's: an allow is held in memory for one
/// session and never written to config or the database, so Koda behaves the
/// same on every machine and a new session asks again. It doesn't add a
/// second gating mechanism beside [`TrustMode`]; it only remembers a yes the
/// mode asked for. A clone shares the set, so the main agent and every
/// sub-agent, background ones included, read and add to one set, as Claude
/// Code applies a sub-agent's "allow for session" to the whole session.
#[derive(Debug, Default, Clone)]
pub struct SessionAllows(std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl SessionAllows {
    /// Whether `key` (from [`session_allow_key`]) was allowed.
    pub fn contains(&self, key: &str) -> bool {
        self.0.lock().is_ok_and(|keys| keys.contains(key))
    }

    /// Allow `key` for the rest of the session.
    pub fn insert(&self, key: String) {
        if let Ok(mut keys) = self.0.lock() {
            keys.insert(key);
        }
    }

    /// Whether a "yes for the session" covers this call: the same key, or,
    /// for a command asking to leave the sandbox, a prefix allowed for the
    /// session that it starts with, as a Codex exec-policy prefix rule
    /// does. A command dangerous in itself (`git push --force` under an
    /// allowed `git push`) is never covered.
    pub fn covers(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        effect: ToolEffect,
        project_root: &Path,
    ) -> bool {
        if session_allow_key(tool_name, args, effect, project_root)
            .is_some_and(|key| self.contains(&key))
        {
            return true;
        }
        if tool_name != "Bash" || !crate::tools::shell::wants_outside_sandbox(args) {
            return false;
        }
        let Some(argv) = args
            .get("command")
            .and_then(|v| v.as_str())
            .and_then(plain_argv)
        else {
            return false;
        };
        let inside = {
            let mut inside = args.clone();
            inside["outside_sandbox"] = serde_json::Value::Bool(false);
            inside
        };
        if resolve_tool_effect(tool_name, &inside) == ToolEffect::Destructive {
            return false;
        }
        let Ok(keys) = self.0.lock() else {
            return false;
        };
        keys.iter()
            .filter_map(|key| key.strip_prefix(PREFIX_KEY))
            .any(|prefix| {
                let words: Vec<&str> = prefix.split('\0').collect();
                argv.len() >= words.len() && argv.iter().zip(&words).all(|(a, w)| a == w)
            })
    }
}

/// The start of a session key that allows a command prefix.
const PREFIX_KEY: &str = "Bash-prefix\0";

/// Prefixes too broad to allow, as Codex refuses to suggest them
/// (`BANNED_PREFIX_SUGGESTIONS`, `codex-rs/core/src/exec_policy.rs`): each
/// would run any program or script.
const BANNED_PREFIXES: &[&[&str]] = &[
    &["bash"],
    &["bash", "-c"],
    &["bash", "-lc"],
    &["sh"],
    &["sh", "-c"],
    &["sh", "-lc"],
    &["zsh"],
    &["zsh", "-c"],
    &["zsh", "-lc"],
    &["dash"],
    &["dash", "-c"],
    &["fish"],
    &["fish", "-c"],
    &["ksh"],
    &["ksh", "-c"],
    &["/bin/bash"],
    &["/bin/bash", "-c"],
    &["/bin/bash", "-lc"],
    &["/bin/sh"],
    &["/bin/sh", "-c"],
    &["/bin/sh", "-lc"],
    &["/bin/zsh"],
    &["/bin/zsh", "-c"],
    &["/bin/zsh", "-lc"],
    &["env"],
    &["git"],
    &["sudo"],
    &["xargs"],
    &["node"],
    &["node", "-e"],
    &["nodejs"],
    &["nodejs", "-e"],
    &["deno"],
    &["deno", "eval"],
    &["bun"],
    &["bun", "-e"],
    &["bun", "run"],
    &["npm", "run"],
    &["pnpm", "run"],
    &["python"],
    &["python", "-"],
    &["python", "-c"],
    &["python3"],
    &["python3", "-"],
    &["python3", "-c"],
    &["py"],
    &["pypy"],
    &["pypy3"],
    &["perl"],
    &["perl", "-e"],
    &["php"],
    &["php", "-r"],
    &["ruby"],
    &["ruby", "-e"],
    &["lua"],
    &["lua", "-e"],
    &["julia"],
    &["julia", "-e"],
    &["Rscript"],
    &["osascript"],
];

/// A command's words, when it is one plain command: no `&&`, `|`, `;`,
/// redirection or substitution, which a prefix can't describe.
fn plain_argv(command: &str) -> Option<Vec<String>> {
    if command.chars().any(|c| "&|;<>`$(){}\n".contains(c)) {
        return None;
    }
    shlex::split(command).filter(|words| !words.is_empty())
}

/// The session key a command asking to leave the sandbox may be allowed
/// under: its `prefix_rule`, when that is a valid prefix of the command.
fn prefix_rule_key(args: &serde_json::Value) -> Option<String> {
    let argv = plain_argv(args.get("command")?.as_str()?)?;
    let rule: Vec<&str> = args
        .get("prefix_rule")?
        .as_array()?
        .iter()
        .map(|word| word.as_str())
        .collect::<Option<_>>()?;
    let banned = BANNED_PREFIXES
        .iter()
        .any(|banned| *banned == rule.as_slice());
    let starts = argv.len() >= rule.len() && argv.iter().zip(&rule).all(|(a, w)| a == w);
    (!rule.is_empty() && !banned && starts && rule.iter().all(|w| !w.contains('\0')))
        .then(|| format!("{PREFIX_KEY}{}", rule.join("\0")))
}

/// The key a call can be allowed for the session under, or `None` when it
/// has to ask every time.
///
/// Only a [`ToolEffect::LocalMutation`] inside the project qualifies.
/// Destructive calls (including leaving the sandbox), remote actions and
/// anything that trips the outside-project floor keep asking.
///
/// The key is what the user saw and said yes to, as in Codex:
/// - **Bash:** the exact command (and whether it runs in the background).
/// - **Edit, Write:** the file, so later edits to it don't ask.
/// - **Anything else:** the exact arguments.
pub fn session_allow_key(
    tool_name: &str,
    args: &serde_json::Value,
    effect: ToolEffect,
    project_root: &Path,
) -> Option<String> {
    // A fetch from a site: "allow for session" covers the site, as Claude
    // Code's allow covers a domain.
    if tool_name == "WebFetch" && effect == ToolEffect::RemoteAction {
        let url = args.get("url").and_then(|v| v.as_str())?;
        let host = crate::tools::web_fetch::host_of(url)?;
        return Some(format!("WebFetch\0{host}"));
    }
    // A command asking to leave the sandbox: its prefix, as Codex's
    // `prefix_rule` (see [`SessionAllows::covers`]).
    if tool_name == "Bash" && crate::tools::shell::wants_outside_sandbox(args) {
        return prefix_rule_key(args);
    }
    if effect != ToolEffect::LocalMutation || reaches_outside_project(tool_name, args, project_root)
    {
        return None;
    }
    let subject = match tool_name {
        "Bash" => {
            let command = args.get("command").and_then(|v| v.as_str())?.trim();
            let background = args["background"].as_bool().unwrap_or(false);
            format!("{background}\0{command}")
        }
        "Edit" | "Write" => file_tool_target(tool_name, args, project_root)?
            .to_string_lossy()
            .into_owned(),
        _ => args.to_string(),
    };
    Some(format!("{tool_name}\0{subject}"))
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
    fn modes_cycle_order_and_clamp() {
        assert_eq!(TrustMode::Auto.next(), TrustMode::Plan);
        assert_eq!(TrustMode::Plan.next(), TrustMode::Auto);
        assert!(TrustMode::Plan < TrustMode::Auto);
        for (parent, child, want) in [
            (TrustMode::Plan, TrustMode::Auto, TrustMode::Plan),
            (TrustMode::Auto, TrustMode::Plan, TrustMode::Plan),
            (TrustMode::Auto, TrustMode::Auto, TrustMode::Auto),
        ] {
            assert_eq!(
                TrustMode::clamp(parent, child),
                want,
                "{parent:?} {child:?}"
            );
        }
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
    //   `koda-cli/tests/suite/regression_test.rs` catches the call-site
    //   regression. Tests target different layers; both must hold.
    //
    // The naming (`fork_identity`, `named_clamps_down`,
    // `child_already_stricter_passes_through`) documents the
    // architectural cases each call site exercises.

    #[test]
    fn derive_child_trust_fork_identity() {
        // Fork passes (mode, mode) — helper collapses to identity.
        // Documents that the symmetry is preserved.
        for parent in [TrustMode::Plan, TrustMode::Auto] {
            assert_eq!(
                derive_child_trust(parent, parent),
                parent,
                "fork (parent==declared) must return parent verbatim"
            );
        }
    }

    #[test]
    fn derive_child_trust_named_clamps_down() {
        // A named child declaring Auto under a Plan parent is narrowed to
        // Plan; one declaring Plan under Auto stays Plan. Clamp never
        // widens.
        assert_eq!(
            derive_child_trust(TrustMode::Plan, TrustMode::Auto),
            TrustMode::Plan
        );
        assert_eq!(
            derive_child_trust(TrustMode::Auto, TrustMode::Plan),
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
        for a in [TrustMode::Plan, TrustMode::Auto] {
            for b in [TrustMode::Plan, TrustMode::Auto] {
                assert_eq!(derive_child_trust(a, b), derive_child_trust(b, a));
            }
        }
    }

    #[test]
    fn modes_parse_show_and_default_to_auto() {
        assert_eq!(TrustMode::parse("auto"), Some(TrustMode::Auto));
        assert_eq!(TrustMode::parse("Plan"), Some(TrustMode::Plan));
        // Safe is gone, and so are the old aliases.
        for gone in ["safe", "yolo", "confirm", "readonly", "nope"] {
            assert_eq!(TrustMode::parse(gone), None, "{gone}");
        }
        assert_eq!(TrustMode::from(0), TrustMode::Plan);
        assert_eq!(TrustMode::from(1), TrustMode::Auto);
        assert_eq!(TrustMode::from(99), TrustMode::Plan, "fail-safe to Plan");
        assert_eq!(TrustMode::Plan.to_string(), "plan");
        assert_eq!(TrustMode::Auto.to_string(), "auto");
        assert_eq!(TrustMode::default(), TrustMode::Auto);
        let shared = new_shared_trust(TrustMode::Auto);
        assert_eq!(cycle_trust(&shared), TrustMode::Plan);
        assert_eq!(read_trust(&shared), TrustMode::Plan);
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
            for mode in [TrustMode::Plan, TrustMode::Auto] {
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
        for tool in ["Write", "Edit", "MemoryWrite"] {
            assert_eq!(
                check_tool(tool, &serde_json::json!({}), TrustMode::Plan, None),
                ToolApproval::Blocked,
                "{tool} should be blocked in Plan mode"
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
            check_tool("WebSearch", &serde_json::json!({}), TrustMode::Auto, None),
            ToolApproval::AutoApprove,
        );
    }

    /// `WebFetch` asks, as Claude Code's does, except for a preapproved
    /// documentation site: Plan asks, Auto doesn't, and "allow for
    /// session" covers the site.
    #[test]
    fn web_fetch_asks_except_for_documentation_sites() {
        let site = serde_json::json!({"url": "https://example.com/a", "prompt": "q"});
        let docs = serde_json::json!({"url": "https://doc.rust-lang.org/std/", "prompt": "q"});
        assert_eq!(
            check_tool("WebFetch", &site, TrustMode::Plan, None),
            ToolApproval::NeedsConfirmation
        );
        if crate::sandbox::is_available() {
            assert_eq!(
                check_tool("WebFetch", &site, TrustMode::Auto, None),
                ToolApproval::AutoApprove
            );
        }
        for mode in [TrustMode::Plan, TrustMode::Auto] {
            assert_eq!(
                check_tool("WebFetch", &docs, mode, None),
                ToolApproval::AutoApprove,
                "{mode:?}"
            );
        }
        let root = Path::new("/project");
        let key = |url: &str| {
            let args = serde_json::json!({"url": url, "prompt": "q"});
            session_allow_key(
                "WebFetch",
                &args,
                resolve_tool_effect("WebFetch", &args),
                root,
            )
        };
        assert_eq!(
            key("https://Example.com/a"),
            key("http://example.com/b?x=1")
        );
        assert_ne!(key("https://example.com/a"), key("https://example.org/a"));
        assert!(key("https://example.com/a").is_some());
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
            check_tool(
                "Bash",
                &serde_json::json!({"command": "rm -rf build"}),
                TrustMode::Auto,
                None
            ),
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

    /// Leaving the sandbox needs the user's yes whatever the command does:
    /// asked in Auto, refused in Plan and in sub-agents, where no
    /// one can answer.
    #[test]
    fn test_bash_outside_sandbox_always_needs_the_user() {
        let args = serde_json::json!({
            "command": "git ls-remote origin",
            "outside_sandbox": true,
            "reason": "List the remote's branches over SSH",
        });
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, None),
            ToolApproval::NeedsConfirmation
        );
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Plan, None),
            ToolApproval::Blocked
        );
        // The same command in the sandbox is read-only and runs freely.
        let sandboxed = serde_json::json!({"command": "git ls-remote origin"});
        assert_eq!(
            check_tool("Bash", &sandboxed, TrustMode::Auto, None),
            ToolApproval::AutoApprove
        );
    }

    /// gh destructive commands need confirmation in Auto (#518).
    #[test]
    fn test_gh_destructive_needs_confirmation_in_auto() {
        for cmd in [
            "gh pr merge 42 --squash",
            "gh issue delete 42",
            "gh repo delete owner/repo",
        ] {
            let args = serde_json::json!({"command": cmd});
            assert_eq!(
                check_tool("Bash", &args, TrustMode::Auto, None),
                ToolApproval::NeedsConfirmation,
                "{cmd} should need confirmation in Auto mode"
            );
        }
    }

    /// gh mutation commands auto-approve in Auto (#518).
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
        }
    }

    #[test]
    fn test_dangerous_bash_needs_confirmation_in_auto() {
        let args = serde_json::json!({"command": "rm -rf target/"});
        assert_eq!(
            check_tool("Bash", &args, TrustMode::Auto, None),
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
        for mode in [TrustMode::Plan, TrustMode::Auto] {
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

    #[test]
    fn coerce_top_level_does_not_affect_derive_child_trust() {
        // Sanity: the sub-agent dispatch path (`derive_child_trust`)
        // is the LEGITIMATE source of Plan and must remain untouched
        // by the top-level coercion. If a parent in Plan spawns a
        // child, the child stays in Plan — coercion never enters
        // the picture. This test pins that separation so a future
        // "unify all trust-routing" refactor can't accidentally
        // funnel sub-agents through a top-level coercion.
        let child_inherits = derive_child_trust(TrustMode::Plan, TrustMode::Auto);
        assert_eq!(
            child_inherits,
            TrustMode::Plan,
            "sub-agent must keep Plan; coercion is top-level-only"
        );
    }

    // ── Allow for this session ──

    /// A command asking to leave the sandbox may be allowed for the session
    /// by its `prefix_rule`, as Codex's model proposes one: only a plain
    /// command that starts with the prefix, and never one too broad to mean
    /// anything.
    #[test]
    fn an_escalations_prefix_is_offered_only_when_it_fits() {
        let root = Path::new("/home/user/project");
        let key = |command: &str, rule: serde_json::Value| {
            let args = serde_json::json!({
                "command": command, "outside_sandbox": true, "reason": "r", "prefix_rule": rule,
            });
            session_allow_key("Bash", &args, resolve_tool_effect("Bash", &args), root)
        };
        let commit = serde_json::json!(["git", "commit"]);
        assert!(key("git commit -m 'one'", commit.clone()).is_some());
        for (command, rule) in [
            ("git commit -m one", serde_json::json!(["git"])),
            ("git commit -m one", serde_json::json!(["bash"])),
            ("git commit -m one", serde_json::json!([])),
            ("git push origin main", commit.clone()),
            ("git commit -m one && rm -rf /", commit.clone()),
            ("git commit -m \"$(cat msg)\"", commit.clone()),
            ("git commit -m one", serde_json::json!("git commit")),
        ] {
            assert_eq!(key(command, rule.clone()), None, "{command} {rule}");
        }
        // No prefix, no session answer: it asks every time.
        let bare = serde_json::json!({"command": "git commit -m x", "outside_sandbox": true});
        assert_eq!(
            session_allow_key("Bash", &bare, resolve_tool_effect("Bash", &bare), root),
            None
        );
    }

    /// An allowed prefix covers later commands that start with it and ask
    /// to leave the sandbox, but not a compound command, another prefix, or
    /// one dangerous in itself.
    #[test]
    fn an_allowed_prefix_covers_what_starts_with_it_and_nothing_dangerous() {
        let root = Path::new("/home/user/project");
        let allows = SessionAllows::default();
        let call = |command: &str| serde_json::json!({"command": command, "outside_sandbox": true});
        let covers = |command: &str| {
            let args = call(command);
            allows.covers("Bash", &args, resolve_tool_effect("Bash", &args), root)
        };
        assert!(!covers("git commit -m one"), "nothing allowed yet");
        for rule in [
            serde_json::json!(["git", "commit"]),
            serde_json::json!(["git", "push"]),
        ] {
            let mut args = call("git commit -m one");
            args["prefix_rule"] = rule.clone();
            if rule[1] == "push" {
                args["command"] = serde_json::json!("git push origin main");
            }
            let key = session_allow_key("Bash", &args, resolve_tool_effect("Bash", &args), root);
            allows.insert(key.expect("a valid prefix"));
        }
        assert!(covers("git commit -m two"));
        assert!(covers("git commit --amend --no-edit"));
        assert!(covers("git push origin feature"));
        assert!(
            !covers("git push --force origin main"),
            "dangerous in itself"
        );
        assert!(!covers("git checkout main"), "another prefix");
        assert!(
            !covers("git commit -m one && git push"),
            "not a plain command"
        );
        let inside = serde_json::json!({"command": "git commit -m two"});
        assert!(
            !allows.covers("Bash", &inside, resolve_tool_effect("Bash", &inside), root),
            "a prefix is for commands that leave the sandbox"
        );
    }

    fn key(tool: &str, args: serde_json::Value, root: &Path) -> Option<String> {
        let effect = resolve_tool_effect(tool, &args);
        session_allow_key(tool, &args, effect, root)
    }

    #[test]
    fn test_session_allow_is_only_for_local_changes_in_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bash = |command: &str| key("Bash", serde_json::json!({ "command": command }), root);

        assert!(bash("cargo fmt").is_some());
        // Destructive, outside the sandbox, outside the project: never.
        assert_eq!(bash("rm -rf target"), None);
        let unsandboxed = serde_json::json!({
            "command": "cargo fmt", "outside_sandbox": true, "reason": "x",
        });
        assert_eq!(key("Bash", unsandboxed, root), None);
        assert_eq!(bash("cd ~ && touch a"), None);
        assert_eq!(
            key(
                "Edit",
                serde_json::json!({"file_path": "/etc/hosts", "old_string": "a", "new_string": "b"}),
                root
            ),
            None
        );
        // Read-only calls don't ask; remote actions (MCP tools) always do.
        assert_eq!(bash("ls"), None);
        let args = serde_json::json!({"issue": 1});
        assert_eq!(
            session_allow_key("tracker__close", &args, ToolEffect::RemoteAction, root),
            None
        );
    }

    #[test]
    fn test_session_allow_key_is_the_command_or_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bash = |args: serde_json::Value| key("Bash", args, root).unwrap();

        assert_eq!(
            bash(serde_json::json!({"command": "cargo fmt", "timeout": 30})),
            bash(serde_json::json!({"command": " cargo fmt "})),
        );
        assert_ne!(
            bash(serde_json::json!({"command": "cargo fmt"})),
            bash(serde_json::json!({"command": "cargo fmt --all"})),
        );
        assert_ne!(
            bash(serde_json::json!({"command": "cargo fmt"})),
            bash(serde_json::json!({"command": "cargo fmt", "background": true})),
        );

        let edit = |path: &str, new: &str| {
            let args = serde_json::json!({"file_path": path, "old_string": "a", "new_string": new});
            key("Edit", args, root).unwrap()
        };
        assert_eq!(edit("src/a.rs", "b"), edit("src/a.rs", "c"));
        assert_ne!(edit("src/a.rs", "b"), edit("src/b.rs", "b"));
        let write = key(
            "Write",
            serde_json::json!({"file_path": "src/a.rs", "content": "x"}),
            root,
        );
        assert_ne!(write.unwrap(), edit("src/a.rs", "b"));
    }

    /// A file that didn't exist when it was allowed has the same key once it
    /// does: macOS temp dirs are reached through the /var → /private/var link.
    #[test]
    fn test_session_allow_key_survives_the_file_being_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let args = serde_json::json!({"file_path": "new/a.txt", "content": "x"});
        let before = key("Write", args.clone(), root).unwrap();
        std::fs::create_dir_all(root.join("new")).unwrap();
        std::fs::write(root.join("new/a.txt"), "x").unwrap();
        assert_eq!(key("Write", args, root).unwrap(), before);
    }

    #[test]
    fn test_session_allows_remember_keys() {
        let allows = SessionAllows::default();
        assert!(!allows.contains("Bash\0false\0cargo fmt"));
        allows.insert("Bash\0false\0cargo fmt".to_string());
        assert!(allows.contains("Bash\0false\0cargo fmt"));
    }
}
