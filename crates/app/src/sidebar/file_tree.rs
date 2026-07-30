//! Builds a flattened, indented file tree for the right sidebar by walking a directory with
//! `std::fs::read_dir`. Pure and GPUI-independent so it's unit testable without a window.
//!
//! Dotfiles/dot-directories are skipped, matching most file explorers' defaults and, more
//! importantly, keeping this fast: `.git` can contain many thousands of loose objects, and
//! walking it would make browsing unusably slow.
//!
//! ## No entry is ever hidden without saying so (GitHub issue #18 §4)
//!
//! There is no longer a render-time cap: `crate::sidebar::render::AdeApp::render_file_tree` is a
//! real virtualized `gpui::uniform_list`, so every visible row is rendered and only the rows
//! genuinely on screen become elements. The old `MAX_RENDERED_FILE_ENTRIES` (500) cap and its
//! "... and N more entries not shown" row are gone.
//!
//! One cap survives, and it is a *load* bound rather than a render one: the recursive walk is
//! bounded by [`FileTreeSettings::max_entries`](crate::settings::store::FileTreeSettings), a
//! real, configurable `settings.toml` value (20,000 by default). It exists because the walk is
//! eager and synchronous - `crate::root::AdeApp::rebuild_palette_file_candidates` needs the whole
//! tree for file search, so it cannot be made lazy - and pointing this app at a directory with a
//! million files should not allocate a million `PathBuf`s before the first frame. When it is hit
//! the listing reports [`FileTreeListing::truncated`] and the sidebar shows a real
//! "Stopped at N entries - load more" action
//! (`crate::sidebar::render::AdeApp::load_more_file_tree_entries`), which re-walks with a
//! tenfold larger budget. Still a budget, deliberately - see that method's own docs - but the
//! row always names the count the walk stopped at, so nothing is ever *silently* cut off.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gpui::Rgba;

#[cfg(test)]
use crate::theme;

/// One row in the flattened file tree: a real filesystem entry, its path, and its depth
/// (0 = direct child of the tree root) for indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
}

/// A completed walk: the flattened entries, plus the two ways it can be less than the whole
/// truth. `truncated` is the honest half of the load cap (see the module docs) and is what the
/// sidebar's "load more" action keys off. `partial` covers the quieter case - a subdirectory the
/// walk couldn't read, or one that was deeper than [`MAX_DEPTH`] - which is deliberately *not*
/// surfaced as a user-facing action but must still stop
/// `crate::sidebar::render::AdeApp::prune_stale_fold_state` from treating this listing as a
/// complete inventory of the worktree's directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTreeListing {
    pub entries: Vec<FileTreeEntry>,
    pub truncated: bool,
    pub partial: bool,
}

impl FileTreeListing {
    /// Whether this listing is a complete inventory of the tree - the only condition under which
    /// "a directory isn't in here" may be taken as "that directory no longer exists".
    pub fn is_complete(&self) -> bool {
        !self.truncated && !self.partial
    }
}

/// The ceiling `crate::sidebar::render::AdeApp::load_more_file_tree_entries` escalates towards -
/// see that method for why the escape hatch stays bounded.
pub const MAX_LOAD_MORE_ENTRIES: usize = 1_000_000;

/// How deep the walk will recurse. A defensive bound on stack depth, not a product decision: the
/// walk is recursive, and while `std::fs::DirEntry::file_type` doesn't follow symlinks (so a
/// symlink loop can't be descended into), a genuinely pathological directory tree could still
/// overflow the background thread's stack, which aborts the process rather than failing
/// gracefully. Anything cut off here marks the listing [`FileTreeListing::partial`].
pub const MAX_DEPTH: usize = 64;

/// Recursively lists `root`'s contents (directories first, then alphabetically within each
/// group) as a flattened, depth-annotated list suitable for indented rendering.
///
/// `limit` is the maximum number of entries to collect. `None` is genuinely unbounded and is a
/// test-only convenience: every production caller passes a real cap (see
/// `crate::root::AdeApp::load_file_tree`), including the sidebar's own "load more" action.
pub fn build_file_tree(root: &Path, limit: Option<usize>) -> io::Result<FileTreeListing> {
    let mut listing = FileTreeListing::default();
    let mut budget = limit.unwrap_or(usize::MAX);
    visit(root, 0, &mut budget, &mut listing)?;
    Ok(listing)
}

