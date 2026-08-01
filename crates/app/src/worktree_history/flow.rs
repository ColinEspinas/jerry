//! Real backing for the work surface footer's `Keep all`/`Discard worktree` buttons
//! ([`work_surface::ActionKind::KeepAllChanges`]/[`work_surface::ActionKind::DiscardWorktree`])
//! and the app-wide `Undo`/`Redo` command-pattern stack ([`undo::UndoStack`]) that makes both
//! genuinely reversible (Revision R10).
//!
//! ## One in-flight flag for all four operations
//!
//! [`AdeApp::worktree_history_op_in_flight`] serializes "keep all changes", "discard worktree",
//! `Undo`, and `Redo`: a click/keystroke for any of the four while the flag is `true` is a no-op,
//! mirroring [`AdeApp::prune_in_flight`]'s own single-flag-per-feature precedent
//! (`crate::rail::render`). This is a deliberate simplification of this project's usual
//! task-slot/generation-guard discipline, not a skip of it: because there can never be more than
//! one of these four background operations in flight at a time, there is no "a slow undo/redo op
//! races a newer one" scenario left to guard against with a separate generation counter - full
//! mutual exclusion already makes it structurally impossible. Every completion handler still
//! only mutates [`AdeApp`] state from inside `this.update`/`this.update_in`, after its real
//! `wt_core::undo::*` call has actually resolved, matching every other real git-backed action in
//! this app.
//!
//! ## Undo/redo never depends on the originating agent tab still existing
//!
//! [`undo::UndoableAction`] carries the worktree path/branch/repo path/outcome its
//! `wt_core::undo` call needs directly - `agent_id` is display context only. This matters
//! concretely for discard: [`AdeApp::execute_discard_worktree`] closes the originating agent
//! tab on success (its cwd no longer exists), and undo/redo must still work correctly with that
//! tab gone. The trade-off this implies is real and worth stating plainly: undoing a discard
//! restores the worktree and its content, but **not** the closed agent tab - out of this
//! revision's scope (a worktree-level undo, not an agent-level one).
//!
//! ## Redoing a discard is a fresh discard, not a replay
//!
//! Undoing "discard worktree" can leave the recreated worktree in a state that isn't byte-
//! identical to what was originally discarded (a stash-apply conflict leaves real conflict
//! markers instead of clean content - see [`wt_core::undo::UndoDiscardOutcome`]'s own docs).
//! Replaying the *original* [`wt_core::undo::DiscardSnapshot`] on redo would silently discard
//! whatever the recreated worktree actually contains now. [`AdeApp::perform_redo`] instead runs
//! a fresh `wt_core::undo::discard_worktree` against the real, current worktree and replaces the
//! stack entry's snapshot with that real result
//! ([`undo::UndoStack::replace_current_redo_snapshot`]) before advancing the cursor - the same
//! "never trust stale recorded state when live ground truth is cheap to re-derive" discipline
//! this project's other identity guards already use, applied here to keep a subsequent `Undo` of
//! that redo acting on what's actually there.

use super::*;

/// Which of the four real, mutually-exclusive-in-flight operations
/// [`AdeApp::worktree_history_op_in_flight`] currently names - see that field's own docs for why
/// this is a real, named kind rather than a bare `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeHistoryOpKind {
    Keep,
    Discard,
    Undo,
    Redo,
}

/// Whether `a` and `b` refer to the same real path, canonicalizing both sides first - mirrors
/// `wt_core::undo::is_main_worktree`'s own canonicalization (see its docs) so a relative,
/// symlinked, or otherwise differently-spelled path still matches correctly rather than silently
/// failing closed. Falls back to a raw comparison if either side can't be canonicalized (e.g. it
/// no longer exists) - the same fallback `is_main_worktree` itself uses.
fn canonical_paths_match(a: &Path, b: &Path) -> bool {
    let a_canon = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b_canon = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a_canon == b_canon
}

