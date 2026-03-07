//! Slash command completion for the TUI input.
//!
//! When the user types `/` and presses Tab, this module cycles
//! through matching command names.

/// All known slash commands.
pub const SLASH_COMMANDS: &[&str] = &[
    "/agent",
    "/compact",
    "/cost",
    "/diff",
    "/diff commit",
    "/diff review",
    "/exit",
    "/expand",
    "/help",
    "/mcp",
    "/memory",
    "/model",
    "/provider",
    "/sessions",
    "/trust",
    "/verbose",
];

/// Tab-completion state tracker.
pub struct SlashCompleter {
    /// Current matches for the prefix.
    matches: Vec<&'static str>,
    /// Index into `matches` for cycling.
    idx: usize,
    /// The original prefix the user typed.
    prefix: String,
}

impl SlashCompleter {
    pub fn new() -> Self {
        Self {
            matches: Vec::new(),
            idx: 0,
            prefix: String::new(),
        }
    }

    /// Attempt to complete the given input.
    ///
    /// Returns `Some(completed_text)` if there's a match, `None` otherwise.
    /// Repeated calls with the same prefix cycle through matches.
    pub fn complete(&mut self, current_text: &str) -> Option<&'static str> {
        if !current_text.starts_with('/') {
            self.reset();
            return None;
        }

        let trimmed = current_text.trim();

        // If prefix changed, rebuild matches
        if trimmed != self.prefix && !self.matches.contains(&trimmed) {
            self.prefix = trimmed.to_string();
            self.matches = SLASH_COMMANDS
                .iter()
                .filter(|cmd| cmd.starts_with(trimmed) && **cmd != trimmed)
                .copied()
                .collect();
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let result = self.matches[self.idx];
        self.idx = (self.idx + 1) % self.matches.len();
        Some(result)
    }

    /// Reset completion state (called when input changes non-Tab).
    pub fn reset(&mut self) {
        self.matches.clear();
        self.idx = 0;
        self.prefix.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_slash_d() {
        let mut c = SlashCompleter::new();
        let first = c.complete("/d");
        assert!(first.is_some());
        assert!(first.unwrap().starts_with("/d"));
    }

    #[test]
    fn test_complete_cycles() {
        let mut c = SlashCompleter::new();
        let a = c.complete("/d");
        let b = c.complete("/d");
        // /diff, /diff commit, /diff review are matches
        assert!(a.is_some());
        assert!(b.is_some());
    }

    #[test]
    fn test_no_match() {
        let mut c = SlashCompleter::new();
        assert!(c.complete("/zzz").is_none());
    }

    #[test]
    fn test_non_slash_returns_none() {
        let mut c = SlashCompleter::new();
        assert!(c.complete("hello").is_none());
    }

    #[test]
    fn test_exact_match_no_complete() {
        let mut c = SlashCompleter::new();
        // "/exit" is an exact match, no further completion
        assert!(c.complete("/exit").is_none());
    }
}
