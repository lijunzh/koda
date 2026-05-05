//! `TurnContext` and `ToolExecutionContext` — typed bundles for the
//! ambient state passed through the inference / tool-dispatch / sub-agent
//! call graph.
//!
//! ## Why
//!
//! Pre-PR-1-of-#1265-item-4, six functions in [`crate::tool_dispatch`]
//! and one in `crate::sub_agent_dispatch` (private module) each took 11–13
//! explicit parameters. Each one carried `#[allow(clippy::too_many_arguments)]`
//! and called the same `(project_root, config, db, session_id, sink,
//! cancel, sub_agent_cache, bg_agents, …)` quartet through every
//! layer. Adding a new ambient field (e.g. a tracing span, a per-turn
//! quota, a feature flag) meant editing every signature.
//!
//! `TurnContext` collects the per-turn ambient context once at the
//! inference-loop entry and threads a single reference through dispatch.
//! `ToolExecutionContext` adds the few fields that are scoped to a
//! single batch of tool calls (`caller_spawner`).
//!
//! ## What stays OUT of TurnContext
//!
//! - **`file_tracker: &mut FileTracker`** — the only mutable piece of
//!   ambient state. Pulling it into `TurnContext` would force a `&mut
//!   TurnContext` everywhere, which conflicts with the parallel-execution
//!   path (the mutation only happens in the post-join `record_tool_result`
//!   loop, not inside the parallel futures themselves). Passing it as a
//!   separate `&mut FileTracker` argument keeps `TurnContext` cheap to
//!   share across borrow boundaries.
//! - **`cmd_rx: &mut mpsc::Receiver<EngineCommand>`** — also `&mut`,
//!   and only relevant to dispatch paths that may issue interactive
//!   approvals (sequential / split-batch). Lives outside the context
//!   for the same reason as `file_tracker`.
//! - **Per-call data** like `tc: &ToolCall`, `result: &str`,
//!   `tool_calls: &[ToolCall]` — these are inputs, not context.
//!
//! ## Lifetimes
//!
//! `TurnContext<'a>` borrows everything by `&'a` reference except
//! `cancel: CancellationToken`, which is internally `Arc`-backed and
//! Clone, so storing by value avoids a third lifetime parameter.
//! `ToolExecutionContext<'a>` borrows the `TurnContext<'a>` and adds
//! per-batch fields with their own (shorter or equal) lifetime.
//!
//! ## Migration policy
//!
//! - **PR-1 of #1265 item 4 (this PR)**: introduce the types only.
//!   Zero callsite migration. Smoke-test via unit tests that construct
//!   one and read its fields.
//! - **PR-2**: migrate `tool_dispatch.rs` (6 functions, 6 allows).
//! - **PR-3**: migrate `sub_agent_dispatch::execute_sub_agent`
//!   (1 function, 1 allow). The owned-args `run_bg_agent` stays
//!   explicit because tokio-spawn requires `'static`.
//! - **Out of scope**: `ChildAgentRegistry::attach` — author has
//!   explicitly opted out (per-call data, not context).

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::child_agent::ChildAgentRegistry;
use crate::config::KodaConfig;
use crate::db::Database;
use crate::engine::EngineSink;
use crate::sub_agent_cache::SubAgentCache;
use crate::tools::ToolRegistry;
use crate::trust::TrustMode;

