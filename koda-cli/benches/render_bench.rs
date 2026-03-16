//! Benchmark: measure cost of re-rendering messages through the styling pipeline.
//!
//! This simulates what the DB-backed render cache would do on scroll:
//! take raw message text → run through MarkdownRenderer → produce Vec<Line>.
//!
//! Usage: cargo bench --bench render_bench

use std::time::Instant;

// We can't easily import koda-cli internals in a bench without lib exposure,
// so we replicate the hot path: markdown parsing + inline formatting.
// This gives us an accurate measure of the string processing cost.

fn main() {
    // Simulate realistic message content
    let messages = generate_test_messages();
    let total_lines: usize = messages.iter().map(|m| m.lines().count()).sum();

    println!("=== Render Cache Re-rendering Benchmark ===");
    println!(
        "Messages: {}, Total lines: {}",
        messages.len(),
        total_lines
    );
    println!();

    // Warm up
    for _ in 0..3 {
        render_all(&messages);
    }

    // Benchmark: render all messages (simulates full cache rebuild)
    let iterations = 100;
    let start = Instant::now();
    for _ in 0..iterations {
        let lines = render_all(&messages);
        std::hint::black_box(lines);
    }
    let elapsed = start.elapsed();
    let per_iter = elapsed / iterations;
    let per_line = elapsed / (iterations * total_lines as u32);

    println!("Full rebuild ({total_lines} lines):");
    println!("  Total: {:?} over {iterations} iterations", elapsed);
    println!("  Per rebuild: {:?}", per_iter);
    println!("  Per line: {:?}", per_line);
    println!();

    // Benchmark: render one page (50 lines) — simulates scroll fetch
    let page: Vec<String> = messages
        .iter()
        .flat_map(|m| m.lines().map(String::from))
        .take(50)
        .collect();
    let page_text = page.join("\n");

    let start = Instant::now();
    for _ in 0..1000 {
        let lines = render_single(&page_text);
        std::hint::black_box(lines);
    }
    let elapsed = start.elapsed();
    let per_page = elapsed / 1000;

    println!("Single page (50 lines):");
    println!("  Per render: {:?}", per_page);
    println!();

    // Benchmark: word wrapping cost
    let long_lines = generate_long_lines(200);
    let start = Instant::now();
    for _ in 0..1000 {
        let wrapped = wrap_lines(&long_lines, 120);
        std::hint::black_box(wrapped);
    }
    let elapsed = start.elapsed();
    let per_wrap = elapsed / 1000;

    println!("Word wrapping (200 long lines → 120 cols):");
    println!("  Per wrap: {:?}", per_wrap);
    println!();

    // Summary
    println!("=== Verdict ===");
    if per_iter.as_millis() < 5 {
        println!(
            "✅ Full rebuild under 5ms ({:?}) — DB-backed cache is viable.",
            per_iter
        );
    } else if per_iter.as_millis() < 16 {
        println!(
            "⚠️  Full rebuild {:?} — within frame budget (16ms) but tight.",
            per_iter
        );
        println!("   Consider capping cache size or lazy rendering.");
    } else {
        println!(
            "❌ Full rebuild {:?} — exceeds frame budget.",
            per_iter
        );
        println!("   Need incremental rendering or background thread.");
    }

    if per_page.as_micros() < 500 {
        println!(
            "✅ Page render under 500µs ({:?}) — scroll fetches are instant.",
            per_page
        );
    } else {
        println!(
            "⚠️  Page render {:?} — may cause scroll jank.",
            per_page
        );
    }
}

/// Simulate the markdown rendering pipeline.
///
/// This replicates what MarkdownRenderer.render_line() does:
/// - Check for code fences, headings, lists, blockquotes
/// - Parse inline formatting (bold, italic, code)
/// - Produce styled spans
fn render_all(messages: &[String]) -> Vec<Vec<StyledSpan>> {
    let mut result = Vec::new();
    let mut in_code_block = false;

    for msg in messages {
        for line in msg.lines() {
            let spans = render_line(line, &mut in_code_block);
            result.push(spans);
        }
    }
    result
}

fn render_single(text: &str) -> Vec<Vec<StyledSpan>> {
    let mut result = Vec::new();
    let mut in_code_block = false;
    for line in text.lines() {
        result.push(render_line(line, &mut in_code_block));
    }
    result
}

#[derive(Clone)]
struct StyledSpan {
    text: String,
    #[allow(dead_code)]
    style: u8, // simplified style tag
}

fn render_line(raw: &str, in_code_block: &mut bool) -> Vec<StyledSpan> {
    // Code fence toggle
    if raw.starts_with("```") {
        *in_code_block = !*in_code_block;
        return vec![StyledSpan {
            text: raw.to_string(),
            style: 1,
        }];
    }

    if *in_code_block {
        // Simulate syntax highlighting cost (string scanning)
        return vec![StyledSpan {
            text: format!("  │ {}", raw),
            style: 2,
        }];
    }

    // Heading
    if raw.starts_with('#') {
        return vec![StyledSpan {
            text: raw.to_string(),
            style: 3,
        }];
    }

    // List item
    if raw.starts_with("- ") || raw.starts_with("* ") {
        return vec![StyledSpan {
            text: raw.to_string(),
            style: 4,
        }];
    }

    // Inline formatting: scan for **, *, `
    parse_inline(raw)
}

