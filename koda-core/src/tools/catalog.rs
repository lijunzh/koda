//! Read-only tool metadata catalog (#1265 item 5, PR-1/N).
//!
//! # Why this module exists
//!
//! Pre-#1265, [`crate::tools::ToolRegistry`] was a 1662-LOC god-object
//! that owned everything tool-related: built-in definitions, the
//! filesystem trait object, the undo stack, the file-read cache, the
//! database/session handles, the skill registry, output caps, the
//! background-process registry, trust mode, sandbox policy, the MCP
//! manager handle, and per-session proxy ports — *plus* every tool's
//! execution body in a single 400-line `match`.
//!
//! That mass smelled bad in three concrete ways:
//!
//! 1. **Read-vs-write coupling.** Pure read-only callers (tests that
//!    just want a definition list, the engine's prompt-builder asking
//!    "what tools exist?") had to construct a full registry with FS,
//!    proxy, MCP, and undo state they'd never touch.
//! 2. **Over-broad lock surface.** Several `RwLock<...>` slots
//!    (`mcp_manager`, `proxy_port`, `socks5_port`, `db`, `session_id`)
//!    coexisted on the same struct so a contention bug in one field's
//!    consumer was impossible to scope to that consumer.
//! 3. **Test fixtures had to mock everything.** Wiring tests for "is
//!    every built-in tool registered?" pulled in skill discovery, FS
//!    initialization, and undo-stack construction.
//!
//! # What this PR does
//!
//! Introduces [`ToolCatalog`] — a focused type that owns *only* the
//! read-only metadata side of the registry:
//!
//! - The map of built-in [`ToolDefinition`]s, populated once at
//!   construction by aggregating each tool sub-module's
//!   `definitions()` function.
//! - The MCP manager handle (a hot-pluggable slot, populated after
//!   MCP servers connect).
//! - Methods that read those two things: name lookup, allowlist /
//!   denylist filtering, MCP-aware effect classification.
//!
//! [`ToolRegistry`] now *composes* one of these via a `catalog` field
//! and delegates the corresponding methods. The public API of
//! `ToolRegistry` is byte-for-byte unchanged in this PR; behavior is
//! preserved exactly. Subsequent PRs in the stack will migrate
//! callers that don't need a full registry to use `ToolCatalog`
//! directly, shrinking `ToolRegistry`'s blast radius incrementally.
//!
//! # Why this PR is types-only
//!
//! Same playbook as the TurnContext stack (#1287 → #1288 → #1290):
//! introducing the type with full delegation in PR-1 means CI proves
//! "no behavior changed" before any caller migration starts. If
//! something here is wrong, the broken test is in the catalog
//! module, not in 30 unrelated call sites.
//!
//! [`ToolRegistry`]: crate::tools::ToolRegistry
//! [`ToolDefinition`]: crate::types::ToolDefinition

use crate::providers::ToolDefinition;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::{
    ToolEffect, agent, ask_user, bg_task_tools, classify_tool, file_tools, glob_tool, grep, memory,
    recall, shell, skill_tools, todo, web_fetch, web_search,
};

/// The read-only metadata side of [`crate::tools::ToolRegistry`].
///
/// Owns the built-in tool definition map and the MCP manager slot.
/// Does **not** own filesystem state, caches, undo, session/DB
/// handles, or proxy ports — those stay on `ToolRegistry` because
/// they're either mutable per-turn or only meaningful during
/// execution.
///
/// ## Threading model
///
/// `definitions` is immutable after `new()` so requires no
/// synchronization. `mcp_manager` is a `std::sync::RwLock` slot so
/// late attachment (after MCP servers connect) doesn't require
/// `&mut self`. Read-side contention is negligible because writers
/// only fire at MCP-server-lifecycle boundaries (connect / refresh /
/// disconnect), not on every tool invocation.
///
/// ## Construction cost
///
/// `new()` walks every per-tool `definitions()` function and inserts
/// the results into a `HashMap`. That's O(builtin_tool_count) and
/// happens once per registry creation. Cheap enough that we don't
/// bother lazy-initializing.
pub struct ToolCatalog {
    /// All built-in tool definitions, keyed by tool name. Populated
    /// at construction time by [`Self::new`]; never mutated.
    definitions: HashMap<String, ToolDefinition>,

