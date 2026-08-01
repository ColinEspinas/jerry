//! Pure, GPUI-free command-pattern undo/redo stack (Revision R10) for the two real worktree-
//! level actions this app can now undo: "keep all changes" (`wt_core::undo::commit_all_changes`)
//! and "discard worktree" (`wt_core::undo::discard_worktree`). Mirrors `crate::palette::state`/
//! `crate::work_surface::state`'s own split: this module only holds already-computed outcomes/snapshots
//! and a cursor over them, so the stack's own push/undo/redo/clear-redo-on-new-action semantics
//! are directly unit-testable without a live GPUI window or a real git repository. Turning a
//! click or a keybinding into a real, background `wt_core::undo::*` call (and pushing its real
//! result here) happens one layer up, in `crate::worktree_history::flow`, which owns the
//! `Context<AdeApp>` and background executor those need.
//!
//! ## Cursor, not two `Vec`s
//!
//! [`UndoStack`] is one `Vec<UndoEntry>` plus a `cursor`: entries `[0..cursor)` are currently
//! *applied*, entries `[cursor..)` are currently *undone* (available to redo). This is the same
//! shape most editors' undo stacks use internally, and it makes "push a new action" trivially
//! correct: [`UndoStack::push`] truncates everything from `cursor` onward *before* appending, so
//! a fresh action always discards whatever redo tail existed - the standard "a new edit clears
//! the redo stack" rule, verified in this module's own tests.
//!
//! ## What this stack does *not* do
//!
//! It never runs a git operation itself - [`UndoEntry::action`] only ever holds an already-
//! computed `wt_core::undo` outcome/snapshot. [`UndoStack::commit_undo`]/[`UndoStack::commit_redo`]
//! must only be called by the caller *after* the real `wt_core::undo::undo_commit_all_changes`/
//! `undo_discard_worktree` call it corresponds to has actually succeeded - moving the cursor
//! speculatively before the real operation completes would let the History palette group and
//! the Keybindings page both show a state that isn't real yet. See
//! `crate::worktree_history::flow`'s own docs for the async completion-handler discipline this
//! implies (the same "only mutate `AdeApp` state from inside `this.update(cx, ..)` once a
//! background task actually resolves" rule every other real git-backed action in this app
//! already follows).

use std::path::{Path, PathBuf};

/// One real, already-performed worktree-level action this app can undo/redo. Each variant
/// carries exactly what its matching `wt_core::undo` undo/redo function needs - never
/// re-derived, since re-deriving "what branch/commit was this" from current, possibly-changed
/// state would defeat the whole point of the identity guards those functions already carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoableAction {
    /// A real `wt_core::undo::commit_all_changes` call. Undo/redo itself is keyed entirely off
    /// `worktree_path`/`outcome`, and works the same whether or not the agent tab that
    /// triggered this still exists.
    KeptAllChanges {
        worktree_path: PathBuf,
        outcome: wt_core::undo::CommitAllChangesOutcome,
    },
    /// A real `wt_core::undo::discard_worktree` call. `repo_path` is recreation context;
    /// `snapshot` is replaced in place by [`UndoStack::replace_current_redo_snapshot`] when this
    /// entry's discard is redone (a fresh `discard_worktree` call, not a replay of the original -
    /// see that method's own docs).
    DiscardedWorktree {
        repo_path: PathBuf,
        worktree_path: PathBuf,
        snapshot: wt_core::undo::DiscardSnapshot,
    },
}

impl UndoableAction {
    /// The worktree this action affected - both variants carry one directly, since both
    /// `wt_core::undo` calls they wrap are always scoped to a single worktree path. Used as real
    /// display context (e.g. `crate::root::AdeApp::build_palette_groups`'s History row
    /// `secondary` line - see `crate::palette::state::HistoryCandidate`'s own docs) distinct from
    /// [`UndoEntry::description`], which already names the branch.
    pub fn worktree_path(&self) -> &Path {
        match self {
            UndoableAction::KeptAllChanges { worktree_path, .. } => worktree_path,
            UndoableAction::DiscardedWorktree { worktree_path, .. } => worktree_path,
        }
    }
}

