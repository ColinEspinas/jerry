//! Builds a flattened, indented file tree for the right sidebar by walking a directory with
//! `std::fs::read_dir`. Pure and GPUI-independent so it's unit testable without a window.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use gpui::Rgba;

/// One row in the flattened file tree: a real filesystem entry, its path, and its depth
/// (0 = direct child of the tree root) for indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
}

/// The loaded tree: [`build_file_tree`]'s pre-order entries, plus the derived per-entry subtree
/// spans [`Self::visible_indices`] uses to skip a collapsed directory's descendants in one step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTree {
    entries: Vec<FileTreeEntry>,
    /// `subtree_spans[i]` is how many entries immediately following `i` are inside entry `i`'s
    /// subtree - `0` for every file, and for a directory whose contents the walk never reached.
    subtree_spans: Vec<usize>,
}

impl FileTree {
    /// Wraps a pre-order, depth-annotated entry list (the shape [`build_file_tree`] produces),
    /// deriving its subtree spans in one O(n) pass. Called on the background executor as the last
    /// step of the walk, never on the foreground thread.
    pub fn new(entries: Vec<FileTreeEntry>) -> Self {
        let subtree_spans = subtree_spans(&entries);
        Self {
            entries,
            subtree_spans,
        }
    }

    /// The rows that should be rendered given which directories are **expanded**, as indices into
    /// this tree. An unexpanded directory's own row still shows; everything nested underneath it
    /// is skipped.
    pub fn visible_indices(&self, expanded: &HashSet<PathBuf>) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut index = 0;
        while let Some(entry) = self.entries.get(index) {
            visible.push(index);
            index += if entry.is_dir && !expanded.contains(&entry.path) {
                // Bounds-checked rather than trusted: a span that was somehow short would render
                // extra rows, but a `+ 0` step would hang the loop.
                1 + self.subtree_spans.get(index).copied().unwrap_or(0)
            } else {
                1
            };
        }
        visible
    }

    /// [`Self::visible_indices`]' borrowing twin - the same rows, as entries.
    pub fn visible_entries(&self, expanded: &HashSet<PathBuf>) -> Vec<&FileTreeEntry> {
        self.visible_indices(expanded)
            .into_iter()
            .map(|index| &self.entries[index])
            .collect()
    }
}

impl Deref for FileTree {
    type Target = [FileTreeEntry];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

/// Derives every entry's subtree span from the list's own pre-order `depth` shape: an entry's
/// subtree is the contiguous run of following entries that are deeper than it, which ends at the
/// first entry whose depth is less than or equal to its own.
fn subtree_spans(entries: &[FileTreeEntry]) -> Vec<usize> {
    let mut spans = vec![0usize; entries.len()];
    // The current entry's open ancestors, outermost first. An entry at depth `d` has exactly `d`
    // of them, so anything deeper on this stack has just been closed by reaching this entry.
    let mut open: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        while open.len() > entry.depth {
            if let Some(ancestor) = open.pop() {
                spans[ancestor] = index - ancestor - 1;
            }
        }
        open.push(index);
    }
    while let Some(ancestor) = open.pop() {
        spans[ancestor] = entries.len() - ancestor - 1;
    }
    spans
}

/// A completed walk: the loaded tree, plus the one way it can still be less than the whole truth.
/// `partial` covers the quiet cases - a subdirectory the walk couldn't read, or one that was
/// deeper than [`MAX_DEPTH`] - which are deliberately *not* surfaced as a user-facing action but
/// must still stop `crate::sidebar::render::AdeApp::prune_stale_fold_state` from treating this
/// listing as a complete inventory of the worktree's directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTreeListing {
    pub tree: FileTree,
    pub partial: bool,
}

impl FileTreeListing {
    /// Whether this listing is a complete inventory of the tree - the only condition under which
    /// "a directory isn't in here" may be taken as "that directory no longer exists".
    pub fn is_complete(&self) -> bool {
        !self.partial
    }
}