    /// Hot-pluggable MCP manager handle. `None` until
    /// [`Self::set_mcp_manager`] is called by `KodaSession::new`
    /// after MCP servers have connected. The outer `RwLock` allows
    /// late attachment without `&mut self`; the inner
    /// `tokio::sync::RwLock` is the manager's own concurrency
    /// primitive (it serves async readers from tool dispatch).
    mcp_manager: RwLock<Option<Arc<tokio::sync::RwLock<crate::mcp::McpManager>>>>,
}

impl ToolCatalog {
    /// Build a fresh catalog containing every built-in tool's
    /// definitions. The MCP slot starts empty and is filled later via
    /// [`Self::set_mcp_manager`].
    ///
    /// Definition aggregation pattern: each tool sub-module exposes a
    /// `definitions() -> Vec<ToolDefinition>` (or `definition() ->
    /// ToolDefinition` for single-def modules like `recall`) — this
    /// method walks them all and inserts into the map. Adding a new
    /// built-in tool means: define the function in your sub-module,
    /// then add one `for def in mymod::definitions() { ... }` block
    /// here. The audit's acceptance criterion ("adding a simple
    /// built-in tool requires one module plus one registration line")
    /// is partially met by the existing per-module `definitions()`
    /// pattern; this PR doesn't change that surface.
    pub fn new() -> Self {
        let mut definitions = HashMap::new();

        // Register all built-in tools. Order doesn't matter — we
        // insert into a HashMap and the LLM never sees the map's
        // iteration order (callers that need stability sort the
        // result, e.g. `all_builtin_tool_names`).
        for def in file_tools::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in grep::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in shell::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in agent::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in bg_task_tools::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in ask_user::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in glob_tool::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in web_fetch::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in web_search::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in todo::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in memory::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        for def in skill_tools::definitions() {
            definitions.insert(def.name.clone(), def);
        }
        // RecallContext is a singleton (not a `Vec`) — it has a
        // single canonical definition and the sub-module reflects
        // that with `definition()` instead of `definitions()`.
        let recall_def = recall::definition();
        definitions.insert(recall_def.name.clone(), recall_def);

        Self {
            definitions,
            mcp_manager: RwLock::new(None),
        }
    }

    /// Attach an MCP connection manager. Called once per session,
    /// after MCP servers have connected and discovered their tools.
    ///
    /// Lock-poisoning policy: if the inner `RwLock` is poisoned we
    /// silently keep the previous value. Matches the precedent set
    /// by the pre-#1265 `set_mcp_manager` on `ToolRegistry` — a
    /// poisoned lock means another thread already panicked, and
    /// piling on with our own panic just makes the ultimate
    /// diagnosis harder.
    pub fn set_mcp_manager(&self, manager: Arc<tokio::sync::RwLock<crate::mcp::McpManager>>) {
        if let Ok(mut guard) = self.mcp_manager.write() {
            *guard = Some(manager);
        }
    }

    /// Read the currently-attached MCP manager, if any. Returns a
    /// cloned `Arc` so callers can hold it across `.await` points
    /// without keeping the catalog's `RwLock` read guard live.
    pub fn mcp_manager(&self) -> Option<Arc<tokio::sync::RwLock<crate::mcp::McpManager>>> {
        self.mcp_manager.read().ok().and_then(|g| g.clone())
    }

    /// Classify a tool into its [`ToolEffect`], using MCP annotations
    /// when available.
    ///
    /// - **Built-in tools** delegate to the free function
    ///   [`classify_tool`].
    /// - **MCP tools** look up cached annotations on the manager.
    /// - If we *think* a name is an MCP tool but the manager isn't
    ///   attached or its lock is contended, we fall back to
    ///   [`ToolEffect::RemoteAction`] — a defensible "side effects
    ///   somewhere remote" guess that errs toward asking for approval.
    pub fn classify_tool_with_mcp(&self, name: &str) -> ToolEffect {
        if crate::mcp::is_mcp_tool_name(name) {
            if let Some(mgr) = self.mcp_manager()
                && let Ok(mgr) = mgr.try_read()
            {
                return mgr.classify_tool(name);
            }
            // Fallback: no manager or lock contention.
            return ToolEffect::RemoteAction;
        }
        classify_tool(name)
    }

