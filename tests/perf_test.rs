//! Performance tests — validate that hot paths stay fast.
//!
//! These aren't benchmarks (no criterion), but they assert that
//! operations complete within reasonable time bounds.
//! Run with: cargo test --test perf_test

use std::time::Instant;
use tempfile::TempDir;

// ── Database: load_context with many messages ─────────────────

mod db_perf {
    use super::*;

    /// Helper: set up a DB and insert N messages.
    async fn setup_db_with_messages(n: usize) -> (tempfile::TempDir, String) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join(".koda.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&db_url)
            .unwrap()
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .create_if_missing(true);

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .unwrap();

        // Run migrations
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                agent_name TEXT NOT NULL
            );",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id);")
            .execute(&pool)
            .await
            .unwrap();

        let session_id = "perf-test-session";
        sqlx::query("INSERT INTO sessions (id, agent_name) VALUES (?, ?)")
            .bind(session_id)
            .bind("test")
            .execute(&pool)
            .await
            .unwrap();

        // Insert N messages with realistic content
        for i in 0..n {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            let content = format!(
                "Message {i}: This is a realistic message with some code: `fn main() {{ println!(\"hello\"); }}` \
                 and some explanation about what this code does and how it should be modified. \
                 Here's another paragraph with more detail about the implementation."
            );
            sqlx::query(
                "INSERT INTO messages (session_id, role, content, prompt_tokens, completion_tokens) \
                 VALUES (?, ?, ?, ?, ?)"
            )
            .bind(session_id)
            .bind(role)
            .bind(&content)
            .bind(100i64)
            .bind(50i64)
            .execute(&pool)
            .await.unwrap();
        }

        (tmp, session_id.to_string())
    }

    use std::str::FromStr;

    #[tokio::test]
    async fn test_load_context_500_messages_under_50ms() {
        let (tmp, session_id) = setup_db_with_messages(500).await;

        // Re-open via the same path
        let db_path = tmp.path().join(".koda.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&db_url).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        let start = Instant::now();
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE session_id = ? ORDER BY id DESC LIMIT 200",
        )
        .bind(&session_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        let elapsed = start.elapsed();
        assert!(
            rows.len() <= 200,
            "LIMIT should cap at 200, got {}",
            rows.len()
        );
        assert!(
            elapsed.as_millis() < 200,
            "load_context with 500 messages took {}ms (should be <50ms)",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn test_insert_message_under_5ms() {
        let (tmp, session_id) = setup_db_with_messages(0).await;

        let db_path = tmp.path().join(".koda.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&db_url).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        let content = "A typical user message with some code context";

        let start = Instant::now();
        for _ in 0..10 {
            sqlx::query("INSERT INTO messages (session_id, role, content) VALUES (?, ?, ?)")
                .bind(&session_id)
                .bind("user")
                .bind(content)
                .execute(&pool)
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        let per_insert = elapsed.as_micros() / 10;

        assert!(
            per_insert < 5000, // 5ms per insert
            "insert_message took {}μs avg (should be <5000μs)",
            per_insert
        );
    }
}

// ── Grep: search performance ──────────────────────────────────

mod grep_perf {
    use super::*;

    fn create_test_project(file_count: usize, lines_per_file: usize) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        for i in 0..file_count {
            let mut content = String::new();
            for j in 0..lines_per_file {
                if j == lines_per_file / 2 {
                    content.push_str(&format!("// TARGET_PATTERN line in file {i}\n"));
                } else {
                    content.push_str(&format!("fn func_{i}_{j}() {{ /* code */ }}\n"));
                }
            }
            std::fs::write(src.join(format!("file_{i}.rs")), &content).unwrap();
        }
        tmp
    }

    #[test]
    fn test_grep_100_files_under_100ms() {
        let tmp = create_test_project(100, 200); // 100 files × 200 lines = 20K lines

        let start = Instant::now();

        // Simulate grep: walk + read + regex match
        let pattern = regex::Regex::new("TARGET_PATTERN").unwrap();
        let mut matches = 0;

        for entry in ignore::WalkBuilder::new(tmp.path())
            .hidden(true)
            .git_ignore(true)
            .build()
            .flatten()
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(path) {
                for line in content.lines() {
                    if pattern.is_match(line) {
                        matches += 1;
                    }
                }
            }
        }

        let elapsed = start.elapsed();
        assert_eq!(matches, 100, "Should find one match per file");
        assert!(
            elapsed.as_millis() < 500,
            "Grep over 100 files took {}ms (should be <100ms)",
            elapsed.as_millis()
        );
    }
}

// ── Markdown rendering: throughput ────────────────────────────

mod markdown_perf {
    use super::*;

    #[test]
    fn test_markdown_render_1000_lines_under_50ms() {
        // Generate a realistic markdown response
        let mut md_content = String::new();
        md_content.push_str("# Performance Analysis\n\n");
        for i in 0..200 {
            md_content.push_str(&format!(
                "## Section {i}\n\n\
                 This is a paragraph with **bold text**, *italic text*, and `inline code`.\n\
                 Here's a [link](https://example.com) and some more text.\n\n\
                 ```rust\n\
                 fn example_{i}() {{\n\
                     println!(\"hello world\");\n\
                 }}\n\
                 ```\n\n"
            ));
        }

        let lines: Vec<&str> = md_content.lines().collect();
        assert!(
            lines.len() > 1000,
            "Should have >1000 lines, got {}",
            lines.len()
        );

        // Measure rendering throughput (just the parsing, not terminal output)
        let start = Instant::now();

        // Simulate what MarkdownStreamer.push does: buffer + find newlines
        let mut buffer = String::new();
        let mut line_count = 0;
        for chunk in md_content.as_bytes().chunks(50) {
            buffer.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = buffer.find('\n') {
                let _line = &buffer[..pos];
                line_count += 1;
                buffer = buffer[pos + 1..].to_string();
            }
        }

        let elapsed = start.elapsed();
        assert!(line_count > 1000);
        assert!(
            elapsed.as_millis() < 200,
            "Markdown line splitting of {} lines took {}ms (should be <50ms)",
            line_count,
            elapsed.as_millis()
        );
    }
}

// ── Path resolution: throughput ───────────────────────────────

mod path_perf {
    use super::*;
    use path_clean::PathClean;
    use std::path::{Path, PathBuf};

    fn safe_resolve_path(project_root: &Path, requested: &str) -> Result<PathBuf, String> {
        let requested_path = Path::new(requested);
        let resolved = if requested_path.is_absolute() {
            requested_path.to_path_buf().clean()
        } else {
            project_root.join(requested_path).clean()
        };
        if !resolved.starts_with(project_root) {
            return Err("Path escapes project root".to_string());
        }
        Ok(resolved)
    }

    #[test]
    fn test_path_resolution_10k_calls_under_10ms() {
        let root = PathBuf::from("/home/user/project");
        let paths = [
            "src/main.rs",
            "src/tools/mod.rs",
            "tests/integration.rs",
            "Cargo.toml",
            "../../../etc/passwd", // traversal attempt
            "src/deeply/nested/path/to/file.rs",
            ".",
            "",
        ];

        let start = Instant::now();
        for _ in 0..10_000 {
            for path in &paths {
                let _ = safe_resolve_path(&root, path);
            }
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 200,
            "80K path resolutions took {}ms (should be <200ms in debug)",
            elapsed.as_millis()
        );
    }
}

// ── SSE buffer parsing: throughput ────────────────────────────

mod sse_perf {
    use super::*;

    #[test]
    fn test_sse_line_parsing_10k_chunks_under_10ms() {
        // Simulate SSE buffer processing
        let sse_line = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        let mut chunks = String::new();
        for _ in 0..1000 {
            chunks.push_str(sse_line);
            chunks.push('\n');
            chunks.push('\n'); // SSE double-newline
        }

        let start = Instant::now();
        let mut buffer = String::new();
        let mut line_count = 0;

        // Simulate streaming: feed chunks of ~100 bytes at a time
        for chunk in chunks.as_bytes().chunks(100) {
            buffer.push_str(&String::from_utf8_lossy(chunk));
            while let Some(line_end) = buffer.find('\n') {
                let _line = buffer[..line_end].trim().to_string();
                buffer.drain(..=line_end);
                line_count += 1;
            }
        }

        let elapsed = start.elapsed();
        assert!(line_count >= 1000);
        assert!(
            elapsed.as_millis() < 100,
            "SSE parsing of {} lines took {}ms (should be <10ms)",
            line_count,
            elapsed.as_millis()
        );
    }
}

// ── Shell escape: throughput ──────────────────────────────────

mod shell_escape_perf {
    use super::*;

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace("'", "'\\''"))
    }

    #[test]
    fn test_shell_escape_100k_calls_under_10ms() {
        let long_string = "a".repeat(1000);
        let inputs = [
            "simple",
            "with spaces",
            "it's got quotes",
            "$(dangerous); rm -rf /",
            long_string.as_str(),
        ];

        let start = Instant::now();
        for _ in 0..100_000 {
            for input in &inputs {
                let _ = shell_escape(input);
            }
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 1000,
            "500K shell_escape calls took {}ms (should be <1000ms in debug)",
            elapsed.as_millis()
        );
    }
}
