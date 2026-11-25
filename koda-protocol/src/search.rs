//! `fuzzyFileSearch`: files and folders whose paths fuzzily match a query,
//! for a client's `@` mentions, so no client walks the file system itself.
//!
//! Codex's method, in its shape (`codex-rs/app-server-protocol`, at
//! upstream commit `06f334aa4b`): the parameters are camelCase and the
//! results keep Codex's snake_case field names. Codex's streaming variant
//! (`fuzzyFileSearch/sessionStart`, `sessionUpdate`, `sessionStop`), which
//! it marks experimental, is left out.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `fuzzyFileSearch`. An empty query or no roots finds nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FuzzyFileSearchParams {
    /// What to match, fuzzily, against each path below a root.
    pub query: String,
    /// The folders to search, as absolute paths. Nothing outside them is
    /// searched.
    pub roots: Vec<String>,
    /// A later request with the same token cancels this one, which then
    /// answers with no files: a client sends one per text box as its user
    /// types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_token: Option<String>,
}

/// The answer to `fuzzyFileSearch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FuzzyFileSearchResponse {
    /// At most 50 matches, best first; equal scores by path.
    pub files: Vec<FuzzyFileSearchResult>,
}

/// One match. The field names are Codex's, snake_case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FuzzyFileSearchResult {
    /// The root it was found under, as the request gave it.
    pub root: String,
    /// Its path, relative to `root`.
    pub path: String,
    /// A file or a folder.
    pub match_type: FuzzyFileSearchMatchType,
    /// The path's last component.
    pub file_name: String,
    /// How well it matches; higher is better.
    pub score: u32,
    /// The positions in `path`, in characters, that match the query, sorted
    /// and without repeats, for highlighting.
    pub indices: Option<Vec<u32>>,
}

/// Whether a match is a file or a folder. A link counts as what it points
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FuzzyFileSearchMatchType {
    /// A file.
    File,
    /// A folder.
    Directory,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_shape_is_codexs() {
        let params: FuzzyFileSearchParams = serde_json::from_value(json!({
            "query": "main",
            "roots": ["/p"],
            "cancellationToken": "composer",
        }))
        .unwrap();
        assert_eq!(params.cancellation_token.as_deref(), Some("composer"));
        let response = FuzzyFileSearchResponse {
            files: vec![FuzzyFileSearchResult {
                root: "/p".into(),
                path: "src/main.rs".into(),
                match_type: FuzzyFileSearchMatchType::File,
                file_name: "main.rs".into(),
                score: 7,
                indices: Some(vec![4, 5]),
            }],
        };
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({"files": [{
                "root": "/p",
                "path": "src/main.rs",
                "match_type": "file",
                "file_name": "main.rs",
                "score": 7,
                "indices": [4, 5],
            }]})
        );
    }
}
