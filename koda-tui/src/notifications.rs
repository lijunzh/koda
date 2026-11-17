//! Desktop notifications and the bell, as Codex's TUI sends them: when a
//! turn ends, when a tool call waits for approval, and when the model asks a
//! question, while the terminal isn't focused.
//!
//! **Settings** (`[tui]`, Codex's three keys): `notifications` (`true`,
//! `false`, or the events to notify for), `notification_method` (`auto`,
//! `osc9`, `osc777`, `bel`) and `notification_condition` (`unfocused`,
//! `always`).
//!
//! **Focus** comes from the terminal's focus reports (`ESC [ I` and
//! `ESC [ O`), which the TUI turns on at start. Until one arrives the
//! terminal counts as focused, as in Codex, so a terminal that never
//! reports focus never notifies under `unfocused`.
//!
//! **How:** `auto` sends OSC 9, a desktop notification, in Ghostty, iTerm2,
//! Kitty, Warp and WezTerm, and the bell elsewhere. Inside tmux an OSC is
//! wrapped so tmux passes it on (it needs `allow-passthrough on`); the bell
//! passes without it. A write that fails turns notifications off for the
//! rest of the session.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use koda_config::{NotificationCondition, NotificationMethod, Notifications};

/// What Koda tells the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notification {
    /// A turn ended: its reply, or why it failed.
    TurnComplete { message: String },
    /// A tool call waits for a yes, or the turn hit its iteration cap.
    ApprovalRequested { subject: String },
    /// The model asked a question.
    UserInputRequested { question: String },
}

/// The most characters of a reply a notification shows, as Codex's.
const PREVIEW_CHARS: usize = 200;
/// The most characters of a question.
const QUESTION_CHARS: usize = 30;
/// Ghostty copies a notification's body into a 255-byte buffer.
const GHOSTTY_BODY_BYTES: usize = 255;

impl Notification {
    /// The notification's text.
    pub(crate) fn display(&self) -> String {
        match self {
            Self::TurnComplete { message } => {
                let flat = flatten(message);
                if flat.is_empty() {
                    "Koda finished".to_string()
                } else {
                    truncate(&flat, PREVIEW_CHARS)
                }
            }
            Self::ApprovalRequested { subject } => {
                format!(
                    "Approval requested: {}",
                    truncate(&flatten(subject), PREVIEW_CHARS)
                )
            }
            Self::UserInputRequested { question } => {
                format!("Question: {}", truncate(&flatten(question), QUESTION_CHARS))
            }
        }
    }

    /// Its name in `[tui] notifications`. Codex's, except that Codex calls
    /// a question `plan-mode-prompt`, since its question tool exists only in
    /// Plan; Koda's works in every mode.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::TurnComplete { .. } => "agent-turn-complete",
            Self::ApprovalRequested { .. } => "approval-requested",
            Self::UserInputRequested { .. } => "user-input-requested",
        }
    }

    /// Whether `settings` lets it through.
    pub(crate) fn allowed_by(&self, settings: &Notifications) -> bool {
        match settings {
            Notifications::Enabled(on) => *on,
            Notifications::Custom(names) => names.iter().any(|name| name == self.type_name()),
        }
    }
}

/// Whitespace runs as one space, trimmed.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` characters, with `…` when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// How a notification reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Osc9,
    Osc777,
    Bel,
}

/// What Koda knows of its terminal, from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Terminal {
    /// It shows OSC 9 as a desktop notification.
    pub(crate) osc9: bool,
    /// It is Ghostty, which caps a notification's body.
    pub(crate) ghostty: bool,
    /// Koda runs inside tmux.
    pub(crate) tmux: bool,
}

impl Terminal {
    /// The terminal `var` describes, as Codex's terminal detection reads
    /// it: `TERM_PROGRAM` first (unless it names tmux), then the variables
    /// each terminal sets. Inside tmux those come from the outer terminal
    /// when the session started there.
    pub(crate) fn detect(var: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
        let tmux = set("TMUX") || set("TMUX_PANE");
        let program = var("TERM_PROGRAM")
            .map(|value| {
                value
                    .chars()
                    .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
                    .map(|c| c.to_ascii_lowercase())
                    .collect::<String>()
            })
            .filter(|value| !value.is_empty() && value != "tmux");
        let name = match program.as_deref() {
            Some("ghostty") => "ghostty",
            Some("iterm" | "iterm2" | "itermapp") => "iterm2",
            Some("warp" | "warpterminal") => "warp",
            Some("wezterm") => "wezterm",
            Some("kitty") => "kitty",
            Some(_) => "other",
            None if set("GHOSTTY_RESOURCES_DIR") => "ghostty",
            None if var("WEZTERM_VERSION").is_some() => "wezterm",
            None if var("ITERM_SESSION_ID").is_some() || var("ITERM_PROFILE").is_some() => "iterm2",
            None if var("KITTY_WINDOW_ID").is_some()
                || var("TERM").is_some_and(|term| term.contains("kitty")) =>
            {
                "kitty"
            }
            None => "other",
        };
        Self {
            osc9: matches!(name, "ghostty" | "iterm2" | "warp" | "wezterm" | "kitty"),
            ghostty: name == "ghostty",
            tmux,
        }
    }
}

