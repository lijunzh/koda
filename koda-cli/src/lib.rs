//! Koda CLI — TUI, headless mode, and ACP server.
//!
//! This crate provides the user-facing interfaces for the Koda AI assistant.
//! The engine lives in [`koda_core`] — this crate handles presentation.
//!
//! ## Entry points
//!
//! - **TUI** (default) — fullscreen ratatui terminal with streaming markdown,
//!   syntax highlighting, slash commands, and mouse selection
//! - **Headless** (`koda -p "..."`) — run a single prompt, print output, exit
//! - **ACP server** (`koda server --stdio`) — JSON-RPC over stdin/stdout for
//!   editor integrations (Zed, VS Code)
//!
//! ## Slash commands
//!
//! | Command | Description |
//! |---------|-------------|
//! | `/help` | Show available commands |
//! | `/model <name>` | Switch model (aliases supported) |
//! | `/provider` | Pick provider interactively |
//! | `/compact` | Summarize old context to free tokens |
//! | `/diff` | Show uncommitted changes |
//! | `/undo` | Revert last file mutation |
//! | `/sessions` | List / resume sessions |
//! | `/memory` | View/edit CLAUDE.md |
//! | `/skills` | List/activate skills |
//! | `/agent` | List sub-agents |
//! | `/key` | Manage API keys |
//! | `/expand` | Replay last tool output |
//! | `/verbose` | Toggle debug output |
//! | `/exit` | Quit |

pub mod acp_adapter;
pub mod repl;
