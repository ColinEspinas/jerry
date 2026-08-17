//! Fixture repositories: what a seeded repo *contains*, built out of [`crate::git`].
//!
//! Each `seed_*` function has an `_at` sibling that takes an existing directory, for the tests
//! that need the repository somewhere specific (a worktree, a nested path) rather than in a
//! throwaway temp directory.

use crate::git::{commit, git};
use std::path::Path;
use tempfile::TempDir;

/// The identity every fixture commit carries. A repository with no `user.email`/`user.name`
/// cannot commit at all, and inheriting the developer's own identity would make fixtures differ
/// between machines.
const AUTHOR_EMAIL: &str = "test@example.com";
const AUTHOR_NAME: &str = "Test User";

/// A git repository on branch `main` with a configured identity and **no commits**.
pub fn seed_empty_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    seed_empty_repo_at(dir.path());
    dir
}

/// [`seed_empty_repo`] into an existing directory.
pub fn seed_empty_repo_at(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", AUTHOR_EMAIL]);
    git(dir, &["config", "user.name", AUTHOR_NAME]);
    // A machine with commit signing configured globally would otherwise block every fixture
    // commit on a passphrase prompt that never arrives in a test runner.
    git(dir, &["config", "commit.gpgsign", "false"]);
    // Git for Windows ships `core.autocrlf=true`, which would materialize CRLF in every fixture
    // checkout and put `\r` into content, diff-hunk and blame assertions the fixture wrote with
    // `\n`. Pinned here so a fixture behaves identically on all three platforms (#440, finding 2).
    git(dir, &["config", "core.autocrlf", "false"]);
    // Every `git commit` runs an auto-gc check, which can fork a real background repack. A fixture
    // that seeds hundreds of commits and then hands the repository straight to `gix` can have its
    // objects moved from loose to packed *while* that walk is opening them - which surfaces as a
    // genuinely intermittent `An object with id <sha> could not be found`, observed twice on the
    // Linux runner in `crate::graph_view::render::graph_virtualization_tests`. Fixtures are
    // short-lived and thrown away, so they gain nothing from packing in the first place.
    git(dir, &["config", "gc.auto", "0"]);
    git(dir, &["config", "maintenance.auto", "false"]);
}

/// A git repository on branch `main` with one commit (`file.txt`) and a clean working tree.
pub fn seed_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    seed_repo_at(dir.path());
    dir
}

/// [`seed_repo`] into an existing directory.
pub fn seed_repo_at(dir: &Path) {
    seed_empty_repo_at(dir);
    commit(dir, "file.txt", "hello\n", "initial commit");
}

/// Three commits (`first`, `second`, `third`) each rewriting `a.txt`, clean working tree at the
/// end — so a graph built from this has exactly three rows (newest first) and no "Working tree"
/// row shifting the indices a test names by literal position.
///
/// Not `seed_commits(dir, 3)`: these three carry real content changes, which is what a test
/// asserting on a diff, a blame or a commit message needs.
pub fn seed_three_commits(dir: &Path) {
    seed_empty_repo_at(dir);
    commit(dir, "a.txt", "1\n", "first");
    commit(dir, "a.txt", "2\n", "second");
    commit(dir, "a.txt", "3\n", "third");
}

/// `count` commits, clean working tree at the end. The first touches `a.txt`; the rest are
/// `--allow-empty`, which keeps a large seed cheap — an empty commit is as real a graph row as
/// any other. Use [`seed_three_commits`] when the *content* of each commit matters.
pub fn seed_commits(dir: &Path, count: usize) {
    seed_empty_repo_at(dir);
    if count == 0 {
        return;
    }
    commit(dir, "a.txt", "1\n", "first");
    for index in 1..count {
        git(
            dir,
            &["commit", "--allow-empty", "-m", &format!("commit {index}")],
        );
    }
}

/// A bare repository on branch `main` — the push/fetch target for remote-facing tests.
pub fn seed_bare_remote() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "--bare", "-b", "main"]);
    // Same reason as `seed_empty_repo_at`'s pair: a receiving repository runs its own auto-gc
    // after a push, and a fixture never benefits from being packed.
    git(dir.path(), &["config", "gc.auto", "0"]);
    git(dir.path(), &["config", "maintenance.auto", "false"]);
    dir
}

/// Adds a real worktree for a new `branch` at `worktree_path`, which must not exist yet.
pub fn add_worktree(repo_path: &Path, branch: &str, worktree_path: &Path) {
    let path = worktree_path
        .to_str()
        .expect("fixture worktree paths are UTF-8");
    git(repo_path, &["worktree", "add", "-b", branch, path]);
}