impl Backend {
    /// The backend `method` names, on `terminal`.
    pub(crate) fn for_method(method: NotificationMethod, terminal: Terminal) -> Self {
        match method {
            NotificationMethod::Auto if terminal.osc9 => Self::Osc9,
            NotificationMethod::Auto | NotificationMethod::Bel => Self::Bel,
            NotificationMethod::Osc9 => Self::Osc9,
            NotificationMethod::Osc777 => Self::Osc777,
        }
    }

    /// The bytes that post `message` on `terminal`.
    pub(crate) fn sequence(self, message: &str, terminal: Terminal) -> String {
        if self == Self::Bel {
            return "\x07".to_string();
        }
        // Control characters would end or reshape the sequence; `;`
        // separates OSC 777's fields.
        let mut body: String = message
            .chars()
            .map(|c| match c {
                ';' if self == Self::Osc777 => '；',
                c if c.is_control() => ' ',
                c => c,
            })
            .collect();
        if terminal.ghostty && body.len() > GHOSTTY_BODY_BYTES {
            body.truncate(body.floor_char_boundary(GHOSTTY_BODY_BYTES - '…'.len_utf8()));
            body.push('…');
        }
        let sequence = match self {
            Self::Osc9 => format!("\x1b]9;{body}\x07"),
            Self::Osc777 => format!("\x1b]777;notify;Koda;{body}\x1b\\"),
            Self::Bel => unreachable!(),
        };
        if terminal.tmux {
            // tmux passes a DCS-wrapped sequence on to the outer terminal,
            // with each ESC doubled.
            format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
        } else {
            sequence
        }
    }
}

/// Whether to notify now, given the condition and the terminal's focus.
pub(crate) fn should_notify(condition: NotificationCondition, focused: bool) -> bool {
    match condition {
        NotificationCondition::Unfocused => !focused,
        NotificationCondition::Always => true,
    }
}

/// The session's notifier: the settings, read once, and the backend, gone
/// after a failed write.
struct Notifier {
    settings: Notifications,
    condition: NotificationCondition,
    backend: Option<Backend>,
    terminal: Terminal,
}

/// The terminal has focus, as its last report said. Focused until a report
/// says otherwise, as in Codex.
static FOCUSED: AtomicBool = AtomicBool::new(true);
static NOTIFIER: Mutex<Option<Notifier>> = Mutex::new(None);

/// Read the settings and the terminal. Called once, when the TUI starts.
pub(crate) fn init() {
    let tui = crate::user_config::tui();
    let terminal = Terminal::detect(|name| std::env::var(name).ok());
    let notifier = Notifier {
        settings: tui.notifications.unwrap_or(Notifications::Enabled(true)),
        condition: tui.notification_condition.unwrap_or_default(),
        backend: Some(Backend::for_method(
            tui.notification_method.unwrap_or_default(),
            terminal,
        )),
        terminal,
    };
    if let Ok(mut slot) = NOTIFIER.lock() {
        *slot = Some(notifier);
    }
}

/// The terminal reported gaining (`true`) or losing focus.
pub(crate) fn set_focused(focused: bool) {
    FOCUSED.store(focused, Ordering::Relaxed);
}

