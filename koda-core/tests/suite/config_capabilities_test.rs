//! The config takes what a provider's Models API reports for its model:
//! koda-core's side of the capability query, against a stub Anthropic
//! server. The providers' own HTTP tests live in `koda-providers`.

use koda_core::config::{KodaConfig, ProviderType};
use koda_core::providers::anthropic::AnthropicProvider;
use koda_test_utils::network::FakeLlmServer;
use serde_json::json;

/// The config takes the context window and output cap the Models API
/// reports for its model, over the pinned table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_takes_the_window_and_output_cap_from_the_api() {
    let server = FakeLlmServer::spawn().await;
    // 777K differs from every table value, so the source is unambiguous.
    server
        .mount_ok(
            "GET",
            r".*/v1/models/claude-sonnet-5-5$",
            json!({"type": "model", "id": "claude-sonnet-5-5",
                   "max_input_tokens": 777_000, "max_tokens": 128_000}),
        )
        .await;
    server.mount_ok("GET", r".*/v1/models$", json!({})).await;
    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));

    let mut config = KodaConfig::default_for_testing(ProviderType::Anthropic);
    config.model = "claude-sonnet-5-5".to_string();
    config.model_settings.model = config.model.clone();
    config.model_settings.max_tokens = None;
    config.recalculate_model_derived();
    assert_eq!(config.max_context_tokens, 1_000_000, "table value first");

    config.query_and_apply_capabilities(&provider).await;
    assert_eq!(config.max_context_tokens, 777_000);
    assert_eq!(config.model_settings.max_context_tokens, 777_000);
    assert_eq!(config.model_settings.max_tokens, Some(128_000));
}