    /// Sorted list of every registered built-in tool name. Used by
    /// wiring tests to verify every tool sub-module is plumbed in.
    /// Sort is stable+lexicographic so test snapshots don't churn.
    pub fn all_builtin_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.definitions.keys().cloned().collect();
        names.sort();
        names
    }

    /// Whether a name maps to a known built-in tool. Does **not**
    /// consult MCP — call sites that want MCP awareness should also
    /// check `mcp_manager().map(|m| m.try_read().has_tool(name))` or
    /// rely on [`Self::get_definitions`] which merges both sources.
    pub fn has_tool(&self, name: &str) -> bool {
        self.definitions.contains_key(name)
    }

    /// Filtered view of all tool definitions (built-ins + MCP).
    ///
    /// Filter semantics — preserved verbatim from the pre-#1265
    /// `ToolRegistry::get_definitions` so the model's tool-listing
    /// behavior is byte-identical:
    ///
    /// - `allowed` non-empty → only those tools (allowlist mode).
    /// - `denied` non-empty → all tools except those (denylist mode).
    /// - Both empty → all tools.
    /// - If both are specified, allowlist wins (deny ignored).
    ///
    /// MCP tools are appended after built-ins. Within each group,
    /// iteration order is `HashMap` order (unspecified) — deliberate:
    /// callers that need a sorted list must sort, and most LLM
    /// providers don't care about order.
    pub fn get_definitions(&self, allowed: &[String], denied: &[String]) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = if !allowed.is_empty() {
            allowed
                .iter()
                .filter_map(|name| self.definitions.get(name).cloned())
                .collect()
        } else if !denied.is_empty() {
            self.definitions
                .values()
                .filter(|d| !denied.contains(&d.name))
                .cloned()
                .collect()
        } else {
            self.definitions.values().cloned().collect()
        };

        // Append MCP tool definitions, applying the same filter
        // semantics. `try_read` (not `read`) is intentional: if a
        // writer is mid-update we'd rather return the built-ins
        // immediately than block the LLM's prompt-build path.
        if let Some(mgr) = self.mcp_manager()
            && let Ok(mgr) = mgr.try_read()
        {
            let mcp_defs = mgr.all_tool_definitions();
            if !allowed.is_empty() {
                for def in mcp_defs {
                    if allowed.contains(&def.name) {
                        defs.push(def);
                    }
                }
            } else if !denied.is_empty() {
                for def in mcp_defs {
                    if !denied.contains(&def.name) {
                        defs.push(def);
                    }
                }
            } else {
                defs.extend(mcp_defs);
            }
        }

        defs
    }
}

impl Default for ToolCatalog {
    /// `Default` is a one-liner over `new()` — exists only because
    /// clippy::new_without_default would otherwise nag. Construction
    /// is non-trivial (per-tool definitions walk) but observably
    /// stateless, so a `Default` impl is honest.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! Catalog-only tests. Construct a `ToolCatalog` in isolation
    //! (no FS, no project root, no undo stack, no caps) — the whole
    //! point of this PR is that you *can*. Pre-#1265 these tests
    //! had to build a full `ToolRegistry`.
    //!
    //! These tests exercise the *invariants* of the catalog (every
    //! built-in registers, filter modes work, MCP slot is a valid
    //! lifecycle). Wiring tests that prove "every tool actually
    //! routes through dispatch" stay in their existing locations
    //! (`koda-core/tests/tool_wiring_test.rs`,
    //! `tool_normalize_test.rs`) because those touch ToolRegistry.

    use super::*;