impl AdeApp {
    /// Looks up worktree `path`'s branch name for display (History/status-line text) - falls
    /// back to the path itself if the worktree list doesn't (yet, or any more) have an entry for
    /// it. Display-only: never consulted by a real `wt_core::undo::*` call.
    fn branch_display_for(&self, path: &Path) -> String {
        self.worktrees
            .iter()
            .find(|item| item.path == path)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Refreshes worktree/diff state after a real git mutation (keep/discard/undo/redo all
    /// change what's on disk) - the same `load_worktrees` + `load_diff` pair
    /// `Self::complete_merge_flow`'s own success arm already uses for the identical reason.
    fn refresh_after_worktree_history_op(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.focused_repo_path();
        self.load_worktrees(cx);
        self.load_diff(repo_path, cx);
    }

    /// The Review footer's `Keep all` action - a real, undoable `wt_core::undo::commit_all_changes`
    /// on agent `id`'s worktree. Not gated by any confirmation (unlike
    /// [`Self::request_discard_worktree`]): it's non-destructive and immediately undoable via
    /// `Undo`.
    pub(crate) fn keep_all_changes(&mut self, id: AgentId, cx: &mut Context<Self>) {
        if self.worktree_history_op_in_flight.is_some() {
            return;
        }
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree_path = agent.cwd.clone();
        let branch_display = self.branch_display_for(&worktree_path);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.worktree_history_op_in_flight = Some(WorktreeHistoryOpKind::Keep);
        self.worktree_history_status =
            Some(format!("keeping all changes in {branch_display}\u{2026}"));
        cx.notify();

        let message = format!("ade: keep all changes ({branch_display})");
        let worktree_path_bg = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { wt_core::undo::commit_all_changes(&worktree_path_bg, &message) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(outcome) => {
                        this.worktree_history_status =
                            Some(format!("kept all changes in {branch_display}"));
                        this.undo_stack.push(
                            undo::UndoableAction::KeptAllChanges {
                                worktree_path,
                                outcome,
                            },
                            format!("Kept all changes ({branch_display})"),
                        );
                        this.refresh_after_worktree_history_op(cx);
                    }
                    Err(err) => {
                        this.worktree_history_status =
                            Some(format!("keep all changes failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
    }

    /// The Review/Fail footer's `Discard worktree` action - two-click confirmation (mirroring
    /// [`Self::request_prune`]'s own reasoning: this is a real, destructive-*feeling* action -
    /// it force-removes a worktree directory - even though [`wt_core::undo::discard_worktree`]
    /// makes it genuinely recoverable via `Undo` now). The first click only arms
    /// [`AdeApp::discard_confirm_armed`] for `id`; a second click on the *same* agent's button
    /// while armed actually runs it.
    pub(crate) fn request_discard_worktree(
        &mut self,
        id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_history_op_in_flight.is_some() {
            return;
        }
        if self.discard_confirm_armed != Some(id) {
            self.discard_confirm_armed = Some(id);
            cx.notify();
            return;
        }
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.execute_discard_worktree(id, window, cx);
    }

    /// Runs the real, already-confirmed discard - only ever reached through
    /// [`Self::request_discard_worktree`]'s second click. `_window` is accepted (not read
    /// directly) purely to match every other footer-button click handler's signature - the real
    /// `Window` [`Self::close_agent`] needs on success is obtained fresh, asynchronously, via
    /// `update_in` in the completion handler below (see this module's own docs).
    pub(in crate::worktree_history) fn execute_discard_worktree(
        &mut self,
        id: AgentId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_history_op_in_flight.is_some() {
            return;
        }
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree_path = agent.cwd.clone();
        let repo_path = self.focused_repo_path();
        let branch_display = self.branch_display_for(&worktree_path);
        self.worktree_history_op_in_flight = Some(WorktreeHistoryOpKind::Discard);
        self.worktree_history_status = Some(format!("discarding {branch_display}\u{2026}"));
        cx.notify();

        let repo_path_bg = repo_path.clone();
        let worktree_path_bg = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        wt_core::undo::discard_worktree(&repo_path_bg, &worktree_path_bg)
                    })
                    .await;
            // `update_in` (not plain `update`): a successful discard closes the now-cwd-less
            // agent tab, and `Self::close_agent` needs a real `Window` to move focus off it -
            // see `vendor/zed/crates/gpui/src/app/async_context.rs`'s `AsyncApp::with_window`,
            // the same mechanism `Self::trigger_goto_definition`'s own completion handler already
            // relies on for the identical reason.
            let _ = this.update_in(cx, |this, window, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(snapshot) => {
                        // `wt_core::undo::DiscardSnapshot::had_ignored_content` is a real,
                        // honest signal that some real content (gitignored files) was *not*
                        // preserved by the stash - surfaced here rather than left computed but
                        // silently unread, which would defeat the entire point of that field.
                        this.worktree_history_status = Some(if snapshot.had_ignored_content {
                            format!(
                                "discarded {branch_display} \u{2014} note: gitignored files \
                                 (build output, .env, ...) were not preserved and are gone"
                            )
                        } else {
                            format!("discarded {branch_display}")
                        });
                        this.undo_stack.push(
                            undo::UndoableAction::DiscardedWorktree {
                                repo_path,
                                worktree_path,
                                snapshot,
                            },
                            format!("Discarded worktree ({branch_display})"),
                        );
                        // The agent's cwd no longer exists - see this module's own docs on why
                        // the tab, unlike the worktree/content itself, is not restored by `Undo`.
                        this.close_agent(id, window, cx);
                        this.refresh_after_worktree_history_op(cx);
                    }
                    Err(err) => {
                        this.worktree_history_status = Some(format!("discard failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
    }

    pub(crate) fn handle_undo_action(
        &mut self,
        _action: &Undo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_undo(cx);
    }

    pub(crate) fn handle_redo_action(
        &mut self,
        _action: &Redo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform_redo(cx);
    }

    /// Undoes [`undo::UndoStack::peek_undo`]'s entry - the real, background
    /// `wt_core::undo::undo_commit_all_changes`/`undo_discard_worktree` call, under that
    /// function's own mandatory identity guard. The stack's cursor only moves
    /// ([`undo::UndoStack::commit_undo`]) once that call has actually succeeded - see this
    /// module's own docs.
    pub(crate) fn perform_undo(&mut self, cx: &mut Context<Self>) {
        if self.worktree_history_op_in_flight.is_some() {
            // Live-reproduced gap an audit caught: the palette hides History rows while busy
            // and the footer buttons are genuinely disabled, but a keybinding press (`mod+Z`)
            // reached this same early return with no status set and no `cx.notify()` - a real
            // "looks actionable, silently does nothing" case, exactly what this app's own rules
            // forbid elsewhere. A real status now makes the refusal visible here too.
            self.worktree_history_status =
                Some("an undo/redo/keep/discard is already running\u{2026}".to_string());
            cx.notify();
            return;
        }
        let Some(entry) = self.undo_stack.peek_undo() else {
            self.worktree_history_status = Some("nothing to undo".to_string());
            cx.notify();
            return;
        };
        let action = entry.action.clone();
        let description = entry.description.clone();
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.worktree_history_op_in_flight = Some(WorktreeHistoryOpKind::Undo);
        self.worktree_history_status =
            Some(format!("undoing \u{2018}{description}\u{2019}\u{2026}"));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    match &action {
                        undo::UndoableAction::KeptAllChanges {
                            worktree_path,
                            outcome,
                            ..
                        } => wt_core::undo::undo_commit_all_changes(worktree_path, outcome)
                            .map(|()| None),
                        undo::UndoableAction::DiscardedWorktree {
                            repo_path,
                            worktree_path,
                            snapshot,
                            ..
                        } => {
                            wt_core::undo::undo_discard_worktree(repo_path, worktree_path, snapshot)
                                .map(Some)
                        }
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match outcome {
                    Ok(discard_outcome) => {
                        this.undo_stack.commit_undo();
                        this.worktree_history_status = Some(match discard_outcome {
                            Some(wt_core::undo::UndoDiscardOutcome::RestoredWithConflicts {
                                stash,
                            }) => format!(
                                "undone: {description} \u{2014} the restored stash had real \
                                 conflicts, check the worktree (fallback stash: {stash})"
                            ),
                            _ => format!("undone: {description}"),
                        });
                        this.refresh_after_worktree_history_op(cx);
                    }
                    Err(err) => {
                        this.worktree_history_status = Some(format!("undo failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
    }

    /// Redoes [`undo::UndoStack::peek_redo`]'s entry. For a kept-all-changes entry this is a
    /// real, guarded `wt_core::undo::redo_commit_all_changes`. For a discarded-worktree entry
    /// this is a *fresh* `wt_core::undo::discard_worktree` against the real, current worktree -
    /// see this module's own docs for why redoing a discard is never a replay of the original
    /// snapshot.
    pub(crate) fn perform_redo(&mut self, cx: &mut Context<Self>) {
        if self.worktree_history_op_in_flight.is_some() {
            // See `Self::perform_undo`'s matching guard for why this sets a real status/notifies
            // rather than silently returning.
            self.worktree_history_status =
                Some("an undo/redo/keep/discard is already running\u{2026}".to_string());
            cx.notify();
            return;
        }
        let Some(entry) = self.undo_stack.peek_redo() else {
            self.worktree_history_status = Some("nothing to redo".to_string());
            cx.notify();
            return;
        };
        let action = entry.action.clone();
        let description = entry.description.clone();
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.worktree_history_op_in_flight = Some(WorktreeHistoryOpKind::Redo);
        self.worktree_history_status =
            Some(format!("redoing \u{2018}{description}\u{2019}\u{2026}"));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result: Result<RedoResult, String> = cx
                .background_executor()
                .spawn(async move {
                    match &action {
                        undo::UndoableAction::KeptAllChanges {
                            worktree_path,
                            outcome,
                            ..
                        } => wt_core::undo::redo_commit_all_changes(worktree_path, outcome)
                            .map(|()| RedoResult::Commit)
                            .map_err(|err| err.to_string()),
                        undo::UndoableAction::DiscardedWorktree {
                            repo_path,
                            worktree_path,
                            snapshot,
                            ..
                        } => {
                            // Mandatory identity guard, mirroring every other real mutation this
                            // feature makes (`wt_core::undo::undo_commit_all_changes`/
                            // `redo_commit_all_changes`/`undo_discard_worktree` all carry one) -
                            // `wt_core::undo::discard_worktree` itself has no way to know what
                            // branch/commit *should* be at `worktree_path` before re-discarding
                            // it, so this check belongs here, the one call site that redoes a
                            // discard. Without it, redoing a discard could force-remove a
                            // completely different worktree/branch that has since come to occupy
                            // the same path.
                            //
                            // Paths are canonicalized on both sides before comparing - mirroring
                            // `wt_core::undo::is_main_worktree`'s own canonicalization for the
                            // equivalent comparison (see its docs) - so a symlinked `/tmp`,
                            // symlinked home, or a relative-vs-absolute spelling mismatch doesn't
                            // silently fail this closed (an audit found this comparison used raw,
                            // non-canonicalized paths, unlike every other identity guard in this
                            // feature). For a detached-`HEAD` snapshot (`snapshot.branch` is
                            // `None`), branch alone can't distinguish worktrees (any detached
                            // worktree at this path would otherwise pass), so this also checks
                            // `snapshot.commit` against the real current `HEAD` commit.
                            let current_entry = wt_core::list_worktrees(repo_path)
                                .map_err(|err| err.to_string())?
                                .into_iter()
                                .flatten()
                                .find(|entry| canonical_paths_match(&entry.path, worktree_path));
                            let identity_matches = match &snapshot.branch {
                                Some(branch) => {
                                    current_entry
                                        .as_ref()
                                        .and_then(|entry| entry.branch.as_deref())
                                        == Some(branch.as_str())
                                }
                                None => {
                                    current_entry
                                        .as_ref()
                                        .and_then(|entry| entry.head_commit.as_deref())
                                        == Some(snapshot.commit.as_str())
                                }
                            };
                            if !identity_matches {
                                return Err(format!(
                                    "cannot redo: {} no longer matches the state it was \
                                     discarded in (expected branch {:?} at commit {}, found {:?}) \
                                     - something else changed it since",
                                    worktree_path.display(),
                                    snapshot.branch,
                                    snapshot.commit,
                                    current_entry.map(|entry| (entry.branch, entry.head_commit))
                                ));
                            }
                            wt_core::undo::discard_worktree(repo_path, worktree_path)
                                .map(RedoResult::Discard)
                                .map_err(|err| err.to_string())
                        }
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(RedoResult::Commit) => {
                        this.undo_stack.commit_redo();
                        this.worktree_history_status = Some(format!("redone: {description}"));
                        this.refresh_after_worktree_history_op(cx);
                    }
                    Ok(RedoResult::Discard(new_snapshot)) => {
                        // Same honest signal `Self::execute_discard_worktree`'s own success arm
                        // surfaces on the *first* discard - an audit found the redo arm silently
                        // dropped it, even though redoing a discard runs a real, fresh
                        // `wt_core::undo::discard_worktree` call (see this module's own docs) that
                        // can lose gitignored content exactly the same way the original did.
                        let had_ignored_content = new_snapshot.had_ignored_content;
                        this.undo_stack.replace_current_redo_snapshot(new_snapshot);
                        this.undo_stack.commit_redo();
                        this.worktree_history_status = Some(if had_ignored_content {
                            format!(
                                "redone: {description} \u{2014} note: gitignored files (build \
                                 output, .env, ...) were not preserved and are gone"
                            )
                        } else {
                            format!("redone: {description}")
                        });
                        this.refresh_after_worktree_history_op(cx);
                    }
                    Err(err) => {
                        this.worktree_history_status = Some(format!("redo failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
    }
}

/// [`AdeApp::perform_redo`]'s real per-variant result, before it's folded back into
/// [`AdeApp::undo_stack`] - a discard's redo carries a fresh snapshot to store
/// ([`undo::UndoStack::replace_current_redo_snapshot`]); a commit's redo carries nothing further.
enum RedoResult {
    Commit,
    Discard(wt_core::undo::DiscardSnapshot),
}

/// Real-git-repo, real-`TestAppContext` regression coverage, mirroring
/// `merge::flow::merge_regression_tests`/`rail::render::prune_regression_tests`' own
/// established idiom.
#[cfg(test)]
mod worktree_history_regression_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::{Entity, TestAppContext};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    /// Same linked-worktree idiom `merge::flow`/`rail::render`'s own test modules use.
    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    /// Spawns a real `Shell` agent (a real pty) in `cwd` and returns its id - the same
    /// `agents.spawn` call `AdeApp::new_agent` itself makes.
    fn spawn_agent(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        cwd: PathBuf,
    ) -> AgentId {
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Shell,
                cwd,
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn keep_all_changes_commits_a_dirty_worktree_and_pushes_a_real_undo_entry(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        let head_before = git_output(&feature, &["rev-parse", "HEAD"]);

        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()),
            "worktree_history_op_in_flight should be set synchronously by keep_all_changes"
        );
        cx.run_until_parked();

        assert!(!app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()));
        assert!(
            !wt_core::is_dirty(&feature).expect("is_dirty"),
            "the worktree must be clean after a real commit"
        );
        let head_after = git_output(&feature, &["rev-parse", "HEAD"]);
        assert_ne!(
            head_before, head_after,
            "a real new commit must have been made"
        );
        assert_eq!(
            fs::read_to_string(feature.join("new.txt")).expect("read"),
            "from feature\n"
        );

        app.read_with(cx, |app, _| {
            assert!(
                app.undo_stack.can_undo(),
                "a real undo entry must have been pushed"
            );
            let entry = app.undo_stack.peek_undo().expect("peek_undo");
            assert!(entry.description.contains("Kept all changes"));
            let undo::UndoableAction::KeptAllChanges { outcome, .. } = &entry.action else {
                panic!("expected a KeptAllChanges entry");
            };
            assert_eq!(outcome.commit, head_after);
        });
    }

    #[gpui::test]
    fn undo_then_redo_of_keep_all_changes_round_trips_through_real_git_state(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        let head_before = git_output(&feature, &["rev-parse", "HEAD"]);

        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();
        let head_after_keep = git_output(&feature, &["rev-parse", "HEAD"]);

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert_eq!(
            git_output(&feature, &["rev-parse", "HEAD"]),
            head_before,
            "undo must move HEAD back to the real pre-commit state"
        );
        assert!(
            wt_core::is_dirty(&feature).expect("is_dirty"),
            "undo must leave the worktree uncommitted again"
        );
        app.read_with(cx, |app, _| {
            assert!(!app.undo_stack.can_undo());
            assert!(app.undo_stack.can_redo());
        });

        app.update(cx, |app, cx| app.perform_redo(cx));
        cx.run_until_parked();
        assert_eq!(
            git_output(&feature, &["rev-parse", "HEAD"]),
            head_after_keep,
            "redo must move HEAD forward to the exact same real commit again"
        );
        app.read_with(cx, |app, _| {
            assert!(app.undo_stack.can_undo());
            assert!(!app.undo_stack.can_redo());
        });
    }

    #[gpui::test]
    fn undo_of_keep_all_changes_is_refused_and_reported_when_head_moved_since(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());

        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();
        let head_after_keep = git_output(&feature, &["rev-parse", "HEAD"]);

        // Something else commits on top before Undo runs - the real
        // `wt_core::undo::HeadMovedSinceRecorded` identity guard must refuse rather than
        // silently discard it.
        fs::write(feature.join("other.txt"), "other\n").expect("write");
        git(&feature, &["add", "other.txt"]);
        git(&feature, &["commit", "-m", "a later, unrelated commit"]);
        let head_after_unrelated = git_output(&feature, &["rev-parse", "HEAD"]);

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();

        assert_eq!(
            git_output(&feature, &["rev-parse", "HEAD"]),
            head_after_unrelated,
            "a refused undo must not have touched real HEAD at all"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.undo_stack.can_undo(),
                "the stack's cursor must not have moved on a refused undo"
            );
            let status = app.worktree_history_status.as_deref().unwrap_or("");
            assert!(
                status.contains("undo failed"),
                "the refusal must be reported honestly, not silently swallowed: {status:?}"
            );
        });
        assert_ne!(head_after_keep, head_after_unrelated);
    }

    #[gpui::test]
    fn a_second_keep_all_changes_while_the_first_is_in_flight_does_not_race_it(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature_a = add_worktree(repo.path(), "feature-a", "feature-a-wt");
        let feature_b = add_worktree(repo.path(), "feature-b", "feature-b-wt");
        fs::write(feature_a.join("a.txt"), "a\n").expect("write");
        fs::write(feature_b.join("b.txt"), "b\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id_a = spawn_agent(&app, cx, feature_a.clone());
        let id_b = spawn_agent(&app, cx, feature_b.clone());

        app.update(cx, |app, cx| app.keep_all_changes(id_a, cx));
        assert!(app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()));

        // A second, independent call while the first is genuinely still in flight (nothing has
        // parked the executor yet) must be a real no-op - not a second racing background task
        // overwriting `_worktree_history_task` and dropping the first mid-commit.
        app.update(cx, |app, cx| app.keep_all_changes(id_b, cx));

        cx.run_until_parked();

        assert!(!app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()));
        assert!(
            !wt_core::is_dirty(&feature_a).expect("is_dirty a"),
            "feature-a's real commit must have gone through"
        );
        assert!(
            wt_core::is_dirty(&feature_b).expect("is_dirty b"),
            "feature-b must be untouched - its call was a real no-op, not a second racing task"
        );
        app.read_with(cx, |app, _| {
            assert!(app.undo_stack.can_undo());
            assert!(
                !app.undo_stack.can_redo(),
                "exactly one entry should exist - the second call never ran"
            );
        });
    }

    #[gpui::test]
    fn discard_worktree_two_click_confirm_removes_it_and_closes_its_agent(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("scratch.txt"), "wip\n").expect("write untracked");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());

        // Click 1: arm.
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.discard_confirm_armed),
            Some(id)
        );
        assert!(feature.exists(), "the first click must not touch anything");

        // Click 2: confirm - runs the real discard.
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        assert!(app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()));
        cx.run_until_parked();

        assert!(
            !feature.exists(),
            "the worktree directory must really be gone"
        );
        assert!(
            app.read_with(cx, |app, _| app
                .agents
                .iter()
                .find(|s| s.id == id)
                .is_none()),
            "the now-cwd-less agent tab must have been closed"
        );
        app.read_with(cx, |app, _| {
            assert!(app.undo_stack.can_undo());
            let entry = app.undo_stack.peek_undo().expect("peek_undo");
            assert!(entry.description.contains("Discarded worktree"));
        });
    }

    /// Regression coverage for `wt_core::undo::DiscardSnapshot::had_ignored_content` actually
    /// being read, not just computed and left silently unused - the whole point of that field
    /// (this module's own docs) is telling the user honestly that something real was lost.
    #[gpui::test]
    fn discard_worktree_status_honestly_notes_when_gitignored_content_was_lost(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join(".gitignore"), "ignored.txt\n").expect("write");
        git(&feature, &["add", ".gitignore"]);
        git(&feature, &["commit", "-m", "add gitignore"]);
        fs::write(
            feature.join("ignored.txt"),
            "real content that will be lost\n",
        )
        .expect("write ignored file");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        cx.run_until_parked();

        let status = app
            .read_with(cx, |app, _| app.worktree_history_status.clone())
            .unwrap_or_default();
        assert!(
            status.contains("gitignored") || status.contains("not preserved"),
            "the status must honestly mention the real, lost gitignored content: {status:?}"
        );
    }

    /// Same regression as `discard_worktree_status_honestly_notes_when_gitignored_content_was_lost`,
    /// but for *redoing* a discard - an audit found the `RedoResult::Discard` completion arm
    /// silently dropped `had_ignored_content`, even though redoing a discard runs a real, fresh
    /// `wt_core::undo::discard_worktree` call (see this module's own docs on "redoing a discard is
    /// a fresh discard, not a replay") that can lose gitignored content exactly the same way the
    /// original discard did.
    #[gpui::test]
    fn redo_of_discard_also_honestly_notes_when_gitignored_content_was_lost(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join(".gitignore"), "ignored.txt\n").expect("write");
        git(&feature, &["add", ".gitignore"]);
        git(&feature, &["commit", "-m", "add gitignore"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        cx.run_until_parked();
        assert!(!feature.exists());

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert!(feature.exists());

        // Real gitignored content is present in the *recreated* worktree before redo re-discards
        // it - the original discard had none, so only the redo's own fresh snapshot can catch
        // this.
        fs::write(
            feature.join("ignored.txt"),
            "real content that will be lost again\n",
        )
        .expect("write ignored file");

        app.update(cx, |app, cx| app.perform_redo(cx));
        cx.run_until_parked();
        assert!(
            !feature.exists(),
            "the redo must have really re-discarded the worktree"
        );

        let status = app
            .read_with(cx, |app, _| app.worktree_history_status.clone())
            .unwrap_or_default();
        assert!(
            status.contains("gitignored") || status.contains("not preserved"),
            "the redo status must honestly mention the real, lost gitignored content too, not \
             silently drop it: {status:?}"
        );
    }

    #[gpui::test]
    fn undo_of_discard_recreates_the_worktree_with_stash_content_restored_but_not_the_agent(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("scratch.txt"), "wip\n").expect("write untracked");
        fs::write(feature.join("base.txt"), "edited\n").expect("modify tracked");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());

        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        cx.run_until_parked();
        assert!(!feature.exists());

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();

        assert!(feature.exists(), "the worktree must be recreated");
        assert_eq!(
            fs::read_to_string(feature.join("scratch.txt")).expect("read restored untracked"),
            "wip\n"
        );
        assert_eq!(
            fs::read_to_string(feature.join("base.txt")).expect("read restored tracked"),
            "edited\n"
        );
        assert!(
            app.read_with(cx, |app, _| app
                .agents
                .iter()
                .find(|s| s.id == id)
                .is_none()),
            "the closed agent tab is deliberately not restored by undo - see this module's \
             own docs"
        );
        app.read_with(cx, |app, _| {
            assert!(!app.undo_stack.can_undo());
            assert!(app.undo_stack.can_redo());
        });
    }

    /// Proves the "fresh discard, not a replay" behaviour with real, distinguishable content
    /// rather than just comparing stash ids: two `git stash push` calls capturing *identical*
    /// content (same tree, same parent, same 1-second-resolution timestamp) can legitimately
    /// hash to the exact same real stash commit id - content-addressed objects are supposed to
    /// do that, live-reproduced while first writing this test with byte-identical content on
    /// both sides. So instead this changes the worktree's real content *between* the undo and
    /// the redo, then undoes the redo too and checks the restored content is the *new* value,
    /// not the stale original - a genuinely stronger, content-based proof that redo re-snapshot
    /// what was really there rather than replaying stale state.
    #[gpui::test]
    fn redo_of_discard_re_discards_the_recreated_worktrees_real_current_content(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("scratch.txt"), "wip v1\n").expect("write untracked");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert!(feature.exists());
        assert_eq!(
            fs::read_to_string(feature.join("scratch.txt")).expect("read"),
            "wip v1\n"
        );

        // Real, different content now, before redo re-discards it.
        fs::write(feature.join("scratch.txt"), "wip v2\n").expect("overwrite untracked");

        app.update(cx, |app, cx| app.perform_redo(cx));
        cx.run_until_parked();
        assert!(
            !feature.exists(),
            "the redo must have really re-discarded the worktree"
        );

        // Undo the redo: the restored content must be the *new* v2 content, proving the redo's
        // snapshot was taken fresh from what was really there, not replayed from the original
        // discard's now-stale v1 snapshot.
        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert!(feature.exists());
        assert_eq!(
            fs::read_to_string(feature.join("scratch.txt")).expect("read after undoing the redo"),
            "wip v2\n",
            "redoing a discard must snapshot the worktree's real, current content - not replay \
             the original discard's now-stale snapshot (see this module's own docs)"
        );
    }

    #[gpui::test]
    fn a_new_keep_all_changes_after_an_undo_clears_the_redo_stack(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("first.txt"), "first\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());

        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();
        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.undo_stack.can_redo()));

        fs::write(feature.join("second.txt"), "second\n").expect("write");
        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.undo_stack.can_undo());
            assert!(
                !app.undo_stack.can_redo(),
                "a fresh action must clear the old redo tail"
            );
        });
    }

    /// Regression coverage for a real identity-guard gap an audit found: unlike every other
    /// mutation in this feature, redoing a discard had no check at all that the worktree
    /// currently sitting at the recorded path is still the one that was discarded - before this
    /// fix, redoing after something else occupied that path with a different branch would have
    /// force-removed the unrelated worktree.
    #[gpui::test]
    fn redo_of_discard_refuses_when_the_path_was_reoccupied_by_a_different_branch_since_the_undo(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("scratch.txt"), "wip\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        app.update_in(cx, |app, window, cx| {
            app.request_discard_worktree(id, window, cx)
        });
        cx.run_until_parked();
        assert!(!feature.exists());

        app.update(cx, |app, cx| app.perform_undo(cx));
        cx.run_until_parked();
        assert!(feature.exists());

        // Something else removes the recreated worktree and puts a genuinely different branch
        // at the exact same path before Redo runs.
        std::process::Command::new("git")
            .current_dir(repo.path())
            .args([
                "worktree",
                "remove",
                "--force",
                feature.to_str().expect("utf8"),
            ])
            .output()
            .expect("remove the recreated worktree");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "unrelated-branch",
                feature.to_str().expect("utf8"),
            ],
        );
        fs::write(feature.join("unrelated.txt"), "not the same worktree\n").expect("write");

        app.update(cx, |app, cx| app.perform_redo(cx));
        cx.run_until_parked();

        assert!(
            feature.exists(),
            "a refused redo must not have force-removed the unrelated worktree now at this path"
        );
        assert!(
            feature.join("unrelated.txt").exists(),
            "the unrelated worktree's real content must be completely untouched"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.undo_stack.can_redo(),
                "a refused redo must not have advanced the cursor"
            );
            let status = app.worktree_history_status.as_deref().unwrap_or("");
            assert!(
                status.contains("redo failed"),
                "the refusal must be reported honestly: {status:?}"
            );
        });
    }

    /// Regression coverage for a real bug an audit caught: the busy label was keyed off the
    /// bare in-flight flag alone, so undoing a "keep all changes" made every visible
    /// `Discard worktree` button (on a *different* agent) falsely read "discarding…". Only
    /// asserts the real, specific [`WorktreeHistoryOpKind`] value itself, not the rendered button
    /// label text (`work_surface_render.rs`'s `label` match arms, ~1299-1318, are what actually
    /// consume this value to pick "discarding…"/"keeping…" vs. the unrelated button's default
    /// label - not exercised here) - named accordingly, an audit found the previous name
    /// (`..._so_an_unrelated_button_never_shows_the_wrong_busy_label`) overclaimed render
    /// coverage this test doesn't actually provide.
    #[gpui::test]
    fn the_in_flight_kind_is_tracked_as_a_specific_enum_value_not_a_bare_bool(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("first.txt"), "first\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();

        // Undo is now the in-flight kind - synchronously, before the executor runs it.
        app.update(cx, |app, cx| app.perform_undo(cx));
        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight),
            Some(WorktreeHistoryOpKind::Undo),
            "sanity check: Undo must be the real, specific in-flight kind right now"
        );
        assert_ne!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight),
            Some(WorktreeHistoryOpKind::Discard),
            "an in-flight Undo must never be mistaken for an in-flight Discard - this is \
             exactly the state that used to make an unrelated Discard button read \
             \"discarding…\""
        );
        cx.run_until_parked();
    }

    /// Regression coverage for a real bug an audit caught: a keybinding-triggered `Undo`/`Redo`
    /// (`mod+Z`/`mod+shift+Z`) hit the same `worktree_history_op_in_flight.is_some()` early
    /// return as every other entry point into this feature, but - unlike the palette (which
    /// hides History rows while busy) and the footer (whose buttons are genuinely disabled) -
    /// used to return with no status set and no `cx.notify()` at all: a keybinding press that
    /// looked like it should do something but silently vanished.
    ///
    /// Calls [`AdeApp::handle_undo_action`]/[`AdeApp::handle_redo_action`] directly - the exact,
    /// real methods `root/mod.rs`'s `.on_action(cx.listener(Self::handle_undo_action))`/
    /// `.on_action(cx.listener(Self::handle_redo_action))` route a real `mod+Z`/`mod+shift+Z`
    /// keybinding to, not a re-implementation of them - rather than a full, simulated
    /// `mod+Z`/`Window::dispatch_action` round trip: both
    /// `TestAppContext::dispatch_action`/`VisualTestContext::dispatch_action` *and*
    /// `VisualTestContext::simulate_keystrokes` internally call `run_until_parked` themselves
    /// (`vendor/zed/crates/gpui/src/app/visual_test_context.rs`'s own `dispatch_action`), which
    /// would let the *whole* real, spawned undo op run to completion before this test ever gets a
    /// chance to fire the second, in-flight `Redo` - defeating the entire point of this test
    /// (live-verified while writing it: a raw `Window::dispatch_action` inside `update_in`,
    /// bypassing that trailing `run_until_parked`, still never reached the handler in this test's
    /// harness - GPUI's action dispatch walks the *rendered* frame's focused node, which this
    /// window never repaints without its own `run_until_parked`/draw cycle). Calling the handler
    /// method directly is what every other real-git-mutation regression test in this same module
    /// already does (`Self::perform_undo`/`Self::perform_redo` called directly) - this test
    /// differs only in going through the one extra, real layer (`handle_undo_action`/
    /// `handle_redo_action`) a keybinding actually adds on top of those.
    #[gpui::test]
    fn a_keybinding_triggered_redo_while_an_undo_is_still_in_flight_is_visibly_rejected_not_silently_dropped(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("first.txt"), "first\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.undo_stack.can_undo()));

        // The real Undo keybinding handler starts the background op...
        app.update_in(cx, |app, window, cx| {
            app.handle_undo_action(&Undo, window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_some()),
            "sanity check: Undo must genuinely be in flight, synchronously, before the executor \
             runs it"
        );

        // ...and the real Redo keybinding handler immediately after, before the first completes,
        // must visibly reject it - not silently vanish.
        app.update_in(cx, |app, window, cx| {
            app.handle_redo_action(&Redo, window, cx);
        });
        let status = app
            .read_with(cx, |app, _| app.worktree_history_status.clone())
            .unwrap_or_default();
        assert!(
            status.contains("already running"),
            "a keybinding-triggered redo while another op is in flight must set a real, \
             visible status rather than silently no-op: {status:?}"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight),
            Some(WorktreeHistoryOpKind::Undo),
            "the rejected redo must not have clobbered the real in-flight kind"
        );

        cx.run_until_parked();
    }

    /// Regression coverage for a real bug an audit caught: the palette's History rows stayed
    /// visible (and looked clickable) while a worktree-history operation was already in flight,
    /// even though `Self::perform_undo`/`perform_redo` already silently no-op in that state -
    /// exactly the "looks actionable but does nothing" pattern this app's rules forbid.
    #[gpui::test]
    fn history_palette_rows_are_hidden_while_an_operation_is_in_flight(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("first.txt"), "first\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let id = spawn_agent(&app, cx, feature.clone());
        app.update(cx, |app, cx| app.keep_all_changes(id, cx));
        cx.run_until_parked();

        let groups_idle = app.read_with(cx, |app, cx| app.build_palette_groups(cx));
        assert!(
            groups_idle.iter().any(|g| g.label == "History"),
            "sanity check: a real undo entry exists, so History should show while idle"
        );

        // Undo is now genuinely in flight, synchronously, before the executor runs it.
        app.update(cx, |app, cx| app.perform_undo(cx));
        let groups_busy = app.read_with(cx, |app, cx| app.build_palette_groups(cx));
        assert!(
            groups_busy.iter().all(|g| g.label != "History"),
            "the History group must not render at all while an operation is genuinely in \
             flight - nothing in it would actually do anything right now"
        );
        cx.run_until_parked();
    }
}
