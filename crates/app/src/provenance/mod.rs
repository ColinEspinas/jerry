//! Per-agent line provenance: which agent wrote each line of each file in a shared worktree
//! (GitHub issue #284).
//!
//! [`store`], [`change_set`], [`persist_state`] and [`flow`] are the backend half (GitHub issue
//! #284): who wrote which line, and nothing about how it looks. [`render`] is the visible half
//! (GitHub issue #287) - the gutter tints, the author chips, the `⚠` shared-file ring and the
//! per-author filter - and it is the *only* module here that knows a colour or a glyph, so the
//! backend stays as testable without a window as it was the day it was written.
//!
//! ## What this answers, and why it is not blame
//!
//! `wt_core::blame` already answers *which commit introduced this line*, and collapses every
//! uncommitted line into a single `is_uncommitted` bucket. That is the right answer to a
//! different question. In a worktree with two agents running, the interesting fact about a dirty
//! line is not "nobody has committed it" - it is **which of them wrote it**, and git has no
//! record of that at all, because both agents write through the same working tree as the same OS
//! user.
//!
//! The competitive audit (`design_handoff_jerry_ade/revision 5/AUDIT-2026-08-13-competitive-v2.md`
//! §3.1) found the closest shipped mechanism to be binary - AI or human - and concluded that
//! "per-agent attribution inside a shared worktree is genuinely unclaimed… the one thing Jerry's
//! architecture is uniquely positioned to do". This folder is that mechanism.
//!
//! ## The model, and its four rules
//!
//! `REVISION-2026-08-14.md` §1 states the model as an extension of Orca's, keeping "Orca's two
//! hard-won rules". All four rules below are load-bearing, and each has its own test:
//!
//! 1. **Attribution is per line, and it is recorded from real edit events.** An agent's edit
//!    arrives as a Claude Code `PostToolUse` hook fact naming the file it just wrote
//!    (`crate::hooks::event::HookReport::edit`, GitHub issue #239's signal). The store then reads
//!    the file, diffs it against the content it last saw, and hands the changed lines to that
//!    agent. An agent whose CLI has no hook layer (Codex today) simply never produces such an
//!    event, and its lines stay [`Author::Unattributed`] - degraded, never guessed.
//! 2. **`you` is a first-class author, deliberately not an agent.** Any change that is *not*
//!    attributable to a live agent's edit event - a save from Jerry's own editor, or anything
//!    else that moves the bytes - flips exactly those lines to [`Author::You`]. See
//!    [`store::WorktreeProvenance::record`].
//! 3. **Attribution is local and never committed.** Nothing in this folder writes into the
//!    worktree, the index, a commit message, a note, or a ref. The only durable artifact is
//!    [`persist_state`]'s sibling file next to `settings.toml`, exactly like every other
//!    persisted Jerry state. `store`'s own
//!    `a_commit_made_from_an_attributed_worktree_carries_no_attribution_artifacts` test proves
//!    this against a real `git log`/`git show`.
//! 4. **One row per path.** `REVISION-2026-08-14.md` §1, rule 1 of "Four rules that are easy to
//!    get wrong": *"A path appears once per worktree, however many agents touched it - `by:
//!    ['s3','s10']` with a combined diffstat, never two rows."* [`change_set`] is where that is
//!    enforced, and it is enforced structurally: the change set is keyed by path, so a second row
//!    for the same path cannot be represented.
//!
//! ## Layout
//!
//! Split the way every feature folder in this crate is split (see `crate::graph_view`'s own docs
//! for the convention):
//!
//! - [`store`] - the pure, GPUI-free, git-free provenance store: line authors, the removal
//!   ledger, and the line diff that carries an author across an edit.
//! - [`change_set`] - the join of a real `wt_core::diff::WorktreeDiff` with the store: one entry
//!   per path, carrying the de-duplicated author union and a per-author `split` that sums to the
//!   combined diffstat by construction.
//! - [`persist_state`] - the on-disk sibling file, so attribution survives a restart.
//! - [`flow`] - the `impl AdeApp` glue: draining the hook layer's edit log, recording a hand edit
//!   from Jerry's own editor, and building the current worktree's change set.
//! - [`render`] - the UI layer: one author to one tint, glyph and sentence, plus the chip group,
//!   the `⚠` ring and the per-author filter (GitHub issue #287).

pub mod change_set;
pub(crate) mod flow;
pub mod persist_state;
pub mod render;
pub mod store;

#[cfg(test)]
mod integration_tests;

/// The durable identity of one agent, as an author of lines.
///
/// This is *not* `crate::work_surface::agents::AgentId`: that is a per-window `u64` counter that
/// restarts at zero on every launch, so persisted attribution keyed by it would silently
/// re-attribute one agent's lines to an unrelated agent after a restart. The key used instead is
/// `crate::review::state::baseline_key` - `<encoded worktree>|<kind>|<spawn second>` - which is
/// this codebase's established durable agent identity, already keying `review-baselines.toml` and
/// `agent-status.toml`, and already resolvable back to a real past agent by
/// `crate::hooks::history`.
///
/// Kept as an opaque newtype rather than a bare `String` so a caller cannot accidentally pass a
/// display label, a session id or a path where an agent key belongs.
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
    ///
    /// Split from the **right**, not the left: the encoded worktree is `utf8:<path>` and a real
    /// path may legitimately contain `|`, so `split('|')` would find the wrong two segments for
    /// such a path. The last two are always the kind label and the spawn second.
    ///
    /// `None` for a key this build cannot read - a record persisted by a future version naming an
    /// agent kind that does not exist here, or a hand-edited state file. The UI renders that as
    /// *no* attribution rather than picking a colour for it: a wrong author is worse than an
    /// absent one, which is the same rule [`Author::Unattributed`] exists for.
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
///
/// A hook payload's `file_path` was absolute in every real capture, but a relative one is a real
/// shape too, and it is relative to the payload's own `cwd` - the directory `claude` was started
/// in, which is not necessarily the worktree root. Resolving it here rather than at each call site
/// keeps the two places that need it (the listener thread, which takes the "before" snapshot, and
/// the drain, which records against it) from ever resolving it differently.
pub fn absolute_edit_path(edit: &crate::hooks::event::EditedFile) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(&edit.path);
    match (path.is_absolute(), edit.cwd.as_deref()) {
        (false, Some(cwd)) => std::path::PathBuf::from(cwd).join(path),
        _ => path,
    }
}

/// Who wrote a line.
///
/// The derived `Ord` is the display order the UI wants and is relied on by
/// [`change_set::ChangeSetEntry::authors`]: agents first (in stable key order), then `you`, then
/// whatever nobody claims. It is a deliberate part of the type, not an accident of variant order.
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
///
/// `wt_core::diff::DiffFile` carries no stored counters (`crate::sidebar::changes::diff_file_stats`
/// recomputes them from hunk lines every time), so this is the first stored diffstat in the
/// codebase - it exists because [`change_set`] must be able to state, as a checked invariant, that
/// the per-author shares sum to the combined total.
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
