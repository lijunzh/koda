//! Sub-agent invocation and lifecycle management.
//!
//! Extracted from `tool_dispatch.rs` — handles `InvokeAgent` execution,
//! background agent spawning, worktree provisioning, and sub-agent caching.
//! Each sub-agent gets its own session, provider, and (optionally) worktree
//! for isolation. Results are cached by `(agent_name, prompt_hash)`.

use crate::approval_flow::request_approval;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::loop_guard;
use crate::memory;
use crate::persistence::Persistence;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::{ChatMessage, ToolCall};
use crate::sub_agent_cache::SubAgentCache;
use crate::tool_dispatch::execute_one_tool;
use crate::tools::{self, ToolRegistry};
use crate::trust::{self, ToolApproval, TrustMode, derive_child_trust};

use anyhow::{Context, Result};
use koda_sandbox::{CwdProvider, GitWorktreeProvider, WorkspaceProvider};

#[cfg(target_os = "macos")]
use koda_sandbox::ClonefileProvider;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Process-wide allocator for sub-agent invocation IDs.
///
/// Phase E of #996. Each `execute_sub_agent` call (foreground or
/// background) draws a fresh id; that id becomes the **spawner tag**
/// for any background work this sub-agent registers, and is the key
/// used to cancel that work when the sub-agent exits.
///
/// Top-level inference uses `None` (no invocation id). Sub-agents at
/// any nesting depth use `Some(N)`.
///
/// `u32::MAX` invocations is comfortably more than any single Code
/// Puppy session needs; we don't bother with wrap-around handling.
/// Starts at 1 so `0` can stay reserved for "unset" should the type
/// ever change.
static NEXT_INVOCATION_ID: AtomicU32 = AtomicU32::new(1);

/// Allocate a fresh invocation id. See [`NEXT_INVOCATION_ID`].
pub(crate) fn next_invocation_id() -> u32 {
    NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// RAII cleanup hook for #996 Phase E.
///
/// On drop, cancels every bg-agent registry entry tagged with this
/// sub-agent's invocation id. That covers the two ways a sub-agent
/// can exit and leave orphans:
///
///   1. **Iteration cap** (`Ok(iteration_cap_marker(...))`) — the
///      sub-agent ran out of inference iterations *while* one of its
///      bg children was still running.
///   2. **Error return** (`Err(...)` from any `?` inside the loop) —
///      e.g. provider failure, persistence failure, `?`-propagated
///      cancellation.
///
/// On the cancel-token path we'd reap anyway via the parent's cascade,
/// but the spawner-scoped cancel is cheap and idempotent so we just
/// always run it. `cancel_for_spawner` is `O(n)` over the registry,
/// which is fine for the < 100-entry registries we expect.
///
/// Background shell processes (`Bash{background:true}`) are *not*
/// covered here — each sub-agent constructs its own `ToolRegistry`
/// with its own `BgRegistry`, which `Drop`-SIGTERMs everything when
/// the registry goes out of scope. That handles shell orphans for
/// free; this struct only needs to deal with the *shared*
/// `BgAgentRegistry`.
struct InvocationCleanup<'a> {
    bg: &'a std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
    invocation_id: u32,
}

impl Drop for InvocationCleanup<'_> {
    fn drop(&mut self) {
        let cancelled = self.bg.cancel_for_spawner(self.invocation_id);
        if cancelled > 0 {
            tracing::debug!(
                spawner = self.invocation_id,
                cancelled,
                "execute_sub_agent exit: cancelled orphaned bg agents",
            );
        }
    }
}

