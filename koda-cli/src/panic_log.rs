//! Panic log: write a forensic record to `~/.config/koda/logs/panic.log`
//! every time the process panics (#1122).
//!
//! Companion to the panic hook in [`crate::tui_viewport::install_panic_hook`]
//! (added in #1120). The hook restores the terminal so the user sees the
//! crash on a sane TTY; this module ensures we ALSO leave a forensic
//! breadcrumb on disk so later debugging doesn't depend on the user
//! scrolling back through their terminal history.
//!
//! # Design constraints
//!
//! 1. **Best-effort.** A panic-in-panic-hook would abort the process before
//!    the original hook prints the message — so every I/O call here is
//!    wrapped in `let _ = ...`. We never propagate errors out of the hook.
//! 2. **No allocation in failure paths.** Most of the formatting uses owned
//!    strings (we're already crashing, GC pressure is irrelevant), but the
//!    write itself buffers locally before touching disk so a failed open
//!    doesn't leave a half-formatted record.
//! 3. **Rotation cap.** Bounded at 5 MB / 3 generations so the panic log
//!    can never grow unboundedly even if a user hits a deterministic crash
//!    on every startup.
//!
//! # Format
//!
//! Each panic appends a delimited record:
//!
//! ```text
//! ======================================================================
//! [2026-04-29T20:55:32Z] PANIC
//! version:    koda 0.2.23
//! location:   koda-core/src/foo.rs:42:8
//! thread:     <unnamed>
//! message:    assertion failed: x > 0
//! backtrace:
//!   <captured if RUST_BACKTRACE=1 or =full, otherwise: "(disabled)">
//! ======================================================================
//! ```

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

/// Maximum panic.log size before rotation (bytes).
pub(crate) const MAX_PANIC_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Number of rotated generations to keep (panic.log.1 .. panic.log.N).
pub(crate) const ROTATION_GENERATIONS: usize = 3;

/// Resolve the panic log path: `<config_dir>/logs/panic.log`.
///
/// Returns `None` when the config directory cannot be determined — in that
/// case we silently skip writing rather than panic-in-panic.
fn panic_log_path() -> Option<PathBuf> {
    let dir = koda_core::db::config_dir().ok()?;
    Some(dir.join("logs").join("panic.log"))
}

/// Public entry point called from the panic hook.
///
/// Writes a forensic record to `panic.log`, rotating first if the file is
/// over [`MAX_PANIC_LOG_BYTES`]. All I/O failures are swallowed — see the
/// module docstring for the panic-in-panic-hook reasoning.
pub fn write_panic_log(info: &PanicHookInfo<'_>) {
    let Some(path) = panic_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = rotate_if_needed(&path, MAX_PANIC_LOG_BYTES, ROTATION_GENERATIONS);

    let Ok(mut file) = OpenOptions::new().append(true).create(true).open(&path) else {
        return;
    };

    // Build the record in-memory first so a partial write (disk full mid-record)
    // produces a truncated tail rather than corrupting the next record's header.
    let mut buf = Vec::with_capacity(1024);
    write_panic_record(&mut buf, info, &iso8601_now(), env!("CARGO_PKG_VERSION"));
    let _ = file.write_all(&buf);
    let _ = file.flush();
}

/// Format a panic record into `writer`. Pure function for unit testing.
///
/// Separated from [`write_panic_log`] so the formatting can be exercised
/// without touching the filesystem or the global panic hook.
pub(crate) fn write_panic_record<W: Write>(
    writer: &mut W,
    info: &PanicHookInfo<'_>,
    timestamp: &str,
    version: &str,
) {
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "(unknown)".to_string());

    // PanicHookInfo::payload() carries the panic argument. The conventional
    // payload types are &str (from `panic!("...")`) and String (from
    // `panic!("{}", ...)`). Anything else falls back to a placeholder.
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".to_string());

    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();

    // Backtrace is only useful when RUST_BACKTRACE is enabled; otherwise
    // capture() returns an unresolved/disabled marker. Record explicitly
    // so users reading panic.log know whether to re-run with the env var.
    let backtrace = std::backtrace::Backtrace::capture();
    let backtrace_str = match backtrace.status() {
        std::backtrace::BacktraceStatus::Captured => format!("{backtrace}"),
        std::backtrace::BacktraceStatus::Disabled => {
            "(disabled — re-run with RUST_BACKTRACE=1 to capture)".to_string()
        }
        _ => "(unsupported on this platform)".to_string(),
    };

    let _ = writeln!(
        writer,
        "======================================================================"
    );
    let _ = writeln!(writer, "[{timestamp}] PANIC");
    let _ = writeln!(writer, "version:    koda {version}");
    let _ = writeln!(writer, "location:   {location}");
    let _ = writeln!(writer, "thread:     {thread}");
    let _ = writeln!(writer, "message:    {message}");
    let _ = writeln!(writer, "backtrace:");
    for line in backtrace_str.lines() {
        let _ = writeln!(writer, "  {line}");
    }
    let _ = writeln!(
        writer,
        "======================================================================\n"
    );
}

/// Rotate `panic.log` if its size exceeds `max_bytes`.
///
/// Generation scheme: shift `panic.log.{N-1} -> panic.log.N`, drop the
/// oldest, then move `panic.log -> panic.log.1`. After rotation,
/// `panic.log` does not exist; callers re-create it via `OpenOptions::create`.
///
/// All I/O is best-effort and silent. If rotation fails halfway, we
/// continue writing into whatever file exists rather than panic-in-panic.
pub(crate) fn rotate_if_needed(
    path: &Path,
    max_bytes: u64,
    generations: usize,
) -> std::io::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        // No file yet → no rotation needed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }

    // Drop the oldest generation if it's about to be overflowed.
    let oldest = numbered_path(path, generations);
    let _ = fs::remove_file(&oldest);

    // Shift remaining generations down: panic.log.N-1 -> panic.log.N
    for n in (1..generations).rev() {
        let src = numbered_path(path, n);
        let dst = numbered_path(path, n + 1);
        if src.exists() {
            let _ = fs::rename(&src, &dst);
        }
    }

    // Move current file into the .1 slot.
    let _ = fs::rename(path, numbered_path(path, 1));
    Ok(())
}

