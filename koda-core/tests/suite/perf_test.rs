//! Performance tests — validate that hot paths stay fast.
//!
//! These aren't benchmarks (no criterion), but they assert that
//! operations complete within reasonable time bounds. Under nextest they run
//! alone (`.config/nextest.toml`): beside other tests on a small CI runner,
//! their wall-clock bounds failed.
//! Run with: cargo test -p koda-core --test all -- perf_test

use koda_state::session_store::SessionStore;
use std::time::Instant;
use tempfile::TempDir;

// ── Database: load_context with many messages ─────────────────

mod db_perf {
    use super::*;
    use koda_state::db::Database;

    #[tokio::test]
    async fn test_load_context_500_messages_under_1s() {
        let tmp = TempDir::new().unwrap();
        let db = Database::init(tmp.path()).await.unwrap();
        let session_id = db.create_session("test", tmp.path()).await.unwrap();

        // Insert 500 messages
        for i in 0..500 {
            let role = if i % 2 == 0 {
                koda_state::db::Role::User
            } else {
                koda_state::db::Role::Assistant
            };
            let content = format!("Message {i}: realistic content with `code` and explanations.");
            db.insert_message(&session_id, &role, Some(&content), None, None, None)
                .await
                .unwrap();
        }

        let start = Instant::now();
        let messages = db.load_context(&session_id).await.unwrap();
        let elapsed = start.elapsed();

        assert!(!messages.is_empty());
        assert!(
            elapsed.as_millis() < 1000,
            "load_context with 500 messages took {}ms (should be <1000ms)",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn test_insert_message_under_500ms() {
        let tmp = TempDir::new().unwrap();
        let db = Database::init(tmp.path()).await.unwrap();
        let session_id = db.create_session("test", tmp.path()).await.unwrap();

        let start = Instant::now();
        for _ in 0..10 {
            db.insert_message(
                &session_id,
                &koda_state::db::Role::User,
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

        // Each insert waits for the disk to flush, so this times the runner's
        // disk as much as Koda: 52 ms on a GitHub Ubuntu runner with the test
        // running alone, a few ms locally. The bound catches a catastrophic
        // regression (an insert that rewrites the table), as Windows' always
        // did, not a slow disk.
        let threshold = 500_000u128; // 500ms

        assert!(
            per_insert < threshold,
            "insert_message took {}µs avg (should be <{}µs)",
            per_insert,
            threshold
        );
    }
}

// ── Path resolution: throughput ───────────────────────────────

mod path_perf {
    use super::*;

    /// Each resolution follows links to check Koda's denied folder, so it
    /// costs a few file system calls: about 18 µs on a Mac Studio. A real
    /// folder, since a made-up one under `/home` goes through macOS's autofs.
    #[test]
    fn test_path_resolution_8k_calls_under_1s() {
        let project = TempDir::new().unwrap();
        let root = project.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/tools")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
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
        for _ in 0..1_000 {
            for path in &paths {
                let _ = koda_core::tools::safe_resolve_path(&root, path);
            }
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 1000,
            "8K path resolutions took {}ms (should be <1000ms)",
            elapsed.as_millis()
        );
    }
}