/// How deep the walk will recurse. A defensive bound on stack depth, not a product decision: the
/// walk is recursive, and while `std::fs::DirEntry::file_type` doesn't follow symlinks (so a
/// symlink loop can't be descended into), a genuinely pathological directory tree could still
/// overflow the background thread's stack, which aborts the process rather than failing
/// gracefully. Anything cut off here marks the listing [`FileTreeListing::partial`].
pub const MAX_DEPTH: usize = 64;

/// The listing the Files tab and palette actually load: git's own answer to "what does this
/// worktree contain" (`wt_core::worktree_files`, decisions.md §6) - tracked plus
/// untracked-unignored files - falling back to the real [`build_file_tree`] walk for a root
/// that is not a git worktree.
///
/// Sourcing from git rather than walking is what keeps this from descending into gitignored
/// build output: on a checkout with a populated `target/` (or the vendored zed tree) the walk
/// was hundreds of thousands of `read_dir` results per reload, re-run at least every 5s
/// (GitHub issue #472). The visible delta is deliberate and matches decisions.md §6's
/// definition of content: gitignored files and empty directories no longer appear.
pub fn build_worktree_file_tree(root: &Path) -> io::Result<FileTreeListing> {
    build_worktree_file_tree_inner(root, SUBMODULE_RECURSION_BUDGET)
}

/// How many levels of nested submodules are grafted before the listing is marked partial
/// instead - a defensive bound against a submodule cycle, not a product decision.
const SUBMODULE_RECURSION_BUDGET: usize = 8;

fn build_worktree_file_tree_inner(
    root: &Path,
    submodule_budget: usize,
) -> io::Result<FileTreeListing> {
    let list = match wt_core::worktree_files::list_worktree_files(root) {
        Ok(list) => list,
        // Only a genuinely non-git root falls back to the walk. Inside a real repository a
        // transient git failure must surface as the error it is - silently swapping to the
        // gitignore-blind walk for one cycle would flash a tree with target/ in it and pay
        // the full-walk cost issue #472 removes.
        Err(err) if root.join(".git").exists() => {
            return Err(io::Error::other(err.to_string()));
        }
        Err(_) => return build_file_tree(root),
    };
    // A submodule's gitlink record is indistinguishable from a file in the plain listing, and
    // its contents are a separate repository the listing never descends into - so gitlinks are
    // typed as directories here and their own listings grafted underneath, preserving what the
    // old filesystem walk showed. Failing to enumerate them (an older git, a corrupt module)
    // only loses the typing, which the grafting loop then records as a partial listing.
    let submodules = wt_core::worktree_files::list_submodule_paths(root).unwrap_or_default();
    let (mut entries, mut partial) = entries_from_git_listing(root, &list, &submodules);

    for submodule in &submodules {
        let sub_root: PathBuf = {
            let mut path = root.to_path_buf();
            path.extend(submodule.split('/'));
            path
        };
        let Some(index) = entries
            .iter()
            .position(|entry| entry.is_dir && entry.path == sub_root)
        else {
            // Dot-filtered or depth-cut - already accounted for at insertion.
            continue;
        };
        if submodule_budget == 0 {
            partial = true;
            continue;
        }
        match build_worktree_file_tree_inner(&sub_root, submodule_budget - 1) {
            Ok(child) => {
                partial |= child.partial;
                let base_depth = entries[index].depth + 1;
                let grafted: Vec<FileTreeEntry> = child
                    .tree
                    .iter()
                    .cloned()
                    .map(|mut entry| {
                        entry.depth += base_depth;
                        entry
                    })
                    .collect();
                entries.splice(index + 1..index + 1, grafted);
            }
            // An uninitialized or unreadable submodule keeps its directory row but its
            // contents are genuinely unknown.
            Err(_) => partial = true,
        }
    }

    Ok(FileTreeListing {
        tree: FileTree::new(entries),
        partial,
    })
}

/// [`entries_from_git_listing`] wrapped as a finished listing, with no submodule typing - the
/// pure, directly-testable shape.
#[cfg(test)]
fn from_git_listing(
    root: &Path,
    list: &wt_core::worktree_files::WorktreeFileList,
) -> FileTreeListing {
    let (entries, partial) = entries_from_git_listing(root, list, &[]);
    FileTreeListing {
        tree: FileTree::new(entries),
        partial,
    }
}

