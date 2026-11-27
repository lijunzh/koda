//! TUI shared context — the mutable state struct for the event loop.
//!
//! ## Module layout
//!
//! - **mod.rs** — struct definition, constructor, lifecycle, command dispatch
//! - **events.rs** — keyboard, mouse, paste, clipboard, history input handling
//! - **menus.rs** — dropdown pickers, menu navigation, wizard state machines

pub(crate) mod background;
mod events;
pub(crate) mod goal;
mod mcp;
mod memory;
mod menus;
mod plan;
mod reconnect;
mod requests;
mod review;
mod session_cmds;
pub(crate) mod settings;
mod side;
pub(crate) mod skills;

use crate::input;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_commands::{self, SlashAction};
use crate::tui_render::TuiRenderer;
use crate::tui_types::{MenuContent, PromptMode, ProviderWizard, Term, TuiState};
use crate::tui_viewport::{draw_viewport, init_terminal, restore_terminal};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_client::protocol::state::TrustMode;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::composer::keymap::RuntimeKeymap;
use crate::composer::textarea::TextArea;

/// All mutable TUI state, extracted from `run()`'s local variables.
///
/// Methods are split across submodules by concern:
/// - **mod.rs** — lifecycle (new, draw, cleanup, event loop, dispatch)
/// - **events.rs** — keyboard, mouse, paste, clipboard, history
/// - **menus.rs** — dropdown pickers, menu navigation, wizards
pub(crate) struct TuiContext {
    // ── UI state ────────────────────────────────────────────
    pub terminal: Term,
    pub textarea: TextArea,
    pub renderer: TuiRenderer,
    pub scroll_buffer: ScrollBuffer,
    pub crossterm_events: EventStream,

    // ── Interaction state ─────────────────────────────────────
    pub tui_state: TuiState,
    pub menu: MenuContent,
    pub prompt_mode: PromptMode,
    pub provider_wizard: Option<ProviderWizard>,
    pub pending_approval_id: Option<koda_client::protocol::rpc::RequestId>,
    /// The host's requests waiting for the one on screen to be answered:
    /// several sub-agents may ask at once.
    pub queued_requests: std::collections::VecDeque<koda_client::protocol::ServerRequest>,

    // ── Control flow ──────────────────────────────────────────
    /// Text pasted into the input box as `[Pasted text #N]` markers.
    pub pastes: input::Pastes,
    /// Images pasted or dropped into the input box, each with its
    /// `[Image #N]` marker; sent with the prompt while the marker stays.
    pub pasted_images: Vec<(String, input::ImageData)>,
    /// Images attached this session, so each marker's number is new.
    pub image_count: usize,
    pub pending_command: Option<String>,
    /// Messages queued with `Tab` during a turn, sent one per turn once the
    /// running one completes.
    pub(crate) queued: crate::queued::Queue,
    pub should_quit: bool,
    /// #1216: Ctrl+C-arm timestamp. When the user presses Ctrl+C at
    /// idle with nothing to interrupt (empty composer, no bg work),
    /// we *don't* exit immediately — that's a one-keystroke loss of
    /// session state, easy to fat-finger. Instead we record the press
    /// here and show a hint; a second Ctrl+C within
    /// [`QUIT_ARM_WINDOW`] confirms exit. Reset on any other action.
    pub quit_armed_at: Option<std::time::Instant>,
    pub inference_start: Option<std::time::Instant>,
    /// Whether the working area shows the turn's plan under its status
    /// line; Ctrl+T hides and shows it, as Claude Code's task list.
    pub plan_shown: bool,
    /// How much of the context window is left, in percent, from the last
    /// `session/tokenUsage/updated` (Codex's reading).
    pub context_left: u32,
    /// The context window the host reports when it differs from the
    /// config's, as after an overflow taught it the server's real one. The
    /// turn borrows `config`, so it is adopted after the turn
    /// (`post_turn_cleanup`).
    pub learned_context_window: Option<usize>,
    /// What was typed, oldest first, for Up and Down.
    pub history: Vec<String>,
    /// The pastes behind each history entry's markers, by entry.
    pub history_pastes: Vec<Vec<(String, String)>>,
    pub history_idx: Option<usize>,
    /// Where [`Self::history`] is kept: a file the TUI owns, as Codex's TUI
    /// keeps `history.jsonl`.
    pub input_history: crate::input_history::InputHistory,
    pub completer: crate::completer::InputCompleter,
    /// The files `@` mentions offer, found by the host.
    pub(crate) file_search: crate::file_search::FileSearch,
    pub mouse_selection: Option<crate::mouse_select::Selection>,
    /// Y-coordinate of the history area top edge (for mouse mapping).
    pub history_area_y: u16,
    /// Height of the history area in rows.
    pub history_area_height: u16,

    /// Non-bracketed paste-burst detector (#1186). Buffers fast-typed
    /// runs that look like a paste from a terminal that doesn't emit
    /// bracketed-paste sequences (older terminals, some SSH setups,
    /// tmux without `set -g set-clipboard on`). When the burst flushes
    /// (via the idle-loop timer in [`TuiContext::run`]), the accumulated
    /// text is dropped into the textarea as a single insert — cheaper
    /// than re-rendering after every fast keystroke and avoids paste
    /// content being interpreted as a sequence of submits when newlines
    /// are involved.
    pub paste_burst: crate::composer::paste_burst::PasteBurst,

