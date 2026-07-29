//! Builds a flattened, indented file tree for the right sidebar by walking a directory with
//! `std::fs::read_dir`. Pure and GPUI-independent so it's unit testable without a window.
//!
//! Dotfiles/dot-directories are skipped, matching most file explorers' defaults and, more
//! importantly, keeping this fast: `.git` can contain many thousands of loose objects, and
//! walking it would make browsing unusably slow. The walk is capped at [`MAX_ENTRIES`] total
//! entries as a defensive bound; once hit, remaining entries are simply omitted.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gpui::Rgba;

use crate::theme;

/// Defensive cap on how many entries [`build_file_tree`] will collect, regardless of how
/// large the real tree is.
const MAX_ENTRIES: usize = 5000;

/// Cap on how many *visible* file-tree rows `crate::root::AdeApp::render_file_tree` turns into
/// GPUI elements, independent of [`MAX_ENTRIES`]'s much larger loaded-tree size. Laying out that
/// many `div`s on every render caused a measured foreground-executor stall while a terminal pane
/// streams output. A virtualized list (`uniform_list`, see `vendor/zed/crates/project_panel`)
/// would scale further but is out of scope here. Also bounds [`visible_entries`]'s allocation.
pub const MAX_RENDERED_FILE_ENTRIES: usize = 500;

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

/// A file tree row's real 13×13 language chip - `design_handoff_jerry_ade/README.md`'s Zone 3
/// table (`.rs`→`rs`, `.toml`→`to`, `.md`→`md`, `.sql`→`sq`, matching `theme::lang::*` from
/// Phase A's `theme.rs`), plus a neutral fallback (`theme::lang::UNKNOWN`) for any other
/// extension so every file row still gets *some* chip rather than one silently missing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LangChip {
    pub label: &'static str,
    pub fg: Rgba,
    pub bg: Rgba,
}

/// Picks the real language chip for a file name by its extension (case-insensitive - `Cargo.TOML`
/// gets the same chip as `Cargo.toml`), per [`LangChip`]'s docs.
pub fn lang_chip_for_name(name: &str) -> LangChip {
    let extension = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    let (label, (fg, bg)) = match extension.as_deref() {
        Some("rs") => ("rs", theme::lang::RS),
        Some("toml") => ("to", theme::lang::TOML),
        Some("md") => ("md", theme::lang::MD),
        Some("sql") => ("sq", theme::lang::SQL),
        _ => (".", theme::lang::UNKNOWN),
    };
    LangChip { label, fg, bg }
}

/// Filters a flattened, depth-annotated [`build_file_tree`] listing down to the rows that
/// should be rendered given which directories are collapsed (a collapsed directory's own row
/// still shows, but everything nested underneath it is hidden until re-expanded).
///
/// Relies on `entries` being in `build_file_tree`'s pre-order depth-first shape: once a
/// collapsed directory at depth `d` is seen, every following entry with `depth > d` is skipped
/// until one with `depth <= d` is reached. Nested collapsed directories inside an already-hidden
/// subtree need no special case - they're skipped along with the rest of their parent.
///
/// The full result length still matters to the caller (`crate::root::AdeApp::render_file_tree`'s
/// "... and N more entries not shown" count), so every visible entry is collected rather than
/// stopping early at [`MAX_RENDERED_FILE_ENTRIES`] - only the initial allocation is capped at
/// that bound instead of `entries.len()` (up to [`MAX_ENTRIES`], 5000), since most trees have far
/// fewer than 500 visible entries.
pub fn visible_entries<'a>(
    entries: &'a [FileTreeEntry],
    collapsed: &HashSet<PathBuf>,
) -> Vec<&'a FileTreeEntry> {
    let mut visible = Vec::with_capacity(entries.len().min(MAX_RENDERED_FILE_ENTRIES));
    let mut hidden_below_depth: Option<usize> = None;

    for entry in entries {
        if let Some(hidden_depth) = hidden_below_depth {
            if entry.depth > hidden_depth {
                continue;
            }
            hidden_below_depth = None;
        }

        visible.push(entry);

        if entry.is_dir && collapsed.contains(&entry.path) {
            hidden_below_depth = Some(entry.depth);
        }
    }

    visible
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

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn each_documented_extension_gets_its_own_chip() {
        let rs = lang_chip_for_name("main.rs");
        assert_eq!(rs.label, "rs");
        assert!(same(rs.fg, theme::lang::RS.0));
        assert!(same(rs.bg, theme::lang::RS.1));

        let toml = lang_chip_for_name("Cargo.toml");
        assert_eq!(toml.label, "to");
        assert!(same(toml.fg, theme::lang::TOML.0));

        let md = lang_chip_for_name("README.md");
        assert_eq!(md.label, "md");
        assert!(same(md.fg, theme::lang::MD.0));

        let sql = lang_chip_for_name("schema.sql");
        assert_eq!(sql.label, "sq");
        assert!(same(sql.fg, theme::lang::SQL.0));
    }

    #[test]
    fn an_unrecognized_extension_gets_the_neutral_fallback_chip() {
        let chip = lang_chip_for_name("image.png");
        assert_eq!(chip.label, ".");
        assert!(same(chip.fg, theme::lang::UNKNOWN.0));
        assert!(same(chip.bg, theme::lang::UNKNOWN.1));
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let upper = lang_chip_for_name("Notes.MD");
        assert_eq!(upper.label, "md");
    }

    #[test]
    fn a_name_with_no_extension_gets_the_fallback_chip() {
        let chip = lang_chip_for_name("Makefile");
        assert_eq!(chip.label, ".");
    }

    fn entry(name: &str, depth: usize, is_dir: bool) -> FileTreeEntry {
        FileTreeEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            depth,
            is_dir,
        }
    }

    #[test]
    fn nothing_collapsed_shows_every_entry() {
        let entries = vec![
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("a.txt", 0, false),
        ];
        let visible = visible_entries(&entries, &HashSet::new());
        assert_eq!(visible.len(), 3);
    }

    #[test]
    fn collapsing_a_directory_hides_only_its_own_descendants() {
        let entries = vec![
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
            entry("a.txt", 0, false),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("sub"));

        let visible = visible_entries(&entries, &collapsed);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        // "sub" itself still shows (its own row is what you click to re-expand it); every
        // entry nested underneath it (at a strictly greater depth) is hidden; "a.txt", a
        // sibling back at "sub"'s own depth, is unaffected.
        assert_eq!(names, vec!["sub", "a.txt"]);
    }

    #[test]
    fn collapsing_a_nested_directory_only_hides_its_own_subtree() {
        let entries = vec![
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
            entry("after-deeper.txt", 1, false),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("deeper"));

        let visible = visible_entries(&entries, &collapsed);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["sub", "nested.txt", "deeper", "after-deeper.txt"]
        );
    }

    #[test]
    fn collapsing_a_directory_with_no_children_hides_nothing_extra() {
        let entries = vec![entry("empty-dir", 0, true), entry("a.txt", 0, false)];
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("empty-dir"));

        let visible = visible_entries(&entries, &collapsed);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn visible_entries_on_a_real_tree_matches_manual_filtering() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");
        fs::write(dir.path().join("a.txt"), "a").expect("write");

        let entries = build_file_tree(dir.path()).expect("build_file_tree");
        let mut collapsed = HashSet::new();
        collapsed.insert(dir.path().join("sub"));

        let visible = visible_entries(&entries, &collapsed);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt"]);
    }
}
