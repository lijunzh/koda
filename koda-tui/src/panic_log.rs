//! A panic, logged: the TUI's panic hook records it as an `error` in the
//! log, with Koda's version and the backtrace, then stores the log
//! database's queue at once, since a release build aborts as the hook
//! returns (`panic = "abort"`).
//!
//! Codex's TUI hook logs `panic: {info}` with `tracing::error!` too. Before
//! the log database, Koda also wrote `panic.log` (#1122): a separate file
//! was the only record that outlived the per-process log files' pruning.
//! The database keeps the panic with the session it happened in.
//!
//! Best-effort throughout: a panic inside the hook would abort before the
//! original hook prints the message, so nothing here can fail loudly.

use std::panic::PanicHookInfo;

/// Log the panic `info` describes as an `error`, with version and
/// backtrace, and store it before returning.
pub(crate) fn record_panic(info: &PanicHookInfo<'_>) {
    tracing::error!(
        target: "koda_cli::panic",
        version = env!("CARGO_PKG_VERSION"),
        backtrace = %backtrace_text(),
        "{}",
        panic_breadcrumb(info)
    );
    koda_state::log_db::flush_blocking(std::time::Duration::from_secs(2));
}

/// The backtrace, or why there isn't one: it is captured only with
/// `RUST_BACKTRACE` set.
fn backtrace_text() -> String {
    let backtrace = std::backtrace::Backtrace::capture();
    match backtrace.status() {
        std::backtrace::BacktraceStatus::Captured => format!("{backtrace}"),
        std::backtrace::BacktraceStatus::Disabled => {
            "(disabled; run with RUST_BACKTRACE=1 to capture it)".to_string()
        }
        _ => "(unsupported on this platform)".to_string(),
    }
}

/// Single-line summary fields extracted from a [`PanicHookInfo`].
///
/// [`panic_breadcrumb`] formats it into the single line [`record_panic`]
/// logs.
///
/// All fields are owned `String`s so the struct can outlive the
/// `PanicHookInfo` borrow if a caller needs to defer formatting.
pub(crate) struct PanicSummary {
    pub thread: String,
    pub location: String,
    pub message: String,
}

/// Extract the human-readable summary fields from a panic hook payload.
///
/// Pure function (no I/O, no logging) so it's safe to call from inside
/// any panic hook implementation. Conventional payload types (`&str`
/// from `panic!("...")`, `String` from `panic!("{}", ...)`) are
/// downcast; anything else degrades to a placeholder.
pub(crate) fn panic_summary(info: &PanicHookInfo<'_>) -> PanicSummary {
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "(unknown)".to_string());

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

    PanicSummary {
        thread,
        location,
        message,
    }
}

/// Format a panic into a single-line breadcrumb suitable for
/// `tracing::error!`. Pure function, no allocations beyond the result.
///
/// Format: `thread '<name>' panicked at <location>: <message>`
///
/// This deliberately mirrors the rustc default panic message so logs
/// scraped by humans grep the same way they would for a normal panic.
pub(crate) fn panic_breadcrumb(info: &PanicHookInfo<'_>) -> String {
    let s = panic_summary(info);
    format!(
        "thread '{}' panicked at {}: {}",
        s.thread, s.location, s.message
    )
}

#[cfg(test)]
mod tests {
    //! A `PanicHookInfo` has no public constructor, so these panic for real
    //! inside `catch_unwind` with a hook of their own, which is global
    //! state: hence `serial`.
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    /// Captured `(thread, location, message)` triple from
    /// `panic_summary` for the breadcrumb tests below.
    static CAPTURED_SUMMARY: Mutex<Option<(String, String, String)>> = Mutex::new(None);
    static CAPTURED_BREADCRUMB: Mutex<Option<String>> = Mutex::new(None);

    #[test]
    #[serial]
    fn panic_summary_extracts_thread_location_and_message() {
        *CAPTURED_SUMMARY.lock().unwrap() = None;
        let saved = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let s = panic_summary(info);
            *CAPTURED_SUMMARY.lock().unwrap() = Some((s.thread, s.location, s.message));
        }));
        let _ = std::panic::catch_unwind(|| {
            panic!("summary test panic");
        });
        std::panic::set_hook(saved);

        let (thread, location, message) =
            CAPTURED_SUMMARY.lock().unwrap().clone().expect("hook ran");
        assert!(!thread.is_empty(), "thread name populated");
        // Location is `file:line:col` — should mention this very file.
        assert!(
            location.contains("panic_log.rs"),
            "location points at panic site, got: {location}"
        );
        assert_eq!(message, "summary test panic");
    }

    #[test]
    #[serial]
    fn panic_breadcrumb_renders_single_line_in_rustc_format() {
        *CAPTURED_BREADCRUMB.lock().unwrap() = None;
        let saved = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            *CAPTURED_BREADCRUMB.lock().unwrap() = Some(panic_breadcrumb(info));
        }));
        let _ = std::panic::catch_unwind(|| {
            panic!("breadcrumb test panic");
        });
        std::panic::set_hook(saved);

        let line = CAPTURED_BREADCRUMB
            .lock()
            .unwrap()
            .clone()
            .expect("hook ran");
        // Single line, no embedded newlines (this is the property that
        // makes it suitable for tracing::error! — a multi-line message
        // would break log parsers and grep workflows).
        assert!(
            !line.contains('\n'),
            "breadcrumb must be single line, got: {line:?}"
        );
        // Mirrors rustc's default panic message format so humans grep
        // it the same way.
        assert!(line.starts_with("thread '"), "got: {line}");
        assert!(line.contains("' panicked at "), "got: {line}");
        assert!(line.contains("breadcrumb test panic"), "got: {line}");
        assert!(line.contains("panic_log.rs"), "got: {line}");
    }

    /// The panic is logged as an `error` with its message, the version and
    /// a backtrace field.
    #[test]
    #[serial]
    fn a_panic_is_logged_as_an_error_with_version_and_backtrace() {
        use std::sync::Arc;
        use tracing_subscriber::prelude::*;

        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone()),
        );
        let saved = std::panic::take_hook();
        std::panic::set_hook(Box::new(record_panic));
        tracing::subscriber::with_default(subscriber, || {
            let _ = std::panic::catch_unwind(|| panic!("logged test panic"));
        });
        std::panic::set_hook(saved);

        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("ERROR koda_cli::panic"), "{text}");
        assert!(text.contains("logged test panic"), "{text}");
        assert!(
            text.contains(concat!("version=\"", env!("CARGO_PKG_VERSION"))),
            "{text}"
        );
        assert!(text.contains("backtrace="), "{text}");
    }
}
