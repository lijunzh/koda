//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.
//!
//! ## Loop flow
//!
//! ```text
//! User message
//!   → Build messages array (history + system prompt)
//!   → Stream response from provider
//!   → If tool calls:
//!       → Normalize tool names (handle model quirks)
//!       → Check approval (auto/confirm based on effect)
//!       → Execute tools (parallel when safe)
//!       → Append results to conversation
//!       → Loop (re-inference with tool results)
//!   → If text response:
//!       → Done — return to REPL
//! ```
//!
//! ## Key behaviors
//!
//! - **Streaming**: tokens are emitted as they arrive via `EngineSink`
//! - **Loop guard**: detects repeated identical tool calls and prompts the user
//! - **Auto-compact**: triggers compaction when context usage exceeds threshold
//! - **Microcompact**: ages old tool results between turns
//! - **Sub-agents**: `InvokeAgent` calls spawn a nested inference loop
//! - **Cancellation**: `Ctrl+C` cancels the current inference gracefully
//!
//! ## Design (DESIGN.md)
//!
//! - **Let the model drive (P3)**: The engine is a mechanical loop. It does
//!   not plan, verify, or make decisions — the model does. This loop streams
//!   the response, dispatches tool calls, and feeds results back.
//! - **Rate Limit Retry (P2)**: Exponential backoff for 429 errors. Long
//!   sessions with Opus hit rate limits regularly.

use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{EngineCommand, EngineEvent, EngineSink};
use crate::file_tracker::FileTracker;
use crate::inference_helpers::{
    AUTO_COMPACT_THRESHOLD, CONTEXT_WARN_THRESHOLD, RATE_LIMIT_MAX_RETRIES, assemble_messages,
    estimate_tokens, is_context_overflow_error, is_image_rejection_error,
    is_network_transient_error, is_rate_limit_error, is_server_error, rate_limit_backoff,
};
use crate::loop_guard::{LoopAction, LoopDetector};
use crate::persistence::Persistence;
use crate::providers::{
    ChatMessage, ImageData, LlmProvider, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
    stream_collector::SseCollector,
};
use crate::skill_scope::SkillToolScope;
use crate::tool_dispatch::{
    can_parallelize, execute_tools_parallel, execute_tools_sequential, execute_tools_split_batch,
};
use crate::tools::ToolRegistry;
use crate::trust::TrustMode;

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Inference loop helpers (tightly coupled to inference_loop — live here)
// ---------------------------------------------------------------------------

/// Per-iteration immutable context shared across inference helpers.
///
/// Bundles the parameters that `assemble_context`, `preflight_compact_if_needed`,
/// and `try_overflow_recovery` all share. Built at the top of each loop iteration
/// (since `system_message` and `iteration` change per turn).
struct TurnState<'a> {
    db: &'a Database,
    session_id: &'a str,
    system_message: &'a ChatMessage,
    pending_images: Option<&'a [ImageData]>,
    iteration: u32,
    config: &'a KodaConfig,
    provider: &'a dyn LlmProvider,
    tool_defs: &'a [ToolDefinition],
    sink: &'a dyn EngineSink,
    cancel: &'a CancellationToken,
}

/// Result of collecting a streamed LLM response.
struct StreamResult {
    /// Accumulated text content from the response.
    text: String,
    /// All thinking/reasoning content produced during the stream, in order.
    ///
    /// Empty string when the model produced no thinking blocks (non-Claude
    /// providers, or Claude with thinking disabled). Persisted to the DB so
    /// it can be re-rendered on session resume.
    thinking_content: String,
    /// Tool calls requested by the model.
    tool_calls: Vec<ToolCall>,
    /// Results from tools executed eagerly during streaming.
    ///
    /// Contains `(tool_call_id, output, success, full_output)` for each
    /// read-only auto-approved tool that finished before the stream ended.
    /// These tools are skipped during normal dispatch.
    eager_results: Vec<(String, String, bool, Option<String>)>,
    /// Token usage statistics.
    usage: TokenUsage,
    /// Total character count of text deltas.
    char_count: usize,
    /// Whether the stream was interrupted by user cancellation (Ctrl+C).
    interrupted: bool,
    /// Whether the stream ended due to a network error.
    ///
    /// When `true` the partial response MUST be discarded — it is incomplete
    /// and storing it would corrupt the session history on resume.
    network_error: Option<String>,
}

/// Load conversation history, assemble messages with the system prompt,
/// attach pending images (first iteration only), and update context tracking.
///
/// This is the single source of truth for context assembly — called on initial
/// build, after pre-flight compaction, and after overflow recovery.
async fn assemble_context(turn: &TurnState<'_>) -> Result<Vec<ChatMessage>> {
    let history = turn.db.load_context(turn.session_id).await?;

    // Run per-tool context analysis for smarter compaction decisions.
    // Logged at debug level; will be surfaced in `/usage` and used by
    // microcompact (#636 P1) once that lands.
    let analysis = crate::context_analysis::analyze_context(&history);
    if analysis.total > 0 {
        tracing::debug!(
            "Context analysis: {} total, {}% tool results, {}% duplicate reads",
            analysis.total,
            analysis.tool_result_percent(),
            analysis.duplicate_read_percent(),
        );
        for (tool, tokens) in analysis.top_tool_results(3) {
            tracing::debug!("  {tool}: ~{tokens} tokens");
        }
    }

    let mut messages = assemble_messages(turn.system_message, &history);

    // Attach pending images to the last user message (first iteration only)
    if turn.iteration == 0
        && let Some(imgs) = turn.pending_images
        && !imgs.is_empty()
        && let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user")
    {
        last_user.images = Some(imgs.to_vec());
    }

    let context_used = estimate_tokens(&messages);
    crate::context::update(context_used, turn.config.max_context_tokens);
    turn.sink.emit(EngineEvent::ContextUsage {
        used: context_used,
        max: turn.config.max_context_tokens,
    });

    // Warn users when approaching the context limit (headless mode silently
    // drops ContextUsage events, so this Warn is the only signal they get).
    let ctx_pct = crate::context::percentage();
    if (CONTEXT_WARN_THRESHOLD..AUTO_COMPACT_THRESHOLD).contains(&ctx_pct) {
        // Include analysis hints so the user knows *why* context is high.
        let mut warning = format!("Context at {ctx_pct}% — approaching limit.");
        let top = analysis.top_tool_results(2);
        if !top.is_empty() {
            let hogs: Vec<String> = top
                .iter()
                .map(|(name, tokens)| format!("{name} (~{tokens} tok)"))
                .collect();
            warning.push_str(&format!(" Top consumers: {}.", hogs.join(", ")));
        }
        let waste = analysis.total_duplicate_waste();
        if waste > 500 {
            warning.push_str(&format!(" ~{waste} tokens wasted on duplicate file reads."));
        }
        warning.push_str(" Run /compact to free up space.");
        turn.sink.emit(EngineEvent::Warn { message: warning });
    }

    Ok(messages)
}