/// Assembles pre-order entries from `list`'s worktree-relative `/`-separated paths: the same
/// dirs-first-then-alphabetical order and the same dot-prefixed-name filter as the
/// [`build_file_tree`] walk, so the two sources render identically where they overlap. Paths in
/// `submodules` are typed as directories (their content is grafted by the caller). A path
/// deeper than [`MAX_DEPTH`] is dropped at insertion - bounding the trie itself, not just the
/// emission - and marks the listing partial.
fn entries_from_git_listing(
    root: &Path,
    list: &wt_core::worktree_files::WorktreeFileList,
    submodules: &[String],
) -> (Vec<FileTreeEntry>, bool) {
    #[derive(Default)]
    struct Node {
        children: std::collections::BTreeMap<String, Node>,
        is_dir: bool,
    }

    let mut top = Node::default();
    let mut partial = list.truncated;
    let mut insert = |path: &str, force_dir: bool, partial: &mut bool| {
        if path
            .split('/')
            .any(|component| component.starts_with('.') || component.is_empty())
        {
            return;
        }
        if path.split('/').count() > MAX_DEPTH {
            *partial = true;
            return;
        }
        let mut node = &mut top;
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            let is_last = components.peek().is_none();
            node = node.children.entry(component.to_string()).or_default();
            node.is_dir |= !is_last || force_dir;
        }
    };
    for file in &list.files {
        insert(file, false, &mut partial);
    }
    for submodule in submodules {
        insert(submodule, true, &mut partial);
    }

    fn emit(node: &Node, dir: &Path, depth: usize, walk: &mut Walk) {
        if depth >= MAX_DEPTH {
            walk.partial = true;
            return;
        }
        let (dirs, files): (Vec<_>, Vec<_>) =
            node.children.iter().partition(|(_, child)| child.is_dir);
        for (name, child) in dirs.into_iter().chain(files) {
            let path = dir.join(name);
            walk.entries.push(FileTreeEntry {
                path: path.clone(),
                name: name.clone(),
                depth,
                is_dir: child.is_dir,
            });
            if child.is_dir {
                emit(child, &path, depth + 1, walk);
            }
        }
    }

    let mut walk = Walk {
        entries: Vec::new(),
        partial,
    };
    emit(&top, root, 0, &mut walk);
    (walk.entries, walk.partial)
}

/// Recursively lists `root`'s contents (directories first, then alphabetically within each
/// group) as a flattened, depth-annotated list suitable for indented rendering.
pub fn build_file_tree(root: &Path) -> io::Result<FileTreeListing> {
    let mut walk = Walk::default();
    visit(root, 0, &mut walk)?;
    Ok(FileTreeListing {
        tree: FileTree::new(walk.entries),
        partial: walk.partial,
    })
}

/// The walk's own mutable accumulator - deliberately not a [`FileTreeListing`], since the spans a
/// [`FileTree`] carries can only be derived once the entry list is finished.
#[derive(Default)]
struct Walk {
    entries: Vec<FileTreeEntry>,
    partial: bool,
}

fn visit(dir: &Path, depth: usize, walk: &mut Walk) -> io::Result<()> {
    if depth >= MAX_DEPTH {
        walk.partial = true;
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
                walk.partial = true;
                None
            }
        })
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .collect();

    children.sort_by(|a, b| {
        let a_is_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_is_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| a.file_name().cmp(&b.file_name()))
    });

    for child in children {
        let path = child.path();
        // A `file_type()` that fails leaves a real directory recorded as a file, so its whole
        // subtree is never walked - the same "incomplete but doesn't look it" hazard as above.
        let is_dir = match child.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(_) => {
                walk.partial = true;
                false
            }
        };
        let name = child.file_name().to_string_lossy().into_owned();
        walk.entries.push(FileTreeEntry {
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
            if visit(&path, depth + 1, walk).is_err() {
                walk.partial = true;
            }
        }
    }

    Ok(())
}

/// A file tree row's real 13×13 language chip (`.rs`→`rs`, `.toml`→`to`, `.md`→`md`,
/// `.sql`→`sq`, matching `theme::lang::*`), plus a neutral fallback (`theme::lang::UNKNOWN`) for
/// any other
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

