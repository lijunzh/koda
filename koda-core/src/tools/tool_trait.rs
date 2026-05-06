//! The `Tool` trait and per-invocation execution context (#1265 item 5, PR-3/N).
//!
//! # Why this module exists
//!
//! Pre-#1265, every built-in tool's behavior was scattered across **four**
//! places in the codebase:
//!
//! 1. `tools/<toolname>.rs` — definition + execution function.
//! 2. `tools::classify_tool(name)` — a giant `match` arm assigning the tool's
//!    [`ToolEffect`] (read-only / local mutation / destructive).
//! 3. `tools::ToolRegistry::execute()` — a 270-line `match` arm dispatching
//!    to the execution function.
//! 4. `undo::is_mutating_tool(name)` + `undo::extract_file_path(name, args)`
//!    — *separate* `match` arms for undo-snapshot eligibility and the path
//!    to snapshot.
//!
//! Adding a single mutating tool meant editing four files. Worse, the two
//! `is_mutating_tool` lists in `tools::` and `undo::` could (and did)
//! drift out of sync — a real bug class.
//!
//! Every reference codebase we surveyed (codex's `ToolHandler`, claude_code's
//! `Tool<Input, Output>` interface, gemini-cli's `DeclarativeTool` class)
//! solves this with a single trait/interface where each tool is a
//! self-contained type. This PR is the koda equivalent.
//!
//! # What this PR does
//!
//! Introduces two types — and migrates **zero** existing tools. The
//! migration is deliberately deferred to PR-4..PR-N so each cohort of
//! tools moves over in a small reviewable diff with its own CI proof.
//!
//! - [`Tool`] — the trait every built-in tool will eventually implement.
//!   Owns the tool's name, schema, classification, undo behavior, and
//!   execution. MCP tools intentionally don't implement this — they
//!   route through [`crate::mcp::McpManager`] via a separate dispatch
//!   path because their schema/behavior is server-defined at runtime.
//!
//! - [`ToolExecCtx`] — the per-invocation context bundle. Holds borrows
//!   to the bits of [`ToolRegistry`] state a tool's `execute()` needs.
//!   Starts intentionally small (only the fields the seam-test tools
//!   need). Migration PRs add fields as required by the tool being
//!   moved over. After all migrations land, a cleanup PR removes the
//!   now-orphaned fields from `ToolRegistry`.
//!
//! # Why it's types-only this PR
//!
//! Same playbook as the TurnContext stack (#1287/#1288/#1290) and the
//! ToolCatalog stack (#1292/#1293):
//!
//! 1. **Land the seam.** New trait + context, exhaustively documented
//!    + tested in isolation. Behavior of the existing dispatch is
//!    byte-for-byte unchanged.
//! 2. **Migrate cohorts.** Each PR moves one tool family
//!    (file ops, search, bash, etc.) onto the trait. Per-PR the
//!    central `match` shrinks; per-PR CI proves no regressions.
//! 3. **Delete dead code.** Final cleanup PR removes `classify_tool`,
//!    `is_mutating_tool` (both copies), `extract_file_path`, and the
//!    central dispatch `match` — replaced by trait dispatch through
//!    [`ToolCatalog`].
//!
//! If something here is wrong, the broken test is in this module —
//! not in 25 unrelated migration PRs.
//!
//! # Why the defensive `classify()` default is `LocalMutation`
//!
//! New tools added without overriding `classify()` will require user
//! approval before running. That's the safe failure mode: an
//! accidentally-permissive default could ship a destructive tool that
//! bypasses approval. An accidentally-restrictive default just
//! prompts the user. We optimize for the former being impossible.
//!
//! # On the ergonomic cost
//!
//! Yes, `#[async_trait]` adds a `Pin<Box<dyn Future>>` wrapper per
//! call. For the koda dispatch path that means one extra allocation
//! per tool invocation (typically a few per turn) — negligible
//! compared to LLM round-trip latency. We've already accepted the
//! same overhead in [`crate::providers::LlmProvider`] so the
//! precedent is set.

use crate::providers::ToolDefinition;
use crate::tools::ToolEffect;
use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;

/// Result returned by every built-in tool's execution path.
///
/// Re-exported here for trait users — the canonical definition lives in
/// [`crate::tools`] (it's the same `ToolResult` already returned by
/// [`crate::tools::ToolRegistry::execute`]). Defining the trait against
/// this exact type means migration PRs don't have to translate result
/// shapes when moving each tool over.
pub use crate::tools::ToolResult;

