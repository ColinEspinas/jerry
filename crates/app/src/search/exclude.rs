//! The always-on, explicit exclude layer search's file discovery walk applies before it ever asks
//! `.gitignore` anything - the first of the two layers GitHub issue #394 reworked
//! [`crate::search::engine`]'s file source into, after a direct live pushback on #387/#388's own
//! fix: "Wait what you made the search respect gitignore? This should have nothing to do with
//! git?"

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::search::glob::GlobList;

/// The always-on denylist - real, small, and not exhaustive by design (see this module's own
/// docs). Every entry is a bare name, which [`crate::search::glob::Glob::parse`]'s own basename
/// rule turns into `**/<name>`, matching that directory at any depth - `target` excludes both
/// `./target` and `crates/app/target`, the same way the panel's own `*.lock` exclude field
/// already matches a lockfile at any depth.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "target",
    ".shared-target",
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    "__pycache__",
    "venv",
    ".venv",
];

/// [`DEFAULT_EXCLUDES`], compiled once into the same [`GlobList`] the panel's own path filter
/// fields use. A fresh list per call rather than a `once_cell`/`lazy_static`: parsing ten short,
/// static, always-valid patterns is microseconds, and every real caller (one worktree walk per
/// keystroke, well downstream of [`crate::search::engine::SEARCH_DEBOUNCE`]) already pays far
/// more than that for the walk itself.
pub fn default_exclude_list() -> GlobList {
    GlobList::parse(&DEFAULT_EXCLUDES.join(","))
}

/// [`DEFAULT_EXCLUDES`] as an owned, editable `Vec<String>` - the real seed value
/// [`crate::settings::store::EditorSettings::search_excludes`]'s own `Default` impl uses, and what
/// every real fresh install starts out with in Settings > Editor > Search before the user ever
/// touches it. See this module's own "GitHub issue #401" docs for why the persisted setting is
/// seeded from this rather than layered additively on top of it.
pub fn default_search_excludes() -> Vec<String> {
    DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect()
}

/// Compiles a real, user-editable pattern list (`EditorSettings::search_excludes`, or a test's own
/// stand-in for it) into the same [`GlobList`] [`default_exclude_list`] builds from the constant -
/// one real compilation path for "whatever list of bare/glob directory patterns is currently in
/// force," whether that list is the compiled-in default or the user's own edited copy of it.
pub fn exclude_list_from(patterns: &[String]) -> GlobList {
    GlobList::parse(&patterns.join(","))
}

/// Every real file under `root`, as an absolute path paired with its worktree-relative,
/// `/`-separated form, skipping any directory [`excludes`] matches **before** it is ever opened -
/// see this module's own docs for why that, not a filter applied after the fact, is what makes an
/// excluded directory genuinely cheap.
pub fn collect_files_excluding(
    root: &Path,
    excludes: &GlobList,
    cap: usize,
) -> (Vec<(PathBuf, String)>, bool) {
    let count = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let files = walk(root, root, excludes, &count, cap, &truncated);
    (files, truncated.load(Ordering::Relaxed))
}

/// The recursive worker behind [`collect_files_excluding`]. Reads `dir`, splits its entries into
/// real files (kept, subject to `cap`) and real subdirectories (kept only when [`excludes`]
/// doesn't match their own worktree-relative path), then recurses into the surviving
/// subdirectories in parallel via `rayon`'s `par_iter` - the standard parallel-recursive-walk
/// shape, and what lets pruning one enormous excluded sibling (`target/`) never hold up progress
/// on every other real directory being walked at the same time.
fn walk(
    dir: &Path,
    root: &Path,
    excludes: &GlobList,
    count: &AtomicUsize,
    cap: usize,
    truncated: &AtomicBool,
) -> Vec<(PathBuf, String)> {
    if truncated.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        // Checked per-entry, not only once per directory: a single real directory can itself
        // hold tens of thousands of flat files (a `target/debug/deps/`-shaped directory not on
        // the exclude list, say), and the cap has to stop a walk mid-directory in that case, not
        // only between directories.
        if truncated.load(Ordering::Relaxed) {
            break;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let Some(relative) = relative_slash_path(root, &path) else {
            continue;
        };
        if file_type.is_dir() {
            if excludes.matches(&relative) {
                // Never descended into - the whole point of checking here rather than filtering
                // the walk's own output afterwards.
                continue;
            }
            subdirs.push(path);
        } else if file_type.is_file() {
            if count.fetch_add(1, Ordering::Relaxed) + 1 >= cap {
                truncated.store(true, Ordering::Relaxed);
            }
            files.push((path, relative));
        }
    }

    let nested: Vec<Vec<(PathBuf, String)>> = subdirs
        .par_iter()
        .map(|subdir| walk(subdir, root, excludes, count, cap, truncated))
        .collect();
    for batch in nested {
        files.extend(batch);
    }
    files
}

