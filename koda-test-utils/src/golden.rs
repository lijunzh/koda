//! Replay of a scripted conversation stored as a JSON file.
//!
//! **`replay()`** loads a golden file and returns a [`MockProvider`] that
//! replays its responses, no real model needed.
//!
//! # File format
//!
//! ```json
//! {
//!   "provider": "anthropic",
//!   "model": "claude-sonnet-4-6",
//!   "responses": [
//!     { "Text": "Hello! How can I help?" },
//!     { "ToolCalls": [{ "id": "call_1", "function_name": "Read", "arguments": "{}" }] },
//!     { "Text": "Done!" }
//!   ]
//! }
//! ```

use crate::{MockProvider, MockResponse};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// On-disk format for a golden file.
#[derive(Debug, Serialize, Deserialize)]
pub struct GoldenFile {
    /// Provider name (for documentation only).
    pub provider: String,
    /// Model used during recording (for documentation only).
    pub model: String,
    /// Recorded responses in order.
    pub responses: Vec<MockResponse>,
}

/// Load a golden file and return a `MockProvider` that replays its responses.
pub fn replay(path: impl AsRef<Path>) -> MockProvider {
    let data = std::fs::read_to_string(path.as_ref())
        .unwrap_or_else(|e| panic!("failed to read golden file {:?}: {e}", path.as_ref()));
    let golden: GoldenFile = serde_json::from_str(&data)
        .unwrap_or_else(|e| panic!("failed to parse golden file {:?}: {e}", path.as_ref()));
    MockProvider::new(golden.responses)
}