/// Per-turn ambient context shared across the inference loop, tool
/// dispatch, and sub-agent invocation.
///
/// Constructed once per turn at the inference-loop entry; threaded
/// by `&` reference through dispatch. All fields are immutable
/// borrows or `Clone`-by-value handles — see module docs for why
/// `file_tracker` and `cmd_rx` stay outside.
pub struct TurnContext<'a> {
    /// Project root — used for path safety + workspace scoping.
    pub project_root: &'a Path,

    /// Per-session config (model, caps, model_settings, …).
    pub config: &'a KodaConfig,

    /// SQLite-backed message + tool-result store.
    pub db: &'a Database,

    /// Active session id for `db` writes.
    pub session_id: &'a str,

    /// Engine event sink (TUI / headless / ACP).
    pub sink: &'a dyn EngineSink,

    /// Per-turn child cancel token. Stored by value because
    /// `CancellationToken` is `Arc`-backed and `Clone`. Cloning is
    /// cheap; nested calls clone freely.
    pub cancel: CancellationToken,

    /// Cache of compiled sub-agent specs (#1086 invocation cache).
    pub sub_agent_cache: &'a SubAgentCache,

    /// Background-agent registry — used for `ListBackgroundTasks`,
    /// `CancelTask`, `WaitTask`, and bg-sub-agent reservations.
    pub bg_agents: &'a Arc<ChildAgentRegistry>,

    /// Per-turn trust snapshot. Read once at turn entry; the user
    /// can cycle trust between turns but not mid-turn.
    pub mode: TrustMode,

    /// Tool registry (built-in + MCP). Per-session; constant within
    /// a single turn.
    pub tools: &'a ToolRegistry,
}

impl<'a> TurnContext<'a> {
    /// Construct a per-turn context. All borrows must outlive `'a`;
    /// the typical caller is the inference loop in
    /// [`crate::inference`], which holds these in named locals.
    #[allow(clippy::too_many_arguments)] // 10 args is the WHOLE POINT — this is the
    // bundling site, exactly one allow at the constructor instead of one per consumer.
    pub fn new(
        project_root: &'a Path,
        config: &'a KodaConfig,
        db: &'a Database,
        session_id: &'a str,
        sink: &'a dyn EngineSink,
        cancel: CancellationToken,
        sub_agent_cache: &'a SubAgentCache,
        bg_agents: &'a Arc<ChildAgentRegistry>,
        mode: TrustMode,
        tools: &'a ToolRegistry,
    ) -> Self {
        Self {
            project_root,
            config,
            db,
            session_id,
            sink,
            cancel,
            sub_agent_cache,
            bg_agents,
            mode,
            tools,
        }
    }
}

/// Per-tool-batch execution context.
///
/// Adds fields scoped narrower than a turn — currently just
/// `caller_spawner`, but a likely home for future per-batch state
/// (tracing span, batch id, retry counter, …).
///
/// Constructed by the dispatch-entry function (`execute_tools_*`) and
/// passed by reference to inner helpers (`execute_one_tool`,
/// `validate_then_execute_one_tool`).
#[derive(Clone, Copy)]
pub struct ToolExecutionContext<'a> {
    /// Borrowed turn context. The narrower lifetime annotation lets
    /// the batch context outlive specific tool-call iterations
    /// without forcing the turn context to.
    pub turn: &'a TurnContext<'a>,

    /// Spawner identity for cleanup routing (Phase E of #996). `None`
    /// at top-level inference (no parent invocation). `Some(id)` when
    /// invoked from inside a sub-agent — bg-task tools tag their
    /// reservations with `spawner = caller_spawner` so the parent
    /// owns the right to cancel/wait its bg children.
    pub caller_spawner: Option<u32>,
}

