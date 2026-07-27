use std::path::PathBuf;
use std::process::ExitStatus;

/// How a spawned `git` process terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitExit {
    /// The process exited normally with the given exit code.
    Code(i32),
    /// The process was terminated by the given signal number (e.g. `SIGKILL`, `SIGSEGV`).
    /// Only ever produced on Unix; a non-zero-but-codeless status on other platforms is
    /// reported as [`GitExit::Unknown`].
    Signal(i32),
    /// The process terminated abnormally and neither an exit code nor a signal number
    /// could be determined.
    Unknown,
}

impl GitExit {
    pub(crate) fn from_status(status: &ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return GitExit::Code(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return GitExit::Signal(signal);
            }
        }
        GitExit::Unknown
    }
}

impl std::fmt::Display for GitExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitExit::Code(code) => write!(f, "exited with status {code}"),
            GitExit::Signal(signal) => write!(f, "was terminated by signal {signal}"),
            GitExit::Unknown => write!(f, "terminated abnormally"),
        }
    }
}

/// Errors produced by `wt-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to open the git repository at the given path via `gix`.
    #[error("failed to open git repository at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: Box<gix::open::Error>,
    },

    /// An I/O error occurred while reading worktree metadata from the filesystem, or while
    /// communicating with a spawned `git` process (e.g. its stdout pipe failed mid-read).
    #[error("failed to read worktree information: {0}")]
    WorktreeIo(#[from] std::io::Error),

    /// Failed to read the `HEAD` reference of a worktree.
    #[error("failed to read HEAD: {0}")]
    Head(#[source] Box<gix::reference::find::existing::Error>),

    /// Failed to resolve `HEAD` to a commit id.
    #[error("failed to resolve HEAD to a commit: {0}")]
    PeelHead(#[source] Box<gix::head::peel::Error>),

    /// Failed to spawn the `git` process.
    #[error("failed to run `git {args}`: {source}")]
    GitSpawn {
        args: String,
        #[source]
        source: std::io::Error,
    },

    /// The `git` process ran but exited with a non-zero status (or was killed by a
    /// signal; see [`GitExit`]).
    #[error("`git {args}` {exit}: {stderr}")]
    GitCommand {
        args: String,
        exit: GitExit,
        stderr: String,
    },

    /// Refused to remove a worktree with uncommitted changes (modified tracked files or
    /// untracked files) because `force` was not set.
    #[error(
        "worktree at {path} has uncommitted changes (tracked or untracked); pass force=true to remove it anyway"
    )]
    DirtyWorktree { path: PathBuf },
}
