//! Approval flow and legacy re-exports.
//!
//! The approval *mode* logic has moved to [`crate::trust`].
//! This module is kept only because `approval_flow.rs` references it —
//! it will be removed in a follow-up cleanup.

pub use crate::last_provider::LastProvider;
pub use crate::trust::{
    ToolApproval, TrustMode, check_tool, check_tool_with_tracker, resolve_tool_effect,
};
