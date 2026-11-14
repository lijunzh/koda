//! `session/memory/read` and `session/memory/add`: Koda's notes, which the
//! model reads every turn after the instruction files. They live under the
//! host's config directory, so a client reads and writes them through the
//! host, not the file system.

use std::path::{Path, PathBuf};

use koda_protocol::memory::{
    MemoryNote, MemoryScope, SessionMemoryAddResponse, SessionMemoryReadResponse,
};
use koda_protocol::rpc::ErrorObject;

use crate::session::{internal, invalid};

/// Where a scope's notes are, for a session in `project_root`.
fn notes_path(project_root: &Path, scope: MemoryScope) -> Result<PathBuf, ErrorObject> {
    match scope {
        MemoryScope::Project => koda_core::memory::project_notes_path(project_root),
        MemoryScope::Global => koda_core::memory::global_notes_path(),
    }
    .map_err(|e| internal(format!("no config directory for Koda's notes: {e:#}")))
}

/// The project's notes, then the global ones.
pub(crate) fn read(project_root: &Path) -> Result<SessionMemoryReadResponse, ErrorObject> {
    let mut notes = Vec::new();
    for scope in [MemoryScope::Project, MemoryScope::Global] {
        let path = notes_path(project_root, scope)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(internal(format!("couldn't read {}: {e}", path.display())));
            }
        };
        notes.push(MemoryNote {
            scope,
            path: path.display().to_string(),
            text,
        });
    }
    Ok(SessionMemoryReadResponse { notes })
}

/// Save `text` to a scope's notes, as `MemoryWrite` does.
pub(crate) fn add(
    project_root: &Path,
    scope: MemoryScope,
    text: &str,
) -> Result<SessionMemoryAddResponse, ErrorObject> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid("nothing to save: the text is blank"));
    }
    let path = notes_path(project_root, scope)?;
    match scope {
        MemoryScope::Project => koda_core::memory::append(project_root, text),
        MemoryScope::Global => koda_core::memory::append_global(text),
    }
    .map_err(|e| internal(format!("couldn't save to {}: {e:#}", path.display())))?;
    Ok(SessionMemoryAddResponse {
        path: path.display().to_string(),
    })
}