/// Per-invocation execution context for a [`Tool`].
///
/// # Lifetime
///
/// All fields are borrows — the context is constructed fresh per call by
/// [`crate::tools::ToolRegistry`] and lives for the duration of the
/// `tool.execute(...)` future. No fields require ownership transfer or
/// `Arc`-cloning at the call site.
///
/// # Growth strategy
///
/// This struct **starts intentionally small.** The fields below are
/// the minimum needed to execute the test tools in this module's
/// `#[cfg(test)]` block. Migration PRs (PR-4..PR-N) add fields as the
/// tool they're migrating actually needs them — never speculatively.
/// This forces the struct's surface to be pulled, not pushed, by real
/// migration needs. Without that discipline `ToolExecCtx` would
/// become a god-bag wearing a different name (the very smell we're
/// trying to escape).
///
/// **Don't add a field here without a tool that uses it in the same
/// PR.** YAGNI.
///
/// # Why borrows, not owned `Arc`s
///
/// Every field a tool reads is already owned by the surrounding
/// [`crate::tools::ToolRegistry`] (or its embedded
/// [`crate::tools::ToolCatalog`]). Re-cloning `Arc`s into the context
/// would be pure ceremony — the `'a` lifetime is plenty. The exception
/// is fields that are themselves `Arc<Mutex<...>>` shared-state
/// handles (e.g. the file-read cache, the bg-process registry); those
/// are passed as `&Arc<...>` so tools can clone-and-await across
/// `.await` points if they need to.
pub struct ToolExecCtx<'a> {
    /// Project root the tool should resolve relative paths against.
    /// Same value [`crate::tools::ToolRegistry::execute`] passes to
    /// every per-tool function today.
    pub project_root: &'a std::path::Path,

    /// File-read cache shared with [`crate::tools::ToolRegistry`].
    /// Held as `&Arc<...>` (rather than `&Mutex<...>` directly)
    /// because file tools sometimes need to clone the handle to
    /// keep it live across an `.await`. Pre-#1265 this is exactly
    /// what `file_tools::read_file` and `file_tools::edit_file` do.
    pub read_cache: &'a crate::tools::FileReadCache,

    /// Filesystem trait object — `LocalFileSystem` in production,
    /// a fake in tests. Lives behind `Arc<dyn FileSystem>` on the
    /// registry; tools only ever need a `&dyn` reference.
    pub fs: &'a dyn koda_sandbox::fs::FileSystem,

    /// Output caps — token-budget-derived limits for tool output
    /// (e.g. `caps.list_entries` for `List`, `caps.grep_matches` for
    /// `Grep`). Added in PR-4 of #1265 item 5 because `ListTool`
    /// needs `caps.list_entries`. Future migrations (Grep, Glob,
    /// Bash, WebFetch) will read other fields off this same handle
    /// — no need to add separate accessors per tool.
    pub caps: &'a crate::output_caps::OutputCaps,

    /// Optional engine sink + call-id pair for tools that stream
    /// progress events to the UI (e.g. `Bash` shell output,
    /// `TodoWrite` todo updates). `None` means "no streaming
    /// requested" — non-streaming tools should ignore this.
    pub sink: Option<(&'a dyn crate::engine::EngineSink, &'a str)>,

    /// Phase E of #996 — opaque caller spawner id, threaded through
    /// to `Bash` so background-shell entries are tagged with the
    /// invoking agent's id. Every other tool ignores this. Top-
    /// level callers pass `None`.
    pub caller_spawner: Option<u32>,

    // ---- Bash-specific fields, added in PR-6 of #1265 item 5 ----
    //
    // These five fields are read exclusively by `BashTool::execute`.
    // They live on the context (rather than e.g. a separate
    // `BashContext`) because:
    //
    //   1. Adding a separate context type would mean either two
    //      `execute(...)` signatures on the trait (one per ctx type),
    //      or a wrapper enum — both worse than five `Option`-free
    //      borrows on the existing struct.
    //   2. Other tools migrating in PR-7/PR-8 (notably `WebFetch`)
    //      will probably need at least `proxy_port` too. Adding it
    //      here once is cheaper than threading it through a new
    //      type later.
    //
    // The growth-by-pull discipline still holds: every field is
    // pulled in by a concrete tool that needs it in this same PR.
    /// Background-process registry, owned by `ToolRegistry`. `Bash`
    /// uses it to spawn `background: true` shells whose lifecycle
    /// outlives a single tool call. No other built-in touches it.
    pub bg_registry: &'a crate::tools::bg_process::BgRegistry,

    /// Trust mode — determines sandbox configuration. Read by
    /// `BashTool::execute`; future tools that gate behavior on
    /// trust (none today) would read it here.
    pub trust: &'a crate::trust::TrustMode,

    /// Sandbox policy resolved at registry construction time, with
    /// optional per-`with_sandbox_policy` override. `BashTool`
    /// passes this to the shell runner so sub-agent invocations
    /// inherit the right confinement.
    pub sandbox_policy: &'a koda_sandbox::SandboxPolicy,

    /// HTTPS proxy port for outbound network access. `None` means
    /// "no proxy configured". `Bash` honors it; `WebFetch` will
    /// honor it when it migrates in PR-7.
    pub proxy_port: Option<u16>,

    /// SOCKS5 proxy port. Same shape and consumers as `proxy_port`.
    pub socks5_port: Option<u16>,

    /// Optional session handle — (`Database`, `session_id`) pair for
    /// tools that read/write per-session state. `None` means "no
    /// active session" — tools that need it should return a
    /// graceful 'requires an active session' message in that case.
    ///
    /// Added in PR-7 of #1265 item 5 because `TodoWrite`,
    /// `RecallContext` (and the bg-task tools, in PR-8) all need
    /// the session pair. Pre-#1265 the dispatch arms snapshotted
    /// these from `RwLock`s on the registry; the fast path now
    /// does the same snapshot once and threads borrows through
    /// the context.
    pub session: Option<(&'a crate::db::Database, &'a str)>,
}