/// `path` relative to `root`, with `/` separators - `None` when it is not under `root` at all.
/// Shared with `crate::search::engine`'s own fallback formatting so the two walks can never
/// disagree about what "worktree-relative" means.
pub(crate) fn relative_slash_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for component in relative.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(&path, content).expect("write");
    }

    fn relatives(files: &[(PathBuf, String)]) -> Vec<&str> {
        let mut out: Vec<&str> = files
            .iter()
            .map(|(_, relative)| relative.as_str())
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn a_default_excluded_directory_is_never_descended_into() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", "fn main() {}\n");
        for i in 0..50 {
            write(root, &format!("target/debug/deps/artifact-{i}.o"), "junk");
        }
        write(
            root,
            "node_modules/left-pad/index.js",
            "module.exports = 1;\n",
        );
        write(root, ".git/COMMIT_EDITMSG", "wip\n");

        let (files, truncated) = collect_files_excluding(root, &default_exclude_list(), 20_000);
        assert!(!truncated);
        assert_eq!(
            relatives(&files),
            vec!["src/lib.rs"],
            "target/, node_modules/ and .git/ must all be pruned before being opened"
        );
    }

    #[test]
    fn a_nested_default_excluded_directory_is_pruned_at_any_depth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "crates/app/src/lib.rs", "fn main() {}\n");
        write(root, "crates/app/target/debug/x.o", "junk");

        let (files, _truncated) = collect_files_excluding(root, &default_exclude_list(), 20_000);
        assert_eq!(relatives(&files), vec!["crates/app/src/lib.rs"]);
    }

    #[test]
    fn a_real_second_build_cache_directory_is_excluded_by_name_alone() {
        // This repository's own real second build-cache directory (#387/#388,
        // this module's own docs) - excluded here even with no `.gitignore` involved at all,
        // which is the whole point of it being a real, explicit entry rather than left to the
        // gitignore layer.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "README.md", "hi\n");
        for i in 0..50 {
            write(root, &format!(".shared-target/deps/artifact-{i}.o"), "junk");
        }

        let (files, _truncated) = collect_files_excluding(root, &default_exclude_list(), 20_000);
        assert_eq!(relatives(&files), vec!["README.md"]);
    }

    #[test]
    fn a_directory_not_on_the_default_list_is_walked_normally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "vendor/lib.rs", "// not excluded by default\n");

        let (files, _truncated) = collect_files_excluding(root, &default_exclude_list(), 20_000);
        assert_eq!(relatives(&files), vec!["vendor/lib.rs"]);
    }

    #[test]
    fn a_symlink_is_never_followed_so_a_loop_cannot_make_the_walk_unbounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", "fn main() {}\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root, root.join("loop")).expect("a real symlink");
            let (files, truncated) = collect_files_excluding(root, &default_exclude_list(), 20_000);
            assert_eq!(relatives(&files), vec!["src/lib.rs"]);
            assert!(!truncated);
        }
    }

    #[test]
    fn default_search_excludes_is_an_owned_copy_of_default_excludes() {
        assert_eq!(
            default_search_excludes(),
            DEFAULT_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "the real settings default must seed from exactly the compiled-in list, not a \
             hand-copied second one that could drift from it"
        );
    }

    #[test]
    fn exclude_list_from_a_custom_pattern_excludes_matching_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", "fn main() {}\n");
        write(root, "coverage/report.html", "<html></html>\n");

        let excludes = exclude_list_from(&["coverage".to_string()]);
        let (files, _truncated) = collect_files_excluding(root, &excludes, 20_000);
        assert_eq!(
            relatives(&files),
            vec!["src/lib.rs"],
            "a user-added pattern must be pruned by the walk exactly like a built-in one"
        );
    }

    #[test]
    fn exclude_list_from_with_a_default_entry_removed_re_includes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", "fn main() {}\n");
        for i in 0..5 {
            write(root, &format!("target/debug/deps/artifact-{i}.o"), "junk");
        }
        write(
            root,
            "node_modules/left-pad/index.js",
            "module.exports = 1;\n",
        );

        // The user's own edited copy of the default list with `target` deleted from it - the
        // real shape `EditorSettings::search_excludes` takes once a row's remove affordance is
        // clicked (`crate::settings::render::AdeApp::remove_search_exclude_pattern`).
        let mut patterns = default_search_excludes();
        patterns.retain(|pattern| pattern != "target");
        let excludes = exclude_list_from(&patterns);

        let (files, _truncated) = collect_files_excluding(root, &excludes, 20_000);
        let found = relatives(&files);
        assert!(
            found.iter().any(|path| path.starts_with("target/")),
            "removing `target` from the user's own list must really re-include it: {found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with("node_modules/")),
            "every other still-present default entry must keep excluding: {found:?}"
        );
        assert!(found.contains(&"src/lib.rs"));
    }

    #[test]
    fn exclude_list_from_an_empty_list_excludes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", "fn main() {}\n");
        write(root, "target/debug/x.o", "junk");

        let excludes = exclude_list_from(&[]);
        let (files, _truncated) = collect_files_excluding(root, &excludes, 20_000);
        assert_eq!(
            relatives(&files),
            vec!["src/lib.rs", "target/debug/x.o"],
            "a user who has genuinely deleted every entry gets an honest, unfiltered walk"
        );
    }

    #[test]
    fn the_cap_stops_the_walk_and_reports_itself_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..30 {
            write(root, &format!("src/file_{i}.rs"), "content\n");
        }

        let (files, truncated) = collect_files_excluding(root, &default_exclude_list(), 10);
        assert!(truncated);
        assert!(
            !files.is_empty() && files.len() < 30,
            "the cap must actually bound the walk: got {} of 30",
            files.len()
        );
    }
}
