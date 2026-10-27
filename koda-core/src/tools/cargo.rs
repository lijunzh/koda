//! Cargo tool: structured compiler and test feedback for Rust projects.
//!
//! Runs `cargo check`, `clippy`, `test` or `build` from the project root
//! inside the same kernel sandbox as `Bash`, with `--message-format=json`.
//! The model gets one line per compiler diagnostic instead of rustc's
//! rendered text, and for `test` a per-binary summary with the failing
//! test names and each panic's location and message.
//!
//! Nothing here runs automatically after an edit. The model decides when
//! to call it, which is the DESIGN.md P3 line between evidence handed to
//! the model and the engine verifying on its behalf. An automatic variant
//! is a separate experiment branch measured by the eval harness.
//!
//! ## Parameters
//!
//! - **`subcommand`** (required): `check`, `clippy`, `test` or `build`
//! - **`package`** (optional): `-p <package>`
//! - **`test_filter`** (optional, `test` only): the libtest name filter
//!
//! ## Output
//!
//! `check` / `clippy` / `build`:
//!
//! ```text
//! cargo check: FAILED, 1 error, 2 warnings (exit code 101, 0.4s)
//! error[E0382] src/lib.rs:5:5 borrow of moved value: `s` (value borrowed here after move)
//! warning[unused_imports] src/lib.rs:1:5 unused import: `std::collections::HashMap`
//! ```
//!
//! Diagnostics are de-duplicated across targets (a crate's lib and its
//! unit-test target report the same error twice), errors come before
//! warnings, and the list is capped at [`MAX_ERRORS`](crate::tools::cargo::MAX_ERRORS)
//! / [`MAX_WARNINGS`](crate::tools::cargo::MAX_WARNINGS)
//! with an "N more omitted" line.
//!
//! `test` prints compile diagnostics the same way, then the parsed
//! libtest output:
//!
//! ```text
//! cargo test: FAILED, 1 passed, 1 failed, 1 ignored in 1 test binary (exit code 101, 1.2s)
//! probe (unittests src/lib.rs): 1 passed, 1 failed, 1 ignored
//!   FAILED tests::fails at src/lib.rs:21:9: assertion `left == right` failed: numbers differ | left: 1 | right: 2
//! ```
//!
//! The full, uncapped report plus the raw libtest text and stderr is stored
//! as the tool's `full_output` for `RecallContext`.
//!
//! ## Safety
//!
//! Classified [`ToolEffect::LocalMutation`]: build scripts, proc macros and
//! tests execute arbitrary code, the same verdict the Bash classifier gives
//! `cargo build`. The command goes through [`crate::sandbox::build`] exactly
//! like `Bash`, so the kernel sandbox, env scrubbing (which forwards
//! `CARGO_HOME`, `RUSTUP_HOME` and `CARGO_TARGET_DIR` for `cargo`) and
//! proxy routing all apply.

use crate::engine::{EngineEvent, EngineSink};
use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Wall-clock ceiling for one cargo invocation.
///
/// Fixed rather than model-supplied: `Bash` caps its `timeout` argument at
/// 300s so the model can't widen its own deadline, but a cold `cargo test`
/// on a workspace needs longer than that, and here there is no argument
/// for the model to abuse.
pub const TIMEOUT_SECS: u64 = 600;
/// Errors listed in the model-facing summary before "N more omitted".
pub const MAX_ERRORS: usize = 30;
/// Warnings listed in the model-facing summary.
pub const MAX_WARNINGS: usize = 10;
/// Failing tests listed in the model-facing summary.
pub const MAX_FAILURES: usize = 30;
/// Lines of a panic message kept per failing test.
const MAX_PANIC_LINES: usize = 6;
/// Memory ceiling for the collected non-JSON output (libtest text plus
/// stderr). Diagnostics are parsed as they arrive and never stored raw.
const MAX_COLLECT_BYTES: usize = 10 * 1024 * 1024;
/// Stderr tail shown in the summary when cargo fails without producing
/// a single diagnostic or test failure (a manifest error, say).
const STDERR_TAIL_LINES: usize = 20;
/// Cap on the stored full output when the sandbox policy is silent.
/// Same value `Bash` uses.
const DEFAULT_MAX_FULL_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "Cargo".to_string(),
        description: "Run cargo on this Rust project and get the result already parsed: \
            one line per compiler error or warning (code, path:line:col, message), and for \
            `test` each failing test's name with the panic location and message. \
            Use it after editing Rust code and before declaring the work done: `check` for a \
            fast compile, `clippy` for lints, `test` to run the tests (add `test_filter` to \
            run a subset), `build` only when a binary is needed. Prefer this over running \
            cargo through Bash: the output is shorter and structured. Runs from the project \
            root; the summary lists at most 30 errors and 10 warnings."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "subcommand": {
                    "type": "string",
                    "enum": ["check", "clippy", "test", "build"],
                    "description": "Which cargo subcommand to run."
                },
                "package": {
                    "type": "string",
                    "description": "Workspace package to run against (`-p <package>`). \
                        Omit to use cargo's default for the project root."
                },
                "test_filter": {
                    "type": "string",
                    "description": "Only with subcommand=test: run the tests whose name \
                        contains this string (libtest filter, e.g. a module path such as \
                        `parser::tests`)."
                }
            },
            "required": ["subcommand"]
        }),
    }]
}

// ── Arguments ──────────────────────────────────────────────────────────

/// The cargo subcommands the tool accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subcommand {
    /// `cargo check`
    Check,
    /// `cargo clippy`
    Clippy,
    /// `cargo test`
    Test,
    /// `cargo build`
    Build,
}

impl Subcommand {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "check" => Some(Self::Check),
            "clippy" => Some(Self::Clippy),
            "test" => Some(Self::Test),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    /// The word cargo sees.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Build => "build",
        }
    }
}

/// Validated tool arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoArgs {
    /// Which subcommand to run.
    pub subcommand: Subcommand,
    /// `-p <package>`, if given.
    pub package: Option<String>,
    /// libtest name filter, `test` only.
    pub test_filter: Option<String>,
}

impl CargoArgs {
    /// Parse and validate the JSON arguments the model sent.
    ///
    /// `package` and `test_filter` may not start with `-`: the command
    /// runs through `sh -c`, and while both values are shell-quoted, a
    /// leading dash would still reach cargo as a flag
    /// (`--manifest-path`, `--config`), which is exactly the kind of
    /// argument the schema doesn't offer.
    pub fn parse(args: &Value) -> Result<Self> {
        let sub = args
            .get("subcommand")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("Missing 'subcommand' argument (one of: check, clippy, test, build)")
            })?;
        let subcommand = Subcommand::parse(sub).ok_or_else(|| {
            anyhow!("Unknown subcommand {sub:?}: expected one of check, clippy, test, build")
        })?;
        let package = optional_arg(args, "package")?;
        let test_filter = optional_arg(args, "test_filter")?;
        if test_filter.is_some() && subcommand != Subcommand::Test {
            bail!("'test_filter' only applies to subcommand \"test\"");
        }
        Ok(Self {
            subcommand,
            package,
            test_filter,
        })
    }

    /// The shell command line that runs inside the sandbox.
    pub fn command_line(&self) -> String {
        let mut cmd = self.display();
        cmd.push_str(" --message-format=json");
        cmd
    }

    /// The command as a human reads it: no `--message-format`. Used by
    /// approval prompts and the TUI tool header.
    pub fn display(&self) -> String {
        let mut cmd = format!("cargo {}", self.subcommand.as_str());
        if let Some(p) = &self.package {
            cmd.push_str(" -p ");
            cmd.push_str(&shell_quote(p));
        }
        if let Some(f) = &self.test_filter {
            cmd.push(' ');
            cmd.push_str(&shell_quote(f));
        }
        cmd
    }
}

