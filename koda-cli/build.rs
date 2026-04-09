// build.rs — bundle the Koda user manual into a single Markdown string.
//
// Reads docs/src/SUMMARY.md to get the canonical chapter order, then
// concatenates each chapter file into $OUT_DIR/koda_docs.md.
//
// koda-cli owns its documentation — koda-core stays clean.

use std::fs;
use std::path::Path;

fn main() {
    // koda requires Unix (macOS or Linux).
    // The Bash tool uses `sh` which does not exist on Windows.
    // Fail at compile time so users get a clear message instead of a broken binary.
    if std::env::var("CARGO_CFG_UNIX").is_err() {
        panic!(
            "koda requires a Unix-like operating system (macOS or Linux). \
             Windows is not supported. On Windows, use WSL2 instead: \
             https://learn.microsoft.com/windows/wsl"
        );
    }

    let docs_dir = Path::new("../docs/src");
    let summary_path = docs_dir.join("SUMMARY.md");

    // Rerun if anything in the docs changes.
    println!("cargo:rerun-if-changed=../docs/src");

    let summary = fs::read_to_string(&summary_path)
        .unwrap_or_else(|e| panic!("build.rs: cannot read {}: {e}", summary_path.display()));

    let chapters = parse_chapter_paths(&summary);

    let mut out = String::with_capacity(64 * 1024);
    out.push_str("# Koda User Manual\n\n");

    for rel_path in &chapters {
        let full_path = docs_dir.join(rel_path);
        match fs::read_to_string(&full_path) {
            Ok(content) => {
                out.push_str(&content);
                // Ensure chapters are separated by a blank line.
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
            }
            Err(e) => {
                // Warn but don't hard-fail — a missing optional chapter
                // shouldn't break the whole build.
                println!(
                    "cargo:warning=build.rs: skipping {}: {e}",
                    full_path.display()
                );
            }
        }
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let dest = Path::new(&out_dir).join("koda_docs.md");
    fs::write(&dest, &out)
        .unwrap_or_else(|e| panic!("build.rs: cannot write {}: {e}", dest.display()));
}

/// Extract ordered `.md` file paths from an mdBook SUMMARY.md.
///
/// Matches lines of the form:
///   `[Title](./path/to/file.md)`
///   `[Introduction](./introduction.md)` (bare link, not a list item)
fn parse_chapter_paths(summary: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in summary.lines() {
        // Find `(./something.md)` or `(path.md)` anywhere on the line.
        if let Some(start) = line.find("](") {
            let rest = &line[start + 2..];
            if let Some(end) = rest.find(')') {
                let raw = &rest[..end];
                // Strip leading `./` if present.
                let rel = raw.strip_prefix("./").unwrap_or(raw);
                if rel.ends_with(".md") && rel != "SUMMARY.md" {
                    paths.push(rel.to_string());
                }
            }
        }
    }
    paths
}
