//! Real `git` CLI invocation for tests.
//!
//! Every entry point takes an argument vector, never an interpolated shell string, so a fixture
//! path containing a space or a quote behaves the same as any other (CLAUDE.md's git rule).
//! This module owns running git and writing files; deciding *what* a fixture repository
//! contains belongs to [`crate::repo`].

use std::path::Path;
use std::process::{Command, Output};

/// Runs `git` in `dir` and panics with both output streams if it fails.
pub fn git(dir: &Path, args: &[&str]) {
    assert_success(dir, args, git_try(dir, args));
}

/// Runs `git` in `dir` with extra environment variables (`GIT_AUTHOR_DATE`, `GIT_EDITOR`, …).
pub fn git_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) {
    let mut command = Command::new("git");
    command.current_dir(dir).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("failed to spawn git");
    assert_success(dir, args, output);
}

/// The trimmed stdout of a successful `git` invocation.
pub fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = git_try(dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_success(dir, args, output);
    stdout
}

/// Runs `git` without asserting anything — for the tests whose subject *is* a failing git
/// command (a conflicted merge, a rejected push).
pub fn git_try(dir: &Path, args: &[&str]) -> Output {
    // `.output()`, not `.status()`: `status()` inherits stdout/stderr, so seeding a repository
    // dumps git's per-file chatter into the test runner's output.
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to spawn git")
}

/// Writes `contents` to `dir`-relative `relative_path`, creating parent directories.
pub fn write_file(dir: &Path, relative_path: impl AsRef<Path>, contents: &str) {
    let path = dir.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent directories");
    }
    std::fs::write(&path, contents).expect("write fixture file");
}

/// Writes a file and commits it — the two-step every fixture repeats.
pub fn commit(dir: &Path, relative_path: impl AsRef<Path>, contents: &str, message: &str) {
    let relative_path = relative_path.as_ref();
    write_file(dir, relative_path, contents);
    git(dir, &["add", path_arg(relative_path)]);
    git(dir, &["commit", "-m", message]);
}

/// Like [`commit`], but with an explicit author/committer timestamp instead of "whenever this
/// line happened to run" — what a test asserting on commit order or on rendered dates needs.
pub fn commit_at(
    dir: &Path,
    relative_path: impl AsRef<Path>,
    contents: &str,
    message: &str,
    unix_seconds: i64,
) {
    let relative_path = relative_path.as_ref();
    write_file(dir, relative_path, contents);
    git(dir, &["add", path_arg(relative_path)]);
    let date = format!("{unix_seconds} +0000");
    git_with_env(
        dir,
        &["commit", "-m", message],
        &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)],
    );
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("fixture paths are UTF-8")
}

fn assert_success(dir: &Path, args: &[&str], output: Output) {
    assert!(
        output.status.success(),
        "git {args:?} failed in {dir:?}:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
mod git_invocation_tests {
    use crate::{commit, commit_at, git, git_output, git_try, seed_empty_repo, seed_repo};

    #[test]
    fn git_output_returns_trimmed_stdout() {
        let repo = seed_repo();

        let branch = git_output(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);

        assert_eq!(
            branch, "main",
            "git_output must hand back stdout with the trailing newline stripped"
        );
    }

    #[test]
    fn git_try_reports_a_failure_instead_of_panicking() {
        let repo = seed_repo();

        let output = git_try(repo.path(), &["rev-parse", "definitely-not-a-ref"]);

        assert!(
            !output.status.success(),
            "git_try is the escape hatch for tests whose subject is a failing git command, so \
             it must report the failure rather than abort the test"
        );
    }

    #[test]
    fn a_path_containing_a_space_survives_the_argument_vector() {
        let repo = seed_empty_repo();

        commit(repo.path(), "a file.txt", "x\n", "spaces");

        let tracked = git_output(repo.path(), &["ls-files"]);
        assert_eq!(
            tracked, "a file.txt",
            "argv, not an interpolated shell string: the space must never split the argument"
        );
    }

    #[test]
    fn commit_creates_missing_parent_directories() {
        let repo = seed_empty_repo();

        commit(
            repo.path(),
            "src/nested/deep.rs",
            "fn main() {}\n",
            "nested",
        );

        assert!(
            repo.path().join("src/nested/deep.rs").is_file(),
            "a fixture naming a nested path must not have to mkdir it first"
        );
    }

    #[test]
    fn commit_at_uses_the_timestamp_it_was_given() {
        let repo = seed_empty_repo();

        commit_at(repo.path(), "a.txt", "1\n", "dated", 1_700_000_000);

        assert_eq!(
            git_output(repo.path(), &["log", "-1", "--format=%at"]),
            "1700000000",
            "an explicitly dated commit is what lets a test assert on ordering deterministically"
        );
    }

    #[test]
    fn git_runs_in_the_directory_it_is_given_not_the_process_cwd() {
        let outer = seed_repo();
        let inner = seed_empty_repo();

        git(inner.path(), &["checkout", "-b", "side"]);

        assert_eq!(
            git_output(outer.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main",
            "each call is scoped to its own `dir`, so parallel fixtures never collide"
        );
    }
}