/// Compose `<basename>.<n>` from a base path: `panic.log` + 2 → `panic.log.2`.
fn numbered_path(base: &Path, n: usize) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

/// ISO 8601 / RFC 3339 timestamp at UTC, second precision.
///
/// Uses `time` because it's already a `koda-cli` dependency. We only need
/// formatting (not parsing) so the default features suffice.
fn iso8601_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "(timestamp unavailable)".to_string())
}

#[cfg(test)]
mod tests {
    //! Tests for panic.log writing + rotation (#1122).
    //!
    //! These are pure unit tests: we don't trigger real panics here (the
    //! panic-hook chain integrity is covered by `tui_viewport`'s existing
    //! `panic_hook_tests` module). Instead we exercise the format + rotate
    //! helpers in isolation.
    //!
    //! Building a `PanicHookInfo` from outside `std::panic` is awkward
    //! (the type's constructor is private), so we trigger a real panic
    //! inside `catch_unwind` and capture the formatted record from inside
    //! our own hook via a static `Mutex<Option<String>>`.
    //!
    //! `serial_test` is required because `panic::set_hook` is global state.
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    /// One-shot slot for a formatted panic record produced inside the
    /// hook. Static because the hook closure must be `'static + Sync`.
    static CAPTURED_RECORD: Mutex<Option<String>> = Mutex::new(None);
    #[test]
    #[serial]
    fn write_panic_record_includes_message_location_version_timestamp() {
        // Trigger a real panic inside catch_unwind so we get a genuine
        // PanicHookInfo — there's no public constructor for that type.
        // The hook formats the record into CAPTURED_RECORD; the test
        // body asserts on the captured string after.
        *CAPTURED_RECORD.lock().unwrap() = None;
        let saved = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut buf: Vec<u8> = Vec::new();
            write_panic_record(&mut buf, info, "2026-01-01T00:00:00Z", "9.9.9");
            *CAPTURED_RECORD.lock().unwrap() = Some(String::from_utf8_lossy(&buf).into_owned());
        }));
        let _ = std::panic::catch_unwind(|| {
            panic!("intentional test panic for #1122");
        });
        std::panic::set_hook(saved);

        let formatted = CAPTURED_RECORD.lock().unwrap().clone().expect("hook ran");

        // Spot-check the salient fields appear.
        assert!(formatted.contains("PANIC"), "header present");
        assert!(
            formatted.contains("intentional test panic for #1122"),
            "message"
        );
        assert!(formatted.contains("2026-01-01T00:00:00Z"), "timestamp");
        assert!(formatted.contains("koda 9.9.9"), "version");
        assert!(formatted.contains("location:"), "location field");
        assert!(formatted.contains("backtrace:"), "backtrace field");
        // Buffer ends with a delimiter line (sanity that we're appending
        // a complete record, not a half one).
        assert!(formatted.trim_end().ends_with('='), "trailing delimiter");
    }

    #[test]
    fn rotate_does_nothing_when_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("panic.log");
        std::fs::write(&path, b"small content").unwrap();
        rotate_if_needed(&path, 1024, 3).unwrap();
        // Original file untouched, no .1 sibling created.
        assert!(path.exists());
        assert!(!numbered_path(&path, 1).exists());
    }

    #[test]
    fn rotate_shifts_generations_when_over_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("panic.log");
        // Seed an "old" panic.log over the threshold so rotation fires.
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        // Pre-existing rotated generations:
        std::fs::write(numbered_path(&path, 1), b"gen1").unwrap();
        std::fs::write(numbered_path(&path, 2), b"gen2").unwrap();

        rotate_if_needed(&path, 100, 3).unwrap();

        // After rotation:
        // - panic.log     : gone (caller will recreate)
        // - panic.log.1   : was-current (200 'x' bytes)
        // - panic.log.2   : was-.1 (b"gen1")
        // - panic.log.3   : was-.2 (b"gen2")
        assert!(!path.exists(), "current rotated away");
        assert_eq!(std::fs::read(numbered_path(&path, 1)).unwrap().len(), 200);
        assert_eq!(std::fs::read(numbered_path(&path, 2)).unwrap(), b"gen1");
        assert_eq!(std::fs::read(numbered_path(&path, 3)).unwrap(), b"gen2");
    }

    #[test]
    fn rotate_drops_oldest_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("panic.log");
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        std::fs::write(numbered_path(&path, 1), b"gen1").unwrap();
        std::fs::write(numbered_path(&path, 2), b"gen2").unwrap();
        std::fs::write(numbered_path(&path, 3), b"oldest_to_drop").unwrap();

        rotate_if_needed(&path, 100, 3).unwrap();

        // .3 should now hold what was in .2 — the previous .3 ("oldest_to_drop")
        // is dropped to keep at most `generations` files around.
        assert_eq!(std::fs::read(numbered_path(&path, 3)).unwrap(), b"gen2");
        assert!(
            !numbered_path(&path, 4).exists(),
            "rotation must not create a 4th generation"
        );
    }

    #[test]
    fn rotate_no_op_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.log");
        // Should not error out — caller passes through every panic, so
        // a fresh install with no prior log must be a clean no-op.
        rotate_if_needed(&path, 100, 3).unwrap();
        assert!(!path.exists());
    }
}