/// The pre-span definition of [`FileTree::visible_indices`], kept as the oracle its own test
/// checks the fast, span-based implementation against: once an unexpanded directory at depth `d`
/// is seen, every following entry with `depth > d` is skipped until one with `depth <= d` is
/// reached. Correct but O(loaded entries) - which is exactly what issue #160 could not afford to
/// keep doing once the load cap was gone.
#[cfg(test)]
fn visible_indices_by_depth(entries: &[FileTreeEntry], expanded: &HashSet<PathBuf>) -> Vec<usize> {
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

/// Horizontal indent per nesting level.
pub const INDENT_STEP: f32 = 13.0;

/// Where level `level`'s vertical indent guide sits, in pixels from the row's left edge.
pub fn indent_guide_x(level: usize) -> f32 {
    ROW_LEFT_PAD + INDENT_STEP * level as f32 + CARET_WIDTH / 2.0
}

/// `render_tree_caret`'s real width - see [`indent_guide_x`].
const CARET_WIDTH: f32 = 8.0;

#[cfg(test)]
mod git_listing_tree_tests {
    use crate::sidebar::file_tree::{build_worktree_file_tree, from_git_listing, MAX_DEPTH};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;
    use wt_core::worktree_files::WorktreeFileList;

    fn listing_of(files: &[&str]) -> WorktreeFileList {
        WorktreeFileList {
            files: files.iter().map(|file| file.to_string()).collect(),
            truncated: false,
        }
    }

    #[test]
    fn assembles_dirs_first_then_files_with_depths_from_git_paths() {
        let root = Path::new("root");
        let tree = from_git_listing(
            root,
            &listing_of(&["src/nested/a.rs", "src/lib.rs", "b.txt", "README.md"]),
        );
        assert!(tree.is_complete());
        let shape: Vec<(&str, usize, bool)> = tree
            .tree
            .iter()
            .map(|entry| (entry.name.as_str(), entry.depth, entry.is_dir))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("src", 0, true),
                ("nested", 1, true),
                ("a.rs", 2, false),
                ("lib.rs", 1, false),
                ("README.md", 0, false),
                ("b.txt", 0, false),
            ],
            "same dirs-first, alphabetical-within-group order the walk produces"
        );
        assert_eq!(
            tree.tree.iter().next().expect("first entry").path,
            root.join("src")
        );
    }

    #[test]
    fn a_dot_prefixed_component_hides_the_whole_path_matching_the_walks_filter() {
        let tree = from_git_listing(
            Path::new("root"),
            &listing_of(&[".github/ci.yml", "src/.hidden/x.rs", "src/real.rs"]),
        );
        let names: Vec<&str> = tree.tree.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["src", "real.rs"]);
    }

    #[test]
    fn a_truncated_git_listing_is_an_incomplete_inventory() {
        let mut listing = listing_of(&["a.txt"]);
        listing.truncated = true;
        assert!(
            !from_git_listing(Path::new("root"), &listing).is_complete(),
            "fold-state pruning must never treat a byte-capped listing as the whole truth"
        );
    }

    #[test]
    fn a_pathologically_deep_path_is_cut_at_max_depth_and_marked_partial() {
        let deep = vec!["d"; MAX_DEPTH + 5].join("/") + "/file.txt";
        let tree = from_git_listing(Path::new("root"), &listing_of(&[deep.as_str()]));
        assert!(!tree.is_complete());
        assert!(tree.tree.iter().all(|entry| entry.depth < MAX_DEPTH));
    }

    #[test]
    fn a_real_repos_gitignored_build_output_is_not_walked_or_listed() {
        let repo = test_support::seed_repo();
        fs::write(repo.path().join(".gitignore"), "target/\n").expect("write");
        test_support::git(repo.path(), &["add", ".gitignore"]);
        test_support::git(repo.path(), &["commit", "-q", "-m", "ignore target"]);
        fs::create_dir_all(repo.path().join("target/debug")).expect("mkdir");
        fs::write(repo.path().join("target/debug/artifact.o"), "junk").expect("write");
        fs::write(repo.path().join("untracked.rs"), "// new").expect("write");

        let tree = build_worktree_file_tree(repo.path()).expect("build_worktree_file_tree");
        let names: Vec<&str> = tree.tree.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            !names.contains(&"target"),
            "gitignored build output must not appear (decisions.md §6), got {names:?}"
        );
        assert!(
            names.contains(&"untracked.rs"),
            "an untracked, unignored file is real content and must appear, got {names:?}"
        );
    }

    #[test]
    fn a_submodules_contents_are_grafted_under_a_directory_typed_row() {
        let sub = test_support::seed_empty_repo();
        fs::write(sub.path().join("inner.rs"), "// inner\n").expect("write");
        test_support::git(sub.path(), &["add", "inner.rs"]);
        test_support::git(sub.path(), &["commit", "-q", "-m", "inner"]);

        let outer = test_support::seed_repo();
        test_support::git(
            outer.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.path().to_str().expect("utf8 path"),
                "vendored",
            ],
        );

        let tree = build_worktree_file_tree(outer.path()).expect("build_worktree_file_tree");
        let vendored = tree
            .tree
            .iter()
            .find(|entry| entry.name == "vendored")
            .expect("the submodule must appear");
        assert!(
            vendored.is_dir,
            "a gitlink is a directory, not a file with a language chip"
        );
        let inner = tree
            .tree
            .iter()
            .find(|entry| entry.name == "inner.rs")
            .expect("the submodule's own contents must appear, as the old walk showed them");
        assert_eq!(inner.depth, vendored.depth + 1);
    }

    #[test]
    fn a_git_failure_inside_a_real_repo_is_an_error_not_a_silent_walk() {
        let dir = TempDir::new().expect("tempdir");
        // A `.git` directory that is not a repository: `git ls-files` fails here, and the
        // fallback walk must NOT paper over that - it would silently show a differently-shaped
        // (gitignore-blind) tree for one cycle.
        fs::create_dir(dir.path().join(".git")).expect("mkdir");
        fs::write(dir.path().join("loose.txt"), "x").expect("write");
        assert!(build_worktree_file_tree(dir.path()).is_err());
    }

    #[test]
    fn a_plain_non_git_directory_still_lists_via_the_walk_fallback() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("loose.txt"), "x").expect("write");
        let tree = build_worktree_file_tree(dir.path()).expect("build_worktree_file_tree");
        let names: Vec<&str> = tree.tree.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["loose.txt"]);
    }
}