/// Pre-flight budget check: if context usage exceeds the threshold, compact
/// before sending to the provider. Re-assembles context after successful compaction.
///
/// Returns the (possibly updated) message vec.
async fn preflight_compact_if_needed(
    turn: &TurnState<'_>,
    messages: Vec<ChatMessage>,
) -> Result<Vec<ChatMessage>> {
    let ctx_pct = crate::context::percentage();
    if ctx_pct < AUTO_COMPACT_THRESHOLD {
        return Ok(messages);
    }

    // Circuit breaker: stop wasting API calls after repeated failures
    if crate::compact::is_compact_circuit_broken() {
        tracing::warn!("Pre-flight: context at {ctx_pct}% but circuit breaker tripped — skipping");
        return Ok(messages);
    }

    tracing::warn!("Pre-flight: context at {ctx_pct}%, attempting auto-compact");
    turn.sink.emit(EngineEvent::Info {
        message: format!("\u{1f4e6} Context at {ctx_pct}% \u{2014} compacting before sending..."),
    });

    match crate::compact::compact_session_with_provider(
        turn.db,
        turn.session_id,
        turn.config.max_context_tokens,
        &turn.config.model_settings,
        turn.provider,
    )
    .await
    {
        Ok(Ok(result)) => {
            turn.sink.emit(EngineEvent::Info {
                message: format!(
                    "\u{2705} Compacted {} messages (~{} token summary)",
                    result.deleted, result.summary_tokens
                ),
            });
            assemble_context(turn).await
        }
        Ok(Err(skip)) => {
            tracing::info!("Pre-flight compact skipped: {skip:?}");
            if matches!(skip, crate::compact::CompactSkip::HistoryTooLarge) {
                crate::compact::record_compact_failure();
                turn.sink.emit(EngineEvent::Warn {
                    message: "\u{26a0}\u{fe0f} Context is full but history is too large for \
                              this model to summarize. Start a new session (/session) or \
                              switch to a model with a larger context window."
                        .to_string(),
                });
            }
            Ok(messages)
        }
        Err(e) => {
            tracing::warn!("Pre-flight compact failed: {e:#}");
            let tripped = crate::compact::record_compact_failure();
            let suffix = if tripped {
                " Auto-compact disabled after repeated failures."
            } else {
                " Continuing anyway..."
            };
            turn.sink.emit(EngineEvent::Warn {
                message: format!("Compact failed: {e:#}.{suffix}"),
            });
            Ok(messages)
        }
    }
}

