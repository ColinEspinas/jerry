//! Shared test fixtures for this workspace: real git repositories, bounded waits, and RAII
//! teardown for spawned processes.
//!
//! Every crate here may dev-depend on this one, so it stays free of `gpui`
//! (`docs/architecture/decisions.md` §1) — including behind a Cargo feature, since workspace
//! feature unification would leak `gpui` back into the core crates' dev graph. GPUI-flavoured
//! helpers live in `crates/app/src/test_support.rs` instead.
//!
//! The policy these fixtures serve — the three tiers, what deserves a test, and what gets
//! deleted — is [`docs/testing.md`](../../../docs/testing.md).

// Every function here runs inside a consumer's `#[cfg(test)]` code, where a broken fixture must
// abort the test with a diagnostic rather than propagate a `Result` nobody can act on.
#![allow(clippy::expect_used)]

mod child;
mod git;
mod repo;
mod wait;

pub use child::ChildGuard;
pub use git::{commit, commit_at, git, git_output, git_try, git_with_env, write_file};
pub use repo::{
    add_worktree, seed_bare_remote, seed_commits, seed_empty_repo, seed_empty_repo_at, seed_repo,
    seed_repo_at, seed_three_commits,
};
pub use wait::{stays_false, wait_until};

/// Re-exported so a consumer can name the type [`seed_repo`] returns without repeating the
/// `tempfile` dependency in its own manifest.
pub use tempfile::TempDir;