/// Run a sub-agent in the background. Owns all data (no borrows).
///
/// **Phase 2 of #1022 (B5 complete):** uses the multi-thread runtime
/// via `tokio::spawn`. This requires `execute_sub_agent`'s future to
/// be `Send`, which we enforce explicitly via the `+ Send` bound on
/// its return type — see the function's signature for the bound and
/// `koda-sandbox/src/ipc.rs` for the matching `Send` bounds on the
/// generic IPC helpers that previously hid a non-Send transitive.
#[allow(clippy::too_many_arguments)]
async fn run_bg_agent(
    project_root: std::path::PathBuf,
    parent_config: KodaConfig,
    db: Database,
    arguments: String,
    sub_agent_cache: SubAgentCache,
    parent_session: String,
    tx: tokio::sync::oneshot::Sender<
        Result<crate::bg_agent::BgPayload, crate::bg_agent::BgPayload>,
    >,
    // B2 of #1022: parent's cancel token, threaded as a `child_token()`
    // so a Ctrl-C in the parent loop cancels the bg agent.
    cancel: CancellationToken,
    // B1 of #1022: parent's trust mode — used both as the approval
    // mode for tool calls inside the bg agent and (via the recursive
    // `execute_sub_agent` call) as the clamp ceiling for the
    // sub-agent's own declared trust.
    parent_trust: TrustMode,
    // B4 of #1022: parent's effective sandbox policy at spawn time.
    // The recursive `execute_sub_agent` composes the child policy
    // onto this so the bg agent inherits any parent narrowing.
    parent_sandbox_policy: koda_sandbox::SandboxPolicy,
    // Layer 0 of #996: status channel — we own the writer end.
    // Pending → Running on entry; one of Completed/Errored/Cancelled
    // before the future returns. The registry's matching
    // `watch::Receiver` is what `/agents` and the (future) status-bar
    // pill read. `send` failures are deliberately ignored: the
    // receiver lives on the entry inside the registry, and the only
    // way `send` returns `Err` is if every receiver was dropped —
    // which means the registry itself was dropped, in which case our
    // result oneshot below is also doomed and the user can't see us
    // anyway. Logging would be noise.
    status_tx: tokio::sync::watch::Sender<crate::bg_agent::AgentStatus>,
) {
    // We don't have per-iteration visibility yet (Layer 4 work);
    // `iter: 0` means "started, no iter info". Once the inference
    // loop inside `execute_sub_agent` learns to push iter updates,
    // the field will reflect 1..=20.
    let _ = status_tx.send(crate::bg_agent::AgentStatus::Running { iter: 0 });

    let (_, mut cmd_rx) = mpsc::channel(1);
    // #1022 B9: bg agents used to run with `NullSink`, so every
    // event inside them was silently dropped — the user only saw
    // the spawn line and the eventual completion line. Now we use
    // `BufferingSink` to capture a narrative trace (tool calls,
    // info, auto-rejected approvals) that ships back over the
    // result oneshot and gets surfaced to the user at
    // result-injection time. See `engine::sink::BufferingSink` for
    // the capture rules.
    let buffering_sink = crate::engine::sink::BufferingSink::new();
    let nested_bg = crate::bg_agent::new_shared();

    // Override background=false to prevent infinite spawn — a bg agent
    // that itself emitted `InvokeAgent { background: true }` would
    // never see its child's result (no inference loop is running
    // *inside* a bg agent to drain results).
    let mut sync_args: serde_json::Value = serde_json::from_str(&arguments).unwrap_or_default();
    sync_args["background"] = serde_json::Value::Bool(false);
    let sync_arguments = serde_json::to_string(&sync_args).unwrap();

    // We need to inspect `cancel` *after* the call to decide between
    // Cancelled and Errored when `execute_sub_agent` returns Err —
    // a cancelled future typically surfaces as an error from inside
    // the loop, but the user-visible state should be "Cancelled",
    // not "Errored". Clone before the move.
    let cancel_for_status = cancel.clone();

    let result = execute_sub_agent(
        &project_root,
        &parent_config,
        &db,
        &sync_arguments,
        parent_trust,
        &buffering_sink,
        cancel,
        &mut cmd_rx,
        None,
        &sub_agent_cache,
        &parent_session,
        &nested_bg,
        &parent_sandbox_policy,
        // Phase E of #996: the bg agent has no in-process parent in
        // the spawner sense — its `nested_bg` registry is fresh, and
        // any bg work it spawns gets tagged with the bg agent's *own*
        // invocation id (allocated inside the recursive call). The
        // parent's cascade-cancel covers cross-registry teardown.
        None,
    )
    .await;

    // Drain the buffered trace exactly once. The events ship back
    // alongside the output (for the success case) or alongside the
    // error message (for the failure case) so the user can see
    // *what the bg agent attempted* even when it failed.
    let events = buffering_sink.take_lines();

    // Set terminal status *before* sending the result oneshot so a
    // racing `snapshot()` between the `tx.send` and the entry being
    // drained sees the terminal state, not stale `Running`.
    match &result {
        Ok(output) => {
            let _ = status_tx.send(crate::bg_agent::AgentStatus::Completed {
                // `summary` is currently the full output — truncation
                // is the display layer's job (Codex pattern: see
                // `COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES`).
                summary: output.clone(),
            });
        }
        Err(e) => {
            // Cancellation typically reaches us as an error from
            // somewhere deep in the loop. Disambiguate by checking
            // the token: if it fired, the user-visible reason is
            // "Cancelled", not the inner error string.
            let status = if cancel_for_status.is_cancelled() {
                crate::bg_agent::AgentStatus::Cancelled
            } else {
                crate::bg_agent::AgentStatus::Errored {
                    error: e.to_string(),
                }
            };
            let _ = status_tx.send(status);
        }
    }

    let _ = match result {
        Ok(output) => tx.send(Ok((output, events))),
        Err(e) => tx.send(Err((format!("Error: {e}"), events))),
    };
}

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
///
/// Results are cached in `sub_agent_cache` keyed by `(agent_name, prompt_hash)`.
/// On cache hit, returns immediately without any LLM calls.
#[tracing::instrument(skip_all, fields(agent_name, cached = false))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_sub_agent<'a>(
    project_root: &'a Path,
    parent_config: &'a KodaConfig,
    db: &'a Database,
    arguments: &'a str,
    mode: TrustMode,
    sink: &'a dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    sub_agent_cache: &'a SubAgentCache,
    parent_session_id: &'a str,
    bg_agents: &'a std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
    // Phase 5 PR-4 of #934: parent's effective sandbox policy. The
    // child policy is composed onto this so the child can only narrow,
    // never widen — see [`koda_sandbox::SandboxPolicy::compose`] for
    // the per-field rules. Pass `&SandboxPolicy::strict_default()`
    // when there is no meaningful parent (top-level invocation).
    parent_sandbox_policy: &'a koda_sandbox::SandboxPolicy,
    // Phase E of #996: the **caller's** invocation id. Used as the
    // `spawner` tag on bg-sub-agent reservations so the parent (not
    // the child) owns the right to wait/cancel its bg children. Pass
    // `None` when called from top-level inference. Distinct from the
    // child's own `my_invocation_id`, which is allocated below and
    // tags any bg work the child itself spawns.
    parent_spawner: Option<u32>,
) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
    async move {
        // Phase E of #996: allocate this invocation's id up-front. It
        // becomes the `caller_spawner` for every tool call inside
        // this sub-agent's loop, AND the `spawner` tag the cleanup
        // hook below uses to reap any orphaned bg work.
        let my_invocation_id = next_invocation_id();
        let args: serde_json::Value = serde_json::from_str(arguments)?;
        let agent_name = args["agent_name"].as_str().unwrap_or("task");
        tracing::Span::current().record("agent_name", agent_name);
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
        let is_fork = agent_name == "fork";
        let background = args["background"].as_bool().unwrap_or(false);

        // Background mode: spawn and return immediately.
        //
        // Phase 1 of #1022 fixes B1–B4 here:
        //  * **B1 trust:** the recursive `execute_sub_agent` call below
        //    receives `mode` (the parent's trust mode) instead of
        //    hard-coded `TrustMode::Auto`. The clamp inside that call
        //    then guarantees the bg agent can only narrow, never widen.
        //  * **B2 cancellation:** the bg task receives a `child_token()`
        //    of the parent's `cancel`. Ctrl-C in the parent loop now
        //    cascades into every in-flight bg agent.
        //  * **B3 lifecycle:** the spawned `JoinHandle` is held by the
        //    registry as an `AbortOnDropHandle`, so a registry drop
        //    aborts the task and releases its worktree.
        //  * **B4 sandbox:** `parent_sandbox_policy.clone()` is captured
        //    at spawn time, so the recursive call composes the bg
        //    agent's policy onto the parent's effective policy instead
        //    of regressing to `strict_default()`.
        //  * **B5 (Phase 2):** the bg agent now runs on the multi-thread
        //    runtime via `tokio::spawn`. We enforced `Send` on
        //    `execute_sub_agent`'s future by switching its signature to
        //    `fn(...) -> impl Future<Output = ...> + Send + 'a`, which
        //    forces the compiler to *prove* Send (vs. silently degrading
        //    when an `async fn` happens to capture a non-Send temporary).
        //    The transitive offender was `koda-sandbox::ipc::{read,write}_message`
        //    — those generic helpers had no `Send` bound on `R`/`W`/`T`,
        //    so MutexGuards held across their awaits weren't Send. Bounds
        //    have been added there as well.
        if background {
            // Phase E of #996: tag the bg-sub-agent task with the
            // **parent's** spawner identity. The parent (not the bg
            // sub-agent itself) owns the right to wait/cancel its
            // bg children via WaitTask/CancelTask. The bg sub-agent's
            // own invocation id (`my_invocation_id` allocated above)
            // is unused on this code path — it would matter if the
            // bg sub-agent itself were to be cancelled-on-parent-exit,
            // but the registry's parent->bg cancel-token cascade
            // already handles that case.
            let reservation = bg_agents.reserve(&cancel, parent_spawner);
            let task_id = reservation.task_id;
            let bg_cancel = reservation.cancel.clone();
            let bg_tx = reservation.tx;
            let bg_rx = reservation.rx;
            let entry_cancel = reservation.cancel;
            // Layer 0 of #996: status sender goes to the spawned
            // future (sole writer); receiver stays on the registry
            // entry so `snapshot()` / `/agents` can read it without
            // touching the spawn site.
            let bg_status_tx = reservation.status_tx;
            let entry_status_rx = reservation.status_rx;

            let project_root_owned = project_root.to_path_buf();
            let parent_config_owned = parent_config.clone();
            let agent_name_owned = agent_name.to_string();
            let prompt_owned = prompt.to_string();
            let arguments_owned = arguments.to_string();
            let sub_agent_cache_owned = sub_agent_cache.clone();
            let parent_session_owned = parent_session_id.to_string();
            let bg_db = db.clone();
            let bg_policy = parent_sandbox_policy.clone();
            let bg_trust = mode;

            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{1f680} {agent_name} launched in background (task {task_id})"
                ),
            });

            let handle = tokio::spawn(run_bg_agent(
                project_root_owned,
                parent_config_owned,
                bg_db,
                arguments_owned,
                sub_agent_cache_owned,
                parent_session_owned,
                bg_tx,
                bg_cancel,
                bg_trust,
                bg_policy,
                bg_status_tx,
            ));

            bg_agents.attach(
                task_id,
                &agent_name_owned,
                &prompt_owned,
                bg_rx,
                entry_cancel,
                entry_status_rx,
                parent_spawner,
                handle,
            );

            return Ok(format!(
                "Background agent '{agent_name_owned}' started (task {task_id}). \
             Results will be injected when complete."
            ));
        }

        // From this point on, any bg work this invocation spawns is
        // tagged with `my_invocation_id`. Install the RAII cleanup
        // guard *now*, after the bg-spawn early-return — a bg-spawn
        // path's child is intentionally meant to outlive us; the
        // guard would (correctly!) reap it as an orphan, which is
        // exactly the behaviour we want to AVOID for the bg branch.
        let _cleanup = InvocationCleanup {
            bg: bg_agents,
            invocation_id: my_invocation_id,
        };
        // Check result cache — identical (agent_name, prompt) pairs hit
        // a cache and skip the LLM call. Cheap to retry idempotent tasks.
        if let Some(cached) = sub_agent_cache.get(agent_name, prompt) {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{26a1} {agent_name}: cache hit, skipping LLM call"),
            });
            tracing::Span::current().record("cached", true);
            return Ok(cached);
        }

        sink.emit(EngineEvent::SubAgentStart {
            agent_name: agent_name.to_string(),
        });

        // Fork inherits parent config; named agents load their own persona
        // but fall back to the parent's provider and model for anything not
        // explicitly set in the agent JSON.
        //
        // Inheritance rules (applied only when the agent JSON leaves a field None):
        //
        // provider + base_url — inherited from parent when the agent JSON sets
        //   neither. If the agent sets its own provider or base_url (e.g. a
        //   test-only "mock" agent or a specialist routed to a different endpoint)
        //   we respect that and leave it alone.
        //
        // model — inherited from parent only when (a) the agent JSON left it
        //   unset AND (b) we are also inheriting the provider. Cross-provider
        //   model names are not portable ("gemini-2.0-flash" means nothing on
        //   Anthropic), so if the agent has its own provider we leave the model
        //   resolved from that provider's defaults.
        let sub_config = if is_fork {
            // Fork inherits the parent config verbatim, *except* for
            // trust — which must come from the **runtime** mode
            // (see `derive_child_trust` doc).
            //
            // **#1022 B17**: was `debug_assert!`, which is *compiled
            // out* in release builds. The fork-trust invariant is a
            // security-relevant property (a future change that
            // accidentally narrowed/widened trust between the clone
            // and use would silently ship). Promoted to `assert!`
            // — the runtime cost is a single enum equality check, so
            // there's no reason to weaken the guarantee for release.
            //
            // **#1022 B19**: pre-fix asserted `cfg.trust ==
            // parent_config.trust` after a clone, which checked the
            // wrong thing — `parent_config.trust` is the *startup*
            // value of the trust mode and ignores `/safe`/`/auto`
            // toggles. The actual invariant is "fork runs at the
            // parent's *runtime* trust", and the runtime mode is
            // `mode`. Now we explicitly write `cfg.trust` from
            // `derive_child_trust(mode, mode)` (= `mode`) and
            // assert against `mode`. The clone-then-overwrite
            // pattern is intentional: keeps the rest of
            // `parent_config` (model, base_url, system prompt
            // overrides, ...) verbatim while making the trust
            // derivation explicit and uniform with the named path.
            let mut cfg = parent_config.clone();
            cfg.trust = derive_child_trust(mode, mode);
            assert!(
                cfg.trust == mode,
                "fork must inherit parent's runtime trust mode exactly"
            );
            cfg
        } else {
            // Load the raw JSON first to see what the agent explicitly set.
            let raw = crate::config::KodaConfig::load_agent_json(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let mut cfg = crate::config::KodaConfig::load(project_root, agent_name)
                .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;

            let agent_has_own_provider = raw.provider.is_some() || raw.base_url.is_some();

            if !agent_has_own_provider {
                // Inherit parent's provider, base_url, and (if unset) model.
                // All three travel together: model names are provider-scoped.
                let model_override = raw.model.is_none().then(|| parent_config.model.clone());
                cfg = cfg.with_overrides(
                    Some(parent_config.base_url.clone()),
                    model_override,
                    Some(parent_config.provider_type.to_string()),
                );
            }
            // else: agent opted into its own provider — use its resolved config
            // as-is. The agent JSON is responsible for any model it needs.

            // Inherit trust: child can never exceed parent's *runtime*
            // trust (#845, #1022 B19). Same pattern as Codex's
            // `apply_spawn_agent_runtime_overrides()` which copies the
            // parent's runtime sandbox_policy onto the child.
            //
            // **#1022 B19**: pre-fix used `parent_config.trust` as the
            // ceiling. That field is the *startup* trust value;
            // `cycle_trust`/`set_trust` mutate the SharedTrustMode
            // atomic but never the config field. So a user who
            // started in Auto and hit `/safe` would get sub-agents
            // clamped against the stale Auto, allowing the child to
            // run with broader privileges than the parent's *current*
            // mode. Real escalation. The runtime mode is `mode`,
            // threaded through `execute_one_tool` from the inference
            // loop — that's the only authoritative value. The helper
            // `derive_child_trust` exists to make this antipattern
            // greppable (see its doc).
            let child_trust = cfg.trust;
            cfg.trust = derive_child_trust(mode, cfg.trust);
            if cfg.trust != child_trust {
                tracing::info!(
                    agent = agent_name,
                    parent = %mode,
                    child = %child_trust,
                    effective = %cfg.trust,
                    "sub-agent trust clamped to match parent",
                );
            }

            cfg
        };

        let sub_session = {
            let sid = db
                .create_session(&sub_config.agent_name, project_root)
                .await?;
            // Fork: copy parent conversation history into the new session.
            //
            // **#1022 B20**: was a per-row loop — N×(`insert_message`
            // + `mark_message_complete` for assistant rows), each
            // call its own fsync, on the synchronous fork hot path.
            // For a 200-message parent that's ~600 round-trips and
            // hundreds of ms of disk wait. Now a single transaction
            // via `copy_messages_into_session` (one fsync at COMMIT,
            // `completed_at` written inline for assistant rows).
            if is_fork {
                let parent_history = db.load_context(parent_session_id).await?;
                db.copy_messages_into_session(&sid, &parent_history).await?
            }
            sid
        };

        db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
            .await?;

        let provider = crate::providers::create_provider(&sub_config);
        // Select workspace provider. Write-capable agents get an isolated
        // workspace; read-only agents share the parent root for free.
        //
        // Per-platform write-isolation choice:
        //
        // - **macOS:** `ClonefileProvider` (APFS clonefile(2)) is
        //   preferred for its ~3-4× provision speedup over git worktree
        //   (Phase 4d / #934). Falls back to git worktree if construction
        //   fails (e.g. `$HOME` unset, project path can't canonicalize).
        // - **Linux + others:** `GitWorktreeProvider`. The Linux CoW
        //   equivalent (4e in #934) is deferred until production
        //   telemetry shows it's worth building.
        //
        // **Documented platform divergence** — see `docs/src/sandbox.md`
        // → "Workspace providers". Both backends provide the same
        // isolation guarantees; only provision speed differs.
        let has_write_tools = !sub_config
            .disallowed_tools
            .iter()
            .any(|t| t == "Write" || t == "Edit");
        // Phase 1 of #1022: explicit `+ Send + Sync` on the trait object.
        // The supertrait bound `WorkspaceProvider: Send + Sync` constrains
        // *implementors*, but Rust trait objects don't auto-inherit those
        // bounds — `Box<dyn WorkspaceProvider>` is `!Send` without the
        // explicit annotation, which makes the whole `execute_sub_agent`
        // future `!Send` and unspawnable.
        let workspace: Box<dyn WorkspaceProvider + Send + Sync> = if has_write_tools {
            pick_write_provider(project_root, agent_name)
        } else {
            Box::new(CwdProvider::new(project_root))
        };
        let effective_root = match workspace.provision(&sub_session).await {
            Ok(path) => {
                if path != project_root {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f333} {agent_name}: isolated in worktree"),
                    });
                }
                path
            }
            Err(e) if has_write_tools => {
                // **#1022 B21**: pre-fix this branch silently fell
                // back to `project_root.to_path_buf()`. With write
                // tools requested, that drops the sub-agent into
                // the parent's unisolated working tree — two
                // parallel sub-agents would race on the same
                // files, exactly the corruption mode the workspace
                // provider exists to prevent. Worse, the only
                // signal was a `tracing::warn!` invisible to most
                // headless / TUI runs.
                //
                // Now: short-circuit with a structural-failure
                // marker (same `[ERROR:` shape as the iteration-cap
                // marker from B18) so the parent agent sees the
                // failure as a sub-agent result and can adapt
                // (retry without write tools, do the work itself,
                // surface to the user). Also cache it so a
                // verbatim re-dispatch with the same prompt
                // doesn't pay the failed-provision cost twice —
                // mirrors the iteration-cap caching policy.
                let reason = e.to_string();
                tracing::warn!("Workspace provision failed for sub-agent '{agent_name}': {reason}");
                sink.emit(EngineEvent::Info {
                    message: format!(
                        "  \u{26a0}\u{fe0f}  {agent_name}: workspace isolation failed, not dispatching ({reason})"
                    ),
                });
                let marker = workspace_provision_failure_marker(agent_name, &reason);
                sub_agent_cache.put(agent_name, prompt, &marker);
                return Ok(marker);
            }
            Err(e) => {
                // Read-only sub-agent (no write tools): isolation
                // wasn't requested, so falling back to project_root
                // is the *intended* behavior — there's no
                // race-on-files corruption mode without write
                // tools. Today this branch is unreachable because
                // `CwdProvider::provision` is infallible, but the
                // explicit arm documents intent and survives a
                // future read-only provider that *can* fail.
                tracing::warn!(
                    "Workspace provision failed (read-only sub-agent '{agent_name}'): {e}"
                );
                project_root.to_path_buf()
            }
        };
        let effective_root_ref = effective_root.as_path();

        let tools = {
            let registry = ToolRegistry::with_trust(
                effective_root.clone(),
                sub_config.max_context_tokens,
                sub_config.trust,
            );
            // Phase 5 PR-4 of #934: compose the parent's effective policy
            // with the child's. Per-field rules in [`SandboxPolicy::compose`]
            // (denies union, allows parent-wins, limits min, trust strictest)
            // ensure the child can never widen the parent's surface — only
            // narrow it. PR-2 installed the child policy verbatim; PR-4
            // makes the install additive over the parent so chains of
            // sub-agents accumulate restrictions monotonically.
            let composed_policy = crate::sandbox::compose_child_policy(
                parent_sandbox_policy,
                sub_config.trust,
                effective_root_ref,
            );
            let registry = registry.with_sandbox_policy(composed_policy);
            match parent_cache {
                Some(cache) => registry.with_shared_cache(cache),
                None => registry,
            }
        };
        let tool_defs = {
            let mut denied = sub_config.disallowed_tools.clone();
            // #1022 B7 (revised): sub-agents cannot spawn sub-agents.
            // Period. Originally only `is_fork` blocked `InvokeAgent`,
            // but that left a sharp edge: named sub-agents could call
            // it, the call fell through to a registry stub returning
            // `"InvokeAgent is handled by the inference loop."` with
            // `success=false`, and the model would hallucinate around
            // the bogus error.
            //
            // Allowing real recursion was the alternative considered,
            // but it requires a depth cap (~hundreds of KB of `async
            // fn` state per level), `Box::pin` on a mutually-recursive
            // future, threading `depth: u32` through five functions,
            // and the resulting design has no use case worth the
            // surface area. Codex matches this stance — their
            // sub-agents can't spawn sub-agents either. The master
            // agent at depth 0 can fire as many parallel/background
            // workers as it wants; workers complete their task and
            // report back. Flat by design.
            //
            // Filtering at the tool-def level keeps the model from
            // ever seeing the tool. The sub-agent dispatch loop also
            // contains a defense-in-depth refusal in case a rogue or
            // scripted model emits `InvokeAgent` regardless.
            if !denied.contains(&"InvokeAgent".to_string()) {
                denied.push("InvokeAgent".to_string());
            }
            // #1022 B8: AskUser requires a live `cmd_rx` connected to the
            // user. Sub-agents have a detached channel (foreground sub-agents
            // get `&mut mpsc::channel(1).1` from the parent dispatch path,
            // bg agents get an even more detached one). Filter the tool out
            // entirely so the model never tries to call it. Without this
            // filter the call falls through to the registry stub and the
            // sub-agent gets `"AskUser is handled by the inference loop."`
            // back as a tool result — which the model then dutifully
            // hallucinates around.
            if !denied.contains(&"AskUser".to_string()) {
                denied.push("AskUser".to_string());
            }
            // Phase E of #996 wired `caller_spawner` through the
            // dispatch layer, so the bg-task tools (ListBackgroundTasks /
            // CancelTask / WaitTask) now correctly scope to the calling
            // sub-agent's own invocation id — a sub-agent only sees
            // bg work *it* spawned. The earlier blanket denylist that
            // hid these tools from sub-agents has been removed.
            tools.get_definitions(&sub_config.allowed_tools, &denied)
        };
        let semantic_memory = if sub_config.skip_memory {
            String::new()
        } else {
            memory::load(project_root)?
        };
        let env = crate::prompt::EnvironmentInfo {
            project_root: effective_root_ref,
            model: &sub_config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = build_system_prompt(
            &sub_config.system_prompt,
            &semantic_memory,
            &sub_config.agents_dir,
            &env,
            &[], // sub-agents have no REPL commands
            &tools.skill_registry,
        );

        for _ in 0..loop_guard::MAX_SUB_AGENT_ITERATIONS {
            // Respect parent cancellation (#286)
            if cancel.is_cancelled() {
                // Release workspace on cancellation (best-effort, no user hint).
                let _ = workspace.release(&sub_session, &effective_root).await;
                return Ok("[cancelled by parent]".to_string());
            }
            let history = db.load_context(&sub_session).await?;
            let mut messages = vec![ChatMessage::text("system", &system_prompt)];
            for msg in &history {
                let tool_calls: Option<Vec<ToolCall>> = msg
                    .tool_calls
                    .as_deref()
                    .and_then(|tc| serde_json::from_str(tc).ok());
                messages.push(ChatMessage {
                    role: msg.role.as_str().to_string(),
                    content: msg.content.clone(),
                    tool_calls,
                    tool_call_id: msg.tool_call_id.clone(),
                    images: None,
                });
            }

            sink.emit(EngineEvent::SpinnerStart {
                message: format!("  🦥 {agent_name} thinking..."),
            });
            let response = provider
                .chat(&messages, &tool_defs, &sub_config.model_settings)
                .await?;
            sink.emit(EngineEvent::SpinnerStop);

            let tool_calls_json = if response.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&response.tool_calls)?)
            };

            db.insert_message(
                &sub_session,
                &Role::Assistant,
                response.content.as_deref(),
                tool_calls_json.as_deref(),
                None,
                Some(&response.usage),
            )
            .await?;

            if response.tool_calls.is_empty() {
                let result = response
                    .content
                    .unwrap_or_else(|| "(no output)".to_string());
                // Cache the result for future identical calls
                sub_agent_cache.put(agent_name, prompt, &result);
                // Release workspace; surface branch hint if agent left changes.
                if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
                    sink.emit(EngineEvent::Info {
                        message: format!("  \u{1f335} {agent_name}: {hint}"),
                    });
                }
                return Ok(result);
            }

            for tc in &response.tool_calls {
                sink.emit(EngineEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.function_name.clone(),
                    args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                    is_sub_agent: true,
                });

                // Sub-agents inherit the parent's approval mode
                let parsed_args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_default();

                // #1022 B7 (revised): defense-in-depth refusal of
                // `InvokeAgent` and `AskUser`. Both are filtered from
                // the sub-agent's `tool_defs` above, so a well-behaved
                // model never emits them. A misbehaving or scripted
                // model still might — short-circuit here with a clear
                // refusal message instead of falling through to
                // `execute_one_tool` (which would happily recurse for
                // InvokeAgent) or the registry stub (which returns
                // confusing `success=false` boilerplate).
                if tc.function_name == "InvokeAgent" {
                    let refusal = "InvokeAgent is not available inside a sub-agent. \
                                   Sub-agents are autonomous workers and cannot spawn \
                                   further sub-agents. Complete the task directly with \
                                   the tools you have, or report back what additional \
                                   dispatch the parent agent should perform.";
                    db.insert_message(
                        &sub_session,
                        &Role::Tool,
                        Some(refusal),
                        None,
                        Some(&tc.id),
                        None,
                    )
                    .await?;
                    continue;
                }
                if tc.function_name == "AskUser" {
                    let refusal = "AskUser is not available inside a sub-agent. \
                                   Sub-agents have no channel to the user; the parent \
                                   agent gathers any required input before delegating. \
                                   Proceed with the information you already have or \
                                   report what's missing.";
                    db.insert_message(
                        &sub_session,
                        &Role::Tool,
                        Some(refusal),
                        None,
                        Some(&tc.id),
                        None,
                    )
                    .await?;
                    continue;
                }

                // #1022 B14: pre-flight validation — catch obvious errors
                // (missing path, bad regex, file-cache violations) before
                // we burn an approval prompt or execute. The top-level
                // sequential dispatcher does the same; without this the
                // sub-agent would hit the same class of errors *after*
                // the user had already approved.
                let validation_error = tools::validate::validate_with_registry(
                    &tools,
                    &tc.function_name,
                    &parsed_args,
                    effective_root_ref,
                )
                .await;

                let output = if let Some(error) = validation_error {
                    format!("Validation error: {error}")
                } else {
                    let approval = trust::check_tool(
                        &tc.function_name,
                        &parsed_args,
                        mode,
                        Some(effective_root_ref),
                    );

                    match approval {
                        ToolApproval::AutoApprove => {
                            // #1022 B6 + B7: route through `execute_one_tool`
                            // (instead of calling `tools.execute()` directly)
                            // so that:
                            //   - mutating tool calls invalidate the
                            //     `SubAgentCache` (B6) — otherwise an
                            //     identical follow-up `InvokeAgent` returns
                            //     a stale cached result.
                            //   - nested `InvokeAgent` from inside this
                            //     sub-agent dispatches recursively into
                            //     `execute_sub_agent` (B7), instead of
                            //     hitting the registry stub that returns
                            //     "InvokeAgent is handled by the inference
                            //     loop." with success=false.
                            //   - Bash output streams through the parent
                            //     sink (free visibility win).
                            let (_id, result, _success, _full) = execute_one_tool(
                                tc,
                                project_root,
                                &sub_config,
                                db,
                                &sub_session,
                                &tools,
                                mode,
                                sink,
                                cancel.clone(),
                                sub_agent_cache,
                                bg_agents,
                                Some(my_invocation_id),
                            )
                            .await;
                            result
                        }
                        ToolApproval::Blocked => {
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
                            let diff_preview = preview::compute(
                                &tc.function_name,
                                &parsed_args,
                                effective_root_ref,
                            )
                            .await;
                            sink.emit(EngineEvent::ActionBlocked {
                                tool_name: tc.function_name.clone(),
                                detail,
                                preview: diff_preview,
                            });
                            "[safe mode] Action blocked.".to_string()
                        }
                        ToolApproval::NeedsConfirmation => {
                            let detail = tools::describe_action(&tc.function_name, &parsed_args);
                            let diff_preview = preview::compute(
                                &tc.function_name,
                                &parsed_args,
                                effective_root_ref,
                            )
                            .await;
                            let effect = crate::trust::resolve_tool_effect_with_registry(
                                &tc.function_name,
                                &parsed_args,
                                &tools,
                            );
                            match request_approval(
                                sink,
                                cmd_rx,
                                &cancel,
                                &tc.function_name,
                                &detail,
                                diff_preview,
                                effect,
                            )
                            .await
                            {
                                Some(ApprovalDecision::Approve) => {
                                    let (_id, result, _success, _full) = execute_one_tool(
                                        tc,
                                        project_root,
                                        &sub_config,
                                        db,
                                        &sub_session,
                                        &tools,
                                        mode,
                                        sink,
                                        cancel.clone(),
                                        sub_agent_cache,
                                        bg_agents,
                                        Some(my_invocation_id),
                                    )
                                    .await;
                                    result
                                }
                                Some(ApprovalDecision::Reject) => "[rejected by user]".to_string(),
                                Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                                    format!("[rejected: {feedback}]")
                                }
                                Some(ApprovalDecision::RejectAuto { reason }) => {
                                    // #1022 B15: same shape as the existing
                                    // sub-agent auto-reject below, so the model
                                    // sees a uniform "no human, here's why"
                                    // signal regardless of whether the auto-
                                    // rejection came from headless policy or
                                    // from a closed approval channel.
                                    format!("[auto-rejected: {reason}]")
                                }
                                None => {
                                    // #1022 B10: `request_approval` returns `None`
                                    // when the command channel is closed (sub-agents
                                    // don't have a live channel to the user) or
                                    // cancelled. Distinguish the two so the model
                                    // gets actionable signal instead of a generic
                                    // "[cancelled]" that looked like the user
                                    // hit Ctrl+C.
                                    if cancel.is_cancelled() {
                                        "[cancelled]".to_string()
                                    } else {
                                        format!(
                                            "[auto-rejected: '{tool}' requires user \
                                             confirmation but this sub-agent has no \
                                             channel to the user. The parent agent \
                                             must pre-approve destructive operations \
                                             or run the tool itself.]",
                                            tool = tc.function_name,
                                        )
                                    }
                                }
                            }
                        }
                    }
                };

                db.insert_message(
                    &sub_session,
                    &Role::Tool,
                    Some(&output),
                    None,
                    Some(&tc.id),
                    None,
                )
                .await?;
            }
        }

        sink.emit(EngineEvent::Warn {
            message: format!(
                "Sub-agent '{agent_name}' hit its iteration limit ({}). Returning partial result.",
                loop_guard::MAX_SUB_AGENT_ITERATIONS
            ),
        });
        // Release workspace on iteration limit exit.
        if let Ok(Some(hint)) = workspace.release(&sub_session, &effective_root).await {
            sink.emit(EngineEvent::Info {
                message: format!("  \u{1f335} {agent_name}: {hint}"),
            });
        }
        // **#1022 B18**: pre-fix returned a free-text
        // `"(sub-agent reached maximum iterations)"` that was visually
        // success-shaped — indistinguishable from a normal sub-agent
        // answer. The parent model would treat it as a tool result
        // and either (a) try to keep working with the meaningless
        // string, or (b) re-issue the same `InvokeAgent` call,
        // burning another 20 LLM iterations to fail the same way.
        //
        // Two prongs to the fix:
        //
        // 1. **Bracketed-marker format** matches the established
        //    failure-result convention (`[cancelled by parent]`,
        //    `[cancelled]`, `[Background agent X failed]`). The
        //    `[ERROR: ...]` prefix is unambiguous to the model:
        //    "this is not a sub-agent answer, this is structural
        //    failure metadata." Includes the cap as a number so the
        //    model can reason about whether the task was decomposable.
        //    Format extracted to `iteration_cap_marker` so the
        //    contract can be unit-tested without standing up the
        //    full sub-agent harness.
        //
        // 2. **Cache the result** so a second identical call returns
        //    the same marker immediately instead of burning another
        //    20 iterations. Keyed by `(agent_name, prompt_hash)`,
        //    so a *reformulated* prompt is still attempted (parent
        //    can adapt). Cache is invalidated on any file mutation
        //    (see `SubAgentCache::invalidate`) so a write-then-retry
        //    flow naturally bypasses the marker. This makes the
        //    iteration-cap behave like every other sub-agent
        //    result — the cache is the right place to record
        //    "don't try this again unless something changed."
        let result = iteration_cap_marker(agent_name, loop_guard::MAX_SUB_AGENT_ITERATIONS);
        sub_agent_cache.put(agent_name, prompt, &result);
        Ok(result)
    }
}