fn visit(
    dir: &Path,
    depth: usize,
    budget: &mut usize,
    listing: &mut FileTreeListing,
) -> io::Result<()> {
    if depth >= MAX_DEPTH {
        listing.partial = true;
        return Ok(());
    }
    let mut children: Vec<fs::DirEntry> = fs::read_dir(dir)?
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            // An entry that fails mid-iteration (an I/O error, a race with a `rmdir`) is
            // skipped - but silently skipping it would make this listing claim to be a complete
            // inventory when it isn't, which is what `prune_stale_fold_state` would then delete
            // real fold state on the strength of.
            Err(_) => {
                listing.partial = true;
                None
            }
        })
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
            listing.truncated = true;
            return Ok(());
        }
        *budget -= 1;

        let path = child.path();
        // A `file_type()` that fails leaves a real directory recorded as a file, so its whole
        // subtree is never walked - the same "incomplete but doesn't look it" hazard as above.
        let is_dir = match child.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(_) => {
                listing.partial = true;
                false
            }
        };
        let name = child.file_name().to_string_lossy().into_owned();
        listing.entries.push(FileTreeEntry {
            path: path.clone(),
            name,
            depth,
            is_dir,
        });

        if is_dir {
            // A directory that fails to read (permissions, race with deletion, etc.) is
            // skipped rather than aborting the whole tree: the sidebar should show as much
            // real structure as it can. It does make this listing an incomplete inventory
            // though, which `partial` records - without it, an unreadable folder would look
            // exactly like a deleted one to `prune_stale_fold_state`, which would then throw
            // away every fold-state entry beneath it.
            if visit(&path, depth + 1, budget, listing).is_err() {
                listing.partial = true;
            }
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
/// gets the same chip as `Cargo.toml`), per [`LangChip`]'s docs. Reads
/// `crate::language::chip_for_extension` - the one canonical registry Revision R8 consolidated
/// this table (and three others) into, rather than an independently-maintained match here.
pub fn lang_chip_for_name(name: &str) -> LangChip {
    let extension = Path::new(name).extension().and_then(|ext| ext.to_str());
    let (label, fg, bg) = crate::language::chip_for_extension(extension);
    LangChip { label, fg, bg }
}

/// Filters a flattened, depth-annotated [`build_file_tree`] listing down to the rows that should
/// be rendered given which directories are **expanded** (an unexpanded directory's own row still
/// shows, but everything nested underneath it is hidden until it is expanded).
///
/// Keyed on expanded-ness, not collapsed-ness, and that inversion is the whole of GitHub issue
/// #18 §1: absence from this set means *collapsed*, so a worktree nobody has ever expanded
/// anything in - including a freshly created one - opens showing only its root-level entries,
/// with no separate "first open" special case anywhere.
///
/// Relies on `entries` being in [`build_file_tree`]'s pre-order depth-first shape: once an
/// unexpanded directory at depth `d` is seen, every following entry with `depth > d` is skipped
/// until one with `depth <= d` is reached. Expanded directories nested inside an already-hidden
/// subtree need no special case - they're skipped along with the rest of their parent, so a stale
/// deep expansion can never make a row appear underneath a collapsed ancestor.
/// Defined in terms of [`visible_indices`] rather than duplicating the walk, so the two can
/// never disagree about which rows are showing.
pub fn visible_entries<'a>(
    entries: &'a [FileTreeEntry],
    expanded: &HashSet<PathBuf>,
) -> Vec<&'a FileTreeEntry> {
    visible_indices(entries, expanded)
        .into_iter()
        .map(|index| &entries[index])
        .collect()
}

/// [`visible_entries`]'s index-returning twin - the same walk, reporting positions in `entries`
/// rather than borrowing from it. `crate::sidebar::render::AdeApp::render_file_tree` needs the
/// result inside a `'static` closure that cannot hold a borrow of the app, and re-running the
/// walk inside that closure would mean walking the whole loaded tree once per `uniform_list`
/// pass (three per frame) instead of once per frame.
pub fn visible_indices(entries: &[FileTreeEntry], expanded: &HashSet<PathBuf>) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut hidden_below_depth: Option<usize> = None;

    for (index, entry) in entries.iter().enumerate() {
        if let Some(hidden_depth) = hidden_below_depth {
            if entry.depth > hidden_depth {
                continue;
            }
            hidden_below_depth = None;
        }

        visible.push(index);

        if entry.is_dir && !expanded.contains(&entry.path) {
            hidden_below_depth = Some(entry.depth);
        }
    }

    visible
}