#[cfg(test)]
mod repo_seed_tests {
    use crate::{
        add_worktree, git_output, seed_bare_remote, seed_commits, seed_empty_repo, seed_repo,
        seed_repo_at, seed_three_commits,
    };
    use tempfile::TempDir;

    fn commit_count(dir: &std::path::Path) -> usize {
        git_output(dir, &["rev-list", "--count", "HEAD"])
            .parse()
            .expect("rev-list --count is a number")
    }

    #[test]
    fn seed_repo_leaves_one_commit_and_a_clean_tree() {
        let repo = seed_repo();

        assert_eq!(commit_count(repo.path()), 1);
        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain"]),
            "",
            "a fixture that starts dirty makes every downstream status assertion ambiguous"
        );
    }

    #[test]
    fn seed_empty_repo_has_a_branch_but_no_commits() {
        let repo = seed_empty_repo();

        // `symbolic-ref`, not `rev-parse --abbrev-ref`: the latter needs HEAD to resolve to a
        // commit, which is exactly what this fixture does not have.
        assert_eq!(
            git_output(repo.path(), &["symbolic-ref", "--short", "HEAD"]),
            "main",
            "the unborn branch must still be `main`, not the machine's init.defaultBranch"
        );
        assert!(
            git_output(repo.path(), &["rev-list", "--all"]).is_empty(),
            "seed_empty_repo is the fixture for code paths that must cope with no commits"
        );
    }

    #[test]
    fn seed_three_commits_leaves_three_commits_and_a_clean_tree() {
        let dir = TempDir::new().expect("tempdir");

        seed_three_commits(dir.path());

        assert_eq!(commit_count(dir.path()), 3);
        assert_eq!(
            git_output(dir.path(), &["log", "--format=%s"]),
            "third\nsecond\nfirst",
            "the three messages are part of the fixture's contract - tests assert on them"
        );
        assert_eq!(
            git_output(dir.path(), &["show", "HEAD:a.txt"]),
            "3",
            "each of the three commits must carry a real content change, not be empty"
        );
        assert_eq!(
            git_output(dir.path(), &["status", "--porcelain"]),
            "",
            "graph tests index rows by position, which a stray working-tree row would shift"
        );
    }

    #[test]
    fn seed_commits_makes_exactly_the_number_asked_for() {
        let dir = TempDir::new().expect("tempdir");

        seed_commits(dir.path(), 7);

        assert_eq!(commit_count(dir.path()), 7);
    }

    /// A seeded repository must never run a background repack. A fixture that seeds hundreds of
    /// commits and hands the result straight to `gix` otherwise races git's auto-gc, which moves
    /// objects from loose to packed mid-walk and produces an intermittent
    /// `An object with id <sha> could not be found` - twice observed on the Linux runner. Pinned
    /// as a real assertion because a silent config line is exactly the kind of thing a later
    /// change drops without noticing.
    #[test]
    fn a_seeded_repo_never_runs_a_background_gc() {
        let repo = seed_repo();

        assert_eq!(git_output(repo.path(), &["config", "gc.auto"]), "0");
        assert_eq!(
            git_output(repo.path(), &["config", "maintenance.auto"]),
            "false"
        );
    }

    #[test]
    fn seed_repo_at_works_inside_an_existing_directory() {
        let parent = TempDir::new().expect("tempdir");
        let nested = parent.path().join("nested/repo");
        std::fs::create_dir_all(&nested).expect("mkdir");

        seed_repo_at(&nested);

        assert_eq!(commit_count(&nested), 1);
    }

    #[test]
    fn seed_bare_remote_is_really_bare() {
        let remote = seed_bare_remote();

        assert_eq!(
            git_output(remote.path(), &["rev-parse", "--is-bare-repository"]),
            "true",
            "a non-bare push target rejects pushes to its checked-out branch"
        );
    }

    #[test]
    fn add_worktree_creates_a_real_second_working_copy() {
        let repo = seed_repo();
        let container = TempDir::new().expect("tempdir");
        let worktree = container.path().join("feature");

        add_worktree(repo.path(), "feature", &worktree);

        assert!(
            worktree.join("file.txt").is_file(),
            "the added worktree must be a real checkout, not just a registered path"
        );
        assert_eq!(
            git_output(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feature",
            "the worktree must be on the branch it was created for"
        );
    }
}