// ── Workspace provider selection ────────────────────────────────────────────
//
// Two cfg-gated definitions of `pick_write_provider` rather than
// inline `cfg!()` branches because:
//
//  * `ClonefileProvider` is itself `cfg(target_os = "macos")` in
//    koda-sandbox; inline branches would still need cfg gating to
//    avoid "unresolved import" on Linux.
//  * Each platform's selection logic is small but distinct (macOS
//    has a fallback path, Linux doesn't), and side-by-side cfg
//    bodies read more honestly than a tangled inline form.
//
// Behavior is documented for users in `docs/src/sandbox.md`
// → "Workspace providers".

#[cfg(target_os = "macos")]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Try `ClonefileProvider` first — its 3-4× provision speedup
    // (Phase 4d / #934 bench) is durable and the implementation is
    // a thin wrapper over the OS primitive designed for this.
    //
    // Fall back to `GitWorktreeProvider` only if construction itself
    // fails (e.g. `$HOME` unset, project path can't canonicalize).
    // Runtime `clonefile(2)` failures (non-APFS volume etc.) surface
    // through the existing `provision()` error path — **#1022 B21**
    // makes that path short-circuit with a structural-failure marker
    // instead of silently dropping the sub-agent into the parent's
    // unisolated working tree (which would let parallel sub-agents
    // race on the same files).
    match ClonefileProvider::new(project_root) {
        Ok(p) => Box::new(p),
        Err(e) => {
            tracing::warn!("ClonefileProvider unavailable, falling back to git worktree: {e}");
            Box::new(GitWorktreeProvider::new(project_root, agent_name))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn pick_write_provider(
    project_root: &std::path::Path,
    agent_name: &str,
) -> Box<dyn WorkspaceProvider + Send + Sync> {
    // Linux + others: GitWorktreeProvider. The Linux CoW equivalent
    // (4e in #934) is parked until production telemetry shows it's
    // worth building — see #934 deferral comments.
    Box::new(GitWorktreeProvider::new(project_root, agent_name))
}

// \u2500\u2500 Iteration-cap marker (#1022 B18) \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

/// Build the structural-failure marker returned when a sub-agent
/// exhausts its iteration budget without producing a final answer.
///
/// Format follows the established bracketed-marker convention used
/// throughout the dispatch path (`[cancelled by parent]`,
/// `[cancelled]`, `[Background agent X failed]`). The `[ERROR:`
/// prefix is the unambiguous "this is metadata, not content" signal
/// to the model.
///
/// Extracted as a free function so the contract (must contain
/// `[ERROR:`, the agent name, the cap as a number, and a
/// re-strategize hint) can be regression-tested without standing up
/// the full sub-agent harness.
fn iteration_cap_marker(agent_name: &str, cap: usize) -> String {
    format!(
        "[ERROR: sub-agent '{agent_name}' exceeded its iteration cap of {cap} without producing \
         a final answer. Decompose the task into smaller pieces, or attempt the work \
         directly without delegating.]"
    )
}

/// **#1022 B21**: structural-failure marker returned when an
/// isolated sub-agent's workspace provider can't provision.
///
/// Replaces the pre-fix silent fallback to `project_root` — which
/// dropped the sub-agent into the parent's unisolated working
/// tree and let parallel sub-agents race on the same files.
///
/// Same format / `[ERROR:` prefix as `iteration_cap_marker` so
/// the model treats it as structural failure metadata rather
/// than a sub-agent answer. Includes the failure reason so the
/// parent can adapt (e.g. switch to a read-only sub-agent if
/// the underlying issue is non-APFS volume / no git repo).
fn workspace_provision_failure_marker(agent_name: &str, reason: &str) -> String {
    format!(
        "[ERROR: sub-agent '{agent_name}' could not provision an isolated workspace and was not \
         dispatched, to avoid corrupting the parent project tree (reason: {reason}). Either \
         resolve the workspace setup issue, retry without write tools, or attempt the work \
         directly without delegating.]"
    )
}

#[cfg(test)]
mod b18_tests {
    //! **#1022 B18** regression tests for the iteration-cap marker.
    //!
    //! These pin the *contract* of the marker string \u2014 not its
    //! exact wording, but the load-bearing pieces:
    //!
    //! - `[ERROR:` prefix so the model treats it as structural
    //!   failure metadata, not a sub-agent answer (the pre-fix
    //!   `"(sub-agent reached maximum iterations)"` had no such
    //!   marker and was indistinguishable from a normal result).
    //! - The agent name appears so multi-agent flows can identify
    //!   which sub-agent capped out.
    //! - The cap appears as a number so the model can reason about
    //!   decomposability ("did it run out of budget at 20 or 200?").
    //! - A re-strategize hint appears so the model has a concrete
    //!   next action instead of just retrying.
    //!
    //! The cache integration (`sub_agent_cache.put` two lines above
    //! the call) is verified by code review \u2014 it's adjacent to the
    //! marker construction and the call shape is obvious. A full
    //! e2e cap-and-retry test would require the mock provider to
    //! issue 21+ tool calls in a row; the cost/value ratio doesn't
    //! justify it for what's effectively a one-line cache write.
    use super::iteration_cap_marker;
    use crate::loop_guard::MAX_SUB_AGENT_ITERATIONS;

    #[test]
    fn marker_has_error_prefix() {
        let m = iteration_cap_marker("scout", 20);
        assert!(
            m.starts_with("[ERROR:"),
            "marker must start with `[ERROR:` so the model treats it \
             as structural failure metadata, not a sub-agent answer; \
             got: {m}"
        );
    }

    #[test]
    fn marker_includes_agent_name() {
        let m = iteration_cap_marker("scout", 20);
        assert!(
            m.contains("'scout'"),
            "marker must name the capped sub-agent; got: {m}"
        );
    }

    #[test]
    fn marker_includes_cap_as_number() {
        let m = iteration_cap_marker("scout", 20);
        assert!(
            m.contains("20"),
            "marker must include the cap as a number so the model can \
             reason about decomposability; got: {m}"
        );
    }

    #[test]
    fn marker_includes_restrategize_hint() {
        let m = iteration_cap_marker("scout", 20);
        // Either of the two suggested actions is fine \u2014 we just
        // want the model to see that it has options other than
        // retrying the same call.
        assert!(
            m.to_lowercase().contains("decompose")
                || m.to_lowercase().contains("attempt the work directly"),
            "marker must give the model a concrete next action; got: {m}"
        );
    }

    #[test]
    fn marker_uses_real_cap_constant() {
        // Sanity: the call site passes `MAX_SUB_AGENT_ITERATIONS`,
        // not a magic number. If someone refactors the constant to
        // a different name and forgets to update the call site, the
        // test wouldn't catch it directly \u2014 but this at least
        // documents the wired-in expectation.
        let m = iteration_cap_marker("agent", MAX_SUB_AGENT_ITERATIONS);
        assert!(m.contains(&MAX_SUB_AGENT_ITERATIONS.to_string()));
    }

    #[test]
    fn marker_is_single_line_for_tool_result_clarity() {
        // Sub-agent results land in `tool_call_id`-keyed message
        // content. Multi-line markers would format awkwardly in
        // the user-facing trace; single-line keeps the failure
        // visible at a glance. This catches a future "helpful"
        // refactor that spreads the message across lines.
        // content. Multi-line markers would format awkwardly in
        // the user-facing trace; single-line keeps the failure
        // visible at a glance. This catches a future "helpful"
        // refactor that spreads the message across lines.
        let m = iteration_cap_marker("scout", 20);
        assert!(
            !m.contains('\n'),
            "marker must be single-line for clean tool-result formatting; got:\n{m}"
        );
    }
}

#[cfg(test)]
mod b21_tests {
    //! **#1022 B21** regression tests for the workspace-provision-
    //! failure marker.
    //!
    //! Pre-fix, an `Err` from `workspace.provision()` silently fell
    //! back to `project_root.to_path_buf()` — dropping the sub-agent
    //! into the parent's unisolated working tree and letting parallel
    //! sub-agents race on the same files. The marker is the
    //! short-circuit that replaces that fallback for write-tool sub-
    //! agents.
    //!
    //! These tests pin the *contract* of the marker (not its exact
    //! wording):
    //!
    //! - `[ERROR:` prefix so the model treats it as structural
    //!   failure metadata, not a sub-agent answer.
    //! - The agent name appears so multi-agent flows can identify
    //!   which sub-agent's workspace failed.
    //! - The failure reason is included so the parent / user can
    //!   diagnose (non-APFS volume, no git repo, etc.).
    //! - A re-strategize hint so the model doesn't just retry the
    //!   same prompt.
    //! - Single-line so it formats cleanly as a tool result.
    //!
    //! End-to-end coverage of the dispatch short-circuit itself
    //! requires injecting a `WorkspaceProvider` into the sub-agent
    //! dispatch path, which is not currently a public seam. Until
    //! that refactor lands, the marker contract here plus a manual
    //! check on a non-APFS macOS volume is the regression net.

    use super::workspace_provision_failure_marker;

    #[test]
    fn marker_has_error_prefix() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.starts_with("[ERROR:"),
            "marker must start with `[ERROR:` so the model treats it as \
             structural failure, not a sub-agent answer; got: {m}"
        );
    }

    #[test]
    fn marker_includes_agent_name() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.contains("'writer'"),
            "marker must name the sub-agent that failed so multi-agent \
             flows can disambiguate; got: {m}"
        );
    }

    #[test]
    fn marker_includes_failure_reason() {
        // The reason is the actionable bit — if it's missing, the
        // user has no way to know whether it's a misconfigured
        // volume, missing git repo, or something else. This
        // guarantees the underlying `e.to_string()` reaches the
        // parent agent intact.
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            m.contains("clonefile: ENOTSUP"),
            "marker must include the failure reason verbatim so it's \
             diagnosable; got: {m}"
        );
    }

    #[test]
    fn marker_does_not_silently_dispatch() {
        // The whole point of B21: the marker explicitly states the
        // sub-agent was *not* dispatched. If a future refactor
        // re-introduces the silent fallback and forgets this
        // wording, the model loses the signal that nothing ran.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("not dispatched") || lower.contains("was not dispatched"),
            "marker must state the sub-agent was not dispatched, so the \
             parent doesn't assume the work happened; got: {m}"
        );
    }

    #[test]
    fn marker_includes_restrategize_hint() {
        // Without a hint, the model would tend to retry the same
        // prompt and hit the same provision failure. Same rationale
        // as the iteration-cap marker.
        let m = workspace_provision_failure_marker("writer", "x");
        let lower = m.to_lowercase();
        assert!(
            lower.contains("directly") || lower.contains("resolve") || lower.contains("retry"),
            "marker must hint at re-strategizing (e.g. resolve setup, \
             retry without write tools, do directly); got: {m}"
        );
    }

    #[test]
    fn marker_is_single_line() {
        let m = workspace_provision_failure_marker("writer", "clonefile: ENOTSUP");
        assert!(
            !m.contains('\n'),
            "marker must be single-line for clean tool-result formatting; got:\n{m}"
        );
    }
}

