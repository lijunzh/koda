//! koda-core's integration tests, one module per former test binary.
//!
//! One binary instead of one per file: each file used to be its own crate,
//! compiled and linked against koda-core separately, which was most of the
//! test build. Codex lays out its core tests the same way. nextest still
//! runs every test in its own process.

mod agent_memory_cache_test;
mod builtin_agent_configs_test;
mod builtin_proxy_e2e_test;
mod cancel_test;
mod compact_reasoning_test;
mod config_capabilities_test;
mod context_management_e2e_test;
mod e2e_agent_test;
mod e2e_background_agent_test;
mod e2e_safety_test;
mod e2e_skills_test;
mod e2e_sub_agent_trust_test;
mod e2e_sub_agent_workspace_test;
mod e2e_test;
mod e2e_tools_test;
mod file_tools_test;
mod golden_test;
mod guarantee_matrix_test;
mod http_client_config_test;
mod inference_context_test;
mod inference_edge_test;
mod inference_recovery_test;
mod inference_retry_test;
mod interactive_process_test;
mod loop_cap_test;
mod mcp_instructions_bootstrap_test;
mod new_tools_test;
mod perf_test;
mod persisting_sink_test;
mod session_allow_test;
mod session_test;
mod snapshot_test;
mod steering_test;
mod token_audit;
mod tool_call_lifecycle_test;
mod tool_normalize_test;
mod tool_wiring_test;
