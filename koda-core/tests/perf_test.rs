//! Performance tests — validate that hot paths stay fast.
//!
//! These aren't benchmarks (no criterion), but they assert that
//! operations complete within reasonable time bounds.
//! Run with: cargo test -p koda-core --test perf_test

use std::time::Instant;
use tempfile::TempDir;

// ── Database: load_context with many messages ─────────────────

mod db_perf {
    use super::*;
    use koda_core::db::Database;

    #[tokio::test]
    async fn test_load_context_500_messages_under_1s() {
        let tmp = TempDir::new().unwrap();
        // Database::init needs HOME for config_dir resolution
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let db = Database::init(tmp.path()).await.unwrap();
        let session_id = db.create_session("test", tmp.path()).await.unwrap();

        // Insert 500 messages
        for i in 0..500 {
            let role = if i % 2 == 0 {
                koda_core::db::Role::User
            } else {
                koda_core::db::Role::Assistant
            };
            let content = format!("Message {i}: realistic content with `code` and explanations.");
            db.insert_message(&session_id, &role, Some(&content), None, None, None)
                .await
                .unwrap();
        }

        let start = Instant::now();
        let messages = db.load_context(&session_id, 128_000).await.unwrap();
        let elapsed = start.elapsed();

        assert!(!messages.is_empty());
        assert!(
            elapsed.as_millis() < 1000,
            "load_context with 500 messages took {}ms (should be <1000ms)",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn test_insert_message_under_50ms() {
        let tmp = TempDir::new().unwrap();
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let db = Database::init(tmp.path()).await.unwrap();
        let session_id = db.create_session("test", tmp.path()).await.unwrap();

        let start = Instant::now();
        for _ in 0..10 {
            db.insert_message(
                &session_id,
                &koda_core::db::Role::User,
                Some("A typical user message"),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let elapsed = start.elapsed();
        let per_insert = elapsed.as_micros() / 10;

        assert!(
            per_insert < 50_000,
            "insert_message took {}µs avg (should be <50000µs)",
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
    fn test_grep_100_files_under_2s() {
        let tmp = create_test_project(100, 200);

        let start = Instant::now();
        let pattern = regex::Regex::new("TARGET_PATTERN").unwrap();
        let mut matches = 0;

        for entry in ignore::WalkBuilder::new(tmp.path())
            .hidden(true)
            .git_ignore(true)
            .build()
            .flatten()
        {
            if !entry.path().is_file() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                for line in content.lines() {
                    if pattern.is_match(line) {
                        matches += 1;
                    }
                }
            }
        }

        let elapsed = start.elapsed();
        assert_eq!(matches, 100);
        assert!(
            elapsed.as_millis() < 2000,
            "Grep over 100 files took {}ms (should be <2000ms)",
            elapsed.as_millis()
        );
    }
}

// ── Path resolution: throughput ───────────────────────────────

mod path_perf {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_path_resolution_10k_calls_under_1s() {
        let root = PathBuf::from("/home/user/project");
        let paths = [
            "src/main.rs",
            "src/tools/mod.rs",
            "tests/integration.rs",
            "Cargo.toml",
            "../../../etc/passwd",
            "src/deeply/nested/path/to/file.rs",
            ".",
            "",
        ];

        let start = Instant::now();
        for _ in 0..10_000 {
            for path in &paths {
                let _ = koda_core::tools::safe_resolve_path(&root, path);
            }
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 1000,
            "80K path resolutions took {}ms (should be <1000ms)",
            elapsed.as_millis()
        );
    }
}

// ── Markdown rendering: throughput ────────────────────────────

mod markdown_perf {
    use super::*;

    #[test]
    fn test_markdown_line_splitting_1000_lines_under_1s() {
        let mut md = String::new();
        md.push_str("# Performance Analysis\n\n");
        for i in 0..200 {
            md.push_str(&format!(
                "## Section {i}\n\n\
                 Paragraph with **bold**, *italic*, and `code`.\n\n\
                 ```rust\n\
                 fn example_{i}() {{ println!(\"hello\"); }}\n\
                 ```\n\n"
            ));
        }

        let lines: Vec<&str> = md.lines().collect();
        assert!(lines.len() > 1000);

        let start = Instant::now();
        let mut buffer = String::new();
        let mut count = 0;
        for chunk in md.as_bytes().chunks(50) {
            buffer.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = buffer.find('\n') {
                let _line = &buffer[..pos];
                count += 1;
                buffer = buffer[pos + 1..].to_string();
            }
        }

        let elapsed = start.elapsed();
        assert!(count > 1000);
        assert!(
            elapsed.as_millis() < 1000,
            "Markdown splitting {} lines took {}ms (should be <1000ms)",
            count,
            elapsed.as_millis()
        );
    }
}

// ── SSE buffer parsing: throughput ────────────────────────────

mod sse_perf {
    use super::*;

    #[test]
    fn test_sse_line_parsing_1k_chunks_under_500ms() {
        let sse_line = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        let mut chunks = String::new();
        for _ in 0..1000 {
            chunks.push_str(sse_line);
            chunks.push_str("\n\n");
        }

        let start = Instant::now();
        let mut buffer = String::new();
        let mut count = 0;

        for chunk in chunks.as_bytes().chunks(100) {
            buffer.push_str(&String::from_utf8_lossy(chunk));
            while let Some(pos) = buffer.find('\n') {
                let _line = buffer[..pos].trim().to_string();
                buffer.drain(..=pos);
                count += 1;
            }
        }

        let elapsed = start.elapsed();
        assert!(count >= 1000);
        assert!(
            elapsed.as_millis() < 500,
            "SSE parsing {} lines took {}ms (should be <500ms)",
            count,
            elapsed.as_millis()
        );
    }
}