fn optional_arg(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            if s.starts_with('-') {
                bail!("'{key}' must not start with '-' (got {s:?})");
            }
            Ok(Some(s.to_string()))
        }
        Some(other) => bail!("'{key}' must be a string (got {other})"),
    }
}

/// Quote one argument for `sh -c`. Plain identifiers pass through
/// unchanged so the TUI header reads naturally (`cargo test -p koda-core`).
fn shell_quote(s: &str) -> String {
    match shlex::try_quote(s) {
        Ok(q) => q.into_owned(),
        // `try_quote` only fails on a NUL byte, which no package name or
        // test filter contains; fall back to dropping the byte rather
        // than passing anything unquoted.
        Err(_) => shlex::try_quote(&s.replace('\0', ""))
            .map(|q| q.into_owned())
            .unwrap_or_default(),
    }
}

/// Command line for approval prompts and the TUI header. Falls back to
/// `cargo ?` when the args don't parse, so a malformed call still shows
/// as a cargo invocation rather than an empty header.
pub fn describe(args: &Value) -> String {
    CargoArgs::parse(args)
        .map(|a| a.display())
        .unwrap_or_else(|_| "cargo ?".to_string())
}

// ── Compiler diagnostics ───────────────────────────────────────────────

/// Severity of a diagnostic the tool reports. rustc's `note`, `help` and
/// `failure-note` top-level messages are dropped; they carry no location
/// of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// `error`
    Error,
    /// `warning`
    Warning,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "error" => Some(Self::Error),
            "warning" => Some(Self::Warning),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One compiler diagnostic, reduced to what the model needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    /// Error or warning.
    pub level: Level,
    /// `E0382`, `unused_imports`, `clippy::needless_return`, if any.
    pub code: Option<String>,
    /// `path:line:col` of the primary span, if the message has one.
    pub location: Option<String>,
    /// rustc's one-line message.
    pub message: String,
    /// The primary span's label, if any: for a type mismatch, the
    /// expected and found types.
    pub label: Option<String>,
}

impl Diagnostic {
    /// Parse one line of `cargo --message-format=json` stdout. Returns
    /// `None` for anything that isn't an error or warning diagnostic
    /// (`compiler-artifact`, `build-finished`, notes, bookkeeping).
    pub fn from_cargo_line(line: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            return None;
        }
        Self::from_message(v.get("message")?)
    }

    /// Parse the `message` object of a `compiler-message` record.
    pub fn from_message(msg: &Value) -> Option<Self> {
        let level = Level::parse(msg.get("level")?.as_str()?)?;
        let message = msg.get("message")?.as_str()?.to_string();
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(Value::as_str)
            .map(String::from);
        let spans = msg.get("spans").and_then(Value::as_array);
        let primary = spans.and_then(|s| {
            s.iter()
                .find(|sp| sp.get("is_primary").and_then(Value::as_bool) == Some(true))
                .or_else(|| s.first())
        });
        let location = primary.and_then(|sp| {
            Some(format!(
                "{}:{}:{}",
                sp.get("file_name")?.as_str()?,
                sp.get("line_start")?.as_u64()?,
                sp.get("column_start")?.as_u64()?
            ))
        });
        let label = primary
            .and_then(|sp| sp.get("label"))
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
            .map(String::from);
        if location.is_none() && is_bookkeeping(&message) {
            return None;
        }
        Some(Self {
            level,
            code,
            location,
            message,
            label,
        })
    }

    /// Render as one line:
    ///
    /// ```text
    /// error[E0382] src/lib.rs:5:5 borrow of moved value: `s` (value borrowed here after move)
    /// ```
    pub fn render(&self) -> String {
        let mut out = String::from(self.level.as_str());
        if let Some(code) = &self.code {
            out.push('[');
            out.push_str(code);
            out.push(']');
        }
        if let Some(loc) = &self.location {
            out.push(' ');
            out.push_str(loc);
        }
        out.push(' ');
        out.push_str(&self.message);
        if let Some(label) = &self.label
            && label != &self.message
        {
            out.push_str(" (");
            out.push_str(label);
            out.push(')');
        }
        out
    }
}

/// rustc's span-less tallies. Cargo usually keeps these out of the JSON
/// stream, but older toolchains emit them as diagnostics of their own.
fn is_bookkeeping(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with("warning emitted")
        || message.ends_with("warnings emitted")
        || message.starts_with("For more information about")
}

/// Drop duplicates (the same diagnostic from a crate's lib and its test
/// target) and split into errors and warnings, each in first-seen order.
pub fn dedup_and_split(diagnostics: Vec<Diagnostic>) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for d in diagnostics {
        if !seen.insert(d.clone()) {
            continue;
        }
        match d.level {
            Level::Error => errors.push(d),
            Level::Warning => warnings.push(d),
        }
    }
    (errors, warnings)
}

// ── libtest output ─────────────────────────────────────────────────────

/// One failing test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailure {
    /// The test's name as libtest prints it (`tests::parse_empty`).
    pub name: String,
    /// `src/lib.rs:21:9` from the panic line, if the test panicked.
    pub location: Option<String>,
    /// The panic message, multi-line parts joined with ` | `.
    pub message: Option<String>,
}

/// The results of one test binary (one `running N tests` block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBinary {
    /// `probe (unittests src/lib.rs)`, `tests/integration.rs`,
    /// `doc-tests probe`, or `test binary N` when stderr named nothing.
    pub name: String,
    /// Tests the block announced (`running N tests`).
    pub announced: u32,
    /// From the `test result:` line.
    pub passed: u32,
    /// From the `test result:` line.
    pub failed: u32,
    /// From the `test result:` line.
    pub ignored: u32,
    /// From the `test result:` line.
    pub filtered_out: u32,
    /// `false` until the `test result:` line is seen. A binary that
    /// crashed (segfault, abort) never prints one.
    pub finished: bool,
    /// Failing tests, with panic details where the `---- stdout ----`
    /// section had them.
    pub failures: Vec<TestFailure>,
}

impl TestBinary {
    fn new(name: String, announced: u32) -> Self {
        Self {
            name,
            announced,
            passed: 0,
            failed: 0,
            ignored: 0,
            filtered_out: 0,
            finished: false,
            failures: Vec::new(),
        }
    }

    /// Tests that actually ran or were counted (passed + failed + ignored).
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.ignored
    }

    fn counts(&self) -> String {
        let mut s = format!("{} passed", self.passed);
        if self.failed > 0 {
            s.push_str(&format!(", {} failed", self.failed));
        }
        if self.ignored > 0 {
            s.push_str(&format!(", {} ignored", self.ignored));
        }
        if self.filtered_out > 0 {
            s.push_str(&format!(", {} filtered out", self.filtered_out));
        }
        s
    }
}

