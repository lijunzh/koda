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

/// Ratatui terminal backed by stdout.
pub(crate) type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

// Viewport height constants removed — fullscreen mode (#472)

// ── Type aliases for dropdown menus ─────────────────────────

pub(crate) type SlashDropdown =
    crate::widgets::dropdown::DropdownState<crate::widgets::slash_menu::SlashCommand>;
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
    ProviderModels(ModelDropdown, koda_core::config::ProviderType),
    /// Key management — pick a provider to set its API key (`/key`).
    Key(ProviderDropdown),
    /// Session picker dropdown (`/sessions` with no args).
    Session(SessionDropdown),
    /// File picker dropdown (auto-appears when the user types `@`).
    File {
        dropdown: FileDropdown,
        /// Text before the `@` token (to reconstruct the full input on selection).
        prefix: String,
    },
    /// Wizard trail — completed steps shown dimmed during a multi-step flow.
    WizardTrail(Vec<(String, String)>),
    /// Approval hotkey bar — shown during inference when the engine requests
    /// a tool approval (`y`/`n`/`a`/`f`).
    Approval {
        id: String,
        tool_name: String,
        detail: String,
    },
    /// AskUser input bar — the model is asking a clarifying question.
    AskUser {
        id: String,
        question: String,
        options: Vec<String>,
    },
    /// Loop cap hotkey bar — continue or stop after the iteration limit is hit.
    LoopCap,
    /// Purge confirmation bar — \[y\] confirm / \[n\] cancel.
    PurgeConfirm { min_age_days: u32, detail: String },
    /// Ctrl+R reverse history search overlay.
    HistorySearch {
        /// Current search query (updated on each keystroke).
        query: String,
        /// Matching history entries, newest first (pre-computed on each keystroke).
        matches: Vec<String>,
        /// Index into `matches` of the highlighted row.
        selected: usize,
    },
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
    WizardInput { label: String },
}

/// Provider setup wizard state machine.
/// Each variant holds the data collected so far.
pub(crate) enum ProviderWizard {
    /// Step 1: provider selected, now need API key (or URL for local providers).
    ApiKey {
        provider_type: koda_core::config::ProviderType,
        base_url: String,
        env_name: String,
    },
    /// Key-only mode from `/key` — set key and return, no model list.
    ApiKeyOnly { env_name: String },
    /// Step 1 (local): provider selected, now need URL.
    Url {
        provider_type: koda_core::config::ProviderType,
    },
}
