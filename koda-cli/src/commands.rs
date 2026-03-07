//! Provider factory — thin wrapper around koda_core::providers.

use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;

/// Create an LLM provider instance from the current config.
pub(crate) fn create_provider(config: &KodaConfig) -> Box<dyn LlmProvider> {
    koda_core::providers::create_provider(config)
}
