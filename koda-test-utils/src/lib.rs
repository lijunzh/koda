//! Test utilities for koda — mock providers, test sinks, and E2E harness.
//!
//! This crate is intentionally **not published** and exists only to support
//! `koda-core` integration tests and downstream test binaries. Its default
//! `core` feature depends on `koda-core` with the `test-support` feature
//! enabled, so all cfg-gated test APIs are available; without it, only the
//! stub HTTP server ([`network`]) is left, for `koda-providers`' tests.
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

#[cfg(feature = "core")]
mod env;
#[cfg(feature = "core")]
pub mod golden;
#[cfg(feature = "core")]
pub mod log;
pub mod network;

// ── Re-exports from koda-core (test-support gated) ─────────────────────────

#[cfg(feature = "core")]
pub use koda_core::config::{KodaConfig, ProviderType};
#[cfg(feature = "core")]
pub use koda_core::engine::EngineEvent;
#[cfg(feature = "core")]
pub use koda_core::engine::sink::TestSink;
#[cfg(feature = "core")]
pub use koda_core::providers::mock::{MockProvider, MockResponse};
#[cfg(feature = "core")]
pub use koda_core::providers::{ChatMessage, LlmProvider, ToolDefinition};
#[cfg(feature = "core")]
pub use koda_state::db::Role;

// ── This crate's own utilities ─────────────────────────────────────────────

#[cfg(feature = "core")]
pub use env::{ENV_MUTEX, Env, EnvBuilder};