/// A self-contained built-in tool.
///
/// Each implementor is a unit struct (or near-unit; some tools like
/// `BashTool` may carry small pre-computed config) that owns its name,
/// JSON schema, effect classification, undo behavior, and execution.
///
/// # Implementing
///
/// Minimal implementation:
///
/// ```ignore
/// pub struct ReadTool;
///
/// #[async_trait::async_trait]
/// impl Tool for ReadTool {
///     fn name(&self) -> &'static str { "Read" }
///     fn definition(&self) -> ToolDefinition { /* schema */ }
///     fn classify(&self, _args: &Value) -> ToolEffect { ToolEffect::ReadOnly }
///     async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
///         crate::tools::file_tools::read_file(
///             ctx.project_root, args, ctx.read_cache, ctx.fs,
///         ).await
///     }
/// }
/// ```
///
/// `extract_undo_path` and `classify` have safe defaults: tools that
/// don't override them are treated as mutating-but-untracked-in-undo.
/// That's the same conservative treatment unknown tools get from
/// today's [`crate::tools::classify_tool`].
///
/// # Why not `dyn` everywhere
///
/// We use `Box<dyn Tool>` only at the registry's storage boundary —
/// tools call into each other (when they do at all) via concrete
/// types. The trait-object overhead is paid once per dispatch, not
/// per intra-tool function call.
///
/// # Why MCP tools don't implement this
///
/// MCP tool schemas are discovered at runtime from the MCP server's
/// `tools/list` response — there's no Rust struct to hang an `impl
/// Tool` on. They route through [`crate::mcp::McpManager::call_tool`]
/// via the existing dispatch path, which stays the way it is.
/// Co-locating MCP and built-in dispatch in a single trait would
/// force one of them to lie about its shape; keeping them parallel
/// is honest.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's canonical name. Must match the `name` field of
    /// [`Self::definition`]. Returned as `&'static str` because every
    /// built-in tool name is a string literal — boxing it into `String`
    /// would be pure ceremony.
    fn name(&self) -> &'static str;

    /// The tool's full schema — name, description, JSON-Schema
    /// parameters. This is what the LLM provider sees. Returned by
    /// value (not `&'static`) because some tool definitions are
    /// constructed at runtime from sub-component metadata (e.g.
    /// `ListAgents`'s description embeds the project's discovered
    /// agent list).
    fn definition(&self) -> ToolDefinition;

    /// Classify the *effect* of executing this tool with the given
    /// arguments. Default is [`ToolEffect::LocalMutation`] — the
    /// **defensive** choice that requires user approval. Tools must
    /// explicitly override to declare themselves [`ToolEffect::ReadOnly`]
    /// or [`ToolEffect::Destructive`].
    ///
    /// Input-aware classification (per-call rather than per-tool) is
    /// the whole point of taking `args` here. `BashTool::classify`
    /// will eventually delegate to
    /// [`crate::bash_safety::classify_bash_command`] so `rm -rf /`
    /// classifies as `Destructive` while `echo hi` is `ReadOnly`.
    /// Today, classification happens *after* dispatch via a separate
    /// match — this trait fixes that asymmetry.
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::LocalMutation
    }

    /// If this tool is about to mutate a file, return the path that
    /// should be snapshotted into the undo stack *before* execution.
    /// `None` means "don't snapshot" — the default for read-only
    /// tools and for mutating tools whose target path can't be
    /// statically extracted from the arguments (e.g. `Bash`).
    ///
    /// Today this logic lives in `undo::extract_file_path` as a
    /// separate `match` on tool name; migration moves it onto the
    /// implementing struct so adding a mutating tool can't forget
    /// to wire its undo behavior.
    fn extract_undo_path(&self, _args: &Value) -> Option<PathBuf> {
        None
    }

    /// Execute the tool. The post-execution machinery (last-writer
    /// recording, last-bash recording, full-output handling) stays
    /// in [`crate::tools::ToolRegistry::execute`] — implementors only
    /// need to do the work and return a `ToolResult`.
    ///
    /// Cancellation: callers (typically [`crate::tools::ToolRegistry::execute`])
    /// are responsible for racing this future against any cancel
    /// token. Tools shouldn't poll cancel state themselves unless
    /// they spawn long-running sub-processes (Bash does this via
    /// the `bg_registry`).
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult;
}

