//! Shared utilities for `koda-cli`.
//!
//! Centralises the one piece of logic that was previously duplicated:
//! getting the current UTC time. Both callers then format the components
//! they need with a plain `format!()` — no magic, no hidden coupling.
//!
//! ## Why `time` and not hand-rolled math?
//!
//! The previous implementations in `transcript.rs` (Hinnant O(1) with a
//! `u32` that overflows in 2106) and `tui_commands.rs` (year-walk O(year)
//! with `u64`) were independently written, disagreed on types, and neither
//! was going to be kept in sync with the other (#818).
//!
//! `time v0.3` is already a transitive dependency (via `ratatui`) so
//! declaring it directly adds zero new weight to the binary.

use time::OffsetDateTime;

/// Return the current instant in UTC.
///
/// Callers format the components they need:
///
/// ```ignore
/// let dt = utc_now();
/// // transcript header:  "2026-04-11 14:30 UTC"
/// format!("{:04}-{:02}-{:02} {:02}:{:02} UTC",
///     dt.year(), dt.month() as u8, dt.day(), dt.hour(), dt.minute())
///
/// // export filename:    "20260411-143022"
/// format!("{:04}{:02}{:02}-{:02}{:02}{:02}",
///     dt.year(), dt.month() as u8, dt.day(),
///     dt.hour(), dt.minute(), dt.second())
/// ```
pub(crate) fn utc_now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}
