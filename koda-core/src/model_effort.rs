//! Reasoning effort per model: the levels a model takes, and the one Koda
//! sends when nobody chose.
//!
//! A table, not a probe: Koda serves a few models, and one that isn't here
//! gets no effort sent, so its server's default applies. Each default is what
//! Claude Code sends for that model, `medium` for the 5.5 generation and
//! `high` before it, so Koda thinks as hard as its peer does on the same model.
//! The default is sent, not left out: the API treats a model's own default the
//! same either way, and the log then names the level used.
//!
//! A level someone chose that the model doesn't take is handled by who chose
//! it. One typed for this run ([`crate::model_effort::check_typed`]) is
//! refused with the levels listed, since a person can fix it. One from an
//! agent config, or carried across a model switch, drops to the highest level
//! at or below it, once with a warning ([`crate::model_effort::applied`]),
//! since nobody is there to ask and a 400 would end the run.
//!
//! Koda is built for the current Claude models: the latest model always
//! outranks backward compatibility (`DESIGN.md`). A Claude model missing from
//! the table is treated as current, with no effort sent. The older line
//! ([`is_older_claude`]) is recognized only to warn ([`older_claude_notice`]).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Effort levels from least to most thinking.
pub const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// What one model takes.
#[derive(Debug, PartialEq, Eq)]
pub struct ModelEffort {
    /// Levels the model accepts, in [`LEVELS`] order.
    pub levels: &'static [&'static str],
    /// The level sent when nobody chose one.
    pub default: &'static str,
}

/// Claude's adaptive-thinking models take the whole ladder.
const CLAUDE_LADDER: &[&str] = &LEVELS;

/// Claude models whose effort Koda sets, by API id. Claude Code 2.1.296's
/// choices, captured against a local stub on 2026-11-03. These models also
/// reject a `budget_tokens` thinking budget.
const TABLE: &[(&str, ModelEffort)] = &[
    (
        "claude-haiku-5-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "medium",
        },
    ),
    (
        "claude-sonnet-5-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "medium",
        },
    ),
    (
        "claude-opus-5-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "medium",
        },
    ),
    (
        "claude-fable-5-1",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "high",
        },
    ),
    (
        "claude-fable-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "high",
        },
    ),
    (
        "claude-opus-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "high",
        },
    ),
    (
        "claude-sonnet-5",
        ModelEffort {
            levels: CLAUDE_LADDER,
            default: "high",
        },
    ),
];

/// The table entry for `model`: an API id, with or without a `-YYYYMMDD`
/// snapshot date, or an alias such as `claude-haiku`.
pub fn lookup(model: &str) -> Option<&'static ModelEffort> {
    let resolved = crate::model_alias::resolve(model).map(|r| r.model_id);
    let id = resolved.as_deref().unwrap_or(model);
    let id = match id.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => id,
    };
    TABLE.iter().find(|(name, _)| *name == id).map(|(_, e)| e)
}

/// The level to send to `model`, given the one chosen for the session, if
/// any. `None` when the model isn't in the table: send nothing.
///
/// A chosen level the model doesn't take drops to the highest it takes at or
/// below, and one that isn't a level at all falls back to the default. Either
/// way a warning is logged once per model and level.
pub fn applied(model: &str, chosen: Option<&str>) -> Option<&'static str> {
    let entry = lookup(model)?;
    let Some(chosen) = chosen else {
        return Some(entry.default);
    };
    if let Some(level) = entry.levels.iter().find(|l| **l == chosen) {
        return Some(level);
    }
    let applied = match LEVELS.iter().position(|l| *l == chosen) {
        Some(rank) => entry
            .levels
            .iter()
            .rev()
            .find(|l| LEVELS.iter().position(|x| x == *l) <= Some(rank))
            .copied()
            .unwrap_or(entry.default),
        None => entry.default,
    };
    warn_once(
        model,
        chosen,
        &format!("{model} doesn't take effort {chosen:?}; sending {applied:?}"),
    );
    Some(applied)
}

/// Check a level typed for this run. `Err` says which levels `model` takes.
/// A model not in the table takes whatever its server takes, so it passes.
pub fn check_typed(model: &str, level: &str) -> Result<(), String> {
    match lookup(model) {
        Some(entry) if !entry.levels.contains(&level) => Err(format!(
            "{model} doesn't take effort {level:?}. Use one of: {}.",
            entry.levels.join(", ")
        )),
        _ => Ok(()),
    }
}