/// Names of the test binaries, in run order, from cargo's stderr
/// (`Running unittests src/lib.rs (target/debug/deps/probe-1a2b)` and
/// `Doc-tests probe`). Cargo runs binaries sequentially and announces
/// each before its `running N tests` block, so the i-th name belongs to
/// the i-th block on stdout.
pub fn binary_names(stderr: &[String]) -> Vec<String> {
    stderr
        .iter()
        .filter_map(|l| {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix("Running ") {
                let (desc, bin) = match rest.rsplit_once(" (") {
                    Some((d, b)) => (d, b.trim_end_matches(')')),
                    None => (rest, ""),
                };
                let crate_name = bin
                    .rsplit('/')
                    .next()
                    .and_then(|f| f.rsplit_once('-'))
                    .map(|(n, _)| n)
                    .unwrap_or("");
                Some(
                    if desc.starts_with("unittests ") && !crate_name.is_empty() {
                        format!("{crate_name} ({desc})")
                    } else {
                        desc.to_string()
                    },
                )
            } else {
                t.strip_prefix("Doc-tests ")
                    .map(|rest| format!("doc-tests {rest}"))
            }
        })
        .collect()
}

/// Parse libtest's human output (the non-JSON lines of `cargo test`
/// stdout) into per-binary results.
pub fn parse_libtest(lines: &[String], names: &[String]) -> Vec<TestBinary> {
    let mut bins: Vec<TestBinary> = Vec::new();
    let mut cur: Option<TestBinary> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if let Some(announced) = parse_running_header(line) {
            if let Some(b) = cur.take() {
                bins.push(b);
            }
            let idx = bins.len();
            let name = names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("test binary {}", idx + 1));
            cur = Some(TestBinary::new(name, announced));
        } else if let Some(b) = cur.as_mut() {
            if let Some((name, status)) = parse_test_line(line) {
                if status == "FAILED" {
                    b.failures.push(TestFailure {
                        name,
                        location: None,
                        message: None,
                    });
                }
            } else if let Some(name) = line
                .strip_prefix("---- ")
                .and_then(|r| r.strip_suffix(" stdout ----"))
            {
                let (location, message, next) = parse_panic_section(lines, i + 1);
                if let Some(f) = b.failures.iter_mut().find(|f| f.name == name) {
                    f.location = location;
                    f.message = message;
                } else {
                    b.failures.push(TestFailure {
                        name: name.to_string(),
                        location,
                        message,
                    });
                }
                i = next;
                continue;
            } else if let Some(counts) = parse_result_line(line) {
                b.passed = counts.passed;
                b.failed = counts.failed;
                b.ignored = counts.ignored;
                b.filtered_out = counts.filtered_out;
                b.finished = true;
            }
        }
        i += 1;
    }
    if let Some(b) = cur.take() {
        bins.push(b);
    }
    bins
}

/// `running 3 tests` / `running 1 test` → `Some(3)` / `Some(1)`.
fn parse_running_header(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("running ")?;
    let (n, unit) = rest.split_once(' ')?;
    if unit != "tests" && unit != "test" {
        return None;
    }
    n.parse().ok()
}

/// `test tests::fails ... FAILED` → `("tests::fails", "FAILED")`.
/// Handles `test name - should panic ... ok` and `... ignored, reason`.
fn parse_test_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("test ")?;
    let (name, status) = rest.rsplit_once(" ... ")?;
    let name = name.strip_suffix(" - should panic").unwrap_or(name);
    let status = status
        .split([' ', ','])
        .next()
        .unwrap_or(status)
        .to_string();
    Some((name.to_string(), status))
}

#[derive(Debug, Default)]
struct ResultCounts {
    passed: u32,
    failed: u32,
    ignored: u32,
    filtered_out: u32,
}

/// `test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.00s`
fn parse_result_line(line: &str) -> Option<ResultCounts> {
    let rest = line.strip_prefix("test result: ")?;
    let (_status, tail) = rest.split_once(". ")?;
    let mut counts = ResultCounts::default();
    for part in tail.split(';') {
        let part = part.trim();
        let Some((n, what)) = part.split_once(' ') else {
            continue;
        };
        let Ok(n) = n.parse::<u32>() else {
            continue;
        };
        match what {
            "passed" => counts.passed = n,
            "failed" => counts.failed = n,
            "ignored" => counts.ignored = n,
            "filtered out" => counts.filtered_out = n,
            _ => {}
        }
    }
    Some(counts)
}

/// Read a `---- name stdout ----` section starting at `start`. Returns the
/// panic location, the message, and the index of the line that ended the
/// section (the next header, `failures:` or `test result:`), which the
/// caller re-examines.
fn parse_panic_section(lines: &[String], start: usize) -> (Option<String>, Option<String>, usize) {
    let mut location = None;
    let mut message_lines: Vec<&str> = Vec::new();
    let mut j = start;
    let mut in_message = false;
    while j < lines.len() {
        let line = lines[j].as_str();
        if line.starts_with("---- ") || line == "failures:" || line.starts_with("test result: ") {
            break;
        }
        if in_message {
            if line.trim().is_empty() || line.starts_with("note: run with") {
                in_message = false;
            } else if message_lines.len() < MAX_PANIC_LINES {
                message_lines.push(line.trim());
            }
        } else if location.is_none()
            && let Some(idx) = line.find("panicked at ")
        {
            let after = &line[idx + "panicked at ".len()..];
            if let Some(msg_rest) = after.strip_prefix('\'') {
                // Pre-1.73 shape: `panicked at 'msg', src/lib.rs:1:5`
                if let Some((msg, loc)) = msg_rest.rsplit_once("', ") {
                    location = Some(loc.to_string());
                    message_lines.push(msg);
                }
            } else {
                // Current shape: `panicked at src/lib.rs:1:5:` then the
                // message on the following lines.
                location = Some(after.trim_end_matches(':').to_string());
                in_message = true;
            }
        } else if location.is_none()
            && message_lines.is_empty()
            && !line.trim().is_empty()
            && !line.starts_with("note: run with")
        {
            // No panic line yet: a test that returned `Err`, or printed
            // before failing. Keep the first line as the message unless
            // a panic shows up later.
            message_lines.push(line.trim());
        }
        j += 1;
    }
    let message = if message_lines.is_empty() {
        None
    } else {
        Some(message_lines.join(" | "))
    };
    (location, message, j)
}

// ── Report ─────────────────────────────────────────────────────────────

/// Everything one cargo run produced, parsed. Built from the collected
/// streams once the process exits; rendered two ways (`summary` for the
/// model, `full_output` for storage).
#[derive(Debug, Clone)]
pub struct Report {
    /// The validated arguments the run was built from.
    pub args: CargoArgs,
    /// cargo's exit code (`-1` if killed by a signal).
    pub exit_code: i32,
    /// Wall time of the run.
    pub elapsed: Duration,
    /// Distinct errors, first-seen order.
    pub errors: Vec<Diagnostic>,
    /// Distinct warnings, first-seen order.
    pub warnings: Vec<Diagnostic>,
    /// Test binaries, run order. Empty unless the subcommand was `test`.
    pub tests: Vec<TestBinary>,
    /// Non-JSON stdout lines (libtest text).
    pub stdout_text: Vec<String>,
    /// stderr lines (cargo status, plus any sandbox-violation block).
    pub stderr: Vec<String>,
}