/// Phase E of #996 — RAII cleanup hook tests.
///
/// `InvocationCleanup`'s job: when a sub-agent invocation exits
/// (success, iteration cap, or error), **fire the cancel token** on
/// every bg-agent registry entry tagged with that invocation's
/// spawner id. The actual reaping (removal from `pending`) happens
/// later — either when the bg future observes its cancel token and
/// returns, then gets `drain_completed`'d, or when the registry's
/// own Drop impl aborts the JoinHandle. Either way, the guard's job
/// is just signalling.
///
/// Two contracts to pin:
///
///   1. **Drop fires cancel on matching entries.** Entries tagged with
///      `Some(my_invocation_id)` must observe their cancel token fire
///      after the guard drops.
///   2. **Drop leaves non-matching entries alone.** Entries tagged with
///      a *different* spawner id (sibling sub-agent, top-level, etc.)
///      must NOT observe their cancel token fire.
///
/// We test the guard in isolation rather than driving a full
/// `execute_sub_agent` because the function is too large to set up
/// in a unit test. The guard's behaviour is the single load-bearing
/// piece; everything else is plumbing the compiler already verified.
#[cfg(test)]
mod invocation_cleanup_tests {
    use super::InvocationCleanup;
    use crate::bg_agent::BgAgentRegistry;
    use std::sync::Arc;

