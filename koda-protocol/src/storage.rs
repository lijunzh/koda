//! The session store as a whole: Koda's own housekeeping for compacted
//! messages, which Codex, keeping no compacted rows, has no requests for.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `storage/read`: what compacted messages the store keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageReadParams {}

/// The answer to `storage/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageReadResponse {
    /// Messages a compaction replaced, kept until purged.
    pub compacted_messages: u64,
    /// Sessions with any.
    pub sessions: u64,
    /// Their size, roughly, in bytes.
    pub size_bytes: u64,
    /// When the oldest was compacted, in Unix seconds; absent with none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_compacted_at: Option<i64>,
}

/// `storage/purge`: delete compacted messages for good.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoragePurgeParams {
    /// Only those compacted at least this many days ago; without it, all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_age_days: Option<u32>,
}

/// The answer to `storage/purge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoragePurgeResponse {
    /// How many messages were deleted.
    pub deleted: u64,
}