/// Every directory in a loaded listing, as the absolute-path set
/// `crate::sidebar::fold_state::FoldState::prune_missing_dirs` prunes stale entries against.
pub fn directory_paths(entries: &[FileTreeEntry]) -> HashSet<PathBuf> {
    entries
        .iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.path.clone())
        .collect()
}

/// A row's left padding before any indentation (`crate::sidebar::render`'s `pl(px(8.0) + indent)`).
pub const ROW_LEFT_PAD: f32 = 8.0;

/// Horizontal indent per nesting level, per `design_handoff_jerry_ade/README.md`'s Zone 3 spec.
pub const INDENT_STEP: f32 = 13.0;

/// Where level `level`'s vertical indent guide sits, in pixels from the row's left edge.
///
/// Deliberately derived from the same two constants the row's own `pl` indent is, plus half of
/// `render_tree_caret`'s real 8px width, so the guide lands exactly under the expand chevron of
/// the ancestor directory it belongs to (issue #18 §3: "aligned with the expand chevrons")
/// instead of at some independently-chosen offset that would drift the first time the indent
/// step changes.
pub fn indent_guide_x(level: usize) -> f32 {
    ROW_LEFT_PAD + INDENT_STEP * level as f32 + CARET_WIDTH / 2.0
}

/// `render_tree_caret`'s real width - see [`indent_guide_x`].
const CARET_WIDTH: f32 = 8.0;