    // ── Session state (shared references) ────────────────────
    /// What the session runs with, as the host reports it.
    pub settings: koda_client::protocol::session::SessionSettings,
    /// The host's providers, for `/provider` and `/key`.
    pub providers: crate::providers::Providers,
    /// The skills a person can load with `$name`, for the `$` and `@`
    /// dropdowns. Skills are found when the session starts.
    pub skills: Vec<koda_client::protocol::skills::SkillMetadata>,
    pub session: TuiSession,
    /// Koda's host, in process: it runs the session's turns.
    pub client: koda_client::InProcessClient,
    /// The session open on the host. When a command has switched
    /// `session.id`, the TUI opens that one before the next turn.
    pub(crate) host_session: String,
    /// The sessions picker forks the session picked rather than resuming
    /// it (`koda fork` without a session).
    pub(crate) fork_on_pick: bool,
    /// The side conversation the TUI is in (`/side`), with the main
    /// session it left.
    pub(crate) side: Option<side::SideState>,
    /// `/side` typed while a turn ran: its question and the turn left
    /// running.
    pub(crate) side_from_turn: Option<(String, String)>,
    /// Ctrl+C on an empty prompt in a side conversation: go back.
    pub(crate) leave_side_requested: bool,
    /// A finished review's results, for the main session's next prompt.
    pub(crate) pending_review: Option<String>,
    /// How the last turn ended, and its final reply.
    pub(crate) last_turn: Option<(koda_client::protocol::turn::TurnStatus, Option<String>)>,
    /// The session's mode, as the host reports it.
    pub mode: TrustMode,
    /// The host's sandbox, from `state/read`: whether Auto is possible.
    pub sandbox: koda_client::protocol::state::SandboxState,
    pub project_root: PathBuf,
    /// Koda's first run here, which opens the provider picker: no config
    /// folder yet, and nothing on the command line chose the provider
    /// (`app.rs` has the rule).
    pub first_run: bool,

    /// Coalescing draw scheduler (#1138). Replaces the old per-iteration
    /// synchronous `terminal.draw()` calls in the inference hot loop.
    /// `frame_requester` is the producer side; clone freely. `draw_rx` is
    /// the consumer side and lives on the inference loop's select.
    pub frame_requester: crate::frame_requester::FrameRequester,
    pub draw_rx: crate::frame_requester::DrawNotifyRx,

    /// What the activity overlay above the status bar shows (#1210): the
    /// turn's sub-agents, from its `ChildTaskUpdate` and
    /// `ChildAgentActivity` events, and the background processes, from
    /// `session/backgroundTerminals/list`. See
    /// [`crate::child_activity::ChildActivityTracker`].
    pub child_activity: crate::child_activity::ChildActivityTracker,
    /// The session's MCP servers as the host reports them, for the status
    /// bar.
    pub(crate) mcp: mcp::McpView,
    /// The MCP view is for a session no longer open: list again.
    pub(crate) mcp_stale: bool,
    /// The session's goal as the host last reported it.
    pub(crate) goal: Option<koda_client::protocol::goal::Goal>,
    /// Where the host is, for reconnecting to it.
    pub(crate) backend: crate::backend::Backend,
    /// The connection to the shared server is lost and reconnecting gave
    /// up: the next prompt tries again first.
    pub(crate) offline: bool,
    /// A turn was running when the connection was lost: said once the TUI
    /// is back, if the server didn't keep it.
    pub(crate) turn_cut: bool,
    /// The newer Koda the server said it is making way for
    /// (`serve/updating`): this window can't go on with it, so once the
    /// server goes it says to start koda again rather than reconnecting.
    pub(crate) updated_to: Option<String>,
}

/// The TUI's view of its session. The host owns the session and runs its
/// turns; the TUI keeps what it still reads and writes directly while idle
/// (history, `/compact`, resume, titles), as the 5c plan settled.
#[derive(Clone)]
pub struct TuiSession {
    /// The open session's id. A command that switches sessions changes it,
    /// and the TUI opens that session on the host before its next turn.
    pub id: String,
    /// The instruction files the host loaded for it, in order, as of its
    /// opening (`instructionSources`), for `/memory`.
    pub instruction_sources: Vec<String>,
}

/// Open session `resume`, or a new one, on the host, with what `named`
/// gives; the host fills in the rest (the saved choice, or a resumed
/// session's own). A resumed session comes with its earlier turns. Returns
/// the mode it opened in, too.
pub(crate) async fn open_host_session(
    client: &koda_client::InProcessClient,
    resume: Option<String>,
    named: &settings::Named,
) -> Result<(
    TuiSession,
    Vec<koda_client::protocol::turn::Turn>,
    Option<koda_client::protocol::state::TrustMode>,
)> {
    use koda_client::protocol::methods;
    use koda_client::protocol::session::{SessionResumeParams, SessionStartParams};
    // This client's folder: on the shared server, the server's own is
    // another's. The run's `-c` values keep a run in-process.
    let cwd = Some(client.project_root().display().to_string());
    let opened = match resume {
        // Codex's `thread/resume`: the session keeps its provider.
        Some(session_id) => client
            .request::<methods::SessionResume>(SessionResumeParams {
                session_id,
                base_url: named.base_url.clone(),
                model: named.model.clone(),
                cwd,
                ..Default::default()
            })
            .await
            .map(|resumed| resumed.session),
        None => client
            .request::<methods::SessionStart>(SessionStartParams {
                model_provider: named.provider.clone(),
                base_url: named.base_url.clone(),
                model: named.model.clone(),
                cwd,
                ..Default::default()
            })
            .await
            .map(|started| started.session),
    };
    let session = opened.map_err(|e| anyhow::anyhow!("{}", opening_refused(e)))?;
    let instruction_sources = session.instruction_sources.unwrap_or_default();
    Ok((
        TuiSession {
            id: session.id,
            instruction_sources,
        },
        session.turns,
        session.mode,
    ))
}

