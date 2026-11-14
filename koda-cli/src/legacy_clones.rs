//! Tidy `~/.koda/clones`, which Koda stopped using in v0.6.0.
//!
//! Before #1496, a writing sub-agent on macOS worked in a `clonefile(2)`
//! copy of the project under `~/.koda/clones/<project-hash>/<slot>/`, and
//! releasing it deleted the copy. Nothing reads that folder now, so what is
//! left there (mostly empty project folders, sometimes a copy a crash never
//! released) is litter. At TUI start, Koda removes the empty folders, and
//! `~/.koda` itself once nothing is left in it. A folder that still holds a
//! copy is never deleted: Koda names it in a startup warning and leaves the
//! deletion to the user, as such a copy could hold edits nobody saved.
//!
//! Codex retires its old shared TUI log file the same way, best effort at
//! startup (`remove_legacy_tui_log_file`). Remove this module once
//! releases before v0.6.0 are long gone.

use std::fs;
use std::path::{Path, PathBuf};

/// Copies still under the legacy clones folder after tidying.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LeftoverClones {
    /// The folder, `~/.koda/clones`.
    pub(crate) path: PathBuf,
    /// How many project folders in it still hold something.
    pub(crate) projects: usize,
}

/// Tidy `<home>/.koda/clones`: remove its empty project folders, then the
/// folder and `<home>/.koda` once they are empty. Best effort: an error
/// leaves things as they are. Returns what is left, if anything.
pub(crate) fn tidy(home: &Path) -> Option<LeftoverClones> {
    let koda_dir = home.join(".koda");
    let clones = koda_dir.join("clones");
    let entries = fs::read_dir(&clones).ok()?;
    let mut projects = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // `remove_dir` refuses a folder that isn't empty, so a copy with
        // content is never touched.
        if !(path.is_dir() && fs::remove_dir(&path).is_ok()) {
            projects += 1;
        }
    }
    if projects > 0 {
        return Some(LeftoverClones {
            path: clones,
            projects,
        });
    }
    let _ = fs::remove_dir(&clones);
    // Only when nothing else lives in `~/.koda`.
    let _ = fs::remove_dir(&koda_dir);
    None
}

/// The startup warning for copies left behind.
pub(crate) fn warning(left: &LeftoverClones) -> String {
    let copies = if left.projects == 1 {
        "1 project folder".to_string()
    } else {
        format!("{} project folders", left.projects)
    };
    format!(
        "{} holds {copies} of old sub-agent copies, unused since Koda 0.6.0. \
         Check them for anything you want, then delete the folder.",
        left.path.display()
    )
}

/// Tidy the user's legacy clones folder; the warning to show, if any.
pub(crate) fn tidy_home() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    tidy(Path::new(&home)).map(|left| warning(&left))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_do_without_the_folder() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(tidy(home.path()), None);
        assert!(!home.path().join(".koda").exists());
    }

    #[test]
    fn empty_project_folders_go_and_so_does_koda_dir() {
        let home = tempfile::tempdir().unwrap();
        let clones = home.path().join(".koda/clones");
        fs::create_dir_all(clones.join("aaaa")).unwrap();
        fs::create_dir_all(clones.join("bbbb")).unwrap();
        assert_eq!(tidy(home.path()), None);
        assert!(!home.path().join(".koda").exists());
    }

    #[test]
    fn a_copy_with_content_is_kept_and_reported() {
        let home = tempfile::tempdir().unwrap();
        let clones = home.path().join(".koda/clones");
        fs::create_dir_all(clones.join("empty")).unwrap();
        let copy = clones.join("full/slot");
        fs::create_dir_all(&copy).unwrap();
        fs::write(copy.join("Cargo.toml"), "[package]").unwrap();

        let left = tidy(home.path()).unwrap();
        assert_eq!(left.projects, 1);
        assert_eq!(left.path, clones);
        assert!(!clones.join("empty").exists());
        assert!(copy.join("Cargo.toml").exists());
        assert!(warning(&left).contains("1 project folder of old sub-agent copies"));
    }

    #[test]
    fn other_files_in_koda_dir_are_left_alone() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".koda/clones/aaaa")).unwrap();
        fs::write(home.path().join(".koda/notes.txt"), "mine").unwrap();
        assert_eq!(tidy(home.path()), None);
        assert!(!home.path().join(".koda/clones").exists());
        assert!(home.path().join(".koda/notes.txt").exists());
    }
}
