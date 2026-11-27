//! Test utilities for koda — mock providers, test sinks, and E2E harness.
//!
//! This crate is intentionally **not published** and exists only for
//! tests. What tests need lives here rather than behind a feature of the
//! published crates, as Codex removed `codex-core`'s `test-support`
//! feature (#11405): no crate builds twice, nothing test-only is public on
//! crates.io, and the `koda` binary can't select a mock. The mock provider
//! ([`mock`]) and the stub HTTP server ([`network`]) need only
//! `koda-providers`; the default `core` feature adds what builds on
//! koda-core.
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
pub mod mock;
pub mod network;
pub mod openai;
#[cfg(feature = "core")]
mod sink;

// ── Re-exports ─────────────────────────────────────────────────────────────

#[cfg(feature = "core")]
pub use koda_core::config::{KodaConfig, ProviderType};
#[cfg(feature = "core")]
pub use koda_core::engine::EngineEvent;
#[cfg(feature = "core")]
pub use koda_core::providers::{ChatMessage, LlmProvider, ToolDefinition};
#[cfg(feature = "core")]
pub use koda_state::db::Role;
pub use mock::{MockProvider, MockResponse};
#[cfg(feature = "core")]
pub use sink::TestSink;

// ── This crate's own utilities ─────────────────────────────────────────────

#[cfg(feature = "core")]
pub use env::{ENV_MUTEX, Env, EnvBuilder};