/// Boxed trait object for storage in the registry.
///
/// Defined as a type alias rather than re-typed at every use site
/// because the `Box<dyn Tool>` type appears in several places once
/// migrations begin (catalog map, registration helpers, test
/// fixtures). Centralizing the alias means any future change (e.g.
/// `Arc<dyn Tool>` for cheap cloning) is a one-line edit.
pub type DynTool = Box<dyn Tool>;

/// Convenience constructor for [`DynTool`] from any `Tool` value.
/// Avoids the noisy `Box::new(...)` at every registration site.
///
/// ```ignore
/// // Eventually in ToolCatalog::new():
/// let tools: Vec<DynTool> = vec![
///     boxed(ReadTool),
///     boxed(WriteTool),
///     // ...
/// ];
/// ```
pub fn boxed<T: Tool + 'static>(tool: T) -> DynTool {
    Box::new(tool)
}

/// Helper to make migration-time wrapping ergonomic. Many existing
/// tool functions take `(project_root, args, cache, fs)` already —
/// this lets a `Tool` impl forward straight through without
/// re-naming arguments.
///
/// Not strictly necessary for the trait to work, but it documents
/// the expected migration shape so PR-4..PR-N have an obvious target.
///
/// (We don't expose the `Arc<...>` shared-state handles via getters
/// because tools that *need* them ought to read them from the
/// `ToolExecCtx` field directly — the explicitness keeps the
/// dependency graph honest.)
impl<'a> ToolExecCtx<'a> {
    /// Project root accessor — symmetric with `read_cache()`/`fs()`
    /// for tools that prefer method-call style. Pure forwarder.
    #[inline]
    pub fn project_root(&self) -> &std::path::Path {
        self.project_root
    }

    /// File-read cache accessor — pure forwarder. Returns the
    /// `FileReadCache` type alias so clippy's `type_complexity` lint
    /// doesn't flag the inner `Arc<Mutex<HashMap<...>>>` shape.
    #[inline]
    pub fn read_cache(&self) -> &crate::tools::FileReadCache {
        self.read_cache
    }

    /// Filesystem accessor — pure forwarder.
    #[inline]
    pub fn fs(&self) -> &dyn koda_sandbox::fs::FileSystem {
        self.fs
    }

    /// Output caps accessor — pure forwarder. Tools that need a
    /// specific limit (e.g. `caps.list_entries`) read it directly
    /// off this handle rather than us exposing a getter per limit.
    #[inline]
    pub fn caps(&self) -> &crate::output_caps::OutputCaps {
        self.caps
    }

