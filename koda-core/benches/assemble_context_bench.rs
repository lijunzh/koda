//! Benchmark: per-iteration cost of context assembly across realistic
//! session sizes — and the win from #1166 audit item A's per-session cache.
//!
//! Two sections:
//!   1. COLD path (cache invalidated each iter) — pre-#1166 behavior baseline.
//!   2. HOT loop (load → 1-row insert → load) — what the inference loop
//!      actually does. Cache hit + delta-fetch.
//!
//! Usage: `cargo bench --bench assemble_context_bench -p koda-core`
//!
//! Phases (cold path):
//!   1. `Database::load_context` — SQL select + row deserialization +
//!      sanitization passes
//!   2. `analyze_context`        — pure CPU pass over messages
//!   3. `assemble_messages`      — pure CPU: history → ChatMessage
//!   4. `estimate_tokens`        — pure CPU char counting

use std::time::{Duration, Instant};

use koda_core::context_analysis::analyze_context;
use koda_core::db::Database;
use koda_core::inference_helpers::{assemble_messages, estimate_tokens};
use koda_core::persistence::{Persistence, Role};
use koda_core::providers::ChatMessage;

const SIZES: &[usize] = &[10, 100, 500, 1000, 2000];
const ITERATIONS_PER_SIZE: usize = 30;
const WARMUP: usize = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("=== assemble_context phase breakdown (cold path, cache invalidated each iter) ===\n");
    println!(
        "{:>6}  {:>14}  {:>14}  {:>14}  {:>14}  {:>14}",
        "N", "load_context", "analyze", "assemble", "estimate", "TOTAL"
    );
    println!("{}", "-".repeat(90));

    for &n in SIZES {
        let (db, _tmp, session) = setup_db_with_messages(n).await;

        for _ in 0..WARMUP {
            db.clear_context_cache_for(&session);
            let _ = db.load_context(&session).await.unwrap();
        }

        // Phase 1: load_context — invalidate before each call to force
        // a full SQL fetch (matches pre-#1166 behavior).
        let mut samples = Vec::with_capacity(ITERATIONS_PER_SIZE);
        for _ in 0..ITERATIONS_PER_SIZE {
            db.clear_context_cache_for(&session);
            let start = Instant::now();
            let result = db.load_context(&session).await.unwrap();
            samples.push(start.elapsed());
            std::hint::black_box(result);
        }
        samples.sort();
        let load_p50 = samples[ITERATIONS_PER_SIZE / 2];

        let history = db.load_context(&session).await.unwrap();
        let system_msg = ChatMessage::text("system", "you are a helpful assistant");

        let analyze_p50 = bench_sync(ITERATIONS_PER_SIZE, || {
            std::hint::black_box(analyze_context(&history));
        });
        let assemble_p50 = bench_sync(ITERATIONS_PER_SIZE, || {
            std::hint::black_box(assemble_messages(&system_msg, &history));
        });
        let messages = assemble_messages(&system_msg, &history);
        let estimate_p50 = bench_sync(ITERATIONS_PER_SIZE, || {
            std::hint::black_box(estimate_tokens(&messages));
        });

        let total = load_p50 + analyze_p50 + assemble_p50 + estimate_p50;
        println!(
            "{:>6}  {:>14}  {:>14}  {:>14}  {:>14}  {:>14}",
            n,
            fmt(load_p50),
            fmt(analyze_p50),
            fmt(assemble_p50),
            fmt(estimate_p50),
            fmt(total),
        );
    }
    println!();

    // ─────────────────────────────────────────────────────────────────
    // Hot-loop simulation (#1166): mimics the inference loop's
    // load → insert tool result → load pattern. Cache hit + 1-row delta.
    // ─────────────────────────────────────────────────────────────────
    println!("=== inference-loop hot path: load → 1-row insert → load (#1166 win) ===\n");
    println!(
        "{:>6}  {:>16}  {:>16}  {:>10}",
        "N", "cold load_p50", "hot load_p50", "speedup"
    );
    println!("{}", "-".repeat(60));

    for &n in SIZES {
        let (db, _tmp, session) = setup_db_with_messages(n).await;

        // Cold baseline: invalidate before each call.
        let cold_p50 = {
            let mut s = Vec::with_capacity(ITERATIONS_PER_SIZE);
            for _ in 0..ITERATIONS_PER_SIZE {
                db.clear_context_cache_for(&session);
                let start = Instant::now();
                let r = db.load_context(&session).await.unwrap();
                s.push(start.elapsed());
                std::hint::black_box(r);
            }
            s.sort();
            s[ITERATIONS_PER_SIZE / 2]
        };

        // Hot loop: cache stays warm; each iter appends 1 row then loads.
        let _ = db.load_context(&session).await.unwrap(); // prime
        let mut s = Vec::with_capacity(ITERATIONS_PER_SIZE);
        for i in 0..ITERATIONS_PER_SIZE {
            db.insert_message(
                &session,
                &Role::User,
                Some(&format!("hot-loop probe {i}")),
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let start = Instant::now();
            let r = db.load_context(&session).await.unwrap();
            s.push(start.elapsed());
            std::hint::black_box(r);
        }
        s.sort();
        let hot_p50 = s[ITERATIONS_PER_SIZE / 2];

        let speedup = cold_p50.as_secs_f64() / hot_p50.as_secs_f64().max(1e-9);
        println!(
            "{:>6}  {:>16}  {:>16}  {:>9.1}x",
            n,
            fmt(cold_p50),
            fmt(hot_p50),
            speedup,
        );
    }
    println!();
    println!("Note: medians of {ITERATIONS_PER_SIZE} runs after {WARMUP}-run warmup.");
}

fn bench_sync<F: FnMut()>(n: usize, mut f: F) -> Duration {
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        f();
        samples.push(start.elapsed());
    }
    samples.sort();
    samples[n / 2]
}

fn fmt(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 {
        format!("{us}\u{00b5}s")
    } else {
        format!("{:.2}ms", d.as_secs_f64() * 1000.0)
    }
}

async fn setup_db_with_messages(n: usize) -> (Database, tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::init(tmp.path()).await.unwrap();
    let session = db.create_session("default", tmp.path()).await.unwrap();

    let user_text = "Please look at this file and tell me what it does. ".repeat(20);
    let assistant_text =
        "Looking at the file now. The function appears to handle the case where ".repeat(15);
    let tool_result_text = "fn foo() { println!(\"hello world\"); }\n".repeat(30);

    for i in 0..n {
        let phase = i % 4;
        match phase {
            0 => {
                db.insert_message(&session, &Role::User, Some(&user_text), None, None, None)
                    .await
                    .unwrap();
            }
            1 => {
                let call_id = format!("tc_{i}");
                let tc = format!(r#"[{{"id":"{call_id}","name":"Read","arguments":"{{}}"}}]"#);
                let mid = db
                    .insert_message(
                        &session,
                        &Role::Assistant,
                        Some(&assistant_text),
                        Some(&tc),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                db.mark_message_complete(mid).await.unwrap();
            }
            _ => {
                let asst_idx = (i / 4) * 4 + 1;
                db.insert_message(
                    &session,
                    &Role::Tool,
                    Some(&tool_result_text),
                    None,
                    Some(&format!("tc_{asst_idx}")),
                    None,
                )
                .await
                .unwrap();
            }
        }
    }

    (db, tmp, session)
}