/// A thinking budget reached a Claude request, from an agent file or a
/// setting meant for Gemini: it isn't sent, since current Claude models take
/// effort and refuse a budget. Logged once per model.
pub fn warn_ignored_thinking_budget(model: &str) {
    warn_once(
        model,
        "thinking budget",
        &format!("{model} takes no thinking budget, so it isn't sent; set the effort instead"),
    );
}

/// Whether `model` is from the older Claude line, which Koda no longer
/// adapts to: Claude 2 and 3, Instant, and the 4.x Haiku, Sonnet and Opus.
pub fn is_older_claude(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    ["claude-2", "claude-3", "claude-instant"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
        || ["haiku", "sonnet", "opus"]
            .iter()
            .any(|line| id.starts_with(&format!("claude-{line}-4")))
}

/// The warning for an older Claude model, the first time it's used in this
/// process; `None` for a current model, or one already warned about.
pub fn older_claude_notice(model: &str) -> Option<String> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if !is_older_claude(model) {
        return None;
    }
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(model.to_string());
    first.then(|| {
        format!(
            "{model} is an older Claude model. Koda is built for the current ones and no \
             longer adapts to older models, so requests may fail. Pick a current model with /model."
        )
    })
}

/// The effort a request to `provider` carries, for the log: the table's
/// level on Anthropic, the chosen one passed through elsewhere.
pub fn sent(provider: &str, model: &str, chosen: Option<&str>) -> Option<String> {
    match provider {
        "anthropic" => applied(model, chosen).map(str::to_string),
        _ => chosen.map(str::to_string),
    }
}

/// Log `message` once per model and `what`: a setting that's corrected on
/// every request shouldn't fill the log.
fn warn_once(model: &str, what: &str, message: &str) {
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((model.to_string(), what.to_string()));
    if first {
        tracing::warn!(model, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_claude_code() {
        for (model, level) in [
            ("claude-haiku-5-5", "medium"),
            ("claude-sonnet-5-5", "medium"),
            ("claude-opus-5-5", "medium"),
            ("claude-fable-5-1", "high"),
            ("claude-opus-5", "high"),
            ("claude-sonnet-5", "high"),
        ] {
            assert_eq!(applied(model, None), Some(level), "{model}");
        }
    }

    #[test]
    fn lookup_reads_snapshots_and_aliases() {
        let haiku = lookup("claude-haiku-5-5");
        assert!(haiku.is_some());
        assert_eq!(lookup("claude-haiku-5-5-20261001"), haiku);
        assert_eq!(lookup("claude-haiku"), haiku);
        // `claude-opus-5` must not be found as a prefix of `claude-opus-5-5`.
        assert_eq!(applied("claude-opus-5-5", None), Some("medium"));
        assert_eq!(applied("claude-opus-5", None), Some("high"));
    }

    #[test]
    fn a_model_not_in_the_table_gets_nothing() {
        assert_eq!(applied("claude-opus-9", None), None);
        assert_eq!(applied("ornith", Some("high")), None);
        assert!(check_typed("ornith", "anything").is_ok());
    }

    #[test]
    fn a_chosen_level_is_sent_and_an_unknown_one_falls_back() {
        assert_eq!(applied("claude-sonnet-5-5", Some("high")), Some("high"));
        assert_eq!(applied("claude-sonnet-5-5", Some("max")), Some("max"));
        assert_eq!(
            applied("claude-sonnet-5-5", Some("minimal")),
            Some("medium")
        );
    }

    #[test]
    fn a_typed_level_the_model_does_not_take_is_refused_with_the_list() {
        assert!(check_typed("claude-haiku-5-5", "xhigh").is_ok());
        let err = check_typed("claude-haiku-5-5", "ultra").unwrap_err();
        assert!(err.contains("low, medium, high, xhigh, max"), "{err}");
    }

    #[test]
    fn the_older_line_is_recognized_and_warned_about_once() {
        for older in [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-6",
            "claude-opus-4-8",
            "claude-3-5-sonnet-20241022",
            "claude-3-opus-20240229",
            "claude-2.1",
            "claude-instant-1.2",
        ] {
            assert!(is_older_claude(older), "{older}");
        }
        for current in [
            "claude-haiku-5-5",
            "claude-sonnet-5",
            "claude-opus-5-5",
            "claude-fable-5-2",
            "claude-opus-9",
            "claude-mythos-5",
        ] {
            assert!(!is_older_claude(current), "{current}");
        }
        assert!(older_claude_notice("claude-opus-4-7").is_some());
        assert!(older_claude_notice("claude-opus-4-7").is_none(), "once");
        assert!(older_claude_notice("claude-opus-5-5").is_none());
    }
}
