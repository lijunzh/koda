//! koda-core's integration tests, one module per former test binary.
//!
//! One binary instead of one per file: each file used to be its own crate,
//! compiled and linked against koda-core separately, which was most of the
//! test build. Codex lays out its core tests the same way. nextest still
//! runs every test in its own process.

mod agent_memory_cache_test;
mod anthropic_http_test;
mod bash_safety_test;
mod builtin_agent_configs_test;
mod builtin_proxy_e2e_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod cancel_test;
mod compact_reasoning_test;
mod context_management_e2e_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod e2e_agent_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod e2e_safety_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod e2e_skills_test;
mod e2e_sub_agent_trust_test;
mod e2e_sub_agent_workspace_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod e2e_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod e2e_tools_test;
mod file_tools_test;
mod gemini_http_test;
mod golden_test;
mod guarantee_matrix_test;
mod http_client_config_test;
mod inference_context_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod inference_edge_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod inference_recovery_test;
mod inference_retry_test;
mod loop_cap_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod mcp_compat_test;
mod mcp_http_transport_test;
mod mcp_instructions_bootstrap_test;
mod new_tools_test;
mod openai_compat_http_test;
mod perf_test;
mod persisting_sink_test;
mod purge_test;
mod session_allow_test;
// Needs the mock providers: --features test-support.
#[cfg(feature = "test-support")]
mod session_test;
mod snapshot_test;
mod steering_test;
mod token_audit;
mod tool_normalize_test;
mod tool_wiring_test;
