//! What a `Command` or `Query` receives: where it acts and who is asking. The caller always
//! supplies the paths; a `Ctx` never reads application state, so every process can build one.

use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// The identity Jerry injected into an agent's environment as `JERRY_AGENT_ID`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentId(pub String);

impl From<&str> for AgentId {
    fn from(value: &str) -> Self {
        AgentId(value.to_owned())
    }
}

impl From<String> for AgentId {
    fn from(value: String) -> Self {
        AgentId(value)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Env-injected identity makes an `Agent`; discovery alone makes a `Human`. Never the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Caller {
    Human,
    Agent { id: AgentId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ctx {
    /// The repository's common `.git` directory, shared by every worktree of it.
    pub repo_path: PathBuf,
    /// The worktree root the action is requested from.
    pub worktree_path: PathBuf,
    pub caller: Caller,
}

impl Ctx {
    /// Derives the worktree root and the common git dir from any directory inside a worktree.
    /// Blocking: runs `git rev-parse` twice.
    pub fn from_cwd(cwd: &Path, caller: Caller) -> Result<Ctx, Error> {
        let worktree_path = jerry_git::worktree_root(cwd)?;
        let repo_path = jerry_git::git_common_dir(&worktree_path)?;
        Ok(Ctx {
            repo_path,
            worktree_path,
            caller,
        })
    }
}

#[cfg(test)]
mod ctx_tests {
    use super::{AgentId, Caller, Ctx};
    use std::fs;
    use test_support::seed_empty_repo;

    #[test]
    fn a_ctx_built_from_a_subdirectory_names_the_worktree_root_and_common_dir() {
        let repo = seed_empty_repo();
        let nested = repo.path().join("src").join("deep");
        fs::create_dir_all(&nested).expect("nested dir");

        let ctx = Ctx::from_cwd(&nested, Caller::Human).expect("inside a repo");

        let root = fs::canonicalize(repo.path()).expect("canonical root");
        assert_eq!(
            fs::canonicalize(&ctx.worktree_path).expect("canonical worktree"),
            root
        );
        assert_eq!(
            fs::canonicalize(&ctx.repo_path).expect("canonical git dir"),
            root.join(".git")
        );
        assert_eq!(ctx.caller, Caller::Human);
    }

    #[test]
    fn outside_a_repository_is_a_typed_git_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let err = Ctx::from_cwd(dir.path(), Caller::Human).expect_err("not a repo");
        assert_eq!(err.code, "git-command-failed");
    }

    #[test]
    fn the_caller_serializes_with_a_kind_tag() {
        let agent = Caller::Agent {
            id: AgentId::from("a-1"),
        };
        assert_eq!(
            serde_json::to_value(&agent).expect("serializable"),
            serde_json::json!({ "kind": "agent", "id": "a-1" })
        );
        assert_eq!(
            serde_json::to_value(Caller::Human).expect("serializable"),
            serde_json::json!({ "kind": "human" })
        );
    }
}