    #[test]
    fn new_registers_every_builtin_tool() {
        let catalog = ToolCatalog::new();
        let names = catalog.all_builtin_tool_names();
        // Spot-check a representative sample from each sub-module.
        // The exhaustive list lives in `tool_wiring_test.rs`; here
        // we just want to prove the aggregation walked every module.
        for expected in [
            "Read",
            "Write",
            "Edit",
            "Delete",
            "List",
            "Grep",
            "Glob",
            "Bash",
            "InvokeAgent",
            "ListBackgroundTasks",
            "CancelTask",
            "WaitTask",
            "AskUser",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "MemoryRead",
            "MemoryWrite",
            "ListSkills",
            "ActivateSkill",
            "RecallContext",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing built-in tool {expected:?} (got {names:?})"
            );
        }
    }

    #[test]
    fn all_builtin_tool_names_returns_sorted() {
        let names = ToolCatalog::new().all_builtin_tool_names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "names must be sorted for stable test snapshots"
        );
    }

    #[test]
    fn has_tool_matches_builtin_set() {
        let catalog = ToolCatalog::new();
        assert!(catalog.has_tool("Read"), "Read must be registered");
        assert!(catalog.has_tool("Bash"), "Bash must be registered");
        assert!(!catalog.has_tool("definitely_not_a_real_tool"));
    }

    #[test]
    fn get_definitions_no_filter_returns_all() {
        let catalog = ToolCatalog::new();
        let defs = catalog.get_definitions(&[], &[]);
        let names: std::collections::HashSet<_> = defs.iter().map(|d| d.name.clone()).collect();
        // Every name in the all-list must appear in the no-filter
        // get_definitions result. (The reverse holds by definition.)
        for name in catalog.all_builtin_tool_names() {
            assert!(names.contains(&name), "missing {name} in no-filter result");
        }
    }

    #[test]
    fn get_definitions_allowlist_only_returns_allowed() {
        let catalog = ToolCatalog::new();
        let defs = catalog.get_definitions(&["Read".to_string(), "Write".to_string()], &[]);
        let names: Vec<_> = defs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Read".to_string()));
        assert!(names.contains(&"Write".to_string()));
    }

    #[test]
    fn get_definitions_denylist_excludes_denied() {
        let catalog = ToolCatalog::new();
        let defs = catalog.get_definitions(&[], &["Bash".to_string()]);
        let names: std::collections::HashSet<_> = defs.iter().map(|d| d.name.clone()).collect();
        assert!(!names.contains("Bash"), "Bash should be filtered out");
        assert!(names.contains("Read"), "Read should still be present");
    }

    #[test]
    fn get_definitions_allowlist_wins_over_denylist() {
        // Verbatim behavior preservation — pre-#1265 the same precedence
        // applied in `ToolRegistry::get_definitions`. Documenting it as
        // a test means a future drift will fail loudly.
        let catalog = ToolCatalog::new();
        let defs = catalog.get_definitions(
            &["Read".to_string()], // allowlist: just Read
            &["Read".to_string()], // denylist: also says no Read
        );
        let names: Vec<_> = defs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, vec!["Read".to_string()], "allowlist must win");
    }

    #[test]
    fn classify_tool_with_mcp_falls_back_for_builtins() {
        let catalog = ToolCatalog::new();
        // Built-ins go through the free `classify_tool` function.
        // We just verify the wrapper preserves that behavior for a
        // representative sample.
        assert_eq!(catalog.classify_tool_with_mcp("Read"), ToolEffect::ReadOnly);
        assert_eq!(
            catalog.classify_tool_with_mcp("Write"),
            ToolEffect::LocalMutation
        );
        assert_eq!(
            catalog.classify_tool_with_mcp("Delete"),
            ToolEffect::Destructive
        );
    }

    #[test]
    fn classify_tool_with_mcp_unknown_mcp_returns_remote_action() {
        // No MCP manager attached → MCP-named tool falls back to
        // RemoteAction (defensible "asks approval" default).
        let catalog = ToolCatalog::new();
        // `__` is the MCP qualifier separator; see `mcp::is_mcp_tool_name`.
        let effect = catalog.classify_tool_with_mcp("someserver__sometool");
        assert_eq!(effect, ToolEffect::RemoteAction);
    }

    #[test]
    fn mcp_manager_starts_empty() {
        let catalog = ToolCatalog::new();
        assert!(catalog.mcp_manager().is_none());
    }
}