fn parse_inline(text: &str) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '`' => {
                if !current.is_empty() {
                    spans.push(StyledSpan {
                        text: std::mem::take(&mut current),
                        style: 0,
                    });
                }
                // Find closing backtick
                let start = i + 1;
                i = start;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                let code_text: String = chars[start..i].iter().collect();
                spans.push(StyledSpan {
                    text: code_text,
                    style: 5,
                });
                i += 1; // skip closing `
            }
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                if !current.is_empty() {
                    spans.push(StyledSpan {
                        text: std::mem::take(&mut current),
                        style: 0,
                    });
                }
                i += 2;
                let start = i;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '*') {
                    i += 1;
                }
                let bold_text: String = chars[start..i].iter().collect();
                spans.push(StyledSpan {
                    text: bold_text,
                    style: 6,
                });
                i += 2; // skip closing **
            }
            _ => {
                current.push(chars[i]);
                i += 1;
            }
        }
    }

    if !current.is_empty() {
        spans.push(StyledSpan {
            text: current,
            style: 0,
        });
    }

    if spans.is_empty() {
        spans.push(StyledSpan {
            text: String::new(),
            style: 0,
        });
    }

    spans
}

fn wrap_lines(lines: &[String], max_width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        if line.len() <= max_width {
            wrapped.push(line.clone());
        } else {
            let mut remaining = line.as_str();
            while remaining.len() > max_width {
                // Find last space within width
                let break_at = remaining[..max_width]
                    .rfind(' ')
                    .unwrap_or(max_width);
                wrapped.push(remaining[..break_at].to_string());
                remaining = &remaining[break_at..].trim_start();
            }
            if !remaining.is_empty() {
                wrapped.push(remaining.to_string());
            }
        }
    }
    wrapped
}

// ── Test data generators ───────────────────────────────────

fn generate_test_messages() -> Vec<String> {
    let mut messages = Vec::new();

    // Typical assistant response with code
    messages.push(
        r#"## Implementation

Here's the updated `FileTracker` with persistence:

```rust
use std::collections::HashSet;
use std::path::PathBuf;

pub struct FileTracker {
    owned: HashSet<PathBuf>,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            owned: HashSet::new(),
        }
    }

    pub fn track_created(&mut self, path: PathBuf) {
        self.owned.insert(path);
    }

    pub fn is_owned(&self, path: &PathBuf) -> bool {
        self.owned.contains(path)
    }

    pub fn untrack(&mut self, path: &PathBuf) {
        self.owned.remove(path);
    }
}
```

Key design decisions:
- **Ownership via `Write` only** — editing a user's file doesn't confer ownership
- **DB-backed, not context-backed** — survives compaction and crashes
- **No hash verification (v1)** — low risk for initial implementation

### Tests

I've added 6 unit tests covering:
1. Basic ownership tracking
2. Untrack behavior
3. Persistence across restarts
4. Session isolation
5. Idempotent tracking
6. No-op untrack"#
            .to_string(),
    );

    // Tool output (bash command result)
    messages.push(
        r#"Exit code: 0

--- stdout ---
   Compiling koda-core v0.4.0
   Compiling koda-cli v0.4.0
    Finished dev [unoptimized + debuginfo] target(s) in 12.34s
     Running unittests src/lib.rs

running 10 tests
test file_tracker::tests::test_track_created ... ok
test file_tracker::tests::test_untrack ... ok
test file_tracker::tests::test_persistence ... ok
test file_tracker::tests::test_session_isolation ... ok
test file_tracker::tests::test_idempotent ... ok
test file_tracker::tests::test_noop_untrack ... ok
test approval::tests::test_owned_auto_approve ... ok
test approval::tests::test_unowned_confirms ... ok
test approval::tests::test_no_tracker_fallback ... ok
test approval::tests::test_owned_confirm_mode ... ok

test result: ok. 10 passed; 0 failed; 0 ignored"#
            .to_string(),
    );

    // Another assistant response with markdown
    messages.push(
        r#"The tests are all passing. Let me also update the **approval integration** to handle the edge case where a file is created and deleted within the same tool batch.

> Note: This follows the same pattern as the `check_tool()` backward-compat approach.

- Modified `check_tool_with_tracker()` in `approval.rs`
- Added session wiring through `InferenceContext`
- Updated all 3 execution paths: sequential, parallel, split-batch

The changes are minimal and backward-compatible. Existing callers (sub-agents, `can_parallelize`) are unaffected."#
            .to_string(),
    );

    // Repeat to simulate a longer session (typical 30-50 messages)
    let base = messages.clone();
    for _ in 0..15 {
        messages.extend(base.clone());
    }

    messages
}

fn generate_long_lines(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            format!(
                "This is line {} with some longer content that would need to be wrapped when the terminal is narrower than this text which keeps going and going to simulate real-world prose output from an LLM response that describes implementation details",
                i
            )
        })
        .collect()
}
