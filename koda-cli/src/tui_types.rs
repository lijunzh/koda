//! TUI type definitions — enums, type aliases, and constants.
//!
//! Extracted from `tui_app.rs` to keep type definitions separate
//! from the event loop logic. See #209.

use ratatui::{Terminal, backend::CrosstermBackend};

/// Ratatui terminal backed by stdout.
pub(crate) type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Minimum viewport height — large enough to fit the slash menu overlay.
/// separator(1) + input(1) + menu(8) + bottom_sep(1) + status(1) = 12
pub(crate) const MIN_VIEWPORT_HEIGHT: u16 = 12;
/// Maximum viewport height to avoid taking over the terminal.
pub(crate) const MAX_VIEWPORT_HEIGHT: u16 = 16;

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
/// Only one menu can be active at a time.
pub(crate) enum MenuContent {
    /// Nothing — menu_area is empty.
    None,
    /// Slash command dropdown (auto-appears on `/`).
    Slash(SlashDropdown),
    /// Model picker dropdown (`/model` with no args).
    Model(ModelDropdown),
    /// Provider picker dropdown (`/provider` with no args).
    Provider(ProviderDropdown),
    /// Session picker dropdown (`/sessions` with no args).
    Session(SessionDropdown),
    /// File picker dropdown (auto-appears on `@`).
    File {
        dropdown: FileDropdown,
        /// Text before the `@` token (to reconstruct the full input).
        prefix: String,
    },
    /// Wizard trail — completed steps shown dimmed during multi-step flow.
    WizardTrail(Vec<(String, String)>),
    /// Approval hotkey bar — shown during inference when engine requests approval.
    Approval {
        id: String,
        tool_name: String,
        detail: String,
    },
    /// Loop cap hotkey bar — continue or stop after iteration limit.
    LoopCap,
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
    /// Wizard text input: label: █ (or label: ••••█ when masked)
    WizardInput {
        label: String,
        #[allow(dead_code)] // TODO: implement textarea masking for API keys
        masked: bool,
    },
}

/// Provider setup wizard state machine.
/// Each variant holds the data collected so far.
pub(crate) enum ProviderWizard {
    /// Step 1: provider selected, now need API key (or URL for local providers).
    NeedApiKey {
        provider_type: koda_core::config::ProviderType,
        base_url: String,
        env_name: String,
    },
    /// Step 1 (local): provider selected, now need URL.
    NeedUrl {
        provider_type: koda_core::config::ProviderType,
    },
}