#[cfg(test)]
mod file_tree_walk_tests {
    use crate::sidebar::file_tree::{build_file_tree, directory_paths, FileTree, MAX_DEPTH};
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn tree_of(root: &Path) -> FileTree {
        build_file_tree(root).expect("build_file_tree").tree
    }

    #[test]
    fn lists_files_and_directories_with_depth() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("b.txt"), "b").expect("write");
        fs::write(dir.path().join("a.txt"), "a").expect("write");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/nested.txt"), "n").expect("write");

        let entries = tree_of(dir.path());

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
        let listing = build_file_tree(dir.path()).expect("build_file_tree");
        assert!(listing.tree.is_empty());
        assert!(listing.is_complete());
    }

    #[test]
    fn nonexistent_root_returns_io_error() {
        let missing = PathBuf::from("/definitely/not/a/real/path/for/ade/file-tree-test");
        let result = build_file_tree(&missing);
        assert!(result.is_err());
    }

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

        let listing = build_file_tree(dir.path()).expect("build_file_tree");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(
            listing.tree.iter().any(|entry| entry.name == "file.txt"),
            "the readable part of the tree must still be listed"
        );
        assert!(listing.partial, "the skipped directory must be reported");
        assert!(
            !listing.is_complete(),
            "and so this listing must never be used as a complete inventory"
        );
    }

    #[test]
    fn a_tree_deeper_than_the_depth_cap_is_reported_as_partial() {
        let dir = TempDir::new().expect("tempdir");
        let mut deep = dir.path().to_path_buf();
        for level in 0..(MAX_DEPTH + 2) {
            deep = deep.join(format!("d{level}"));
        }
        fs::create_dir_all(&deep).expect("mkdir -p");

        let listing = build_file_tree(dir.path()).expect("build_file_tree");

        assert_eq!(listing.tree.len(), MAX_DEPTH);
        assert!(listing.partial);
    }

    /// GitHub issue #160, at the level the walk itself decides it: a tree with more entries than
    /// the removed 20,000-entry default cap must come back whole, with no truncation flag left
    /// anywhere to hang a "load more" row off, and no render cap dropping any of it either
    /// (issue #18 §4 - the sidebar's virtualized list is what keeps a big tree cheap to draw).
    ///
    /// Built as a wide, shallow fan (200 directories x 105 files) rather than 21,000 files in one
    /// folder, so it also exercises the recursive descent the old budget used to cut short
    /// mid-subtree.
    #[test]
    fn a_tree_larger_than_the_removed_twenty_thousand_entry_cap_loads_completely() {
        const DIRS: usize = 200;
        const FILES_PER_DIR: usize = 105;
        // 200 directory rows + 21,000 file rows - comfortably past the 20,000 the walk used to
        // stop at. Checked at compile time so the fixture can never be shrunk below the removed
        // cap and go on passing for the wrong reason.
        const EXPECTED: usize = DIRS + DIRS * FILES_PER_DIR;
        const _: () = assert!(EXPECTED > 20_000);

        let dir = TempDir::new().expect("tempdir");
        for d in 0..DIRS {
            let sub = dir.path().join(format!("d-{d:03}"));
            fs::create_dir(&sub).expect("mkdir");
            for f in 0..FILES_PER_DIR {
                fs::write(sub.join(format!("f-{f:03}.txt")), "x").expect("write");
            }
        }

        let listing = build_file_tree(dir.path()).expect("build_file_tree");

        assert_eq!(
            listing.tree.len(),
            EXPECTED,
            "every folder and every file must be loaded - the old cap stopped at 20,000"
        );
        assert!(
            listing.is_complete(),
            "and a walk that reached everything is a complete inventory"
        );
        assert_eq!(
            listing.tree.visible_entries(&HashSet::new()).len(),
            DIRS,
            "with nothing expanded every one of the directory rows is visible, and no render cap \
             may drop any of them"
        );
        // The last directory's last file is the entry furthest past the old cut-off; naming it
        // proves the walk really continued rather than merely counting to a bigger number.
        let last = dir
            .path()
            .join(format!("d-{:03}", DIRS - 1))
            .join(format!("f-{:03}.txt", FILES_PER_DIR - 1));
        assert!(
            listing.tree.iter().any(|entry| entry.path == last),
            "{} is ~1,000 entries past the removed cap and must still be present",
            last.display()
        );
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
}

