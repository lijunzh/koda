//! Test utilities for koda — mock providers, test sinks, and E2E harness.
//!
//! This crate is intentionally **not published** and exists only to support
//! `koda-core` integration tests and downstream test binaries.  It depends on
//! `koda-core` with the `test-support` feature enabled, so all cfg-gated test
//! APIs are available.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use koda_test_utils::{Env, MockProvider, MockResponse};
//!
//! #[tokio::test]
//! async fn my_test() {
//!     let env = Env::new().await;
//!     env.insert_user_message("hello").await;
//!     let provider = MockProvider::new(vec![MockResponse::Text("hi".into())]);
//!     let events = env.run_inference(&provider).await;
//!     // assert on events…
//! }
//! ```

mod env;

// ── Re-exports from koda-core (test-support gated) ─────────────────────────

pub use koda_core::config::{KodaConfig, ProviderType};
pub use koda_core::db::Role;
pub use koda_core::engine::EngineEvent;
pub use koda_core::engine::sink::TestSink;
pub use koda_core::providers::mock::{MockProvider, MockResponse};
pub use koda_core::providers::{ChatMessage, LlmProvider, ToolDefinition};

// ── This crate's own utilities ─────────────────────────────────────────────

pub use env::{ENV_MUTEX, Env, EnvBuilder};
pub use insta;