    /// **Test-only** convenience constructor. Builds a `ToolExecCtx`
    /// from the four borrows that vary across tool tests, filling in
    /// safe defaults for the ten fields that don't (no sink, no
    /// caller spawner, no proxy ports, default trust + sandbox).
    ///
    /// Why a `for_test` rather than `Default`: `Default` requires
    /// owned values, but the context is borrows. A test fixture
    /// constructor accepts the borrows the caller already owns and
    /// fills in the rest.
    ///
    /// **Hard rule:** every field default here must be safe to use in
    /// production too — a test calling `for_test` should never
    /// observe behavior that wouldn't happen in a real session with
    /// the same set of provided values. If a future field can't be
    /// safely defaulted, this constructor stops compiling and the
    /// test author has to think about the field. Good failure mode.
    #[cfg(test)]
    pub(crate) fn for_test(
        project_root: &'a std::path::Path,
        read_cache: &'a crate::tools::FileReadCache,
        fs: &'a dyn koda_sandbox::fs::FileSystem,
        caps: &'a crate::output_caps::OutputCaps,
        bg_registry: &'a crate::tools::bg_process::BgRegistry,
        trust: &'a crate::trust::TrustMode,
        sandbox_policy: &'a koda_sandbox::SandboxPolicy,
    ) -> Self {
        Self {
            project_root,
            read_cache,
            fs,
            caps,
            sink: None,
            caller_spawner: None,
            bg_registry,
            trust,
            sandbox_policy,
            proxy_port: None,
            socks5_port: None,
            session: None,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the seam itself. The point of this PR is "the trait
    //! and context compile and dispatch correctly" — these tests
    //! exercise that with two mock tools (one read-only, one mutating)
    //! that have nothing to do with the real built-in tool set.
    //!
    //! Real-tool migration tests will land in PR-4 alongside the
    //! `ReadTool`/`WriteTool`/etc. structs they exercise.

    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Minimal read-only tool for testing the trait dispatch path.
    /// Returns the literal arg string so tests can assert on it.
    struct EchoReadTool;

    #[async_trait]
    impl Tool for EchoReadTool {
        fn name(&self) -> &'static str {
            "EchoRead"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "EchoRead".to_string(),
                description: "Echo args back; for trait tests only.".to_string(),
                parameters: json!({"type": "object"}),
            }
        }
        fn classify(&self, _args: &Value) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn execute(&self, _ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
            ToolResult {
                output: args.to_string(),
                success: true,
                full_output: None,
            }
        }
    }

    /// Mock mutating tool that declares an undo path. Verifies the
    /// `extract_undo_path` override is wired through.
    struct MutatingMockTool;