    #[tokio::test]
    async fn drop_cancels_entries_tagged_with_matching_spawner() {
        let reg = Arc::new(BgAgentRegistry::new());
        // Tag two bg entries with our invocation id. The 4th tuple
        // element is a clone of the entry's cancel token — we use it
        // as an observer to detect the guard firing the cancel.
        let (_id_a, _tx_a, _status_tx_a, cancel_a) =
            reg.register_test_with_status("scout", "a", Some(7));
        let (_id_b, _tx_b, _status_tx_b, cancel_b) =
            reg.register_test_with_status("scout", "b", Some(7));
        assert!(
            !cancel_a.is_cancelled() && !cancel_b.is_cancelled(),
            "setup"
        );

        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            cancel_a.is_cancelled(),
            "entry A's cancel token must fire when guard drops"
        );
        assert!(
            cancel_b.is_cancelled(),
            "entry B's cancel token must fire when guard drops"
        );
    }

    #[tokio::test]
    async fn drop_leaves_entries_with_different_spawner_alone() {
        let reg = Arc::new(BgAgentRegistry::new());
        // Mix: one tagged with our id, one with a sibling's, one with None.
        let (_id_mine, _tx_m, _status_tx_m, cancel_mine) =
            reg.register_test_with_status("a", "mine", Some(7));
        let (_id_sib, _tx_s, _status_tx_s, cancel_sibling) =
            reg.register_test_with_status("a", "sibling", Some(8));
        let (_id_top, _tx_t, _status_tx_t, cancel_toplevel) =
            reg.register_test_with_status("a", "toplevel", None);

        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            cancel_mine.is_cancelled(),
            "my own (spawner=7) entry must be cancelled"
        );
        assert!(
            !cancel_sibling.is_cancelled(),
            "sibling sub-agent's (spawner=8) entry must NOT be cancelled"
        );
        assert!(
            !cancel_toplevel.is_cancelled(),
            "top-level (spawner=None) entry must NOT be cancelled"
        );
    }

    #[tokio::test]
    async fn drop_with_no_matching_entries_is_noop() {
        let reg = Arc::new(BgAgentRegistry::new());
        let (_id, _tx, _status_tx, cancel) = reg.register_test_with_status("a", "x", Some(99));

        // Cleanup for an invocation that never spawned anything —
        // common case: a sub-agent that did nothing bg-related.
        drop(InvocationCleanup {
            bg: &reg,
            invocation_id: 7,
        });

        assert!(
            !cancel.is_cancelled(),
            "unrelated entry's cancel token must not fire"
        );
    }
}