impl Report {
    /// Assemble a report from the raw streams.
    pub fn new(
        args: CargoArgs,
        exit_code: i32,
        elapsed: Duration,
        diagnostics: Vec<Diagnostic>,
        stdout_text: Vec<String>,
        stderr: Vec<String>,
    ) -> Self {
        let (errors, warnings) = dedup_and_split(diagnostics);
        let tests = if args.subcommand == Subcommand::Test {
            parse_libtest(&stdout_text, &binary_names(&stderr))
        } else {
            Vec::new()
        };
        Self {
            args,
            exit_code,
            elapsed,
            errors,
            warnings,
            tests,
            stdout_text,
            stderr,
        }
    }

    /// Test binaries that ran at least one test. Doc-test and
    /// integration binaries with nothing to run are noise.
    fn active_tests(&self) -> impl Iterator<Item = &TestBinary> {
        self.tests.iter().filter(|b| b.total() > 0 || !b.finished)
    }

    /// Whether any test failed, counting binaries that never reported.
    fn tests_failed(&self) -> bool {
        self.tests
            .iter()
            .any(|b| b.failed > 0 || !b.failures.is_empty() || !b.finished)
    }

    fn diag_counts(&self) -> String {
        format!(
            "{}, {}",
            count(self.errors.len(), "error", "errors"),
            count(self.warnings.len(), "warning", "warnings")
        )
    }

    fn test_counts(&self) -> String {
        if self.tests.is_empty() {
            return if self.errors.is_empty() {
                "no test binaries ran".to_string()
            } else {
                format!("build failed with {}", self.diag_counts())
            };
        }
        let active: Vec<&TestBinary> = self.active_tests().collect();
        let passed: u32 = active.iter().map(|b| b.passed).sum();
        let failed: u32 = active.iter().map(|b| b.failed).sum();
        let ignored: u32 = active.iter().map(|b| b.ignored).sum();
        let unfinished = active.iter().filter(|b| !b.finished).count();
        let mut s = format!("{passed} passed, {failed} failed");
        if ignored > 0 {
            s.push_str(&format!(", {ignored} ignored"));
        }
        s.push_str(&format!(
            " in {}",
            count(active.len(), "test binary", "test binaries")
        ));
        if unfinished > 0 {
            s.push_str(&format!(
                ", {} without a result line",
                count(unfinished, "binary", "binaries")
            ));
        }
        if active.is_empty()
            && let Some(f) = &self.args.test_filter
        {
            s.push_str(&format!(" (filter {f:?} matched no tests)"));
        }
        if !self.errors.is_empty() {
            s.push_str(&format!("; build had {}", self.diag_counts()));
        }
        s
    }

    /// The one-line summary: status, counts, exit code, elapsed time.
    pub fn headline(&self) -> String {
        let status = if self.exit_code == 0 { "ok" } else { "FAILED" };
        let counts = match self.args.subcommand {
            Subcommand::Test => self.test_counts(),
            _ => self.diag_counts(),
        };
        format!(
            "{}: {status}, {counts} (exit code {}, {:.1}s)",
            self.args.display(),
            self.exit_code,
            self.elapsed.as_secs_f64()
        )
    }

    fn push_diagnostics(&self, out: &mut String, max_errors: usize, max_warnings: usize) {
        push_capped(out, &self.errors, max_errors, "errors");
        push_capped(out, &self.warnings, max_warnings, "warnings");
    }

    fn push_tests(&self, out: &mut String, max_failures: usize) {
        let mut remaining = max_failures;
        let mut omitted = 0usize;
        for b in self.active_tests() {
            out.push('\n');
            out.push_str(&b.name);
            out.push_str(": ");
            if b.finished {
                out.push_str(&b.counts());
            } else {
                out.push_str(&format!(
                    "no result line after announcing {} (the binary may have crashed or timed out)",
                    count(b.announced as usize, "test", "tests")
                ));
            }
            for f in &b.failures {
                if remaining == 0 {
                    omitted += 1;
                    continue;
                }
                remaining -= 1;
                out.push_str("\n  FAILED ");
                out.push_str(&f.name);
                if let Some(loc) = &f.location {
                    out.push_str(" at ");
                    out.push_str(loc);
                }
                if let Some(msg) = &f.message {
                    out.push_str(": ");
                    out.push_str(msg);
                }
            }
        }
        if omitted > 0 {
            out.push_str(&format!(
                "\n... {} omitted",
                count(omitted, "more failing test", "more failing tests")
            ));
        }
    }

    /// Whether the summary should carry a stderr tail: cargo failed but
    /// nothing structured explains why (manifest error, missing
    /// subcommand, sandbox denial).
    fn needs_stderr_tail(&self) -> bool {
        if self.stderr.is_empty() {
            return false;
        }
        if self.stderr.iter().any(|l| l == "<sandbox_violations>") {
            return true;
        }
        self.exit_code != 0 && self.errors.is_empty() && !self.tests_failed()
    }

    /// The model-facing summary: headline, capped diagnostics, test
    /// results, and a stderr tail when nothing else explains a failure.
    pub fn summary(&self) -> String {
        let mut out = self.headline();
        self.push_diagnostics(&mut out, MAX_ERRORS, MAX_WARNINGS);
        if self.args.subcommand == Subcommand::Test {
            self.push_tests(&mut out, MAX_FAILURES);
        }
        if self.needs_stderr_tail() {
            let start = self.stderr.len().saturating_sub(STDERR_TAIL_LINES);
            out.push_str(&format!(
                "\n\n--- stderr (last {} of {} lines) ---\n",
                self.stderr.len() - start,
                self.stderr.len()
            ));
            out.push_str(&self.stderr[start..].join("\n"));
        }
        out
    }

    /// Everything, uncapped, for DB storage and `RecallContext`.
    pub fn full_output(&self, max_bytes: usize) -> String {
        let mut out = self.headline();
        self.push_diagnostics(&mut out, usize::MAX, usize::MAX);
        if self.args.subcommand == Subcommand::Test {
            self.push_tests(&mut out, usize::MAX);
        }
        if !self.stdout_text.is_empty() {
            out.push_str("\n\n--- stdout ---\n");
            out.push_str(&self.stdout_text.join("\n"));
        }
        if !self.stderr.is_empty() {
            out.push_str("\n\n--- stderr ---\n");
            out.push_str(&self.stderr.join("\n"));
        }
        if out.len() > max_bytes {
            out.truncate(max_bytes);
            while !out.is_char_boundary(out.len()) {
                out.pop();
            }
            out.push_str(&format!(
                "\n\n[... output truncated at {max_bytes} bytes ...]"
            ));
        }
        out
    }
}

fn push_capped(out: &mut String, diags: &[Diagnostic], max: usize, what: &str) {
    for d in diags.iter().take(max) {
        out.push('\n');
        out.push_str(&d.render());
    }
    if diags.len() > max {
        out.push_str(&format!("\n... {} more {what} omitted", diags.len() - max));
    }
}

fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

// ── Execution ──────────────────────────────────────────────────────────