    #[async_trait]
    impl Tool for MutatingMockTool {
        fn name(&self) -> &'static str {
            "MutatingMock"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "MutatingMock".to_string(),
                description: "Mock mutating tool for trait tests.".to_string(),
                parameters: json!({"type": "object"}),
            }
        }
        fn classify(&self, _args: &Value) -> ToolEffect {
            ToolEffect::LocalMutation
        }
        fn extract_undo_path(&self, args: &Value) -> Option<PathBuf> {
            args.get("path").and_then(|v| v.as_str()).map(PathBuf::from)
        }
        async fn execute(&self, _ctx: &ToolExecCtx<'_>, _args: &Value) -> ToolResult {
            ToolResult {
                output: "mutated".to_string(),
                success: true,
                full_output: None,
            }
        }
    }

    /// Tool that doesn't override `classify` — must default to the
    /// defensive `LocalMutation`.
    struct DefaultClassifyTool;

    #[async_trait]
    impl Tool for DefaultClassifyTool {
        fn name(&self) -> &'static str {
            "DefaultClassify"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "DefaultClassify".to_string(),
                description: "Tests the defensive default.".to_string(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _ctx: &ToolExecCtx<'_>, _args: &Value) -> ToolResult {
            ToolResult {
                output: String::new(),
                success: true,
                full_output: None,
            }
        }
    }

    /// Build a minimal `ToolExecCtx` for tests that don't exercise FS
    /// or sink fields. Uses the production `LocalFileSystem` (cheap
    /// to construct) and a fresh empty cache. Caps come from the
    /// `for_context` helper with a generous budget so List-style
    /// tools don't accidentally truncate test fixtures.
    fn make_ctx<'a>(
        root: &'a std::path::Path,
        cache: &'a crate::tools::FileReadCache,
        fs: &'a dyn koda_sandbox::fs::FileSystem,
        caps: &'a crate::output_caps::OutputCaps,
        bg: &'a crate::tools::bg_process::BgRegistry,
        trust: &'a crate::trust::TrustMode,
        policy: &'a koda_sandbox::SandboxPolicy,
    ) -> ToolExecCtx<'a> {
        ToolExecCtx {
            project_root: root,
            read_cache: cache,
            fs,
            caps,
            sink: None,
            caller_spawner: None,
            bg_registry: bg,
            trust,
            sandbox_policy: policy,
            proxy_port: None,
            socks5_port: None,
            session: None,
        }
    }

    #[tokio::test]
    async fn execute_returns_result_through_trait() {
        let tool = EchoReadTool;
        let cache: crate::tools::FileReadCache = Arc::new(Mutex::new(HashMap::new()));
        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let root = std::path::PathBuf::from("/tmp");
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let ctx = make_ctx(&root, &cache, &fs, &caps, &bg, &trust, &policy);

        let args = json!({"hello": "world"});
        let result = tool.execute(&ctx, &args).await;

        assert!(result.success);
        assert_eq!(result.output, args.to_string());
    }

    #[test]
    fn name_matches_definition_name() {
        let t = EchoReadTool;
        // Invariant the migration audit relies on. Drift here would
        // break dispatch (registry keyed by `name()`, definition
        // listed by `definition().name`).
        assert_eq!(t.name(), t.definition().name);
    }

    #[test]
    fn classify_override_is_observed() {
        // ReadOnly tool reports ReadOnly...
        assert_eq!(EchoReadTool.classify(&json!({})), ToolEffect::ReadOnly);
        // ...mutating tool reports LocalMutation.
        assert_eq!(
            MutatingMockTool.classify(&json!({})),
            ToolEffect::LocalMutation
        );
    }

    #[test]
    fn classify_default_is_local_mutation() {
        // The defensive default — see module docs for rationale.
        // A tool that forgets to override classify gets gated behind
        // approval, which is the safe failure mode.
        assert_eq!(
            DefaultClassifyTool.classify(&json!({})),
            ToolEffect::LocalMutation
        );
    }

    #[test]
    fn extract_undo_path_default_is_none() {
        // The default impl returns None — read-only tools don't need
        // to override it.
        assert!(EchoReadTool.extract_undo_path(&json!({})).is_none());
    }

    #[test]
    fn extract_undo_path_override_picks_up_args() {
        let t = MutatingMockTool;
        // Path present → Some
        let p = t.extract_undo_path(&json!({"path": "src/main.rs"}));
        assert_eq!(p, Some(PathBuf::from("src/main.rs")));
        // No path arg → None (graceful)
        assert!(t.extract_undo_path(&json!({})).is_none());
        // Wrong type → None (graceful)
        assert!(t.extract_undo_path(&json!({"path": 42})).is_none());
    }

    #[test]
    fn boxed_helper_produces_dyn_tool() {
        // Compile-time test: `boxed(...)` must produce something
        // storable as `DynTool`. The vec literal forces that.
        let tools: Vec<DynTool> = vec![
            boxed(EchoReadTool),
            boxed(MutatingMockTool),
            boxed(DefaultClassifyTool),
        ];
        assert_eq!(tools.len(), 3);
        // Round-trip through the trait object also works.
        assert_eq!(tools[0].name(), "EchoRead");
        assert_eq!(tools[1].name(), "MutatingMock");
        assert_eq!(tools[2].name(), "DefaultClassify");
    }

    #[tokio::test]
    async fn dyn_dispatch_executes_correct_tool() {
        // Trait-object dispatch is the production code path — verify
        // it works for both tool variants. This is the single most
        // important test in this module: if dyn dispatch breaks, the
        // entire migration plan is dead.
        let tools: Vec<DynTool> = vec![boxed(EchoReadTool), boxed(MutatingMockTool)];
        let cache: crate::tools::FileReadCache = Arc::new(Mutex::new(HashMap::new()));
        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let root = std::path::PathBuf::from("/tmp");
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let ctx = make_ctx(&root, &cache, &fs, &caps, &bg, &trust, &policy);

        let r1 = tools[0].execute(&ctx, &json!({})).await;
        assert!(r1.success);
        assert!(r1.output.contains("{}"));

        let r2 = tools[1].execute(&ctx, &json!({})).await;
        assert!(r2.success);
        assert_eq!(r2.output, "mutated");
    }

    #[test]
    fn ctx_accessors_match_field_values() {
        // The accessor methods exist as ergonomic forwarders; verify
        // they actually return the borrowed values (not, say, Default).
        let cache: crate::tools::FileReadCache = Arc::new(Mutex::new(HashMap::new()));
        let fs = koda_sandbox::fs::LocalFileSystem::new();
        let root = std::path::PathBuf::from("/tmp/whatever");
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let ctx = make_ctx(&root, &cache, &fs, &caps, &bg, &trust, &policy);

        assert_eq!(ctx.project_root(), root.as_path());
        // Cache is a single shared handle — pointer-equality check.
        assert!(Arc::ptr_eq(ctx.read_cache(), &cache));
    }
}
