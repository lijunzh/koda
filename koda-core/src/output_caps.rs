//! Centralized tool output caps, scaled to the model's context window.
//!
//! All tool output limits live here instead of being scattered across 6+ files.
//! Caps scale linearly from a floor (current defaults) up to a 4× ceiling
//! as the context window grows.
//!
//! Scaling formula:
//!   `clamp(base × (ctx / BASELINE), base, base × MAX_SCALE)`
//!
//! | Context window | Scale factor | Effect            |
//! |----------------|-------------|-------------------|
//! | 4K             | 0.04×       | Floor (base)      |
//! | 100K           | 1.0×        | Base (current)    |
//! | 200K           | 2.0×        | 2× current        |
//! | 1M             | 10.0×       | Ceiling (4× base) |

/// Baseline context window for scaling (100K tokens = 1.0× factor).
const BASELINE: f64 = 100_000.0;

/// Maximum scale multiplier (4× base values).
const MAX_SCALE: f64 = 4.0;

/// Pre-computed output caps for all tools in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCaps {
    /// Max chars for tool results stored in conversation history.
    /// Base: 10,000 (was `MAX_TOOL_RESULT_CHARS` in `tool_dispatch.rs`).
    pub tool_result_chars: usize,

    /// Max chars for web page body content.
    /// Base: 15,000 (was `MAX_BODY_CHARS` in `web_fetch.rs`).
    pub web_body_chars: usize,

    /// Max lines of shell command output.
    /// Base: 256 (was `MAX_OUTPUT_LINES` in `shell.rs`).
    pub shell_output_lines: usize,

    /// Max grep matches returned.
    /// Base: 100 (was `MAX_MATCHES` in `grep.rs`).
    pub grep_matches: usize,

    /// Max directory listing entries.
    /// Base: 200 (was `MAX_ENTRIES` in `file_tools.rs`).
    pub list_entries: usize,

    /// Max glob search results.
    /// Base: 200 (was `MAX_RESULTS` in `glob_tool.rs`).
    pub glob_results: usize,
}

impl OutputCaps {
    // ── Base values (floors) ─────────────────────────────────
    const BASE_TOOL_RESULT_CHARS: usize = 10_000;
    const BASE_WEB_BODY_CHARS: usize = 15_000;
    const BASE_SHELL_OUTPUT_LINES: usize = 256;
    const BASE_GREP_MATCHES: usize = 100;
    const BASE_LIST_ENTRIES: usize = 200;
    const BASE_GLOB_RESULTS: usize = 200;

    /// Compute caps scaled to the given context window size (in tokens).
    pub fn for_context(max_context_tokens: usize) -> Self {
        let factor = (max_context_tokens as f64 / BASELINE).clamp(1.0, MAX_SCALE);

        Self {
            tool_result_chars: scale(Self::BASE_TOOL_RESULT_CHARS, factor),
            web_body_chars: scale(Self::BASE_WEB_BODY_CHARS, factor),
            shell_output_lines: scale(Self::BASE_SHELL_OUTPUT_LINES, factor),
            grep_matches: scale(Self::BASE_GREP_MATCHES, factor),
            list_entries: scale(Self::BASE_LIST_ENTRIES, factor),
            glob_results: scale(Self::BASE_GLOB_RESULTS, factor),
        }
    }
}

impl Default for OutputCaps {
    /// Default caps (100K context baseline — matches legacy hardcoded values).
    fn default() -> Self {
        Self::for_context(100_000)
    }
}

/// Scale a base value by factor, rounding to nearest integer.
fn scale(base: usize, factor: f64) -> usize {
    (base as f64 * factor).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_context_gets_base_values() {
        let caps = OutputCaps::for_context(4_096);
        assert_eq!(caps.tool_result_chars, OutputCaps::BASE_TOOL_RESULT_CHARS);
        assert_eq!(caps.shell_output_lines, OutputCaps::BASE_SHELL_OUTPUT_LINES);
        assert_eq!(caps.grep_matches, OutputCaps::BASE_GREP_MATCHES);
        assert_eq!(caps.list_entries, OutputCaps::BASE_LIST_ENTRIES);
    }

    #[test]
    fn baseline_context_gets_base_values() {
        let caps = OutputCaps::for_context(100_000);
        assert_eq!(caps.tool_result_chars, 10_000);
        assert_eq!(caps.web_body_chars, 15_000);
        assert_eq!(caps.shell_output_lines, 256);
        assert_eq!(caps.grep_matches, 100);
        assert_eq!(caps.list_entries, 200);
        assert_eq!(caps.glob_results, 200);
    }

    #[test]
    fn double_context_doubles_caps() {
        let caps = OutputCaps::for_context(200_000);
        assert_eq!(caps.tool_result_chars, 20_000);
        assert_eq!(caps.web_body_chars, 30_000);
        assert_eq!(caps.shell_output_lines, 512);
        assert_eq!(caps.grep_matches, 200);
        assert_eq!(caps.list_entries, 400);
        assert_eq!(caps.glob_results, 400);
    }

    #[test]
    fn million_context_caps_at_4x() {
        let caps = OutputCaps::for_context(1_000_000);
        assert_eq!(caps.tool_result_chars, 40_000);
        assert_eq!(caps.web_body_chars, 60_000);
        assert_eq!(caps.shell_output_lines, 1024);
        assert_eq!(caps.grep_matches, 400);
        assert_eq!(caps.list_entries, 800);
        assert_eq!(caps.glob_results, 800);
    }

    #[test]
    fn default_matches_baseline() {
        assert_eq!(OutputCaps::default(), OutputCaps::for_context(100_000));
    }

    #[test]
    fn intermediate_context_scales_linearly() {
        let caps = OutputCaps::for_context(150_000);
        // 1.5× base
        assert_eq!(caps.tool_result_chars, 15_000);
        assert_eq!(caps.shell_output_lines, 384);
    }
}
