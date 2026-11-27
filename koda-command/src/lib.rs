//! How Koda reads a shell command: whether it is destructive, and what it
//! does in words. Koda's approval rules act on the answer; the kernel
//! sandbox decides what a command can touch.
//!
//! - [`bash_safety`]: classifies a command into a [`ToolEffect`], token by
//!   token with `shlex`, heredocs and command substitutions included.
//! - [`scratch_path`]: the temp and cache directories outside the project
//!   that file tools may write.
//! - [`parse_command`]: what a command does, in words a person reads
//!   (Codex's `parse_command`), for clients to show.
//!
//! Split out of `koda-core` in 0.7.1, as Codex's first split was its shell
//! command parsing (`codex-shell-command`); `koda-core` re-exports both
//! modules and [`ToolEffect`] under their old paths. This crate follows
//! koda-cli's version, and its Rust API makes no semver promise of its own:
//! it is published for `cargo install koda-cli`, not as a library to build
//! on.

/// Shell command classification: what a command does.
pub mod bash_safety;
/// What a command does, in words: reads a file, lists, searches.
pub mod parse_command;
/// Temp and cache directories outside the project.
pub mod scratch_path;

/// Effect classification for tool calls.
///
/// Two-axis model: what does the tool touch (local vs. remote)
/// and how severe are its effects (read vs. mutate vs. destroy)?
///
/// # Examples
///
/// ```
/// use koda_command::ToolEffect;
///
/// assert!(!ToolEffect::ReadOnly.is_mutating());
/// assert!(ToolEffect::LocalMutation.is_mutating());
/// assert!(ToolEffect::Destructive.is_mutating());
/// assert!(ToolEffect::RemoteAction.is_mutating());
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "PascalCase")]
pub enum ToolEffect {
    /// No side-effects: file reads, grep, git status.
    ReadOnly,
    /// Side-effects on remote services only: GitHub API, WebFetch POST.
    RemoteAction,
    /// Mutates local filesystem or state: Write, Edit, MemoryWrite.
    LocalMutation,
    /// Irreversible or high-blast-radius: rm -rf, git push --force, DROP TABLE.
    Destructive,
}

impl ToolEffect {
    /// `true` for any effect class **other than** `ReadOnly`.
    ///
    /// Replaces the legacy free function `tools::is_mutating_tool`
    /// (deleted in PR-9 of #1265 item 5). Lifting it onto the enum
    /// puts the predicate next to the variants it inspects, so any
    /// future class addition forces the author to think about which
    /// side of this fence it belongs on.
    #[inline]
    pub fn is_mutating(self) -> bool {
        !matches!(self, ToolEffect::ReadOnly)
    }
}
