//! TUI type definitions — enums, type aliases, and constants.
//!
//! Extracted from `tui_app.rs` to keep type definitions separate
//! from the event loop logic. See #209.
//!
//! ## Key types
//!
//! | Type | Role |
//! |------|------|
//! | `TuiState` | Idle vs. actively inferring |
//! | [`MenuContent`] | What is shown in the menu area below the status bar |
//! | `PromptMode` | Normal chat vs. wizard text entry |
//! | `ProviderWizard` | Multi-step provider setup state machine |
//! | `Term` | Type alias for the ratatui terminal backed by crossterm |

use ratatui::{Terminal, backend::CrosstermBackend};

/// The TUI's ratatui terminal: stdout, or in tests a buffer the test reads.
pub(crate) type Term = Terminal<Screen>;

/// Where the TUI draws. A test draws into a [`ratatui::backend::TestBackend`]
/// it keeps a handle to, so it can read the screen while the TUI runs, as
/// Codex's TUI tests render into one.
pub(crate) enum Screen {
    /// The real terminal.
    Stdout(CrosstermBackend<std::io::Stdout>),
    /// A test's screen.
    #[cfg(test)]
    Test(std::sync::Arc<std::sync::Mutex<ratatui::backend::TestBackend>>),
}

impl Screen {
    /// Whether this is the real terminal, which needs its modes restored.
    pub(crate) fn is_stdout(&self) -> bool {
        matches!(self, Screen::Stdout(_))
    }
}

/// What the TUI draws on and reads from.
pub(crate) enum TuiIo {
    /// The terminal Koda runs in.
    Terminal,
    /// A test's screen, and the events it sends.
    #[cfg(test)]
    Test {
        screen: std::sync::Arc<std::sync::Mutex<ratatui::backend::TestBackend>>,
        events: Events,
    },
}

/// The TUI's input events: the terminal's, or a test's.
pub(crate) type Events = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Send>,
>;

/// Run `$call` on whichever backend `$screen` is.
macro_rules! on_backend {
    ($screen:expr, $b:ident => $call:expr) => {
        match $screen {
            Screen::Stdout($b) => $call,
            #[cfg(test)]
            Screen::Test(test) => {
                let mut guard = test.lock().unwrap_or_else(|p| p.into_inner());
                let $b = &mut *guard;
                match $call {
                    Ok(value) => Ok(value),
                    Err(never) => match never {},
                }
            }
        }
    };
}

impl ratatui::backend::Backend for Screen {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        on_backend!(self, b => b.draw(content))
    }
    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        on_backend!(self, b => b.append_lines(n))
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        on_backend!(self, b => b.hide_cursor())
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        on_backend!(self, b => b.show_cursor())
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        on_backend!(self, b => b.get_cursor_position())
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        let position = position.into();
        on_backend!(self, b => b.set_cursor_position(position))
    }
    fn clear(&mut self) -> std::io::Result<()> {
        on_backend!(self, b => b.clear())
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        on_backend!(self, b => b.clear_region(clear_type))
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        match self {
            Screen::Stdout(b) => b.size(),
            #[cfg(test)]
            Screen::Test(test) => {
                let guard = test.lock().unwrap_or_else(|p| p.into_inner());
                match guard.size() {
                    Ok(size) => Ok(size),
                    Err(never) => match never {},
                }
            }
        }
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        on_backend!(self, b => b.window_size())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        on_backend!(self, b => b.flush())
    }
    fn scroll_region_up(
        &mut self,
        region: std::ops::Range<u16>,
        amount: u16,
    ) -> std::io::Result<()> {
        on_backend!(self, b => b.scroll_region_up(region, amount))
    }
    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        amount: u16,
    ) -> std::io::Result<()> {
        on_backend!(self, b => b.scroll_region_down(region, amount))
    }
}

// Viewport height constants removed — fullscreen mode (#472)

// ── Type aliases for dropdown menus ─────────────────────────

pub(crate) type SlashDropdown =
    crate::widgets::dropdown::DropdownState<crate::composer::slash_popup::SlashCommand>;
pub(crate) type ModelDropdown =
    crate::widgets::dropdown::DropdownState<crate::widgets::model_menu::ModelItem>;
pub(crate) type ProviderDropdown =
    crate::widgets::dropdown::DropdownState<crate::widgets::provider_menu::ProviderItem>;
pub(crate) type SessionDropdown =
    crate::widgets::dropdown::DropdownState<crate::widgets::session_menu::SessionItem>;
pub(crate) type FileDropdown =
    crate::widgets::dropdown::DropdownState<crate::widgets::file_menu::FileItem>;

// ── Session state ────────────────────────────────────────────

/// What the TUI is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TuiState {
    /// Waiting for user input (no inference running).
    Idle,
    /// An inference turn is running.
    Inferring,
}

