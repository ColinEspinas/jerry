//! Builds a flattened, indented file tree for the right sidebar by walking a real
//! directory with `std::fs::read_dir`. Pure and GPUI-independent so it's unit testable
//! without a window.
//!
//! Scope decisions (documented per the step-3 brief):
//! - Dotfiles/dot-directories (names starting with `.`) are skipped, matching most file
//!   explorers' defaults and, more importantly, keeping this fast: the main worktree's
//!   `.git` directory can contain many thousands of loose objects, and walking it would
//!   make this feature slow to the point of being unusable for its actual purpose (letting
//!   someone browse their project's files).
//! - The walk is capped at [`MAX_ENTRIES`] total entries as a defensive bound against
//!   pathological trees (this is a UI sidebar, not a full indexer); once the cap is hit,
//!   remaining entries are simply omitted rather than erroring.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Defensive cap on how many entries [`build_file_tree`] will collect, regardless of how
/// large the real tree is.
const MAX_ENTRIES: usize = 5000;

/// One row in the flattened file tree: a real filesystem entry, its path, and its depth
/// (0 = direct child of the tree root) for indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
}

/// Recursively lists `root`'s contents (directories first, then alphabetically within each
/// group) as a flattened, depth-annotated list suitable for indented rendering.
pub fn build_file_tree(root: &Path) -> io::Result<Vec<FileTreeEntry>> {
    let mut entries = Vec::new();
    let mut budget = MAX_ENTRIES;
    visit(root, 0, &mut entries, &mut budget)?;
    Ok(entries)
}

fn visit(
    dir: &Path,
    depth: usize,
    out: &mut Vec<FileTreeEntry>,
    budget: &mut usize,
) -> io::Result<()> {
    let mut children: Vec<fs::DirEntry> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .collect();

    children.sort_by(|a, b| {
        let a_is_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_is_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // Directories first (`false < true`, so flip the comparison), then alphabetically.
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| a.file_name().cmp(&b.file_name()))
    });

    for child in children {
        if *budget == 0 {
            return Ok(());
        }
        *budget -= 1;

        let path = child.path();
        let is_dir = child.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let name = child.file_name().to_string_lossy().into_owned();
        out.push(FileTreeEntry {
            path: path.clone(),
            name,
            depth,
            is_dir,
        });

        if is_dir {
            // A directory that fails to read (permissions, race with deletion, etc.) is
            // skipped rather than aborting the whole tree: the sidebar should show as much
            // real structure as it can.
            let _ = visit(&path, depth + 1, out, budget);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lists_files_and_directories_with_depth() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("b.txt"), "b").expect("write");
        fs::write(dir.path().join("a.txt"), "a").expect("write");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");

        let entries = build_file_tree(dir.path()).expect("build_file_tree");

        // Directories sort before files at the same depth; "sub" (dir) before "a.txt"/"b.txt".
        assert_eq!(entries[0].name, "sub");
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].depth, 0);

        assert_eq!(entries[1].name, "nested.txt");
        assert_eq!(entries[1].depth, 1);
        assert!(!entries[1].is_dir);

        let names: Vec<&str> = entries[2..].iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn skips_dotfiles_and_dot_directories() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join(".hidden"), "x").expect("write");
        fs::create_dir(dir.path().join(".git")).expect("mkdir");
        fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main").expect("write");
        fs::write(dir.path().join("visible.txt"), "v").expect("write");

        let entries = build_file_tree(dir.path()).expect("build_file_tree");

        assert_eq!(entries.len(), 1, "only the non-dot entry should be listed");
        assert_eq!(entries[0].name, "visible.txt");
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let dir = TempDir::new().expect("tempdir");
        let entries = build_file_tree(dir.path()).expect("build_file_tree");
        assert!(entries.is_empty());
    }

    #[test]
    fn nonexistent_root_returns_io_error() {
        let missing = PathBuf::from("/definitely/not/a/real/path/for/ade/file-tree-test");
        let result = build_file_tree(&missing);
        assert!(result.is_err());
    }
}