/// Attempt to start a chat stream with exponential backoff on retryable errors.
///
/// Retries on:
/// - Rate-limit / overload signals (`is_rate_limit_error`)
/// - Transient network failures (`is_network_transient_error`) — idle
///   read timeouts, connection resets, broken pipes, DNS hiccups, etc.
///   These are the dominant failure mode on long reasoning turns where the
///   model server goes silent past `KODA_READ_TIMEOUT_SECS`. See #1119.
///
/// Returns `Ok(Some(rx))` on success, `Ok(None)` if cancelled during retries,
/// or `Err` for non-retriable failures.
async fn try_with_rate_limit(
    provider: &dyn LlmProvider,
    messages: &[ChatMessage],
    tool_defs: &[ToolDefinition],
    model_settings: &crate::config::ModelSettings,
    cancel: &CancellationToken,
    sink: &dyn EngineSink,
) -> Result<Option<SseCollector>> {
    let mut last_err = None;
    for attempt in 0..RATE_LIMIT_MAX_RETRIES {
        let result = tokio::select! {
            result = provider.chat_stream(messages, tool_defs, model_settings) => result,
            _ = cancel.cancelled() => return Ok(None),
        };
        match result {
            Ok(collector) => return Ok(Some(collector)),
            Err(e)
                if (is_rate_limit_error(&e) || is_network_transient_error(&e))
                    && attempt + 1 < RATE_LIMIT_MAX_RETRIES =>
            {
                let delay = rate_limit_backoff(attempt);
                let kind = if is_rate_limit_error(&e) {
                    "Rate limited"
                } else {
                    "Network glitch"
                };
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "\u{23f3} {kind}. Retrying in {}s (attempt {}/{})...",
                        delay.as_secs(),
                        attempt + 1,
                        RATE_LIMIT_MAX_RETRIES
                    ),
                });
                tracing::warn!(
                    "{} (attempt {}/{}): {e:#}",
                    kind,
                    attempt + 1,
                    RATE_LIMIT_MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                sink.emit(EngineEvent::SpinnerStart {
                    message: format!("Retrying (attempt {})...", attempt + 2),
                });
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Retries exhausted")))
}

/// Recover from a context overflow error: compact the session, re-assemble
/// context, and retry the provider call once.
///
/// Returns `Ok(Some((rx, messages)))` on success (receiver + updated messages),
/// `Ok(None)` if cancelled during retry, or `Err` if compaction/retry fails.
async fn try_overflow_recovery(
    turn: &TurnState<'_>,
    original_err: anyhow::Error,
) -> Result<Option<(SseCollector, Vec<ChatMessage>)>> {
    turn.sink.emit(EngineEvent::SpinnerStop);
    turn.sink.emit(EngineEvent::Warn {
        message: "\u{26a0}\u{fe0f} Provider rejected request (context overflow). \
             Compacting and retrying..."
            .to_string(),
    });
    tracing::warn!("Context overflow from provider: {original_err:#}");

    match crate::compact::compact_session_with_provider(
        turn.db,
        turn.session_id,
        turn.config.max_context_tokens,
        &turn.config.model_settings,
        turn.provider,
    )
    .await
    {
        Ok(Ok(result)) => {
            turn.sink.emit(EngineEvent::Info {
                message: format!(
                    "\u{2705} Compacted {} messages. Retrying...",
                    result.deleted
                ),
            });
        }
        _ => {
            return Err(original_err)
                .context("LLM inference failed (context overflow, compaction unsuccessful)");
        }
    }

    let messages = assemble_context(turn).await?;

    turn.sink.emit(EngineEvent::SpinnerStart {
        message: "Retrying...".into(),
    });
    let collector = tokio::select! {
        result = turn.provider.chat_stream(&messages, turn.tool_defs, &turn.config.model_settings) => {
            result.context("LLM inference failed after compaction retry")?
        }
        _ = turn.cancel.cancelled() => return Ok(None),
    };
    Ok(Some((collector, messages)))
}

/// Collect a streamed LLM response, executing read-only tools eagerly.
///
/// When a `ToolCallReady` event arrives (Anthropic `content_block_stop`),
/// and the tool is read-only + auto-approved, it executes immediately while
/// subsequent tool call arguments are still being streamed. This overlaps
/// tool execution with LLM generation time — the key latency optimization
/// from Claude Code's `StreamingToolExecutor` pattern.
///
/// Handles thinking → response state transitions, cancellation via `CancellationToken`,
/// and spinner lifecycle. Returns a `StreamResult` — the caller is responsible for
/// persistence and early-return on interruption.
async fn collect_stream(
    rx: &mut mpsc::Receiver<StreamChunk>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
    tools: &ToolRegistry,
    mode: TrustMode,
    project_root: &Path,
) -> StreamResult {
    let mut full_text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut eager_results: Vec<(String, String, bool, Option<String>)> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut first_token = true;
    let mut char_count: usize = 0;
    // Permanent accumulator — never cleared, flows into StreamResult.
    let mut thinking_content = String::new();
    // True while we are inside a thinking block (between ThinkingStart and ThinkingDone).
    let mut in_thinking_block = false;
    let mut response_banner_shown = false;
    let mut thinking_banner_shown = false;
    let mut interrupted = false;

    loop {
        let chunk = tokio::select! {
            c = rx.recv() => c,
            _ = cancel.cancelled() => {
                interrupted = true;
                None
            }
        };

        if interrupted || cancel.is_cancelled() {
            sink.emit(EngineEvent::SpinnerStop);
            if !full_text.is_empty() {
                sink.emit(EngineEvent::TextDone);
            }
            sink.emit(EngineEvent::Warn {
                message: "Interrupted".into(),
            });
            return StreamResult {
                text: full_text,
                thinking_content,
                tool_calls,
                eager_results,
                usage,
                char_count,
                interrupted: true,
                network_error: None,
            };
        }

        let Some(chunk) = chunk else { break };

        match chunk {
            StreamChunk::TextDelta(delta) => {
                if first_token {
                    if in_thinking_block {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingDone);
                        in_thinking_block = false;
                        thinking_banner_shown = true;
                    }
                    sink.emit(EngineEvent::SpinnerStop);
                    first_token = false;
                }

                if !response_banner_shown && !delta.trim().is_empty() {
                    sink.emit(EngineEvent::ResponseStart);
                    response_banner_shown = true;
                }

                full_text.push_str(&delta);
                char_count += delta.len();
                sink.emit(EngineEvent::TextDelta {
                    text: delta.clone(),
                });
            }
            StreamChunk::ThinkingDelta(delta) => {
                if !thinking_banner_shown {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingStart);
                    thinking_banner_shown = true;
                }
                in_thinking_block = true;
                sink.emit(EngineEvent::ThinkingDelta {
                    text: delta.clone(),
                });
                thinking_content.push_str(&delta);
            }
            StreamChunk::ToolCallReady(tc) => {
                // A single tool call finished streaming (Anthropic content_block_stop).
                // If it's read-only and auto-approved, execute it now while
                // subsequent tool calls are still being streamed.
                if in_thinking_block {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    in_thinking_block = false;
                }
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_default();
                let is_read_only = !tools.catalog().is_mutating_call(&tc.function_name, &args);
                let is_auto_approved = !matches!(
                    crate::trust::check_tool(&tc.function_name, &args, mode, Some(project_root),),
                    crate::trust::ToolApproval::NeedsConfirmation
                        | crate::trust::ToolApproval::Blocked
                );

                if is_read_only && is_auto_approved && tc.function_name != "InvokeAgent" {
                    // Execute eagerly — read-only tools are fast (10–50ms),
                    // the channel buffers incoming chunks while we run.
                    tracing::debug!("Eager dispatch: {} (id={})", tc.function_name, tc.id);
                    // Eager dispatch only fires for read-only tools (filtered
                    // above). Bash is never read-only, so `caller_spawner=None`
                    // is correct here — there is no Bash path to mis-tag.
                    let r = tools
                        .execute(&tc.function_name, &tc.arguments, None, None)
                        .await;
                    eager_results.push((tc.id.clone(), r.output, r.success, r.full_output));
                }
                // Always add to tool_calls for persistence and normal flow
                tool_calls.push(tc);
            }
            StreamChunk::ToolCalls(tcs) => {
                if in_thinking_block {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    in_thinking_block = false;
                }
                sink.emit(EngineEvent::SpinnerStop);
                // Append — some tool calls may already be in the list from ToolCallReady
                tool_calls.extend(tcs);
            }
            StreamChunk::Done(u) => {
                if in_thinking_block {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    // `in_thinking_block` not cleared — loop breaks immediately.
                }
                usage = u;
                break;
            }
            StreamChunk::NetworkError(err) => {
                // Connection dropped mid-stream. Stop rendering and surface a
                // warning. The partial response will be discarded by the caller.
                sink.emit(EngineEvent::SpinnerStop);
                if !full_text.is_empty() {
                    sink.emit(EngineEvent::TextDone);
                }
                sink.emit(EngineEvent::Warn {
                    message: format!("Connection lost mid-stream — turn discarded ({err})"),
                });
                return StreamResult {
                    text: full_text,
                    thinking_content,
                    tool_calls,
                    eager_results,
                    usage,
                    char_count,
                    interrupted: false,
                    network_error: Some(err),
                };
            }
        }
    }

    sink.emit(EngineEvent::TextDone);

    if first_token {
        sink.emit(EngineEvent::SpinnerStop);
    }

    StreamResult {
        text: full_text,
        thinking_content,
        tool_calls,
        eager_results,
        usage,
        char_count,
        interrupted: false,
        network_error: None,
    }
}

// ---------------------------------------------------------------------------
// Inference loop
// ---------------------------------------------------------------------------

/// All parameters for the inference loop, bundled into a single struct.
pub struct InferenceContext<'a> {
    /// Project root directory.
    pub project_root: &'a Path,
    /// Global configuration.
    pub config: &'a KodaConfig,
    /// Database handle for message persistence.
    pub db: &'a Database,
    /// Current session identifier.
    pub session_id: &'a str,
    /// System prompt for this session.
    pub system_prompt: &'a str,
    /// LLM provider to use.
    pub provider: &'a dyn LlmProvider,
    /// Tool registry with all available tools.
    pub tools: &'a ToolRegistry,
    /// Pre-computed tool definitions sent to the LLM.
    pub tool_defs: &'a [ToolDefinition],
    /// Images attached to the current prompt (consumed on first turn).
    pub pending_images: Option<Vec<ImageData>>,
    /// Current trust mode.
    pub mode: TrustMode,
    /// Event sink for streaming output to the client.
    pub sink: &'a dyn EngineSink,
    /// Cancellation token for graceful interruption.
    pub cancel: CancellationToken,
    /// Channel for receiving client commands (approval responses, etc.).
    pub cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    /// File lifecycle tracker for ownership-aware approval (#465).
    pub file_tracker: &'a mut FileTracker,
    /// Background sub-agent registry (#1022 B12). Owned by [`crate::session::KodaSession`]
    /// so bg agents survive across turns; this is just a borrow into
    /// the loop. Drained at the top of every iteration to inject
    /// completed bg results into the conversation.
    pub bg_agents: &'a std::sync::Arc<crate::child_agent::ChildAgentRegistry>,
    /// Cross-turn sub-agent result cache (#1022 B12). Owned by [`crate::session::KodaSession`].
    /// Generation-based invalidation on mutating tool calls is
    /// honored uniformly via `crate::tool_dispatch::execute_one_tool`.
    pub sub_agent_cache: &'a crate::sub_agent_cache::SubAgentCache,
}