/// Result of a cargo run: the model-facing summary plus the stored full
/// report. Same shape as `shell::ShellOutput`.
#[derive(Debug, Clone)]
pub struct CargoOutput {
    /// Compact report for the model's context window.
    pub summary: String,
    /// Uncapped report plus raw streams, for DB storage / `RecallContext`.
    pub full_output: Option<String>,
}

/// What the stream reader accumulates while cargo runs.
#[derive(Default)]
struct Collected {
    /// Parsed as they arrive; the JSON itself is never stored.
    diagnostics: Vec<Diagnostic>,
    /// Non-JSON stdout (libtest text).
    stdout_text: Vec<String>,
    stderr: Vec<String>,
    bytes: usize,
}

impl Collected {
    fn push_stdout(&mut self, line: String, sink: &Option<(&dyn EngineSink, String)>) {
        if line.starts_with('{') {
            if let Some(d) = Diagnostic::from_cargo_line(&line) {
                self.diagnostics.push(d);
            }
            return;
        }
        if let Some((sink, id)) = sink {
            sink.emit(EngineEvent::ToolOutputLine {
                id: id.clone(),
                line: line.clone(),
                is_stderr: false,
            });
        }
        if self.bytes < MAX_COLLECT_BYTES {
            self.bytes += line.len();
            self.stdout_text.push(line);
        }
    }

    fn push_stderr(&mut self, line: String, sink: &Option<(&dyn EngineSink, String)>) {
        if let Some((sink, id)) = sink {
            sink.emit(EngineEvent::ToolOutputLine {
                id: id.clone(),
                line: line.clone(),
                is_stderr: true,
            });
        }
        if self.bytes < MAX_COLLECT_BYTES {
            self.bytes += line.len();
            self.stderr.push(line);
        }
    }
}

/// Read both streams to EOF, parsing JSON diagnostics on the fly and
/// streaming the human-readable lines (cargo's progress on stderr, libtest
/// output on stdout) to the TUI sink when one is attached.
async fn read_streams(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    out: &mut Collected,
    sink: &Option<(&dyn EngineSink, String)>,
) -> std::io::Result<()> {
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;
    while !stdout_done || !stderr_done {
        tokio::select! {
            line = stdout_lines.next_line(), if !stdout_done => match line? {
                Some(l) => out.push_stdout(l, sink),
                None => stdout_done = true,
            },
            line = stderr_lines.next_line(), if !stderr_done => match line? {
                Some(l) => out.push_stderr(l, sink),
                None => stderr_done = true,
            },
        }
    }
    Ok(())
}

/// Run cargo from `project_root` inside the sandbox and parse its output.
///
/// Goes through [`crate::sandbox::build`] like `Bash`, so the kernel
/// sandbox, env scrubbing and proxy env all apply. `CARGO_TERM_COLOR=never`
/// keeps ANSI codes out of the libtest text. The child is `kill_on_drop`,
/// so cancelling the turn (which drops this future) kills cargo too.
pub async fn run_cargo(
    project_root: &Path,
    args: &Value,
    trust: &crate::trust::TrustMode,
    policy: &koda_sandbox::SandboxPolicy,
    proxy_port: Option<u16>,
    socks5_port: Option<u16>,
    sink: Option<(&dyn EngineSink, &str)>,
) -> Result<CargoOutput> {
    let parsed = CargoArgs::parse(args)?;
    let command = parsed.command_line();
    tracing::info!("Running {command}");
    let start = Instant::now();

    let mut cmd = crate::sandbox::build(
        &command,
        project_root,
        trust,
        policy,
        proxy_port,
        socks5_port,
    )?;
    cmd.env("CARGO_TERM_COLOR", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to run cargo: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cargo stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("cargo stderr was not captured"))?;

    let mut collected = Collected::default();
    let sink_info = sink.map(|(s, id)| (s, id.to_string()));
    let result = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_SECS),
        read_streams(stdout, stderr, &mut collected, &sink_info),
    )
    .await;

    match result {
        Ok(Ok(())) => {
            let status = child.wait().await.map_err(|e| anyhow!("wait: {e}"))?;
            let exit_code = status.code().unwrap_or(-1);
            crate::tools::shell::annotate_violations(exit_code, &command, &mut collected.stderr);
            let report = Report::new(
                parsed,
                exit_code,
                start.elapsed(),
                collected.diagnostics,
                collected.stdout_text,
                collected.stderr,
            );
            let max_bytes = policy
                .limits
                .max_output_bytes
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                .unwrap_or(DEFAULT_MAX_FULL_OUTPUT_BYTES);
            Ok(CargoOutput {
                summary: report.summary(),
                full_output: Some(report.full_output(max_bytes)),
            })
        }
        Ok(Err(e)) => Err(anyhow!("Stream read error: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            let msg = format!("{} timed out after {TIMEOUT_SECS}s", parsed.display());
            Ok(CargoOutput {
                summary: msg.clone(),
                full_output: Some(msg),
            })
        }
    }
}

// ── Tool trait ─────────────────────────────────────────────────────────

/// `Cargo`: sandboxed `cargo check` / `clippy` / `test` / `build` with
/// parsed output.
pub struct CargoTool;