impl<'a> ToolExecutionContext<'a> {
    /// Construct a per-batch execution context.
    pub fn new(turn: &'a TurnContext<'a>, caller_spawner: Option<u32>) -> Self {
        Self {
            turn,
            caller_spawner,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests: construct each context with stub-y values and read
    //! every field. The point isn't to test behavior (there is none —
    //! these are dumb data bundles); it's to pin the public shape so a
    //! future PR can't accidentally widen / narrow / reshape the type
    //! without updating callers.
    //!
    //! Real call-graph migration arrives in PR-2 (tool_dispatch) and
    //! PR-3 (sub_agent_dispatch); those will exercise the contexts
    //! through the existing 2594-test workspace battery.
    //!
    //! Note: we use `MemDb::new()` and an inline `EngineSink` impl to
    //! avoid pulling in heavy fixtures for what is genuinely a
    //! shape-only test.
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;

    use crate::config::ProviderType;
    use crate::engine::EngineEvent;
    use crate::trust::TrustMode;

    struct NullSink;
    impl EngineSink for NullSink {
        fn emit(&self, _event: EngineEvent) {}
    }

    /// Build the heaviest fixtures (db + tools) once per test. Kept
    /// inline rather than promoted to `koda-test-utils` because
    /// turn_context's tests are intentionally minimal — the call
    /// graph migration in PR-2/PR-3 will exercise these types
    /// through the real workspace battery.
    async fn fixtures() -> (TempDir, crate::config::KodaConfig, crate::db::Database) {
        let dir = TempDir::new().expect("tempdir");
        let config = crate::config::KodaConfig::default_for_testing(ProviderType::Mock);
        let db = crate::db::Database::open(&dir.path().join("turn_ctx.db"))
            .await
            .expect("open db");
        (dir, config, db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turn_context_holds_all_fields() {
        // Arrange: minimum-viable stand-ins for every TurnContext field.
        let (dir, config, db) = fixtures().await;
        let root: PathBuf = dir.path().to_path_buf();
        let cancel = CancellationToken::new();
        let cache = SubAgentCache::new();
        let bg_agents = Arc::new(ChildAgentRegistry::new());
        let tools = ToolRegistry::new(root.clone(), 8_000);
        let sink = NullSink;

        // Act: build the context.
        let ctx = TurnContext::new(
            &root,
            &config,
            &db,
            "session-smoke",
            &sink,
            cancel.clone(),
            &cache,
            &bg_agents,
            TrustMode::Safe,
            &tools,
        );

        // Assert: every field is reachable + carries the value we passed.
        assert_eq!(ctx.project_root, root.as_path());
        assert_eq!(ctx.session_id, "session-smoke");
        assert!(matches!(ctx.mode, TrustMode::Safe));
        assert!(!ctx.cancel.is_cancelled());
        // sub_agent_cache + bg_agents + db + config + tools + sink are
        // pointer-equal to the inputs — checked transitively by
        // construction (no copy / move happens for &-borrowed fields).
        // Pointer equality on the heap-allocated Arc:
        assert!(Arc::ptr_eq(ctx.bg_agents, &bg_agents));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_execution_context_borrows_turn() {
        let (dir, config, db) = fixtures().await;
        let root: PathBuf = dir.path().to_path_buf();
        let cache = SubAgentCache::new();
        let bg_agents = Arc::new(ChildAgentRegistry::new());
        let tools = ToolRegistry::new(root.clone(), 8_000);
        let sink = NullSink;

        let turn = TurnContext::new(
            &root,
            &config,
            &db,
            "session-tx",
            &sink,
            CancellationToken::new(),
            &cache,
            &bg_agents,
            TrustMode::Auto,
            &tools,
        );

        // Top-level invocation: no parent spawner.
        let tx = ToolExecutionContext::new(&turn, None);
        assert!(tx.caller_spawner.is_none());
        assert_eq!(tx.turn.session_id, "session-tx");
        assert!(matches!(tx.turn.mode, TrustMode::Auto));

        // Nested invocation (e.g. inside a sub-agent): explicit spawner.
        let nested = ToolExecutionContext::new(&turn, Some(42));
        assert_eq!(nested.caller_spawner, Some(42));
        // Same turn context underneath — sanity check the borrow.
        assert_eq!(nested.turn.session_id, "session-tx");
    }

    #[test]
    fn tool_execution_context_is_copy() {
        // Cheap to pass by value through inner dispatch helpers; this
        // pins the trait so a future field addition doesn't silently
        // remove `Copy` and force callers to clone.
        fn assert_copy<T: Copy>() {}
        assert_copy::<ToolExecutionContext<'_>>();
    }
}