/// Post `notification` if the settings and the terminal's focus allow it.
pub(crate) fn notify(notification: Notification) {
    let Ok(mut slot) = NOTIFIER.lock() else {
        return;
    };
    let Some(notifier) = slot.as_mut() else {
        return;
    };
    if !notification.allowed_by(&notifier.settings)
        || !should_notify(notifier.condition, FOCUSED.load(Ordering::Relaxed))
    {
        return;
    }
    let Some(backend) = notifier.backend else {
        return;
    };
    let bytes = backend.sequence(&notification.display(), notifier.terminal);
    let mut out = std::io::stdout();
    if let Err(e) = out.write_all(bytes.as_bytes()).and_then(|()| out.flush()) {
        tracing::warn!("Couldn't send a notification ({e}); turning them off for this session");
        notifier.backend = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn auto_is_osc9_in_the_terminals_that_show_it_and_the_bell_elsewhere() {
        for vars in [
            vec![("TERM_PROGRAM", "ghostty")],
            vec![("TERM_PROGRAM", "iTerm.app")],
            vec![("TERM_PROGRAM", "WezTerm")],
            vec![("TERM_PROGRAM", "WarpTerminal")],
            vec![("TERM", "xterm-kitty")],
            // Inside tmux, the outer terminal's own variable.
            vec![
                ("TERM_PROGRAM", "tmux"),
                ("TMUX", "/tmp/t,1,0"),
                ("GHOSTTY_RESOURCES_DIR", "/x"),
            ],
        ] {
            let terminal = Terminal::detect(env(&vars));
            assert_eq!(
                Backend::for_method(NotificationMethod::Auto, terminal),
                Backend::Osc9,
                "{vars:?}"
            );
        }
        for vars in [
            vec![("TERM_PROGRAM", "Apple_Terminal")],
            vec![("TERM_PROGRAM", "vscode")],
            vec![("TERM_PROGRAM", "tmux"), ("TMUX", "/tmp/t,1,0")],
            vec![],
        ] {
            let terminal = Terminal::detect(env(&vars));
            assert_eq!(
                Backend::for_method(NotificationMethod::Auto, terminal),
                Backend::Bel,
                "{vars:?}"
            );
        }
    }

    #[test]
    fn a_named_method_wins_over_the_terminal() {
        let apple = Terminal::detect(env(&[("TERM_PROGRAM", "Apple_Terminal")]));
        assert_eq!(
            Backend::for_method(NotificationMethod::Osc9, apple),
            Backend::Osc9
        );
        assert_eq!(
            Backend::for_method(NotificationMethod::Osc777, apple),
            Backend::Osc777
        );
        let ghostty = Terminal::detect(env(&[("TERM_PROGRAM", "ghostty")]));
        assert_eq!(
            Backend::for_method(NotificationMethod::Bel, ghostty),
            Backend::Bel
        );
    }

    #[test]
    fn sequences_are_osc9_osc777_or_bel() {
        let plain = Terminal::default();
        assert_eq!(Backend::Bel.sequence("x", plain), "\x07");
        assert_eq!(Backend::Osc9.sequence("done", plain), "\x1b]9;done\x07");
        assert_eq!(
            Backend::Osc777.sequence("a;b", plain),
            "\x1b]777;notify;Koda;a；b\x1b\\"
        );
        // A control character can't end the sequence early.
        assert_eq!(
            Backend::Osc9.sequence("a\x07b\x1bc", plain),
            "\x1b]9;a b c\x07"
        );
    }

    #[test]
    fn tmux_gets_the_osc_wrapped_and_the_bell_as_is() {
        let tmux = Terminal {
            tmux: true,
            ..Terminal::default()
        };
        assert_eq!(
            Backend::Osc9.sequence("hi", tmux),
            "\x1bPtmux;\x1b\x1b]9;hi\x07\x1b\\"
        );
        assert_eq!(Backend::Bel.sequence("hi", tmux), "\x07");
    }

    #[test]
    fn ghostty_bodies_are_capped_at_255_bytes() {
        let ghostty = Terminal {
            osc9: true,
            ghostty: true,
            tmux: false,
        };
        let long = "é".repeat(300);
        let seq = Backend::Osc9.sequence(&long, ghostty);
        let body = seq
            .strip_prefix("\x1b]9;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert!(body.len() <= 255, "{}", body.len());
        assert!(body.ends_with('…'));
        // Other terminals get it whole.
        let full = Backend::Osc9.sequence(&long, Terminal::default());
        assert!(full.contains(&long));
    }

    #[test]
    fn messages_are_flattened_and_cut() {
        let reply = Notification::TurnComplete {
            message: format!("Fixed  it.\n\n{}", "x".repeat(300)),
        };
        let shown = reply.display();
        assert!(shown.starts_with("Fixed it. xxx"), "{shown}");
        assert_eq!(shown.chars().count(), PREVIEW_CHARS);
        assert!(shown.ends_with('…'));
        assert_eq!(
            Notification::TurnComplete {
                message: " \n".into()
            }
            .display(),
            "Koda finished"
        );
        assert_eq!(
            Notification::ApprovalRequested {
                subject: "Bash: cargo test".into()
            }
            .display(),
            "Approval requested: Bash: cargo test"
        );
        assert_eq!(
            Notification::UserInputRequested {
                question: "Which branch should I merge into, main or release?".into()
            }
            .display(),
            "Question: Which branch should I merge i…"
        );
    }

    #[test]
    fn the_setting_picks_events_by_codexs_names() {
        let done = Notification::TurnComplete {
            message: String::new(),
        };
        let ask = Notification::ApprovalRequested {
            subject: String::new(),
        };
        assert!(done.allowed_by(&Notifications::Enabled(true)));
        assert!(!done.allowed_by(&Notifications::Enabled(false)));
        let only_approvals = Notifications::Custom(vec!["approval-requested".into()]);
        assert!(ask.allowed_by(&only_approvals));
        assert!(!done.allowed_by(&only_approvals));
    }

    #[test]
    fn unfocused_waits_for_focus_to_go() {
        assert!(!should_notify(NotificationCondition::Unfocused, true));
        assert!(should_notify(NotificationCondition::Unfocused, false));
        assert!(should_notify(NotificationCondition::Always, true));
    }
}