/// One entry in the [`UndoStack`]: a real [`UndoableAction`] and a human-readable description for
/// the History palette group/status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub action: UndoableAction,
    pub description: String,
}

/// The real undo/redo stack - see this module's own docs for the cursor model.
#[derive(Debug, Default)]
pub struct UndoStack {
    entries: Vec<UndoEntry>,
    /// Entries `[0..cursor)` are applied; `[cursor..)` are undone.
    cursor: usize,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a freshly-performed action as the new top of the stack. Any existing redo tail
    /// (`[cursor..)`) is discarded first - the standard "a new action clears the redo stack"
    /// rule.
    pub fn push(&mut self, action: UndoableAction, description: String) {
        self.entries.truncate(self.cursor);
        self.entries.push(UndoEntry {
            action,
            description,
        });
        self.cursor = self.entries.len();
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.entries.len()
    }

    /// The entry a call to undo would act on right now, without moving the cursor - a caller
    /// reads this to decide *what* real `wt_core::undo` call to run, then calls
    /// [`Self::commit_undo`] only once that real call has actually succeeded.
    pub fn peek_undo(&self) -> Option<&UndoEntry> {
        self.cursor.checked_sub(1).and_then(|i| self.entries.get(i))
    }

    /// The entry a call to redo would act on right now - see [`Self::peek_undo`]'s own docs for
    /// why this doesn't move the cursor itself.
    pub fn peek_redo(&self) -> Option<&UndoEntry> {
        self.entries.get(self.cursor)
    }