#[cfg(test)]
mod tree_visibility_tests {
    use crate::sidebar::file_tree::{
        build_file_tree, visible_indices_by_depth, FileTree, FileTreeEntry,
    };
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn entry(name: &str, depth: usize, is_dir: bool) -> FileTreeEntry {
        FileTreeEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            depth,
            is_dir,
        }
    }

    /// `sub/` holding `nested.txt` and `deeper/`, `deeper/` holding `deepest.txt`, plus a
    /// root-level `a.txt` and an `empty-dir/` with nothing in it - one shape every expansion case
    /// below reads out of.
    fn sample_tree() -> FileTree {
        FileTree::new(vec![
            entry("empty-dir", 0, true),
            entry("sub", 0, true),
            entry("nested.txt", 1, false),
            entry("deeper", 1, true),
            entry("deepest.txt", 2, false),
            entry("a.txt", 0, false),
        ])
    }

    fn visible_names(tree: &FileTree, expanded: &[&str]) -> Vec<String> {
        let expanded: HashSet<PathBuf> = expanded.iter().map(PathBuf::from).collect();
        tree.visible_entries(&expanded)
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// Issue #18 §1's default state and the rules that grow out of it, over one tree: absence
    /// from the expanded set means collapsed, expansion reveals only the folder's own immediate
    /// children, and a stale deep expansion never punches a hole through a collapsed ancestor.
    #[test]
    fn expanding_reveals_exactly_the_expanded_folders_own_children() {
        let tree = sample_tree();
        for (expanded, expected) in [
            (&[][..], &["empty-dir", "sub", "a.txt"][..]),
            (
                &["sub"][..],
                &["empty-dir", "sub", "nested.txt", "deeper", "a.txt"][..],
            ),
            (
                &["sub", "deeper"][..],
                &[
                    "empty-dir",
                    "sub",
                    "nested.txt",
                    "deeper",
                    "deepest.txt",
                    "a.txt",
                ][..],
            ),
            // `deeper` is expanded but its own parent is not - it must stay hidden regardless.
            (&["deeper"][..], &["empty-dir", "sub", "a.txt"][..]),
            // Expanding a folder with no children reveals nothing extra.
            (&["empty-dir"][..], &["empty-dir", "sub", "a.txt"][..]),
        ] {
            assert_eq!(
                visible_names(&tree, expanded),
                expected,
                "expanded {expanded:?}"
            );
        }
    }

    /// The span-based skip must agree with the depth-scan it replaced on a real, nested tree -
    /// for every combination of expanded folders, not just the one a hand-written case picks.
    #[test]
    fn span_based_visibility_matches_the_depth_scan_for_every_expansion() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("a/b/c")).expect("mkdir -p");
        fs::create_dir_all(dir.path().join("a/d")).expect("mkdir -p");
        fs::create_dir(dir.path().join("e")).expect("mkdir");
        fs::write(dir.path().join("a/b/c/deep.txt"), "x").expect("write");
        fs::write(dir.path().join("a/b/mid.txt"), "x").expect("write");
        fs::write(dir.path().join("a/d/other.txt"), "x").expect("write");
        fs::write(dir.path().join("e/leaf.txt"), "x").expect("write");
        fs::write(dir.path().join("root.txt"), "x").expect("write");

        let tree = build_file_tree(dir.path()).expect("build_file_tree").tree;
        let dirs: Vec<PathBuf> = tree
            .iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.path.clone())
            .collect();
        assert_eq!(dirs.len(), 5, "a/ a/b/ a/b/c/ a/d/ e/");

        for mask in 0..(1u32 << dirs.len()) {
            let expanded: HashSet<PathBuf> = dirs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, path)| path.clone())
                .collect();
            assert_eq!(
                tree.visible_indices(&expanded),
                visible_indices_by_depth(&tree, &expanded),
                "span-based visibility diverged from the depth scan for expansion mask {mask:#b}"
            );
        }
    }
}