/// Why the host didn't open a session, in words for the person: a session
/// another Koda process writes is said as Codex's TUI says it ("open in
/// another app"), with what to do.
fn opening_refused(e: koda_client::RequestError) -> String {
    match e {
        koda_client::RequestError::Server { error, .. }
            if koda_state::writer_lock::is_active_writer(&error.message) =>
        {
            format!(
                "This session is open in another Koda process: the background server, which \
                 keeps a session for a minute after its TUI closes, a `koda exec` run, or a \
                 TUI started with --no-daemon or --oss. Wait a minute or close it there, then \
                 try again. ({})",
                error.message
            )
        }
        other => other.to_string(),
    }
}

/// Fork session `source` on the host (`session/fork`) and open the copy,
/// for `koda fork`, as [`open_host_session`] opens one, with what `named`
/// gives over the source's own, as `codex fork` takes `-m`.
pub(crate) async fn fork_host_session(
    client: &koda_client::InProcessClient,
    source: &str,
    named: &settings::Named,
) -> Result<(
    TuiSession,
    Vec<koda_client::protocol::turn::Turn>,
    Option<koda_client::protocol::state::TrustMode>,
)> {
    use koda_client::protocol::{methods, session::SessionForkParams};
    let forked = client
        .request::<methods::SessionFork>(SessionForkParams {
            session_id: source.to_string(),
            approval_policy: None,
            model_provider: named.provider.clone(),
            base_url: named.base_url.clone(),
            model: named.model.clone(),
            // This client's folder: on the shared server, the server's own
            // is another's. The run's `-c` values keep a run in-process.
            cwd: Some(client.project_root().display().to_string()),
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("couldn't fork session {source}: {e}"))?;
    let instruction_sources = forked.session.instruction_sources.unwrap_or_default();
    Ok((
        TuiSession {
            id: forked.session.id,
            instruction_sources,
        },
        forked.session.turns,
        forked.session.mode,
    ))
}

/// Draw `turns`, a session's earlier turns, through the live renderer, and
/// say so if the last was interrupted.
pub(crate) fn replay_turns(
    renderer: &mut TuiRenderer,
    turns: &[koda_client::protocol::turn::Turn],
    buffer: &mut ScrollBuffer,
) {
    use koda_client::protocol::item::Item;
    use koda_client::protocol::turn::TurnStatus;
    for turn in turns {
        renderer.replay_turn(turn, buffer);
    }
    let Some(last) = turns.last() else {
        return;
    };
    // Between the earlier turns and the new ones.
    buffer.push_lines([
        ratatui::text::Line::default(),
        ratatui::text::Line::styled(
            "  \u{2500}\u{2500}\u{2500} session resumed \u{2500}\u{2500}\u{2500}",
            crate::tui_output::DIM,
        ),
        ratatui::text::Line::default(),
    ]);
    if last.status == TurnStatus::Interrupted {
        // Its prompt when nothing came of it; nothing when something did.
        let unanswered = match last.items.as_slice() {
            [only] => match &only.item {
                Item::UserMessage { content, .. } => {
                    Some(koda_client::protocol::turn::text_of(content))
                }
                _ => None,
            },
            _ => None,
        };
        buffer.push_lines(crate::tui_output::interrupted_turn_banner(
            unanswered.as_deref(),
        ));
    }
}

/// Draw session `session_id`'s earlier turns, read with `session/read`, as
/// `/resume` and the sessions picker switch to it; as Codex's picker
/// replays the thread it opens.
pub(crate) async fn show_earlier_turns(
    client: &koda_client::InProcessClient,
    session_id: &str,
    renderer: &mut TuiRenderer,
    buffer: &mut ScrollBuffer,
) {
    use koda_client::protocol::{methods, session::SessionReadParams};
    // Another session's replies, for `/copy`, come with its turns.
    renderer.forget_replies();
    let read = client
        .request::<methods::SessionRead>(SessionReadParams {
            session_id: session_id.to_string(),
            include_turns: true,
        })
        .await;
    match read {
        Ok(read) => replay_turns(renderer, &read.session.turns, buffer),
        Err(e) => {
            crate::tui_output::warn_msg(buffer, format!("Couldn't read its earlier turns: {e}"))
        }
    }
}

/// What a turn starts with, and what to give back if it can't start.
pub(crate) struct TurnInput {
    /// The message: the prompt as typed, with pastes.
    pub message: String,
    pub images: Vec<crate::input::ImageData>,
    /// The paths mentioned with `@`, which the host reads.
    pub mentions: Vec<String>,
    /// What the user typed, and the pastes behind its markers, for the
    /// input box.
    pub typed: String,
    pub pastes: Vec<(String, String)>,
    /// The pasted images and their markers, for the input box too.
    pub pasted_images: Vec<(String, crate::input::ImageData)>,
}

/// What a turn on the host does.
pub(crate) enum TurnRequest {
    /// Answer a prompt.
    Prompt(TurnInput),
    /// Compact the session (`/compact`).
    Compact,
    /// Follow a turn already running, as after a side conversation, with
    /// the requests it made meanwhile.
    Watch {
        turn_id: String,
        waiting: Vec<koda_client::events::Outgoing>,
    },
}

/// Outcome of dispatching a single command.
pub(crate) enum CommandOutcome {
    /// No input was available to dispatch.
    NoInput,
    /// Command was fully handled (slash command, dropdown opened, etc.).
    Handled,
    /// Non-slash input — ready to start an inference turn.
    StartInference(TurnInput),
    /// `/compact`: the host compacts the session as a turn.
    Compact,
    /// /quit was invoked.
    Quit,
}

impl TuiContext {
    /// Initialize all TUI state. Call before entering the event loop.
    ///
    /// This handles provider setup, auto-detection, terminal init,
    /// onboarding, and everything that `run()` used to do before `loop {`.
    pub async fn new(
        project_root: PathBuf,
        named: settings::Named,
        resume: Option<crate::session_id::Resume>,
        fork: bool,
        version_check: tokio::task::JoinHandle<Option<String>>,
        first_run: bool,
        backend: crate::backend::Backend,
    ) -> Result<Self> {
        // Input history is the TUI's own file. The first time, what Koda kept
        // in its database before comes over.
        let home = koda_config::config_dir()?;
        let input_history = crate::input_history::InputHistory::in_dir(&home);
        if !input_history.exists() {
            let earlier = koda_client::earlier_input_history(&home).await;
            if let Err(e) = input_history.import(earlier) {
                tracing::warn!("couldn't bring the input history over: {e}");
            }
        }
        let history = input_history.load_entries();

        // Koda's host runs the session's turns and builds each session's
        // agent (its tools, MCP servers and prompt); the TUI is its client,
        // on the shared server or in this process.
        let (mut client, backend_note) =
            crate::backend::client(&backend, &project_root, &home).await?;
        // `named` is what the flags gave, and nothing else: the host fills
        // in the rest from the saved choice for a new session, or a resumed
        // one's own. A provider goes by its id.
        let providers = crate::providers::Providers::fetch(&client).await;
        let mut named = named;
        if let Some(typed) = named.provider.take() {
            named.provider = Some(providers.id_for(&typed));
        }
        // A mistyped id (once, a llama-server pid) used to run a session
        // that didn't exist; now it stops the start.
        let resume = match &resume {
            Some(resume) => Some(crate::session_id::resolve_target(&client, resume).await?),
            None => None,
        };
        // `koda fork` opens a copy of it instead.
        let (session, earlier_turns, opened_mode) = match resume {
            Some(source) if fork => fork_host_session(&client, &source, &named).await?,
            resume => open_host_session(&client, resume, &named).await?,
        };
        let skills = skills::user_skills(&client.request_handle(), &project_root).await;

        // The mode the host opened it in: a resumed session's own, or the
        // flags' or the config's, after the host's rule: Auto needs the
        // sandbox.
        let initial_mode = opened_mode.unwrap_or(TrustMode::Safe);
        let sandbox = client
            .request::<koda_client::protocol::methods::StateRead>(
                koda_client::protocol::state::StateReadParams {},
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .sandbox;

        // What the session runs with, which the host decided: the saved
        // choice, a resumed session's own, or the flags. The TUI mirrors it.
        let (session_settings, host_warnings) =
            settings::fetch_settings(&mut client, &session.id, initial_mode).await?;
        // Flags are for this run only, as Codex's are: only `/model` and
        // `/provider` save a choice into config.toml.

        let mut startup_lines =
            crate::startup::collect_startup_lines(&session_settings, &providers);
        for warning in host_warnings {
            startup_lines.push(crate::startup::warning_line(warning));
        }
        if let Some(note) = backend_note {
            startup_lines.push(crate::startup::warning_line(note));
        }
        startup_lines.extend(crate::startup::home_dir_warning_lines(&project_root));
        if let Some(warning) = crate::legacy_clones::tidy_home() {
            startup_lines.push(crate::startup::warning_line(warning));
        }
        for warning in crate::retired_env::warnings() {
            startup_lines.push(crate::startup::warning_line(warning));
        }
        if let Ok(Some(latest)) = version_check.await
            && let Some((current, latest)) = crate::updates::update_available(&latest)
        {
            startup_lines.extend(crate::startup::update_notice_lines(current, &latest));
        }

        // Terminal + textarea
        let terminal = init_terminal()?;

        // Composer textarea (codex `bottom_pane::textarea` port; #1178 PR 2).
        // No cursor/style/placeholder configuration here — codex's textarea
        // owns its own cursor styling (block in vim normal, bar in insert)
        // and koda renders the empty-state "Type a message..." hint outside
        // the textarea in `tui_viewport::draw_viewport` so we don't depend on
        // ratatui-textarea's placeholder feature (codex doesn't have an
        // equivalent at the textarea level).
        //
        // Keymap (PR 3): construct defaults and hand them to the textarea so
        // its internal `input(key)` dispatcher routes to the editor / vim /
        // vim-operator handlers correctly. The textarea owns its own clone of
        // the keymap from this point on; TuiContext does not need to keep a
        // copy. Future PRs that need the keymap (e.g. the key-hint footer)
        // can either ask the textarea for it or rebuild defaults on the fly.
        let mut textarea = TextArea::new();
        textarea.set_keymap_bindings(&RuntimeKeymap::defaults());

        let mut renderer = TuiRenderer::new();
        renderer.model = session_settings.model.clone();

        // A model alias picks its own provider, so it skips onboarding too;
        // the session says it chose by one.
        let by_alias = named.model.is_some() && session_settings.alias == named.model;
        let first_run = first_run && !by_alias;
        let mut menu = MenuContent::None;
        if first_run {
            // Onboarding: auto-open provider dropdown
            // (emit_above happens after terminal is created below)
            let items = menus::provider_items(&providers, false);
            menu = MenuContent::Provider(crate::widgets::dropdown::DropdownState::new(
                items,
                "\u{1f43b} Choose your LLM provider",
            ));
        }

        // Spawn the coalescing draw scheduler (#1138). The inference loop's
        // select consumes from `draw_rx`; everywhere else holds clones of
        // `frame_requester` to fire-and-forget redraw requests.
        let (frame_requester, draw_rx) = crate::frame_requester::spawn_frame_scheduler();
        // `@` mentions' files, found by the host; an answer wakes the loop
        // so the dropdown shows it.
        let file_search =
            crate::file_search::FileSearch::new(client.request_handle(), &project_root)
                .waking(frame_requester.clone());
        let completer = crate::completer::InputCompleter::with_file_search(file_search.clone());

        let mut scroll_buffer = ScrollBuffer::new(2500);
        // The view the file names, as Claude Code's `viewMode`.
        match crate::user_config::tui().view_mode.unwrap_or_default() {
            koda_config::ViewMode::Default => {}
            koda_config::ViewMode::Verbose => scroll_buffer.set_full(true),
            koda_config::ViewMode::Focus => scroll_buffer.set_turns_folded(true),
        }
        scroll_buffer.push_lines(startup_lines);
        // A resumed session's earlier turns, drawn as they were live, as
        // Codex's TUI replays a resumed thread.
        replay_turns(&mut renderer, &earlier_turns, &mut scroll_buffer);

        let mut ctx = Self {
            terminal,
            textarea,
            renderer,
            scroll_buffer,
            crossterm_events: EventStream::new(),
            tui_state: TuiState::Idle,
            menu,
            prompt_mode: PromptMode::Chat,
            provider_wizard: None,
            pending_approval_id: None,
            queued_requests: Default::default(),
            pastes: input::Pastes::default(),
            pasted_images: Vec::new(),
            image_count: 0,
            pending_command: None,
            queued: Default::default(),
            should_quit: false,
            quit_armed_at: None,
            inference_start: None,
            plan_shown: true,
            context_left: 100,
            learned_context_window: None,
            history: history.iter().map(|(text, _)| text.clone()).collect(),
            history_pastes: history.into_iter().map(|(_, pastes)| pastes).collect(),
            input_history,
            history_idx: None,
            completer,
            file_search,
            mouse_selection: None,
            history_area_y: 0,
            history_area_height: 0,
            paste_burst: crate::composer::paste_burst::PasteBurst::default(),
            settings: session_settings,
            providers,
            skills,
            host_session: session.id.clone(),
            fork_on_pick: false,
            side: None,
            side_from_turn: None,
            leave_side_requested: false,
            pending_review: None,
            last_turn: None,
            session,
            client,
            mode: initial_mode,
            sandbox,
            project_root,
            first_run,
            frame_requester,
            draw_rx,
            child_activity: crate::child_activity::ChildActivityTracker::default(),
            mcp: mcp::McpView::default(),
            mcp_stale: true,
            goal: None,
            backend,
            offline: false,
            turn_cut: false,
            updated_to: None,
        };
        ctx.refresh_model_names().await;
        Ok(ctx)
    }

    /// The completer's model names, from `model/list` for the session's
    /// provider: the host's list, aliases and ids, as `/model` offers them.
    pub(crate) async fn refresh_model_names(&mut self) {
        let models = self
            .list_models(
                &self.settings.model_provider.clone(),
                &self.settings.base_url.clone(),
            )
            .await;
        let mut names: Vec<String> = Vec::new();
        for model in models {
            if let Some(alias) = model.alias {
                names.push(alias);
            }
            names.push(model.id);
        }
        names.dedup();
        self.completer.set_model_names(names);
    }

    /// Draw the fullscreen viewport.
    pub fn draw(&mut self) -> Result<()> {
        tracing::debug!(
            target: "koda_cli::diag::child_activity",
            stage = "draw",
            "TuiContext::draw() invoked"
        );
        // Clamp scroll offset to valid bounds before rendering.
        // Necessary after terminal resize changes wrapping math.
        // Use history panel height, not full terminal height — otherwise
        // max_offset is too small and scrolling up re-engages sticky.
        let (w, _) = self.term_dims();
        let vh = (self.history_area_height as usize).max(1);
        self.scroll_buffer.clamp_offset(w, vh);

        let mode = self.mode;
        let ctx = self.context_left;
        let tui_state = self.tui_state;
        let prompt_mode = &self.prompt_mode;
        let elapsed = self
            .inference_start
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0);
        let menu = &self.menu;
        let scroll_buffer = &self.scroll_buffer;
        let textarea = &self.textarea;
        let settings = &self.settings;
        let selection = self.mouse_selection.as_ref();
        let mcp_info = self.mcp.status_bar();
        let project_root = self.project_root.clone();
        // #1210: the overlay above the status bar shows the turn's
        // sub-agents and the session's background processes, and what
        // stops them.
        let (child_activity_rows, child_activity_total) = self.child_activity.build_rows();
        let child_activity_hint = self.child_activity.hint();
        let side = self.side.is_some();

        let mut history_rect = None;
        if let Err(e) = self.terminal.draw(|f| {
            history_rect = Some(draw_viewport(
                f,
                textarea,
                &settings.model,
                mode,
                self.sandbox.available,
                ctx,
                tui_state,
                prompt_mode,
                elapsed,
                menu,
                scroll_buffer,
                selection,
                mcp_info,
                &project_root,
                &child_activity_rows,
                child_activity_total,
                child_activity_hint,
                side,
                &[],
            ));
        }) {
            tracing::debug!("draw skipped: {e}");
        }
        if let Some(rect) = history_rect {
            self.history_area_y = rect.y;
            self.history_area_height = rect.height;
        }
        // #1354: self-perpetuating draw loop. While any non-terminal
        // bg agent or process exists, schedule the next frame ~1 s out
        // so the age column ticks and the activity pill stays live
        // even when the user is idle and no engine events are
        // arriving. `child_activity_total` is already filtered to
        // non-terminal entries by `ChildActivityTracker::build_rows`,
        // so the ticker auto-stops the moment work drains.
        if child_activity_total > 0 {
            self.frame_requester
                .schedule_frame_in(std::time::Duration::from_secs(1));
        }
        Ok(())
    }

    /// Push a message line into the scroll buffer.
    pub fn emit(&mut self, line: Line<'static>) {
        self.scroll_buffer.push(line);
    }

    /// Clean up terminal and print exit info.
    pub async fn cleanup(&mut self) {
        restore_terminal(&mut self.terminal);
        // A side conversation goes with the TUI; the hint is the main
        // session's.
        if let Some(side) = self.side.take() {
            let _ = self
                .client
                .request::<koda_client::protocol::methods::SessionDelete>(
                    koda_client::protocol::session::SessionDeleteParams {
                        session_id: self.session.id.clone(),
                    },
                )
                .await;
            self.session = side.main;
        }
        crate::startup::print_resume_hint(&self.session.id);
    }

    /// The main event loop.
    pub async fn run_event_loop(
        &mut self,
        cmd_tx: &mpsc::Sender<crate::inference::TurnCommand>,
        cmd_rx: &mut mpsc::Receiver<crate::inference::TurnCommand>,
    ) -> Result<()> {
        loop {
            if self.should_quit {
                break;
            }
            if self.mcp_stale {
                self.refresh_mcp().await;
            }
            self.refresh_background(false).await;

            // ── The side conversation: back from it, or into it ──
            if std::mem::take(&mut self.leave_side_requested)
                && let Some((turn_id, waiting)) = self.leave_side().await
            {
                self.run_inference_turn(TurnRequest::Watch { turn_id, waiting }, cmd_tx, cmd_rx)
                    .await?;
                continue;
            }
            if let Some((question, turn)) = self.side_from_turn.take() {
                if let CommandOutcome::StartInference(input) =
                    self.enter_side(&question, Some(turn)).await
                {
                    self.run_inference_turn(TurnRequest::Prompt(input), cmd_tx, cmd_rx)
                        .await?;
                }
                continue;
            }

            // ── A message queued during the last turn: the next prompt ──
            // One per turn, as Codex sends them. The input box keeps what
            // is being typed, with its pastes and images.
            let mut draft = None;
            if self.tui_state == TuiState::Idle
                && self.pending_command.is_none()
                && matches!(self.prompt_mode, PromptMode::Chat)
                && let Some(message) = self.queued.next()
            {
                let icon = crate::mode::icon(self.mode);
                self.scroll_buffer.push(Line::from(vec![
                    Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                    Span::raw(message.text.clone()),
                ]));
                draft = Some((
                    std::mem::replace(&mut self.pastes.list, message.pastes),
                    std::mem::replace(&mut self.pasted_images, message.images),
                ));
                self.pending_command = Some(message.text);
            }

            // ── Dispatch pending commands ────────────────────
            if self.tui_state == TuiState::Idle
                && let Some(raw) = self.pending_command.take()
            {
                // Offline: a prompt reconnects first; a command such as
                // /quit needs no server. Still offline, the prompt goes
                // back to the input box.
                if self.offline && !raw.trim_start().starts_with('/') {
                    if let Some(turn_id) = self.reconnect().await {
                        self.run_inference_turn(
                            TurnRequest::Watch {
                                turn_id,
                                waiting: Vec::new(),
                            },
                            cmd_tx,
                            cmd_rx,
                        )
                        .await?;
                    }
                    if self.offline || self.should_quit {
                        self.textarea.set_text_clearing_elements(&raw);
                        if let Some((pastes, images)) = draft.take() {
                            self.pastes.list.extend(pastes);
                            self.pasted_images.extend(images);
                        }
                        continue;
                    }
                }
                let outcome = self.dispatch_command(&raw).await;
                if let Some((pastes, images)) = draft.take() {
                    self.pastes.list.extend(pastes);
                    self.pasted_images.extend(images);
                }
                match outcome {
                    CommandOutcome::NoInput => {}
                    CommandOutcome::Handled => continue,
                    CommandOutcome::Quit => {
                        self.should_quit = true;
                        continue;
                    }
                    CommandOutcome::StartInference(input) => {
                        self.run_inference_turn(TurnRequest::Prompt(input), cmd_tx, cmd_rx)
                            .await?;
                        // A review's turn: back to the main session.
                        self.finish_review().await;
                        continue;
                    }
                    CommandOutcome::Compact => {
                        self.run_inference_turn(TurnRequest::Compact, cmd_tx, cmd_rx)
                            .await?;
                        continue;
                    }
                }
            }

            self.show_next_request();
            // The host answered an `@` search: the dropdown shows it.
            if self.file_search.take_fresh() {
                self.refresh_mention_menu();
            }
            // Redraw viewport (resize if textarea grew/shrank)
            self.draw()?;

            // ── Idle: wait for keyboard input or bg-agent activity ──
            //
            // The optional paste-burst flush arm (#1186) only races the
            // event branch when a non-bracketed paste is currently being
            // accumulated. The `if` guard makes the branch disabled when
            // there's no pending burst, so we don't churn timers in the
            // common case (idle waiting for the user). When a burst IS
            // active and the user stops typing, the timer fires and we
            // commit the buffered text into the textarea as one insert.
            tokio::select! {
                Some(Ok(ev)) = self.crossterm_events.next() => {
                    self.handle_idle_event(ev).await?;
                }
                // The host's notifications while idle: MCP startups move
                // the status bar; the rest are no turn's business, as a
                // turn drops them when it starts.
                message = self.client.next_event(), if !self.offline => {
                    // The host is gone: the shared server stopped or the
                    // connection broke. Reconnect, as Codex's TUI does, and
                    // follow a turn the server kept running.
                    let Some(message) = message else {
                        if let Some(turn_id) = self.reconnect().await {
                            self.run_inference_turn(
                                TurnRequest::Watch {
                                    turn_id,
                                    waiting: Vec::new(),
                                },
                                cmd_tx,
                                cmd_rx,
                            )
                            .await?;
                        }
                        continue;
                    };
                    if self.note_main_session(&message) {
                        continue;
                    }
                    self.note_session(&message);
                    // A turn the host started by itself, on the session's
                    // goal: follow it as one of ours.
                    if let koda_client::events::Outgoing::Notification(
                        koda_client::protocol::ServerNotification::TurnStarted(started),
                    ) = &message
                        && started.session_id == self.session.id
                    {
                        let turn_id = started.turn.id.clone();
                        self.run_inference_turn(
                            TurnRequest::Watch {
                                turn_id,
                                waiting: Vec::new(),
                            },
                            cmd_tx,
                            cmd_rx,
                        )
                        .await?;
                        continue;
                    }
                    if self.mcp.observe(&self.session.id, &message) {
                        self.mcp_stale = true;
                    }
                    // A background agent may ask while no turn runs.
                    if self.take_request(&message) {
                        continue;
                    }
                    if self.note_update(&message) {
                        continue;
                    }
                    // A warning about no session, such as a Claude model
                    // this Koda doesn't support yet, which the host says
                    // once the Models API has answered.
                    if let koda_client::events::Outgoing::Notification(
                        koda_client::protocol::ServerNotification::Warning(w),
                    ) = message
                        && w.session_id.is_none()
                    {
                        crate::tui_output::warn_msg(&mut self.scroll_buffer, w.message);
                    }
                }
                // #1354: bg-activity overlay self-perpetuating tick.
                // `TuiContext::draw()` calls `schedule_frame_in(1 s)`
                // whenever there's non-terminal bg work, so this arm
                // wakes the loop on each tick \u2014 the loop iterates,
                // calls `draw()` again at the top, and the cycle
                // continues until work drains. Without this arm the
                // notification would queue with no consumer and the
                // pill freezes between engine events / keystrokes.
                Some(()) = self.draw_rx.recv() => {
                    // No body needed: redraw happens via `self.draw()?`
                    // at the top of the next loop iteration.
                }
                _ = tokio::time::sleep(
                    crate::composer::paste_burst::PasteBurst::recommended_flush_delay(),
                ), if self.paste_burst.is_active() => {
                    self.flush_paste_burst_if_due();
                }
            }
        }
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════
    // Command dispatch (slash commands, dropdown openers, inference prep)
    // ═══════════════════════════════════════════════════════════

    /// Dispatch a raw input string.
    async fn dispatch_command(&mut self, raw: &str) -> CommandOutcome {
        let input = raw.trim().to_string();
        if input.is_empty() {
            return CommandOutcome::NoInput;
        }
        if input.starts_with('/') {
            return self.dispatch_slash(&input).await;
        }
        self.prepare_inference_start(&input).await
    }

    async fn dispatch_slash(&mut self, input: &str) -> CommandOutcome {
        if input.trim() == "/vim" {
            // Toggle vim-mode editing on the textarea (PR 3 of #1178).
            // The textarea owns the mode flag; we just flip it and emit
            // a confirmation line so the user sees the change in scrollback.
            // The status-bar `VIM:NORMAL` / `VIM:INSERT` pill (rendered in
            // `tui_viewport::draw_viewport`) reflects the current mode on
            // the next frame.
            let now_on = !self.textarea.is_vim_enabled();
            self.textarea.set_vim_enabled(now_on);
            let msg = if now_on {
                "\u{1f43b} Vim mode: ON \u{2014} Esc \u{2192} NORMAL, i/a/o \u{2192} INSERT."
            } else {
                "\u{1f43b} Vim mode: OFF."
            };
            self.scroll_buffer.push(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::Cyan),
            )));
            return CommandOutcome::Handled;
        }
        if input.trim() == "/model" {
            self.open_model_picker().await;
            return CommandOutcome::Handled;
        }
        if matches!(input.trim(), "/new" | "/clear") {
            self.start_new_session().await;
            return CommandOutcome::Handled;
        }
        if input.trim() == "/fork" {
            self.fork_session().await;
            return CommandOutcome::Handled;
        }
        if let Some(arg) = input
            .trim()
            .strip_prefix("/review")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            return self.enter_review(arg).await;
        }
        if let Some(question) = ["/side", "/btw"].iter().find_map(|command| {
            input
                .trim()
                .strip_prefix(command)
                .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        }) {
            return self.enter_side(question, None).await;
        }
        if input.trim() == "/status" {
            self.show_status().await;
            return CommandOutcome::Handled;
        }
        if let Some(arg) = input
            .trim()
            .strip_prefix("/goal")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            goal::command(
                &self.client.request_handle(),
                &self.session.id,
                arg,
                &mut self.scroll_buffer,
            )
            .await;
            return CommandOutcome::Handled;
        }
        if let Some(name) = input
            .trim()
            .strip_prefix("/rename")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            self.rename_session(name).await;
            return CommandOutcome::Handled;
        }
        if let Some(count) = input
            .trim()
            .strip_prefix("/rewind")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            self.rewind_session(count).await;
            return CommandOutcome::Handled;
        }
        if input.trim() == "/init" {
            return self.init_project();
        }
        if let Some(prompt) = input
            .trim()
            .strip_prefix("/plan")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            return self.enter_plan(prompt.trim()).await;
        }
        if input.trim() == "/provider" {
            self.open_provider_picker();
            return CommandOutcome::Handled;
        }
        if let Some(arg) = input.trim().strip_prefix("/provider ") {
            let arg = arg.trim();
            if menus::is_provider_url(arg) {
                // A URL is the endpoint itself: the host tells the provider
                // from it, as `koda --base-url` does, and the choice is saved
                // like a name picked from the wizard.
                self.switch_provider(None, Some(arg.trim_end_matches('/').to_string()), None)
                    .await;
            } else {
                self.start_provider_wizard(arg);
            }
            return CommandOutcome::Handled;
        }
        if let Some(arg) = input
            .trim()
            .strip_prefix("/mcp")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            self.handle_mcp(arg.trim()).await;
            return CommandOutcome::Handled;
        }
        if matches!(input.trim(), "/resume" | "/sessions") {
            if input.trim() == "/sessions" {
                crate::tui_output::dim_msg(
                    &mut self.scroll_buffer,
                    "/sessions is now /resume.".into(),
                );
            }
            self.open_session_picker().await;
            return CommandOutcome::Handled;
        }

        let action = tui_commands::handle_slash_command(
            &mut self.scroll_buffer,
            input,
            &mut self.session,
            (&mut self.mode, &self.sandbox),
            &mut self.renderer,
            &self.project_root,
            &mut self.pending_command,
            &mut self.menu,
            &mut self.child_activity,
            &self.client,
        )
        .await;

        match action {
            SlashAction::Continue => {
                self.reinit_after_slash().await;
                CommandOutcome::Handled
            }
            SlashAction::OpenKeyMenu => {
                self.open_key_picker().await;
                CommandOutcome::Handled
            }
            SlashAction::OpenFeedback => {
                self.open_feedback_picker();
                CommandOutcome::Handled
            }
            SlashAction::ChangeModel(model) => {
                self.change_model(&model, settings::ModelScope::Default)
                    .await;
                CommandOutcome::Handled
            }
            SlashAction::Memory(arg) => {
                self.memory_command(arg.as_deref()).await;
                CommandOutcome::Handled
            }
            SlashAction::Compact => CommandOutcome::Compact,
            SlashAction::Quit => {
                self.scroll_buffer.push(Line::styled(
                    "\u{1f43b} Goodbye!",
                    Style::default().fg(Color::Cyan),
                ));
                CommandOutcome::Quit
            }
        }
    }

    async fn reinit_after_slash(&mut self) {
        // Fullscreen mode: no terminal reinit needed.
        // Just refresh completer state and model name.
        self.refresh_model_names().await;
        self.renderer.model = self.settings.model.clone();
    }

    async fn prepare_inference_start(&mut self, input: &str) -> CommandOutcome {
        let mut processed = input::process_input(input);
        let pastes = self.pastes.take();
        // Pasted images whose markers are still in the text, in their
        // order, then any image path typed in the text.
        let pasted_images = std::mem::take(&mut self.pasted_images);
        let mut images = input::images_by_marker(input, &pasted_images);
        images.append(&mut processed.images);
        processed.images = images;

        for (i, _img) in processed.images.iter().enumerate() {
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f5bc} Image {}", i + 1),
                    Style::default().fg(Color::Magenta),
                ),
            ]));
        }

        for path in &processed.mentions {
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f4ce} {path}"),
                    Style::default().fg(Color::Cyan),
                ),
            ]));
        }
        let mut user_message = processed.prompt.clone();

        // Each paste goes where its marker is.
        user_message = input::expand_pastes(&user_message, &pastes);

        // The host saves the message and titles a new session from it; a
        // message it couldn't save comes back to the input box
        // (`run_inference_turn`).
        CommandOutcome::StartInference(TurnInput {
            message: user_message,
            images: processed.images,
            mentions: processed.mentions,
            typed: input.to_string(),
            pastes,
            pasted_images,
        })
    }

    // ═══════════════════════════════════════════════════════════
    // Idle-mode event handling (keyboard, paste, menu, wizard, history)
    // ═══════════════════════════════════════════════════════════
}

#[cfg(test)]
mod opening_tests {
    use super::opening_refused;
    use koda_client::RequestError;
    use koda_client::protocol::rpc::{ErrorObject, codes};

    fn refused(message: &str) -> RequestError {
        RequestError::Server {
            method: "session/start",
            error: ErrorObject::new(codes::INVALID_REQUEST, message),
        }
    }

    #[test]
    fn a_session_written_elsewhere_says_where_and_what_to_do() {
        let said = opening_refused(refused("session abc already has an active writer"));
        assert!(
            said.starts_with("This session is open in another Koda process"),
            "{said}"
        );
        assert!(said.contains("the background server"), "{said}");
        assert!(said.contains("close it there"), "{said}");
    }

    #[test]
    fn other_refusals_read_as_before() {
        let said = opening_refused(refused("no session 'abc'"));
        assert_eq!(said, "session/start failed: no session 'abc' (code -32600)");
    }
}