/// How many of a row's indent guides belong to the selected file's own ancestor chain, and so
/// should be drawn in the highlighted colour (issue #18 §3's optional "highlight the active
/// guide"). Always a prefix of the row's levels: two paths share a *prefix* of ancestors, never a
/// gap in the middle, so this is one count rather than a per-level set.
///
/// Returns `0` (nothing highlighted) when nothing is selected, when the selection isn't inside
/// this tree's root, or when the two paths diverge immediately - the common case for most rows.
pub fn active_guide_levels(root: &Path, entry: &Path, selected: Option<&Path>) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let (Ok(entry_relative), Ok(selected_relative)) =
        (entry.strip_prefix(root), selected.strip_prefix(root))
    else {
        return 0;
    };
    let depth = entry_relative.components().count().saturating_sub(1);
    let shared = entry_relative
        .components()
        .zip(selected_relative.components())
        .take_while(|(a, b)| a == b)
        .count();
    shared.min(depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tree_of(root: &Path) -> Vec<FileTreeEntry> {
        build_file_tree(root, None)
            .expect("build_file_tree")
            .entries
    }

    #[test]
    fn lists_files_and_directories_with_depth() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("b.txt"), "b").expect("write");
        fs::write(dir.path().join("a.txt"), "a").expect("write");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");

        let entries = tree_of(dir.path());

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

        let entries = tree_of(dir.path());

        assert_eq!(entries.len(), 1, "only the non-dot entry should be listed");
        assert_eq!(entries[0].name, "visible.txt");
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let dir = TempDir::new().expect("tempdir");
        let listing = build_file_tree(dir.path(), None).expect("build_file_tree");
        assert!(listing.entries.is_empty());
        assert!(!listing.truncated);
    }

    #[test]
    fn nonexistent_root_returns_io_error() {
        let missing = PathBuf::from("/definitely/not/a/real/path/for/ade/file-tree-test");
        let result = build_file_tree(&missing, None);
        assert!(result.is_err());
    }

    /// The load cap must be honest in both directions: a walk that hits it says so, and one that
    /// doesn't must never claim it did (which would put a "Show all entries" action in the
    /// sidebar for a tree that is already complete).
    #[test]
    fn a_walk_that_hits_its_limit_reports_truncation_and_one_that_does_not_says_so() {
        let dir = TempDir::new().expect("tempdir");
        for index in 0..10 {
            fs::write(dir.path().join(format!("f-{index}.txt")), "x").expect("write");
        }

        let capped = build_file_tree(dir.path(), Some(4)).expect("build_file_tree");
        assert_eq!(capped.entries.len(), 4);
        assert!(capped.truncated);

        let complete = build_file_tree(dir.path(), Some(10)).expect("build_file_tree");
        assert_eq!(complete.entries.len(), 10);
        assert!(
            !complete.truncated,
            "a walk that collected everything must not report truncation, even when the entry \
             count exactly equals the budget"
        );

        let uncapped = build_file_tree(dir.path(), None).expect("build_file_tree");
        assert_eq!(uncapped.entries.len(), 10);
        assert!(!uncapped.truncated);
    }

    /// A directory the walk can't read is skipped (so one bad folder never blanks the sidebar),
    /// but the listing must *say* it is incomplete - otherwise
    /// `crate::sidebar::render::AdeApp::prune_stale_fold_state` would read the missing subtree
    /// as deleted and permanently drop its fold state.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_makes_the_listing_partial_but_not_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("open")).expect("mkdir");
        fs::write(dir.path().join("open/file.txt"), "x").expect("write");
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::write(locked.join("hidden-from-us.txt"), "x").expect("write");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");

        if fs::read_dir(&locked).is_ok() {
            // Running as root (or on a filesystem that ignores the mode) - the premise doesn't
            // hold, so this would pass for the wrong reason.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
            return;
        }

        let listing = build_file_tree(dir.path(), None).expect("build_file_tree");
        // Restore before any assertion can fail, or `TempDir`'s own cleanup fails too.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(
            listing.entries.iter().any(|entry| entry.name == "file.txt"),
            "the readable part of the tree must still be listed"
        );
        assert!(listing.partial, "the skipped directory must be reported");
        assert!(
            !listing.is_complete(),
            "and so this listing must never be used as a complete inventory"
        );
        assert!(!listing.truncated, "no entry budget was involved");
    }

    /// The recursion bound - a defensive stack guard, reported the same honest way.
    #[test]
    fn a_tree_deeper_than_the_depth_cap_is_reported_as_partial() {
        let dir = TempDir::new().expect("tempdir");
        let mut deep = dir.path().to_path_buf();
        for level in 0..(MAX_DEPTH + 2) {
            deep = deep.join(format!("d{level}"));
        }
        fs::create_dir_all(&deep).expect("mkdir -p");

        let listing = build_file_tree(dir.path(), None).expect("build_file_tree");

        assert_eq!(listing.entries.len(), MAX_DEPTH);
        assert!(listing.partial);
        assert!(!listing.truncated);
    }

    /// The listing has to hold hundreds of entries in a single directory without any cap of its
    /// own - the sidebar's virtualized list is what keeps that cheap to render, not a cut-off
    /// here (issue #18 §4).
    #[test]
    fn a_large_directory_is_listed_completely() {
        let dir = TempDir::new().expect("tempdir");
        for index in 0..800 {
            fs::write(dir.path().join(format!("f-{index:03}.txt")), "x").expect("write");
        }

        let listing = build_file_tree(dir.path(), Some(20_000)).expect("build_file_tree");

        assert_eq!(listing.entries.len(), 800);
        assert!(!listing.truncated);
        let expanded = HashSet::new();
        assert_eq!(
            visible_entries(&listing.entries, &expanded).len(),
            800,
            "every entry in a flat directory is visible with nothing expanded, and none of them \
             may be dropped by a render cap"
        );
    }

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    #[test]
    fn each_documented_extension_gets_its_own_chip() {
        let rs = lang_chip_for_name("main.rs");
        assert_eq!(rs.label, "rs");
        assert!(same(rs.fg, theme::lang::RS.0.into()));
        assert!(same(rs.bg, theme::lang::RS.1.into()));

        let toml = lang_chip_for_name("Cargo.toml");
        assert_eq!(toml.label, "to");
        assert!(same(toml.fg, theme::lang::TOML.0.into()));

        let md = lang_chip_for_name("README.md");
        assert_eq!(md.label, "md");
        assert!(same(md.fg, theme::lang::MD.0.into()));

        let sql = lang_chip_for_name("schema.sql");
        assert_eq!(sql.label, "sq");
        assert!(same(sql.fg, theme::lang::SQL.0.into()));
    }

    #[test]
    fn an_unrecognized_extension_gets_the_neutral_fallback_chip() {
        let chip = lang_chip_for_name("image.png");
        assert_eq!(chip.label, ".");
        assert!(same(chip.fg, theme::lang::UNKNOWN.0.into()));
        assert!(same(chip.bg, theme::lang::UNKNOWN.1.into()));
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

    /// Issue #18 §1's default state, at the level it's actually decided: nothing expanded means
    /// root-level entries only.
    #[test]
    fn nothing_expanded_shows_only_root_level_entries() {
        let entries = vec![
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("a.txt", 0, false),
        ];
        let visible = visible_entries(&entries, &HashSet::new());
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt"]);
    }

    #[test]
    fn expanding_a_directory_reveals_only_its_own_immediate_subtree() {
        let entries = vec![
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
            entry("a.txt", 0, false),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("sub"));

        let visible = visible_entries(&entries, &expanded);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        // "deeper" shows because its parent is expanded, but its own children stay hidden until
        // it is expanded too.
        assert_eq!(names, vec!["sub", "nested.txt", "deeper", "a.txt"]);
    }

    #[test]
    fn expanding_a_whole_chain_reveals_the_deepest_entry() {
        let entries = vec![
            entry("sub", 0, true),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("sub"));
        expanded.insert(PathBuf::from("deeper"));

        let visible = visible_entries(&entries, &expanded);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "deeper", "deepest.txt"]);
    }

    /// A stale deep expansion (e.g. one restored from disk whose parent the user has since
    /// collapsed) must never punch a hole through a collapsed ancestor.
    #[test]
    fn an_expanded_directory_under_a_collapsed_ancestor_stays_hidden() {
        let entries = vec![
            entry("sub", 0, true),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
            entry("a.txt", 0, false),
        ];
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("deeper"));

        let visible = visible_entries(&entries, &expanded);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt"]);
    }

    #[test]
    fn expanding_a_directory_with_no_children_reveals_nothing_extra() {
        let entries = vec![entry("empty-dir", 0, true), entry("a.txt", 0, false)];
        let mut expanded = HashSet::new();
        expanded.insert(PathBuf::from("empty-dir"));

        let visible = visible_entries(&entries, &expanded);
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn visible_entries_on_a_real_tree_matches_manual_filtering() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");
        fs::write(dir.path().join("a.txt"), "a").expect("write");

        let entries = tree_of(dir.path());
        let mut expanded = HashSet::new();
        expanded.insert(dir.path().join("sub"));

        let visible = visible_entries(&entries, &expanded);
        let names: Vec<&str> = visible.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "nested.txt", "a.txt"]);
    }

    #[test]
    fn directory_paths_collects_every_directory_and_no_files() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::create_dir(dir.path().join("sub/deeper")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");

        let dirs = directory_paths(&tree_of(dir.path()));

        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&dir.path().join("sub")));
        assert!(dirs.contains(&dir.path().join("sub/deeper")));
    }

    #[test]
    fn indent_guides_line_up_with_each_levels_expand_chevron() {
        // The chevron for depth `d` starts at `8 + 13*d` and is 8 wide, so its centre - and the
        // guide - is at `12 + 13*d`.
        assert_eq!(indent_guide_x(0), 12.0);
        assert_eq!(indent_guide_x(1), 25.0);
        assert_eq!(indent_guide_x(2), 38.0);
    }

    #[test]
    fn no_selection_means_no_active_guides() {
        let root = Path::new("/repo");
        assert_eq!(
            active_guide_levels(root, &root.join("src/app/main.rs"), None),
            0
        );
    }

    #[test]
    fn the_selected_files_whole_ancestor_chain_is_active() {
        let root = Path::new("/repo");
        let selected = root.join("src/app/main.rs");
        // The selected row itself: both of its guides (for `src` and `src/app`) are active.
        assert_eq!(
            active_guide_levels(root, &selected, Some(&selected)),
            2,
            "the selected file's own ancestor guides are the active chain"
        );
        // A sibling of the selected file shares the same two ancestors.
        assert_eq!(
            active_guide_levels(root, &root.join("src/app/other.rs"), Some(&selected)),
            2
        );
        // A row in a different subtree shares only `src`.
        assert_eq!(
            active_guide_levels(root, &root.join("src/other/thing.rs"), Some(&selected)),
            1
        );
        // A row that diverges immediately shares nothing.
        assert_eq!(
            active_guide_levels(root, &root.join("docs/readme.md"), Some(&selected)),
            0
        );
    }

    /// A row can never highlight more guides than it draws - `min(shared, depth)` is what stops
    /// a deeper selected path from claiming a guide level this row doesn't have.
    #[test]
    fn active_guides_never_exceed_a_rows_own_depth() {
        let root = Path::new("/repo");
        let selected = root.join("src/app/deep/main.rs");
        assert_eq!(
            active_guide_levels(root, &root.join("src"), Some(&selected)),
            0
        );
        assert_eq!(
            active_guide_levels(root, &root.join("src/app"), Some(&selected)),
            1
        );
    }

    #[test]
    fn a_selection_outside_the_tree_root_highlights_nothing() {
        let root = Path::new("/repo");
        assert_eq!(
            active_guide_levels(
                root,
                &root.join("src/main.rs"),
                Some(Path::new("/elsewhere/src/main.rs"))
            ),
            0
        );
    }
}
