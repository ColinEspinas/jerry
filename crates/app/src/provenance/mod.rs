//! Per-agent line provenance: which agent wrote each line of each file in a shared worktree
//! (GitHub issue #284).

pub mod change_set;
pub(crate) mod flow;
pub mod persist_state;
pub mod render;
pub mod store;

#[cfg(test)]
mod integration_tests;

/// The durable identity of one agent, as an author of lines.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentKey(String);

impl AgentKey {
    pub fn new(key: impl Into<String>) -> AgentKey {
        AgentKey(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Which CLI this agent is - parsed back out of the key
    /// (`<encoded worktree>|<kind label>|<spawn second>`), which is the only place the fact
    /// survives once the agent itself has closed. This is what turns a stored author into a
    /// colour and a name for GitHub issue #287's gutter bar and author chips.
    pub fn kind(&self) -> Option<crate::work_surface::agents::AgentKind> {
        let mut parts = self.0.rsplit('|');
        let _spawned_at = parts.next()?;
        let label = parts.next()?;
        // Nothing before the kind label means this is not a `baseline_key` at all, only something
        // shaped a bit like the tail of one.
        parts.next()?;
        crate::work_surface::agents::AgentKind::from_label(label)
    }
}

impl std::fmt::Display for AgentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The absolute path an [`crate::hooks::event::EditedFile`] names.
pub fn absolute_edit_path(edit: &crate::hooks::event::EditedFile) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(&edit.path);
    match (path.is_absolute(), edit.cwd.as_deref()) {
        (false, Some(cwd)) => std::path::PathBuf::from(cwd).join(path),
        _ => path,
    }
}

/// Who wrote a line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Author {
    /// A specific agent, by its durable key.
    Agent(AgentKey),
    /// The human, by their own hand. First-class, and deliberately **not** an agent: the whole
    /// point of Orca's second rule is that a hand edit is not "the AI, but different" - it is the
    /// one author whose lines are not an agent's work.
    You,
    /// Nobody on record. A line that was already there when Jerry started watching, an agent with
    /// no hook layer, or a file whose recorded state was invalidated. Rendered as unattributed,
    /// never guessed at.
    Unattributed,
}

impl Author {
    /// The agent that wrote this line, if it was an agent at all.
    pub fn agent(&self) -> Option<&AgentKey> {
        match self {
            Author::Agent(key) => Some(key),
            Author::You | Author::Unattributed => None,
        }
    }

    /// Whether this author is a real, named party - i.e. anything the UI can put a chip on.
    /// [`Author::Unattributed`] is the absence of an answer, not an answer.
    pub fn is_known(&self) -> bool {
        !matches!(self, Author::Unattributed)
    }
}

/// Added/removed line counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiffStat {
    pub added: u32,
    pub removed: u32,
}

impl DiffStat {
    pub fn new(added: u32, removed: u32) -> DiffStat {
        DiffStat { added, removed }
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// Saturating so a pathological diff can never wrap a counter into a smaller number - the
    /// caps in `wt_core::diff` (300 files, 2000 hunk lines per file) make overflow unreachable in
    /// practice, and a saturated total is still an honest "at least this many".
    pub fn plus(self, other: DiffStat) -> DiffStat {
        DiffStat {
            added: self.added.saturating_add(other.added),
            removed: self.removed.saturating_add(other.removed),
        }
    }
}

impl std::ops::AddAssign for DiffStat {
    fn add_assign(&mut self, rhs: DiffStat) {
        *self = self.plus(rhs);
    }
}