#[cfg(test)]
mod lang_chip_tests {
    use crate::sidebar::file_tree::lang_chip_for_name;
    use crate::theme;
    use gpui::Rgba;

    fn same(a: Rgba, b: Rgba) -> bool {
        a.r == b.r && a.g == b.g && a.b == b.b && a.a == b.a
    }

    /// One table over the whole mapping: each documented extension's own chip, the neutral
    /// fallback for an unrecognized one and for a name with no extension at all, and the
    /// case-insensitive match.
    #[test]
    fn every_name_gets_its_documented_chip_or_the_neutral_fallback() {
        for (name, label, colors) in [
            ("main.rs", "rs", theme::lang::RS),
            ("Cargo.toml", "to", theme::lang::TOML),
            ("README.md", "md", theme::lang::MD),
            ("schema.sql", "sq", theme::lang::SQL),
            ("Notes.MD", "md", theme::lang::MD),
            ("image.png", ".", theme::lang::UNKNOWN),
            ("Makefile", ".", theme::lang::UNKNOWN),
        ] {
            let chip = lang_chip_for_name(name);
            assert_eq!(chip.label, label, "{name}");
            assert!(same(chip.fg, colors.0.into()), "{name} foreground");
            assert!(same(chip.bg, colors.1.into()), "{name} background");
        }
    }
}

#[cfg(test)]
mod indent_geometry_tests {
    use crate::sidebar::file_tree::indent_guide_x;

    #[test]
    fn indent_guides_line_up_with_each_levels_expand_chevron() {
        // The chevron for depth `d` starts at `8 + 13*d` and is 8 wide, so its centre - and the
        // guide - is at `12 + 13*d`.
        assert_eq!(indent_guide_x(0), 12.0);
        assert_eq!(indent_guide_x(1), 25.0);
        assert_eq!(indent_guide_x(2), 38.0);
    }
}