/// Run the inference loop: send messages, stream responses, dispatch tool calls.
///
/// This is a thin wrapper around the private `inference_loop_inner`
/// that always finalizes the undo stack's pending snapshots on return — regardless
/// of whether the inner loop completed, errored, or was cancelled
/// (#1264). Without this, file mutations made by tools during the
/// turn would stay in `pending` forever and never become an undoable
/// entry, so the `/undo` slash command would always report "Nothing
/// to undo" no matter how many files had been written.
///
/// `commit_turn` is a no-op when `pending` is empty, so turns with
/// zero file mutations don't grow the undo stack.
#[tracing::instrument(skip_all, fields(session_id = %ctx.session_id, agent = %ctx.config.agent_name))]
pub async fn inference_loop(ctx: InferenceContext<'_>) -> Result<()> {
    // `&ToolRegistry` is `Copy`, so capturing here doesn't move `ctx`.
    // We need this handle to call `commit_turn` after the inner
    // function returns and releases its borrow on `ctx.tools`.
    let tools = ctx.tools;
    let result = inference_loop_inner(ctx).await;
    if let Ok(mut undo) = tools.undo.lock() {
        undo.commit_turn();
    }
    result
}

async fn inference_loop_inner(ctx: InferenceContext<'_>) -> Result<()> {
    let InferenceContext {
        project_root,
        config,
        db,
        session_id,
        system_prompt,
        provider,
        tools,
        tool_defs,
        pending_images,
        mode,
        sink,
        cancel,
        cmd_rx,
        file_tracker,
        bg_agents,
        sub_agent_cache,
    } = ctx;

    // #1108 P1b: wrap the user-facing sink with `PersistingSink` so
    // every `Info` / `ChildTaskUpdate` event lands in `session_events`
    // for the markdown transcript export. Pre-#1108 these were
    // sink-only and disappeared once the user closed the TUI.
    let _persisting_sink = crate::engine::sink::PersistingSink::new(
        sink,
        std::sync::Arc::new(db.clone()) as std::sync::Arc<dyn crate::persistence::Persistence>,
        session_id.to_string(),
        None,
    );
    let sink: &dyn EngineSink = &_persisting_sink;

    // #1265 item 4 PR-2: collect the per-turn ambient state into a
    // single `TurnContext` so the dispatch helpers don't each take
    // 11–13 explicit args. Reconstructed each turn (`mode` can change
    // between turns), but borrows the same long-lived locals from the
    // destructure above.
    let turn_ctx = crate::turn_context::TurnContext::new(
        project_root,
        config,
        db,
        session_id,
        sink,
        cancel.clone(),
        sub_agent_cache,
        bg_agents,
        mode,
        tools,
    );
    // Phase E of #996: top-level inference has no spawner identity —
    // bg-task tools see the global scope. Sub-agents construct their
    // own `ToolExecutionContext` with `Some(parent_spawner)` inside
    // `sub_agent_dispatch`.
    let tool_ctx = crate::turn_context::ToolExecutionContext::new(&turn_ctx, None);

    // Hard cap is configurable per-agent; user can extend it interactively.
    let mut hard_cap = config.max_iterations;
    let mut iteration = 0u32;
    let mut made_tool_calls = false;
    let mut retried_empty = false;
    let mut loop_detector = LoopDetector::new();
    // #1022 B12: `bg_agents` and `sub_agent_cache` now live on
    // `KodaSession` and are borrowed in via `InferenceContext`. Local
    // construction here was the root cause of the cross-turn drop bug:
    // when this function returned, the `Arc<ChildAgentRegistry>` dropped,
    // every still-pending bg task was aborted by `AbortOnDropHandle`,
    // and the result was lost. Owning on the session means the
    // registry survives across turns and the next call drains anything
    // that completed during the idle gap.
    let mut skill_scope = SkillToolScope::new();
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;
    let mut total_cache_read_tokens: i64 = 0;
    let mut total_thinking_tokens: i64 = 0;
    let mut total_char_count: usize = 0;
    let loop_start = Instant::now();

    // Pre-build the base system message (avoids re-cloning 4-8KB per iteration)
    let base_system_prompt = system_prompt.to_string();

    // Microcompact: clear old tool results before the first LLM call.
    // Time-based trigger — only fires when the idle gap since the last
    // assistant message exceeds the threshold (not during active tool use).
    if let Ok(Some(mc)) = crate::microcompact::microcompact_session(db, session_id).await {
        sink.emit(EngineEvent::Info {
            message: format!(
                "\u{1f9f9} Microcompact: cleared {} old tool results (~{} tokens)",
                mc.cleared, mc.tokens_saved,
            ),
        });
    }

    loop {
        // #1321: bg-task status events normally flow continuously
        // through the session-spawned forwarder (see
        // `KodaSession::attach_event_sink`). When that forwarder is
        // active, `drain_status_events()` is a benign empty-vec
        // no-op (the forwarder owns the receiver) and this loop
        // does nothing.
        //
        // For clients that don't / can't attach a session-lifetime
        // sink — headless mode (per-turn `HeadlessSink`), the ACP
        // server (per-turn `AcpSink` with per-turn `session_id`),
        // tests, and any external embedder using `inference_loop`
        // directly — this drain is the only way bg-status events
        // reach the sink. It still incurs the pre-#1321 latency
        // bound (events visible only between iterations, parked
        // for the duration of any in-flight tool call) but that's
        // strictly no-worse than the pre-#1321 baseline for those
        // call sites. Migrate them later by exposing a session-
        // lifetime sink and calling `attach_event_sink`.
        for ev in bg_agents.drain_status_events() {
            sink.emit(ev);
        }

        // Inject completed background agent results as user messages
        for bg_result in bg_agents.drain_completed() {
            let status = if bg_result.success {
                "completed"
            } else {
                "failed"
            };
            let injection = format!(
                "[Background agent '{}' {status}]\n\
                 Original task: {}\n\
                 Result:\n{}",
                bg_result.agent_name, bg_result.prompt, bg_result.output
            );
            // #1022 B9: surface the bg agent's narrative trace so the
            // user can see what it did, not just that it finished.
            // Pre-fix this was a single-line "✅ X completed" with
            // *no visibility* into intermediate tool calls (bg agents
            // ran with NullSink). Now the trace is appended as
            // bullet-formatted lines under the completion header.
            // Trace goes to the *user* via Info; the model sees the
            // result via the injected user message above (which is
            // intentionally trace-free — the model already chose
            // those tool calls and doesn't need to re-read its own
            // history).
            let mut msg = format!(
                "  \u{2705} Background agent '{}' {status}",
                bg_result.agent_name
            );
            if !bg_result.events.is_empty() {
                msg.push('\n');
                msg.push_str(&bg_result.events.join("\n"));
            }
            sink.emit(EngineEvent::Info { message: msg });
            // #1108 P2a: persist each captured bg-agent trace line as
            // a `sub_agent_event` row keyed by the parent's
            // `InvokeAgent` tool_call_id. Pre-#1108 the trace was
            // sink-only — the markdown transcript export had no
            // record of what the bg agent did. With this in place the
            // transcript renderer can fold the trace under the
            // parent's `InvokeAgent` tool result.
            if let Some(parent_call_id) = bg_result.parent_tool_call_id.as_deref() {
                use crate::persistence::session_event_kind as sek;
                for line in &bg_result.events {
                    // Fire-and-forget: a DB hiccup must not crash a
                    // turn just because we couldn't persist a trace
                    // line. Same policy as `PersistingSink::persist`.
                    if let Err(e) = db
                        .insert_session_event(
                            session_id,
                            sek::SUB_AGENT_EVENT,
                            line,
                            Some(parent_call_id),
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %e, session_id, parent_call_id,
                            "failed to persist bg-agent trace line"
                        );
                    }
                }
            }
            // #1159: write the completion as a `Role::Tool` row keyed on the
            // parent's `InvokeAgent` tool_call_id, NOT as a synthetic
            // `Role::User` message.
            //
            // Why the change:
            //   - Pre-fix, completed bg-agent results landed as fake user
            //     turns, polluting the parent's context and breaking the
            //     `tool_use_id` ↔ `tool_result_id` pairing invariant that
            //     providers (especially Anthropic) prefer.
            //   - The `InvokeAgent` dispatch already emitted a synchronous
            //     "started (agent:N)" tool_result on the dispatch turn.
            //     This second tool_result with the *same* `tool_call_id`
            //     overwrites that stub at read time via
            //     `dedupe_tool_results_by_call_id` in `load_context`
            //     (logical update via append + dedupe).
            //
            // Fallback: tests that bypass the dispatch path have no
            // `parent_tool_call_id` — keep the old user-message shape
            // for those callers so existing test fixtures don't break.
            if let Some(parent_call_id) = bg_result.parent_tool_call_id.as_deref() {
                db.insert_tool_message_with_full(
                    session_id,
                    &injection,
                    parent_call_id,
                    &injection,
                )
                .await?;
            } else {
                db.insert_message(session_id, &Role::User, Some(&injection), None, None, None)
                    .await?;
            }
        }

        // Drain any `QueueNext` inputs the client sent during the previous
        // iteration ("mid-turn steer" lane).  We non-blockingly drain the
        // whole channel, collect texts, and batch-insert one user message
        // before re-querying the provider.
        //
        // Other command types (ApprovalResponse, AskUserResponse, LoopDecision)
        // are only ever sent while the engine is actively blocking on cmd_rx
        // inside their respective select! arms, so they will not be present
        // here at iteration start.  Any unexpected commands are logged and
        // discarded — they are benign at this position.
        {
            let mut next_texts: Vec<String> = Vec::new();
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    EngineCommand::QueueNext { text } => next_texts.push(text),
                    other => {
                        // #1232 §6: name the variant instead of
                        // dumping `Discriminant(N)` (which forces
                        // devs to grep the source to figure out
                        // which `EngineCommand` integer N maps to)
                        // — and use `.kind()` rather than `{:?}` so
                        // potentially-sensitive payload fields
                        // (e.g. `AskUserResponse.answer`,
                        // `QueueNext.text`) stay out of logs.
                        tracing::warn!(
                            "inference_loop: unexpected command at iteration start (discarded): {}",
                            other.kind()
                        );
                    }
                }
            }
            if !next_texts.is_empty() {
                let combined = next_texts.join("\n\n");
                sink.emit(EngineEvent::Info {
                    message: format!(
                        "  \u{1f4e5} Injecting {} steer{} into current turn",
                        next_texts.len(),
                        if next_texts.len() == 1 { "" } else { "s" },
                    ),
                });
                db.insert_message(session_id, &Role::User, Some(&combined), None, None, None)
                    .await?;
            }
        }

        if iteration >= hard_cap {
            let recent = loop_detector.recent_names();
            sink.emit(EngineEvent::LoopCapReached {
                cap: hard_cap,
                recent_tools: recent,
            });

            // Wait for client decision via EngineCommand::LoopDecision
            let extra = loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(EngineCommand::LoopDecision { action }) => {
                            break action.extra_iterations();
                        }
                        Some(EngineCommand::Interrupt) => {
                            cancel.cancel();
                            break 0;
                        }
                        None => break 0,
                        _ => continue,
                    },
                    _ = cancel.cancelled() => break 0,
                }
            };

            if extra == 0 {
                break Ok(());
            }
            hard_cap += extra;
        }

        // Build system prompt with git context.
        //
        // (#1077 Phase B) Progress and todo sections are no longer
        // injected here. Per `DESIGN.md § Progress Tracking:
        // Model-Owned, History-Persisted, Engine-Surfaced`, the model
        // owns its own plan via `TodoWrite`, the conversation history
        // persists it (the tool calls are themselves in the message
        // stream), and the engine surfaces transitions via
        // `EngineEvent::TodoUpdate`. Re-injecting either was the
        // anti-pattern this issue removes — every reference project
        // (claude_code_src / codex / zed / gemini-cli) abstains.
        // Compaction (`compact.rs`) preserves outstanding tasks and
        // file paths verbatim, which is the durable defense against
        // context-window forgetting.
        let git_line = crate::git::git_context(project_root)
            .map(|ctx| format!("\n{ctx}"))
            .unwrap_or_default();
        let system_prompt_full = format!("{base_system_prompt}{git_line}");
        let system_message = ChatMessage::text("system", &system_prompt_full);

        // Apply skill-scoped tool filtering: when a skill with `allowed_tools`
        // is active, only those tools (+ meta-tools) are sent to the LLM.
        let scoped_tool_defs = skill_scope.filter_tool_defs(tool_defs);

        let active_tool_defs: &[ToolDefinition] = &scoped_tool_defs;

        // Build per-iteration immutable context for helpers
        let turn = TurnState {
            db,
            session_id,
            system_message: &system_message,
            pending_images: pending_images.as_deref(),
            iteration,
            config,
            provider,
            tool_defs: active_tool_defs,
            sink,
            cancel: &cancel,
        };

        // Assemble context (load history, attach images, track usage)
        let messages = assemble_context(&turn).await?;

        // Pre-flight budget check: if context is critically high, compact first
        let messages = preflight_compact_if_needed(&turn, messages).await?;

        // Track whether this turn carried image attachments so we can surface
        // a targeted warning if the provider rejects them.
        let had_images = turn
            .pending_images
            .map(|imgs| !imgs.is_empty())
            .unwrap_or(false);

        // Stream the response (with rate limit retry)
        sink.emit(EngineEvent::SpinnerStart {
            message: "Thinking...".into(),
        });

        let stream_result = try_with_rate_limit(
            provider,
            &messages,
            active_tool_defs,
            &config.model_settings,
            &cancel,
            sink,
        )
        .await;

        // Handle cancellation during rate limit retries
        let stream_result: Result<SseCollector> = match stream_result {
            Ok(Some(c)) => Ok(c),
            Ok(None) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: "Interrupted".into(),
                });
                return Ok(());
            }
            Err(e) => Err(e),
        };

        // Graceful recovery: if the provider returns a context-overflow error,
        // compact and retry once before giving up.
        let SseCollector {
            mut rx,
            handle: sse_handle,
        } = match stream_result {
            Ok(c) => c,
            Err(e) if is_context_overflow_error(&e) => {
                match try_overflow_recovery(&turn, e).await? {
                    Some((rx, _updated)) => rx,
                    None => {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::Warn {
                            message: "Interrupted".into(),
                        });
                        return Ok(());
                    }
                }
            }
            Err(e) if is_server_error(&e) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Provider returned a server error: {e:#}. \
                         This often means the model can't handle the current \
                         conversation state. Try a different model or start a new session."
                    ),
                });
                return Ok(());
            }
            Err(e) if had_images && is_image_rejection_error(&e) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "⚠ This model rejected the image attachment — \
                         it likely does not support vision input. \
                         Switch to a vision-capable model such as \
                         claude-sonnet, gemini-flash, or gpt-4o. ({e})"
                    ),
                });
                return Ok(());
            }
            Err(e) => {
                return Err(e).context("LLM inference failed");
            }
        };

        // Collect the streamed response
        let stream_result = collect_stream(&mut rx, sink, &cancel, tools, mode, project_root).await;

        if stream_result.interrupted {
            // Kill the background HTTP reader immediately so the TCP
            // connection closes and the server (LM Studio, vLLM, or any single-slot
            // server) can accept the next request (#825).
            sse_handle.abort();
            let has_text = !stream_result.text.is_empty();
            let has_thinking = !stream_result.thinking_content.is_empty();
            if has_text || has_thinking {
                let mid = db
                    .insert_message(
                        session_id,
                        &Role::Assistant,
                        if has_text {
                            Some(stream_result.text.as_str())
                        } else {
                            None
                        },
                        None,
                        None,
                        None,
                    )
                    .await?;
                if has_thinking {
                    db.update_message_thinking_content(mid, &stream_result.thinking_content)
                        .await?;
                }
            }
            return Ok(());
        }

        // Network drop: warning already emitted by collect_stream.
        // Discard the partial response — storing it would corrupt the session.
        if stream_result.network_error.is_some() {
            sse_handle.abort();
            return Ok(());
        }

        let full_text = stream_result.text;
        let stream_thinking = stream_result.thinking_content;
        // Normalize tool names from model output to canonical PascalCase (#548).
        // Models may emit lowercase or snake_case names ("list", "read_file").
        // This runs for all providers — the canonical fast-path is a single
        // HashMap lookup — and must happen here (not in providers) so dispatch,
        // approval, loop guard, undo, and persistence all see consistent
        // canonical names.
        let tool_calls = crate::tool_normalize::normalize_tool_calls(stream_result.tool_calls);
        let usage = stream_result.usage;
        let char_count = stream_result.char_count;

        // Empty response after tool use — retry once before giving up.
        if tool_calls.is_empty()
            && made_tool_calls
            && full_text.trim().is_empty()
            && usage.stop_reason != "max_tokens"
            && !retried_empty
        {
            retried_empty = true;
            sink.emit(EngineEvent::SpinnerStart {
                message: "Empty response — retrying...".into(),
            });
            continue;
        }

        // Persist the assistant response
        let content = if full_text.is_empty() {
            None
        } else {
            Some(full_text.as_str())
        };
        let tool_calls_json = if tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tool_calls)?)
        };

        let msg_id = db
            .insert_message(
                session_id,
                &Role::Assistant,
                content,
                tool_calls_json.as_deref(),
                None,
                Some(&usage),
            )
            .await?;

        // Mark the message as fully delivered. This distinguishes clean
        // completions from interrupted/in-progress turns on session resume.
        db.mark_message_complete(msg_id).await?;

        // Persist thinking content produced by Claude extended thinking.
        // Only set for assistant messages from models with thinking enabled;
        // all other providers leave this empty and we skip the UPDATE.
        if !stream_thinking.is_empty() {
            db.update_message_thinking_content(msg_id, &stream_thinking)
                .await?;
        }

        // If no tool calls, we already streamed the response — done
        if tool_calls.is_empty() {
            if usage.stop_reason == "max_tokens" {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} hit max_tokens limit — response was truncated. \
                         The context may be too large. Try /compact or start a new session.",
                        config.model,
                    ),
                });
                continue;
            } else if made_tool_calls && full_text.trim().is_empty() {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} produced an empty response after tool use. \
                         Try rephrasing, run /compact, or switch models with /model.",
                        config.model,
                    ),
                });
            }
            // `last_prompt_tokens` (this iteration's prompt size) drives the
            // context-window % meter: it reflects current context occupancy.
            // `total_prompt_tokens` (cumulative across iterations) drives the
            // Footer billing line. Conflating them caused #946 — see below.
            let last_prompt_tokens = usage.prompt_tokens;
            total_prompt_tokens += usage.prompt_tokens;
            total_completion_tokens += usage.completion_tokens;
            total_cache_read_tokens += usage.cache_read_tokens;
            total_thinking_tokens += usage.thinking_tokens;
            total_char_count += char_count;

            let display_tokens = if total_completion_tokens > 0 {
                total_completion_tokens
            } else {
                (total_char_count / 4) as i64
            };

            let total_elapsed = loop_start.elapsed();
            let total_secs = total_elapsed.as_secs_f64();
            let rate = if total_secs > 0.0 && display_tokens > 0 {
                display_tokens as f64 / total_secs
            } else {
                0.0
            };

            let context = crate::context::format_footer();

            // Correct the heuristic context estimate with the actual
            // prompt_tokens reported by the provider.  The estimate emitted
            // in assemble_context() was based on chars/3.5, which consistently
            // underreports for code, tool results, and JSON payloads.
            // This corrective event fires once per completed turn, after all
            // tool calls resolve, so the status bar reflects real usage.
            //
            // We use `last_prompt_tokens` (most recent iteration) rather than
            // `total_prompt_tokens` (cumulative across iterations) because the
            // meter measures *current context-window occupancy*, not cumulative
            // billing. Summing iterations would double-count the shared history
            // and let the meter exceed 100% on multi-tool-call turns (#946).
            crate::context::update(last_prompt_tokens as usize, config.max_context_tokens);
            sink.emit(EngineEvent::ContextUsage {
                used: last_prompt_tokens as usize,
                max: config.max_context_tokens,
            });

            sink.emit(EngineEvent::Footer {
                prompt_tokens: total_prompt_tokens,
                completion_tokens: total_completion_tokens,
                cache_read_tokens: total_cache_read_tokens,
                thinking_tokens: total_thinking_tokens,
                total_chars: total_char_count,
                elapsed_ms: total_elapsed.as_millis() as u64,
                rate,
                context,
            });

            return Ok(());
        }

        // Accumulate token usage across iterations.
        // (`last_prompt_tokens` is scoped to the no-tool-calls terminating
        // branch above — the only reader of it. Each loop iteration creates
        // its own binding when that branch runs, so there's nothing to update
        // here for the meter.)
        total_prompt_tokens += usage.prompt_tokens;
        total_completion_tokens += usage.completion_tokens;
        total_cache_read_tokens += usage.cache_read_tokens;
        total_thinking_tokens += usage.thinking_tokens;
        total_char_count += char_count;

        made_tool_calls = true;

        // Record results from eagerly-executed tools (dispatched during streaming)
        let eager_ids: std::collections::HashSet<String> = stream_result
            .eager_results
            .iter()
            .map(|(id, _, _, _)| id.clone())
            .collect();

        if !eager_ids.is_empty() {
            tracing::info!(
                "{} tool(s) executed eagerly during streaming",
                eager_ids.len()
            );
            for (tc_id, result, success, full_output) in &stream_result.eager_results {
                // Find the matching ToolCall for metadata
                if let Some(tc) = tool_calls.iter().find(|tc| tc.id == *tc_id) {
                    sink.emit(EngineEvent::ToolCallStart {
                        id: tc_id.clone(),
                        name: tc.function_name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        is_sub_agent: false,
                    });
                    crate::tool_dispatch::record_tool_result(
                        tc,
                        result,
                        *success,
                        full_output.as_deref(),
                        &turn_ctx,
                        file_tracker,
                    )
                    .await?;
                }
            }
        }

        // Filter out eagerly-executed tools from the remaining dispatch
        let remaining_tools: Vec<ToolCall> = tool_calls
            .iter()
            .filter(|tc| !eager_ids.contains(&tc.id))
            .cloned()
            .collect();

        // Skill scope enforcement: reject tool calls blocked by the active scope.
        // Blocked tools get an error result recorded without execution.
        let remaining_tools = if skill_scope.is_active() {
            let mut allowed = Vec::with_capacity(remaining_tools.len());
            for tc in remaining_tools {
                if let Some(err_msg) = skill_scope.check_tool_call(&tc.function_name) {
                    let parsed_args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                    sink.emit(EngineEvent::ToolCallStart {
                        id: tc.id.clone(),
                        name: tc.function_name.clone(),
                        args: parsed_args,
                        is_sub_agent: false,
                    });
                    crate::tool_dispatch::record_tool_result(
                        &tc,
                        &err_msg,
                        false,
                        None,
                        &turn_ctx,
                        file_tracker,
                    )
                    .await?;
                } else {
                    allowed.push(tc);
                }
            }
            allowed
        } else {
            remaining_tools
        };

        // Execute remaining tool calls — parallelize when possible.
        // #1022 B13: pass `file_tracker` so the parallelizability check
        // sees the same Koda-owned-file downgrade the sequential path
        // sees. Without it, batches containing `Delete owned.tmp` are
        // spuriously refused parallelization (perf regression).
        if remaining_tools.len() > 1
            && can_parallelize(&remaining_tools, mode, project_root, Some(file_tracker))
        {
            execute_tools_parallel(&remaining_tools, tool_ctx, file_tracker).await?;
        } else if remaining_tools.len() > 1 {
            execute_tools_split_batch(&remaining_tools, tool_ctx, cmd_rx, file_tracker).await?;
        } else if !remaining_tools.is_empty() {
            execute_tools_sequential(&remaining_tools, tool_ctx, cmd_rx, file_tracker).await?;
        }

        // Update skill scope: if any ActivateSkill call was made, check whether
        // the newly activated skill has allowed_tools.
        {
            let scope_calls: Vec<(String, serde_json::Value)> = tool_calls
                .iter()
                .map(|tc| {
                    let args: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_default();
                    (tc.function_name.clone(), args)
                })
                .collect();
            let was_active = skill_scope.is_active();
            skill_scope.update_from_tool_calls(&scope_calls, &tools.skill_registry);
            // Log scope transitions
            match (was_active, skill_scope.is_active()) {
                (false, true) => {
                    sink.emit(EngineEvent::Info {
                        message: "\u{1f512} Skill tool scope activated — tool set restricted"
                            .into(),
                    });
                }
                (true, false) => {
                    sink.emit(EngineEvent::Info {
                        message: "\u{1f513} Skill tool scope cleared — full tool set restored"
                            .into(),
                    });
                }
                _ => {}
            }
        }

        // Loop detection: consecutive identical tool calls → feedback or stop.
        // Modeled after Gemini CLI: first detection injects a nudge message,
        // second detection (model ignored feedback) hard-stops.
        match loop_detector.record(&tool_calls) {
            LoopAction::Ok => {}
            LoopAction::InjectFeedback(detail) => {
                tracing::warn!(%detail, "Loop detected — injecting feedback");
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Loop detected: {detail}. Injecting feedback to nudge the model."
                    ),
                });
                // Inject a system-style user message to redirect the model.
                db.insert_message(
                    session_id,
                    &Role::User,
                    Some(&format!(
                        "System: Potential loop detected — {detail}. \
                         Please take a step back and confirm you're making forward progress. \
                         If not, analyze your previous actions and try a different approach. \
                         Avoid repeating the same tool calls without new results."
                    )),
                    None,
                    None,
                    None,
                )
                .await?;
                loop_detector.clear_after_feedback();
                // Continue the loop — give the model a chance to recover
            }
            LoopAction::HardStop(detail) => {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Loop guard: {detail} — model ignored feedback, stopping. \
                         This often means the current model isn't capable enough for the \
                         task; consider switching to a stronger model. \
                         Send a follow-up message to continue."
                    ),
                });
                break Ok(());
            }
        }

        iteration += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sink::TestSink;
    use crate::providers::{StreamChunk, TokenUsage, ToolCall};
    use crate::trust::TrustMode;
    use tokio::sync::mpsc;

    /// Helper: create a ToolRegistry backed by a temp directory.
    fn test_tools(root: &Path) -> ToolRegistry {
        ToolRegistry::new(root.to_path_buf(), 100_000)
    }

    /// Helper: send chunks into a channel and collect_stream them.
    async fn run_collect(
        chunks: Vec<StreamChunk>,
        cancel: Option<CancellationToken>,
    ) -> StreamResult {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = TestSink::new();
        let cancel = cancel.unwrap_or_default();
        let tmp = tempfile::tempdir().unwrap();
        let tools = test_tools(tmp.path());

        // Send all chunks in a background task.
        tokio::spawn(async move {
            for chunk in chunks {
                let _ = tx.send(chunk).await;
            }
            // tx drops here → stream ends
        });

        collect_stream(&mut rx, &sink, &cancel, &tools, TrustMode::Auto, tmp.path()).await
    }

    // ── Text streaming ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_accumulates_text_deltas() {
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("Hello ".into()),
                StreamChunk::TextDelta("world!".into()),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.text, "Hello world!");
        assert!(!result.interrupted);
        assert!(result.network_error.is_none());
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.char_count, 12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_empty_stream_returns_empty() {
        let result = run_collect(vec![StreamChunk::Done(TokenUsage::default())], None).await;

        assert!(result.text.is_empty());
        assert!(!result.interrupted);
        assert!(result.tool_calls.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_preserves_usage_from_done() {
        let usage = TokenUsage {
            prompt_tokens: 42,
            completion_tokens: 17,
            stop_reason: "end_turn".into(),
            ..Default::default()
        };
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("hi".into()),
                StreamChunk::Done(usage),
            ],
            None,
        )
        .await;

        assert_eq!(result.usage.prompt_tokens, 42);
        assert_eq!(result.usage.completion_tokens, 17);
        assert_eq!(result.usage.stop_reason, "end_turn");
    }

    // ── Thinking blocks ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_thinking_then_text() {
        let result = run_collect(
            vec![
                StreamChunk::ThinkingDelta("Let me think...".into()),
                StreamChunk::TextDelta("Answer!".into()),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        // Thinking content must be captured in the dedicated field.
        assert_eq!(result.thinking_content, "Let me think...");
        // Thinking deltas should NOT appear in the text output.
        assert_eq!(result.text, "Answer!");
    }

    // ── Tool calls ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_tool_calls_batch() {
        let tc = ToolCall {
            id: "tc_1".into(),
            function_name: "Bash".into(),
            arguments: r#"{"command":"echo hi"}"#.into(),
            thought_signature: None,
        };
        let result = run_collect(
            vec![
                StreamChunk::ToolCalls(vec![tc]),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function_name, "Bash");
        assert!(result.text.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_eager_executes_read_only_tool() {
        // Read is read-only + auto-approved → should be eagerly executed.
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("hello.txt");
        std::fs::write(&test_file, "file content").unwrap();

        let tc = ToolCall {
            id: "tc_eager".into(),
            function_name: "Read".into(),
            arguments: serde_json::json!({"file_path": test_file.to_string_lossy()}).to_string(),
            thought_signature: None,
        };

        let (tx, mut rx) = mpsc::channel(32);
        let sink = TestSink::new();
        let cancel = CancellationToken::new();
        let tools = test_tools(tmp.path());

        tokio::spawn(async move {
            let _ = tx.send(StreamChunk::ToolCallReady(tc)).await;
            let _ = tx.send(StreamChunk::ToolCalls(vec![])).await;
            let _ = tx.send(StreamChunk::Done(TokenUsage::default())).await;
        });

        let result =
            collect_stream(&mut rx, &sink, &cancel, &tools, TrustMode::Auto, tmp.path()).await;

        assert_eq!(result.tool_calls.len(), 1, "tool call should be recorded");
        assert_eq!(result.eager_results.len(), 1, "should have 1 eager result");
        let (id, output, success, _) = &result.eager_results[0];
        assert_eq!(id, "tc_eager");
        assert!(output.contains("file content"), "eager result: {output}");
        assert!(success);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_does_not_eagerly_execute_mutating_tool() {
        // Write is mutating → should NOT be eagerly executed.
        let tc = ToolCall {
            id: "tc_write".into(),
            function_name: "Write".into(),
            arguments: r#"{"file_path":"/tmp/x","content":"y"}"#.into(),
            thought_signature: None,
        };
        let result = run_collect(
            vec![
                StreamChunk::ToolCallReady(tc),
                StreamChunk::ToolCalls(vec![]),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.tool_calls.len(), 1);
        assert!(
            result.eager_results.is_empty(),
            "Write should NOT be eagerly executed"
        );
    }

    // ── Cancellation ─────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_cancellation_sets_interrupted() {
        use std::sync::Arc;
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let (tx, mut rx) = mpsc::channel(32);
        // **#1109 F3**: Arc the sink so the spawned producer can
        // observe when the consumer has emitted the partial chunk.
        // Replaces the old `sleep(50ms)` polling pattern (the only
        // remaining wall-clock-budget test in this file) with the
        // signal-based `TestSink::wait_for` primitive added in F3.
        let sink = Arc::new(TestSink::new());
        let tmp = tempfile::tempdir().unwrap();
        let tools = test_tools(tmp.path());

        let sink_for_producer = Arc::clone(&sink);
        tokio::spawn(async move {
            // 1. Send the partial chunk.
            let _ = tx.send(StreamChunk::TextDelta("partial".into())).await;

            // 2. Wait until `collect_stream` has actually consumed it
            //    (signalled by the TextDelta event reaching the sink).
            //    Was: sleep(50ms) — fine on a laptop, racy on macOS CI.
            sink_for_producer
                .wait_for(std::time::Duration::from_secs(5), |ev| {
                    matches!(
                        ev,
                        crate::engine::EngineEvent::TextDelta { text } if text == "partial"
                    )
                })
                .await
                .expect("consumer should emit TextDelta within 5s");

            // 3. Now safe to cancel: we know `partial` is in the result.
            cancel_clone.cancel();

            // 4. Try to send another chunk after cancel — it should
            //    NOT appear in the final result. No sleep needed: the
            //    main task's `collect_stream.await` will have returned
            //    by the time we reach the assertions, so the post-
            //    cancel send races nothing.
            let _ = tx.send(StreamChunk::TextDelta(" ignored".into())).await;
        });

        let result = collect_stream(
            &mut rx,
            sink.as_ref(),
            &cancel,
            &tools,
            TrustMode::Auto,
            tmp.path(),
        )
        .await;

        assert!(result.interrupted);
        assert!(result.network_error.is_none());
        // Partial text should be captured up to cancellation.
        assert!(result.text.contains("partial"));
        // Defensive: the post-cancel send must not leak into result.text.
        assert!(
            !result.text.contains("ignored"),
            "post-cancel chunk leaked into result.text: {:?}",
            result.text
        );
    }

    // ── Network errors ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_network_error_preserves_partial() {
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("partial response".into()),
                StreamChunk::NetworkError("connection reset".into()),
            ],
            None,
        )
        .await;

        assert!(!result.interrupted);
        assert_eq!(result.network_error.as_deref(), Some("connection reset"));
        assert_eq!(result.text, "partial response");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collect_stream_network_error_with_no_text() {
        let result = run_collect(vec![StreamChunk::NetworkError("timeout".into())], None).await;

        assert!(result.text.is_empty());
        assert!(result.network_error.is_some());
    }
}