#[async_trait]
impl Tool for CargoTool {
    fn name(&self) -> &'static str {
        "Cargo"
    }

    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "Cargo")
            .expect("cargo::definitions() must contain Cargo")
    }

    /// Always `LocalMutation`, whatever the subcommand: `check` runs build
    /// scripts and proc macros, `test` runs the tests. Matches the Bash
    /// classifier's verdict on `cargo build`.
    fn classify(&self, _args: &Value) -> ToolEffect {
        ToolEffect::LocalMutation
    }

    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &Value) -> ToolResult {
        match run_cargo(
            ctx.project_root,
            args,
            ctx.trust,
            ctx.sandbox_policy,
            ctx.proxy_port,
            ctx.socks5_port,
            ctx.sink,
        )
        .await
        {
            Ok(o) => ToolResult {
                output: o.summary,
                success: true,
                full_output: o.full_output,
            },
            Err(e) => ToolResult {
                output: format!("Error: {e:#}"),
                success: false,
                full_output: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are real `cargo --message-format=json` captures from
    /// rustc 1.99.1 (`tests/fixtures/cargo/`), with the capture paths
    /// rewritten to `/home/user/...`.
    fn fixture_lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    fn diagnostics_from(jsonl: &str) -> Vec<Diagnostic> {
        jsonl
            .lines()
            .filter_map(Diagnostic::from_cargo_line)
            .collect()
    }

    fn args(sub: Subcommand) -> CargoArgs {
        CargoArgs {
            subcommand: sub,
            package: None,
            test_filter: None,
        }
    }

    const CHECK_ERRORS: &str = include_str!("../../tests/fixtures/cargo/check_errors.stdout.jsonl");
    const CHECK_ERRORS_STDERR: &str =
        include_str!("../../tests/fixtures/cargo/check_errors.stderr.txt");
    const CLIPPY: &str = include_str!("../../tests/fixtures/cargo/clippy_warnings.stdout.jsonl");
    const WS_BUILD_OK: &str = include_str!("../../tests/fixtures/cargo/ws_build_ok.stdout.jsonl");
    const WS_TEST_COMPILE_ERROR: &str =
        include_str!("../../tests/fixtures/cargo/ws_test_compile_error.stdout.jsonl");
    const WS_TEST_COMPILE_ERROR_STDERR: &str =
        include_str!("../../tests/fixtures/cargo/ws_test_compile_error.stderr.txt");
    const TEST_FAILURES: &str = include_str!("../../tests/fixtures/cargo/test_failures.stdout.txt");
    const TEST_FAILURES_STDERR: &str =
        include_str!("../../tests/fixtures/cargo/test_failures.stderr.txt");
    const WS_TEST_OK: &str = include_str!("../../tests/fixtures/cargo/ws_test_ok.stdout.txt");
    const WS_TEST_OK_STDERR: &str =
        include_str!("../../tests/fixtures/cargo/ws_test_ok.stderr.txt");

    // ── compiler messages ───────────────────────────────────────────

    #[test]
    fn check_fixture_yields_one_error_two_warnings_and_drops_failure_note() {
        let diags = diagnostics_from(CHECK_ERRORS);
        assert_eq!(diags.len(), 3, "{diags:#?}");
        let (errors, warnings) = dedup_and_split(diags);
        assert_eq!(errors.len(), 1);
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            errors[0].render(),
            "error[E0382] src/lib.rs:5:5 borrow of moved value: `s` (value borrowed here after move)"
        );
        assert_eq!(
            warnings[0].render(),
            "warning[unused_imports] src/lib.rs:1:5 unused import: `std::collections::HashMap`"
        );
        assert_eq!(
            warnings[1].render(),
            "warning[unused_variables] src/lib.rs:9:9 unused variable: `x`"
        );
    }

    #[test]
    fn non_message_records_are_ignored() {
        // A clean build: compiler-artifact + build-finished only.
        assert!(diagnostics_from(WS_BUILD_OK).is_empty());
        assert!(Diagnostic::from_cargo_line("not json").is_none());
        assert!(
            Diagnostic::from_cargo_line(r#"{"reason":"build-finished","success":true}"#).is_none()
        );
    }

    #[test]
    fn clippy_lint_codes_are_kept() {
        let diags = diagnostics_from(CLIPPY);
        let codes: Vec<_> = diags.iter().filter_map(|d| d.code.as_deref()).collect();
        assert!(codes.contains(&"clippy::needless_return"), "{codes:?}");
        assert!(codes.contains(&"unused_variables"), "{codes:?}");
        let needless = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("clippy::needless_return"))
            .unwrap();
        assert_eq!(
            needless.render(),
            "warning[clippy::needless_return] src/lib.rs:2:5 unneeded `return` statement"
        );
    }

    #[test]
    fn duplicates_across_lib_and_test_targets_are_merged() {
        // `cargo test` compiles the lib and the lib-test target; the same
        // E0308 arrives twice (plus two failure-notes).
        let diags = diagnostics_from(WS_TEST_COMPILE_ERROR);
        assert_eq!(diags.len(), 2);
        let (errors, warnings) = dedup_and_split(diags);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(warnings.is_empty());
        assert_eq!(
            errors[0].render(),
            "error[E0308] beta/src/lib.rs:1:34 mismatched types (expected `u32`, found `&str`)"
        );
    }

    #[test]
    fn bookkeeping_messages_without_spans_are_dropped() {
        let msg = json!({
            "level": "error",
            "message": "aborting due to 1 previous error; 2 warnings emitted",
            "spans": [],
            "code": null
        });
        assert!(Diagnostic::from_message(&msg).is_none());
        let msg = json!({"level": "warning", "message": "2 warnings emitted", "spans": []});
        assert!(Diagnostic::from_message(&msg).is_none());
        // A span-less error that is NOT bookkeeping (linker failure) stays.
        let msg = json!({"level": "error", "message": "linking with `cc` failed: exit status: 1", "spans": []});
        let d = Diagnostic::from_message(&msg).unwrap();
        assert_eq!(d.render(), "error linking with `cc` failed: exit status: 1");
    }

    #[test]
    fn primary_span_wins_over_secondary_spans() {
        // E0382 lists the "value moved here" span first and the primary
        // span second; the location must come from the primary one.
        let diags = diagnostics_from(CHECK_ERRORS);
        let e = diags.iter().find(|d| d.level == Level::Error).unwrap();
        assert_eq!(e.location.as_deref(), Some("src/lib.rs:5:5"));
    }

    #[test]
    fn summary_puts_errors_first_and_caps_both_levels() {
        let mut diags = Vec::new();
        for i in 0..12 {
            diags.push(Diagnostic {
                level: Level::Warning,
                code: None,
                location: Some(format!("src/w.rs:{i}:1")),
                message: format!("warning {i}"),
                label: None,
            });
        }
        for i in 0..35 {
            diags.push(Diagnostic {
                level: Level::Error,
                code: Some("E0001".into()),
                location: Some(format!("src/e.rs:{i}:1")),
                message: format!("error {i}"),
                label: None,
            });
        }
        let report = Report::new(
            args(Subcommand::Check),
            101,
            Duration::from_millis(1234),
            diags,
            vec![],
            vec![],
        );
        let summary = report.summary();
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(
            lines[0],
            "cargo check: FAILED, 35 errors, 12 warnings (exit code 101, 1.2s)"
        );
        // 30 errors, then the omission line, then 10 warnings, then its omission line.
        assert!(lines[1].starts_with("error[E0001] src/e.rs:0:1"));
        assert!(lines[30].starts_with("error[E0001] src/e.rs:29:1"));
        assert_eq!(lines[31], "... 5 more errors omitted");
        assert!(lines[32].starts_with("warning src/w.rs:0:1"));
        assert_eq!(lines[42], "... 2 more warnings omitted");
        assert_eq!(lines.len(), 43, "{summary}");
        // Full output is uncapped.
        let full = report.full_output(usize::MAX);
        assert!(full.contains("error[E0001] src/e.rs:34:1"));
        assert!(full.contains("warning src/w.rs:11:1"));
        assert!(!full.contains("omitted"));
    }

    #[test]
    fn clean_check_summary_is_one_line() {
        let report = Report::new(
            args(Subcommand::Check),
            0,
            Duration::from_millis(400),
            diagnostics_from(WS_BUILD_OK),
            vec![],
            fixture_lines(
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.03s",
            ),
        );
        assert_eq!(
            report.summary(),
            "cargo check: ok, 0 errors, 0 warnings (exit code 0, 0.4s)"
        );
    }

    #[test]
    fn failure_without_diagnostics_shows_stderr_tail() {
        // e.g. no Cargo.toml in the project root.
        let report = Report::new(
            args(Subcommand::Build),
            101,
            Duration::from_millis(10),
            vec![],
            vec![],
            fixture_lines(
                "error: could not find `Cargo.toml` in `/home/user/notes` or any parent directory",
            ),
        );
        let summary = report.summary();
        assert!(
            summary.contains("--- stderr (last 1 of 1 lines) ---"),
            "{summary}"
        );
        assert!(summary.contains("could not find `Cargo.toml`"), "{summary}");
    }

    #[test]
    fn failure_with_diagnostics_omits_redundant_stderr() {
        let report = Report::new(
            args(Subcommand::Check),
            101,
            Duration::from_millis(10),
            diagnostics_from(CHECK_ERRORS),
            vec![],
            fixture_lines(CHECK_ERRORS_STDERR),
        );
        let summary = report.summary();
        assert!(!summary.contains("--- stderr"), "{summary}");
        assert!(!summary.contains("could not compile"), "{summary}");
        // ...but the full output keeps it.
        assert!(
            report
                .full_output(usize::MAX)
                .contains("could not compile `probe`")
        );
    }

    #[test]
    fn sandbox_violation_block_is_always_surfaced() {
        let report = Report::new(
            args(Subcommand::Check),
            101,
            Duration::from_millis(10),
            diagnostics_from(CHECK_ERRORS),
            vec![],
            fixture_lines("<sandbox_violations>\ndeny file-write* /etc/x\n</sandbox_violations>"),
        );
        assert!(report.summary().contains("<sandbox_violations>"));
    }

    // ── libtest ──────────────────────────────────────────────────────

    #[test]
    fn test_failures_fixture_names_the_failing_test_with_panic_details() {
        let stdout = fixture_lines(TEST_FAILURES)
            .into_iter()
            .filter(|l| !l.starts_with('{'))
            .collect::<Vec<_>>();
        let stderr = fixture_lines(TEST_FAILURES_STDERR);
        let bins = parse_libtest(&stdout, &binary_names(&stderr));
        assert_eq!(bins.len(), 1, "{bins:#?}");
        let b = &bins[0];
        assert_eq!(b.name, "probe (unittests src/lib.rs)");
        assert_eq!((b.announced, b.passed, b.failed, b.ignored), (3, 1, 1, 1));
        assert!(b.finished);
        assert_eq!(b.failures.len(), 1);
        let f = &b.failures[0];
        assert_eq!(f.name, "tests::fails");
        assert_eq!(f.location.as_deref(), Some("src/lib.rs:21:9"));
        assert_eq!(
            f.message.as_deref(),
            Some("assertion `left == right` failed: numbers differ | left: 1 | right: 2")
        );
    }

    #[test]
    fn test_summary_lists_each_active_binary_and_failure() {
        let stdout = fixture_lines(TEST_FAILURES)
            .into_iter()
            .filter(|l| !l.starts_with('{'))
            .collect::<Vec<_>>();
        let diags = diagnostics_from(TEST_FAILURES);
        let report = Report::new(
            args(Subcommand::Test),
            101,
            Duration::from_millis(1200),
            diags,
            stdout,
            fixture_lines(TEST_FAILURES_STDERR),
        );
        let summary = report.summary();
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(
            lines[0],
            "cargo test: FAILED, 1 passed, 1 failed, 1 ignored in 1 test binary (exit code 101, 1.2s)"
        );
        // The fixture crate still carries its two warnings; they come
        // before the test block, as the design orders it.
        assert!(lines[1].starts_with("warning[unused_imports]"), "{summary}");
        assert!(
            lines[2].starts_with("warning[unused_variables]"),
            "{summary}"
        );
        assert_eq!(
            lines[3],
            "probe (unittests src/lib.rs): 1 passed, 1 failed, 1 ignored"
        );
        assert_eq!(
            lines[4],
            "  FAILED tests::fails at src/lib.rs:21:9: assertion `left == right` failed: numbers differ | left: 1 | right: 2"
        );
        assert_eq!(lines.len(), 5, "{summary}");
    }

    #[test]
    fn workspace_test_ok_pairs_stdout_blocks_with_stderr_names_in_order() {
        let stdout = fixture_lines(WS_TEST_OK)
            .into_iter()
            .filter(|l| !l.starts_with('{'))
            .collect::<Vec<_>>();
        let names = binary_names(&fixture_lines(WS_TEST_OK_STDERR));
        assert_eq!(
            names,
            vec![
                "alpha (unittests src/lib.rs)",
                "beta (unittests src/lib.rs)",
                "doc-tests alpha",
                "doc-tests beta",
            ]
        );
        let bins = parse_libtest(&stdout, &names);
        assert_eq!(bins.len(), 4);
        assert_eq!(bins[1].name, "beta (unittests src/lib.rs)");
        assert_eq!(bins[1].passed, 2);

        let report = Report::new(
            args(Subcommand::Test),
            0,
            Duration::from_millis(80),
            vec![],
            stdout,
            fixture_lines(WS_TEST_OK_STDERR),
        );
        // Only the binary that ran tests is listed.
        assert_eq!(
            report.summary(),
            "cargo test: ok, 2 passed, 0 failed in 1 test binary (exit code 0, 0.1s)\n\
             beta (unittests src/lib.rs): 2 passed"
        );
    }

    #[test]
    fn test_with_compile_error_reports_build_failure_and_no_binaries() {
        let report = Report::new(
            args(Subcommand::Test),
            101,
            Duration::from_millis(500),
            diagnostics_from(WS_TEST_COMPILE_ERROR),
            vec![],
            fixture_lines(WS_TEST_COMPILE_ERROR_STDERR),
        );
        let summary = report.summary();
        assert_eq!(
            summary,
            "cargo test: FAILED, build failed with 1 error, 0 warnings (exit code 101, 0.5s)\n\
             error[E0308] beta/src/lib.rs:1:34 mismatched types (expected `u32`, found `&str`)"
        );
    }

    #[test]
    fn filter_matching_nothing_is_called_out() {
        let stdout = fixture_lines(
            "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n",
        );
        let report = Report::new(
            CargoArgs {
                subcommand: Subcommand::Test,
                package: None,
                test_filter: Some("nope".into()),
            },
            0,
            Duration::from_millis(100),
            vec![],
            stdout,
            fixture_lines("     Running unittests src/lib.rs (target/debug/deps/probe-aaaa)"),
        );
        assert_eq!(
            report.summary(),
            "cargo test nope: ok, 0 passed, 0 failed in 0 test binaries (filter \"nope\" matched no tests) (exit code 0, 0.1s)"
        );
    }

    #[test]
    fn crashed_binary_without_result_line_is_reported() {
        let stdout = fixture_lines("\nrunning 2 tests\ntest a ... ok\n");
        let report = Report::new(
            args(Subcommand::Test),
            -1,
            Duration::from_millis(100),
            vec![],
            stdout,
            fixture_lines("     Running unittests src/lib.rs (target/debug/deps/probe-aaaa)"),
        );
        let summary = report.summary();
        assert!(
            summary.contains("1 binary without a result line"),
            "{summary}"
        );
        assert!(
            summary.contains(
                "probe (unittests src/lib.rs): no result line after announcing 2 tests (the binary may have crashed or timed out)"
            ),
            "{summary}"
        );
    }

    #[test]
    fn old_style_panic_line_is_parsed() {
        let stdout = fixture_lines(
            "\nrunning 1 test\ntest t::old ... FAILED\n\nfailures:\n\n---- t::old stdout ----\nthread 't::old' panicked at 'boom here', src/lib.rs:3:5\n\n\nfailures:\n    t::old\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        );
        let bins = parse_libtest(&stdout, &[]);
        assert_eq!(bins[0].name, "test binary 1");
        let f = &bins[0].failures[0];
        assert_eq!(f.location.as_deref(), Some("src/lib.rs:3:5"));
        assert_eq!(f.message.as_deref(), Some("boom here"));
    }

    #[test]
    fn failing_test_without_panic_keeps_its_first_output_line() {
        let stdout = fixture_lines(
            "\nrunning 1 test\ntest t::err ... FAILED\n\nfailures:\n\n---- t::err stdout ----\nError: \"bad input\"\n\n\nfailures:\n    t::err\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        );
        let bins = parse_libtest(&stdout, &[]);
        let f = &bins[0].failures[0];
        assert!(f.location.is_none());
        assert_eq!(f.message.as_deref(), Some("Error: \"bad input\""));
    }

    #[test]
    fn test_line_parsing_handles_should_panic_and_ignored_reasons() {
        assert_eq!(
            parse_test_line("test a::b - should panic ... ok"),
            Some(("a::b".into(), "ok".into()))
        );
        assert_eq!(
            parse_test_line("test slow ... ignored, needs network"),
            Some(("slow".into(), "ignored".into()))
        );
        assert_eq!(
            parse_test_line("test src/lib.rs - a (line 3) ... FAILED"),
            Some(("src/lib.rs - a (line 3)".into(), "FAILED".into()))
        );
        assert!(parse_test_line("test result: ok. 1 passed").is_none());
    }

    #[test]
    fn failing_tests_are_capped_in_summary_only() {
        let mut stdout = vec![String::new(), "running 40 tests".to_string()];
        for i in 0..40 {
            stdout.push(format!("test t{i} ... FAILED"));
        }
        stdout.push("test result: FAILED. 0 passed; 40 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s".into());
        let report = Report::new(
            args(Subcommand::Test),
            101,
            Duration::from_millis(100),
            vec![],
            stdout,
            vec![],
        );
        let summary = report.summary();
        assert_eq!(summary.matches("\n  FAILED ").count(), MAX_FAILURES);
        assert!(
            summary.ends_with("... 10 more failing tests omitted"),
            "{summary}"
        );
        assert_eq!(
            report
                .full_output(usize::MAX)
                .matches("\n  FAILED ")
                .count(),
            40
        );
    }

    // ── arguments ────────────────────────────────────────────────────

    #[test]
    fn args_parse_and_quote() {
        let a = CargoArgs::parse(
            &json!({"subcommand": "test", "package": "koda-core", "test_filter": "parse tests"}),
        )
        .unwrap();
        assert_eq!(a.display(), "cargo test -p koda-core 'parse tests'");
        assert_eq!(
            a.command_line(),
            "cargo test -p koda-core 'parse tests' --message-format=json"
        );
        let a = CargoArgs::parse(&json!({"subcommand": "clippy"})).unwrap();
        assert_eq!(a.command_line(), "cargo clippy --message-format=json");
        // Empty optional strings mean "absent".
        let a = CargoArgs::parse(&json!({"subcommand": "check", "package": "  "})).unwrap();
        assert!(a.package.is_none());
    }

    #[test]
    fn args_reject_bad_input() {
        let err = CargoArgs::parse(&json!({})).unwrap_err().to_string();
        assert!(err.contains("Missing 'subcommand'"), "{err}");
        let err = CargoArgs::parse(&json!({"subcommand": "publish"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unknown subcommand \"publish\""), "{err}");
        // Flag injection through the free-text args.
        let err =
            CargoArgs::parse(&json!({"subcommand": "check", "package": "--manifest-path=/etc/x"}))
                .unwrap_err()
                .to_string();
        assert!(err.contains("must not start with '-'"), "{err}");
        let err = CargoArgs::parse(&json!({"subcommand": "check", "test_filter": "x"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("only applies to subcommand \"test\""), "{err}");
        let err = CargoArgs::parse(&json!({"subcommand": "check", "package": 7}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a string"), "{err}");
    }

    #[test]
    fn shell_metacharacters_are_quoted() {
        let a =
            CargoArgs::parse(&json!({"subcommand": "test", "test_filter": "x; rm -rf /"})).unwrap();
        assert_eq!(a.display(), "cargo test 'x; rm -rf /'");
    }

    #[test]
    fn describe_falls_back_on_bad_args() {
        assert_eq!(
            describe(&json!({"subcommand": "build", "package": "koda-cli"})),
            "cargo build -p koda-cli"
        );
        assert_eq!(describe(&json!({})), "cargo ?");
    }

    // ── Tool trait ───────────────────────────────────────────────────

    #[test]
    fn cargo_tool_metadata_matches_definition() {
        let t = CargoTool;
        assert_eq!(t.name(), "Cargo");
        let def = t.definition();
        assert_eq!(def.name, "Cargo");
        assert_eq!(def.parameters["required"], json!(["subcommand"]));
        assert_eq!(
            def.parameters["properties"]["subcommand"]["enum"],
            json!(["check", "clippy", "test", "build"])
        );
        assert!(def.parameters["properties"]["package"].is_object());
        assert!(def.parameters["properties"]["test_filter"].is_object());
        assert!(
            t.extract_undo_path(&json!({"subcommand": "build"}))
                .is_none()
        );
    }

    /// The guidance on when to run it lives in the description, not the
    /// system prompt; a regression that drops it would leave the model
    /// without a reason to prefer this over `Bash`.
    #[test]
    fn cargo_tool_description_says_when_to_use_it() {
        let desc = CargoTool.definition().description;
        assert!(desc.contains("after editing Rust code"), "{desc}");
        assert!(desc.contains("before declaring the work done"), "{desc}");
        assert!(desc.contains("Bash"), "{desc}");
    }

    #[test]
    fn cargo_tool_is_local_mutation_for_every_subcommand() {
        let t = CargoTool;
        for sub in ["check", "clippy", "test", "build"] {
            assert_eq!(
                t.classify(&json!({"subcommand": sub})),
                ToolEffect::LocalMutation,
                "{sub}"
            );
        }
        assert_eq!(t.classify(&json!({})), ToolEffect::LocalMutation);
        assert_eq!(t.classify(&Value::Null), ToolEffect::LocalMutation);
    }

    // ── end to end ───────────────────────────────────────────────────

    /// Drives a real `cargo test` on a throwaway crate through the
    /// sandbox builder. Proves the whole path: argument validation, the
    /// sandboxed spawn with `CARGO_TERM_COLOR=never`, concurrent stream
    /// reading, stderr-name pairing, and the summary shape.
    #[tokio::test]
    async fn run_cargo_reports_a_failing_test_from_a_real_crate() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skip: cargo not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fine() {}\n    #[test]\n    fn boom() { assert_eq!(1 + 1, 3, \"math\"); }\n}\n",
        )
        .unwrap();

        let out = run_cargo(
            tmp.path(),
            &json!({"subcommand": "test"}),
            &crate::trust::TrustMode::Safe,
            &koda_sandbox::SandboxPolicy::strict_default(),
            None,
            None,
            None,
        )
        .await
        .expect("run_cargo should complete");
        let s = &out.summary;
        assert!(
            s.starts_with(
                "cargo test: FAILED, 1 passed, 1 failed in 1 test binary (exit code 101, "
            ),
            "{s}"
        );
        assert!(
            s.contains("probe (unittests src/lib.rs): 1 passed, 1 failed"),
            "{s}"
        );
        assert!(s.contains("FAILED tests::boom at src/lib.rs:6:17: assertion `left == right` failed: math | left: 2 | right: 3"), "{s}");
        let full = out.full_output.expect("full output stored");
        assert!(full.contains("--- stdout ---"), "{full}");
        assert!(full.contains("test tests::boom ... FAILED"), "{full}");
    }
}