    /// Moves the cursor back one, marking [`Self::peek_undo`]'s entry as undone. Must only be
    /// called after the real git-level undo it corresponds to has actually succeeded - see this
    /// module's own docs.
    pub fn commit_undo(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    /// Moves the cursor forward one, marking [`Self::peek_redo`]'s entry as applied again - see
    /// [`Self::commit_undo`]'s own docs for the "only after the real op succeeded" rule this
    /// mirrors.
    pub fn commit_redo(&mut self) {
        if self.cursor < self.entries.len() {
            self.cursor += 1;
        }
    }

    /// Replaces [`Self::peek_redo`]'s entry's [`UndoableAction::DiscardedWorktree`] snapshot in
    /// place, just before [`Self::commit_redo`] is called for it - used when redoing a discard,
    /// which is always a *fresh* `wt_core::undo::discard_worktree` call against whatever the
    /// undo actually recreated (not a replay of the original snapshot, which may no longer
    /// describe real, current content) - see `crate::worktree_history::flow::AdeApp::
    /// perform_redo`'s own docs. A no-op if the current redo entry isn't a
    /// [`UndoableAction::DiscardedWorktree`] (defensive; never expected to actually happen given
    /// how the caller drives this).
    pub fn replace_current_redo_snapshot(&mut self, snapshot: wt_core::undo::DiscardSnapshot) {
        if let Some(entry) = self.entries.get_mut(self.cursor) {
            if let UndoableAction::DiscardedWorktree {
                snapshot: existing, ..
            } = &mut entry.action
            {
                *existing = snapshot;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kept(commit: &str, parent: Option<&str>) -> UndoableAction {
        UndoableAction::KeptAllChanges {
            worktree_path: PathBuf::from("/tmp/wt"),
            outcome: wt_core::undo::CommitAllChangesOutcome {
                branch: Some("feature".to_string()),
                commit: commit.to_string(),
                parent: parent.map(str::to_string),
            },
        }
    }

    fn discarded() -> UndoableAction {
        UndoableAction::DiscardedWorktree {
            repo_path: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt2"),
            snapshot: wt_core::undo::DiscardSnapshot {
                branch: Some("other".to_string()),
                commit: "deadbeef".to_string(),
                stash: None,
                had_ignored_content: false,
            },
        }
    }

    #[test]
    fn a_fresh_stack_has_nothing_to_undo_or_redo() {
        let stack = UndoStack::new();
        assert!(!stack.can_undo());
        assert!(!stack.can_redo());
        assert!(stack.peek_undo().is_none());
        assert!(stack.peek_redo().is_none());
    }

    #[test]
    fn pushing_makes_it_undoable_and_not_redoable() {
        let mut stack = UndoStack::new();
        stack.push(kept("c1", Some("p1")), "Kept all changes".to_string());
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
        assert_eq!(stack.peek_undo().unwrap().description, "Kept all changes");
    }

    #[test]
    fn undo_then_redo_round_trips_back_to_the_same_applied_state() {
        let mut stack = UndoStack::new();
        stack.push(kept("c1", None), "first".to_string());
        stack.commit_undo();
        assert!(!stack.can_undo());
        assert!(stack.can_redo());
        assert_eq!(stack.peek_redo().unwrap().description, "first");

        stack.commit_redo();
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
    }

    #[test]
    fn a_new_action_after_an_undo_clears_the_redo_stack() {
        let mut stack = UndoStack::new();
        stack.push(kept("c1", None), "first".to_string());
        stack.commit_undo();
        assert!(stack.can_redo());

        stack.push(kept("c2", None), "second".to_string());
        assert!(
            !stack.can_redo(),
            "a fresh action must discard the old redo tail, not sit alongside it"
        );
        assert!(stack.can_undo());
        assert_eq!(stack.peek_undo().unwrap().description, "second");
    }

    #[test]
    fn multiple_undo_and_redo_calls_walk_the_stack_in_order() {
        let mut stack = UndoStack::new();
        stack.push(kept("c1", None), "one".to_string());
        stack.push(kept("c2", None), "two".to_string());
        stack.push(kept("c3", None), "three".to_string());

        assert_eq!(stack.peek_undo().unwrap().description, "three");
        stack.commit_undo();
        assert_eq!(stack.peek_undo().unwrap().description, "two");
        stack.commit_undo();
        assert_eq!(stack.peek_undo().unwrap().description, "one");
        stack.commit_undo();
        assert!(!stack.can_undo());

        assert_eq!(stack.peek_redo().unwrap().description, "one");
        stack.commit_redo();
        assert_eq!(stack.peek_redo().unwrap().description, "two");
        stack.commit_redo();
        assert_eq!(stack.peek_redo().unwrap().description, "three");
        stack.commit_redo();
        assert!(!stack.can_redo());
    }

    #[test]
    fn commit_undo_and_commit_redo_never_move_the_cursor_out_of_bounds() {
        let mut stack = UndoStack::new();
        // No entries at all - both must be harmless no-ops, not a panic/underflow.
        stack.commit_undo();
        stack.commit_redo();
        assert!(!stack.can_undo());
        assert!(!stack.can_redo());

        stack.push(kept("c1", None), "one".to_string());
        stack.commit_redo(); // already fully applied - must stay a no-op
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
        stack.commit_undo();
        stack.commit_undo(); // already fully undone - must stay a no-op
        assert!(!stack.can_undo());
    }

    #[test]
    fn replace_current_redo_snapshot_only_touches_a_discarded_worktree_entry_at_the_cursor() {
        let mut stack = UndoStack::new();
        stack.push(discarded(), "discarded one".to_string());
        stack.commit_undo();
        assert!(stack.can_redo());

        let fresh = wt_core::undo::DiscardSnapshot {
            branch: Some("other".to_string()),
            commit: "newsha".to_string(),
            stash: Some("stashsha".to_string()),
            had_ignored_content: false,
        };
        stack.replace_current_redo_snapshot(fresh.clone());

        let UndoableAction::DiscardedWorktree { snapshot, .. } = &stack.peek_redo().unwrap().action
        else {
            panic!("expected a DiscardedWorktree entry");
        };
        assert_eq!(*snapshot, fresh);
    }

    #[test]
    fn replace_current_redo_snapshot_is_a_no_op_when_there_is_nothing_to_redo() {
        let mut stack = UndoStack::new();
        // Nothing pushed at all - must not panic.
        stack.replace_current_redo_snapshot(wt_core::undo::DiscardSnapshot {
            branch: None,
            commit: "x".to_string(),
            stash: None,
            had_ignored_content: false,
        });
        assert!(!stack.can_redo());
    }
}