/// What's currently shown in the `menu_area` below the status bar.
///
/// Only one menu can be active at a time. The TUI switches between
/// variants in response to slash commands, `@` completions, and
/// engine events (approval requests, loop cap, etc.).
pub enum MenuContent {
    /// Nothing — menu_area is empty (normal chat state).
    None,
    /// Slash command dropdown (auto-appears when the user types `/`).
    Slash(SlashDropdown),
    /// Model picker dropdown (`/model` with no args).
    Model(ModelDropdown),
    /// Provider picker dropdown (`/provider` with no args).
    Provider(ProviderDropdown),
    /// Provider model list — second step of `/provider` (pick provider → list its models).
    ProviderModels(
        ModelDropdown,
        koda_client::protocol::model::ModelProviderSummary,
    ),
    /// Key management — pick a provider to set its API key (`/key`).
    Key(ProviderDropdown),
    /// Session picker dropdown (`/resume` with no args).
    Session(SessionDropdown),
    /// `/feedback`: the kind of report, then, once `classification` is
    /// chosen, whether to include logs and session history.
    Feedback {
        dropdown:
            crate::widgets::dropdown::DropdownState<crate::widgets::feedback_menu::FeedbackItem>,
        classification: Option<&'static str>,
    },
    /// "Implement this plan?", after a Plan turn that proposed one.
    PlanImplement(crate::widgets::dropdown::DropdownState<crate::widgets::plan_menu::PlanChoice>),
    /// File picker dropdown (auto-appears when the user types `@`).
    File {
        dropdown: FileDropdown,
        /// Text before the `@` token (to reconstruct the full input on selection).
        prefix: String,
    },
    /// Wizard trail — completed steps shown dimmed during a multi-step flow.
    WizardTrail(Vec<(String, String)>),
    /// Approval hotkey bar — shown during inference when the engine requests
    /// a tool approval (`y`/`s`/`n`/`f`).
    Approval {
        /// The host's request this answers.
        id: koda_client::protocol::rpc::RequestId,
        tool_name: String,
        detail: String,
        /// Whether `s` (yes for the rest of the session) is offered.
        allow_for_session: bool,
        /// What `s` covers, when it's more than the call itself: a command
        /// prefix (`` `git commit …` this session ``) or a host
        /// (`example.com this session`).
        session_label: Option<String>,
        /// The sub-agent asking (`explore` or `explore a1b2…`), when it isn't
        /// the main agent: Esc then refuses only this call, as in Claude Code.
        agent: Option<String>,
        /// A background agent asks: answered without the turn's feedback and
        /// Auto keys, since it may ask while no turn runs.
        background: bool,
    },
    /// AskUser input bar — the model is asking a clarifying question.
    AskUser {
        /// The host's request this answers.
        id: koda_client::protocol::rpc::RequestId,
        question: String,
        options: Vec<String>,
        /// An MCP server's form, asked a field at a time; `None` for the
        /// model's question.
        form: Option<Box<crate::elicitation_form::ElicitationForm>>,
    },
    /// Loop cap hotkey bar — continue or stop after the iteration limit is hit.
    LoopCap {
        /// The request this answers.
        id: koda_client::protocol::rpc::RequestId,
    },
    /// Ctrl+R reverse history search overlay.
    HistorySearch {
        /// Current search query (updated on each keystroke).
        query: String,
        /// Matching history entries, newest first (pre-computed on each keystroke).
        matches: Vec<String>,
        /// Index into `matches` of the highlighted row.
        selected: usize,
    },
    /// Multi-line keybinding reference overlay (#1194 / #1195). Opened by
    /// pressing `?` while the composer is empty and no other menu is
    /// active; dismissed by `?` again, `Esc`, or any other key.
    /// Content is built by [`crate::widgets::shortcuts_overlay`].
    ShortcutsOverlay,
}

impl MenuContent {
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, MenuContent::None)
    }
}

/// What the prompt input area is currently doing.
/// Normally it's chat input. During wizard flows, it's repurposed
/// for text input (API key, URL, etc.).
#[derive(Clone)]
pub(crate) enum PromptMode {
    /// Normal chat input: ⚡> █
    Chat,
    /// Wizard text input: label: █
    ///
    /// `mask: true` swaps the textarea render path to
    /// `render_ref_masked` so the user's keystrokes display as `*` —
    /// used for API key entry (PR 6 of #1178). The underlying buffer is
    /// still cleartext (the wizard handler reads it via `textarea.text()`),
    /// so this is purely a display-layer protection against shoulder-
    /// surfing / screen recording, not a memory-safety feature.
    WizardInput { label: String, mask: bool },
}

/// Provider setup wizard state machine.
/// Each variant holds the data collected so far.
pub(crate) enum ProviderWizard {
    /// `/feedback`'s last step: the note to send with the report.
    FeedbackNote {
        classification: &'static str,
        include_logs: bool,
    },
    /// Step 1: provider selected, now need API key (or URL for local providers).
    ApiKey {
        provider: koda_client::protocol::model::ModelProviderSummary,
    },
    /// Key-only mode from `/key` — set key and return, no model list.
    ApiKeyOnly {
        provider: koda_client::protocol::model::ModelProviderSummary,
    },
    /// `/key`'s Brave Search entry: the key `WebSearch` uses.
    WebSearchKey {
        key: koda_client::protocol::config::WebSearchApiKeyReadResponse,
    },
    /// Step 1 (local): provider selected, now need URL.
    Url {
        provider: koda_client::protocol::model::ModelProviderSummary,
    },
}
