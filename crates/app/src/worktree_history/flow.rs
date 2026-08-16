//! Real backing for `Keep all changes` and `Discard worktree` and, since Revision R12 §5, the
//! Changes panel commit composer's "commit staged files"
//! (`crate::sidebar::render::AdeApp::commit_staged_files`).
//!
//! Their surfaces, after GitHub issue #295 cut the agent pane's action bar down to a readout
//! (`STAGE-A-CHANGELOG.md` §4t): `Discard worktree` is the failed-run strip's second button
//! ([`work_surface::ActionKind::DiscardWorktree`]) and the rail worktree menu's
//! `Remove worktree…`; `Keep all changes` is the title bar's `Agent` menu, which is now its only
//! home - §4r deleted the finished-agent button as "a fiction borrowed from buffer-based
//! inline-diff tools", since the agent's edits are already on disk.
//!
//! ## One in-flight flag for all three operations
//!
//! [`AdeApp::worktree_history_op_in_flight`] serializes "keep all changes", "discard worktree",
//! and "commit staged files": a click for any of the three while the flag is `true` is a no-op,
//! mirroring [`AdeApp::prune_in_flight`]'s own single-flag-per-feature precedent
//! (`crate::rail::render`). Every completion handler still only mutates [`AdeApp`] state from
//! inside `this.update`/`this.update_in`, after its real `wt_core::undo::*` call has actually
//! resolved, matching every other real git-backed action in this app.
//!
//! `commit_staged_files` is real (`wt_core::undo::commit_paths`: a real `git add -- <paths>` +
//! `git commit`), and, like `Keep`/`Discard`, has no undo integration - this project's
//! worktree-level undo/redo action was removed outright (GitHub issue #47), not merely left
//! unwired here.

use super::*;

/// Which of the three real, mutually-exclusive-in-flight operations
/// [`AdeApp::worktree_history_op_in_flight`] currently names - see that field's own docs for why
/// this is a real, named kind rather than a bare `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeHistoryOpKind {
    Keep,
    Discard,
    /// The Changes panel commit composer's "commit staged files" (Revision R12 §5) -
    /// `crate::sidebar::render::AdeApp::commit_staged_files`.
    Commit,
}

impl AdeApp {
    /// Looks up worktree `path`'s branch name for display (History/status-line text) - falls
    /// back to the path itself if the worktree list doesn't (yet, or any more) have an entry for
    /// it. Display-only: never consulted by a real `wt_core::undo::*` call.
    ///
    /// `pub(crate)`, not private: `crate::sidebar::render::AdeApp::commit_staged_files` (Revision
    /// R12 §5's commit composer) reuses this same lookup for its own status-line text rather than
    /// duplicating it.
    pub(crate) fn branch_display_for(&self, path: &Path) -> String {
        self.worktrees
            .iter()
            .find(|item| item.path == path)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Refreshes worktree/diff state after a real git mutation (keep/discard both change what's
    /// on disk) - the same `load_worktrees` + `load_diff` pair `Self::complete_merge_flow`'s own
    /// success arm already uses for the identical reason.
    fn refresh_after_worktree_history_op(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.focused_repo_path();
        self.load_worktrees(cx);
        self.load_diff(repo_path, cx);
    }

    /// The Review footer's `Keep all` action - a real `wt_core::undo::commit_all_changes` on
    /// agent `id`'s worktree. Not gated by any confirmation (unlike
    /// [`Self::request_discard_worktree`]): it's non-destructive.
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
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::undo::commit_all_changes(&worktree_path, &message) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(_outcome) => {
                        this.worktree_history_status =
                            Some(format!("kept all changes in {branch_display}"));
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
    /// [`Self::request_prune`]'s own reasoning: this is a real, destructive action - it
    /// force-removes a worktree directory, preserving uncommitted/untracked content in a real git
    /// stash first (see [`wt_core::undo::discard_worktree`]'s own docs), but not undoable from
    /// within this app). The first click only arms [`AdeApp::discard_confirm_armed`] for `id`; a
    /// second click on the *same* agent's button while armed actually runs it.
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
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return;
        };
        let worktree_path = agent.cwd.clone();
        self.execute_discard_worktree_path(worktree_path, cx);
    }

    /// The rail's `Remove worktree…` row (GitHub issue #290), keyed by worktree path - two
    /// clicks, exactly like [`Self::request_discard_worktree`]'s button, and running the same
    /// real [`Self::execute_discard_worktree_path`] underneath.
    ///
    /// Keyed by *path*, not by agent, because a worktree row's menu is reachable for a worktree
    /// with no agent in it at all - and because the worktree is what the operation really acts
    /// on. The arming state lives in [`AdeApp::remove_worktree_confirm_armed`] and is cleared by
    /// every path that closes the menu, so a half-confirmed removal can never survive into the
    /// next time the menu is opened.
    ///
    /// Returns whether this call really ran the removal (`false` when it only armed, or when
    /// another worktree-history operation was already in flight), so the caller knows whether to
    /// leave the menu open for the confirming click. The in-flight check is deliberately made
    /// *before* the confirmation is touched, exactly as
    /// `crate::graph_view::render::AdeApp::request_graph_delete_branch`'s own audit finding
    /// requires: disarming and then refusing to run would silently turn the click the user is
    /// about to repeat into a dead one.
    pub(crate) fn request_discard_worktree_path(
        &mut self,
        worktree_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.worktree_history_op_in_flight.is_some() {
            return false;
        }
        if self.remove_worktree_confirm_armed.as_deref() != Some(worktree_path.as_path()) {
            let branch_display = self.branch_display_for(&worktree_path);
            self.worktree_history_status = Some(format!(
                "click Remove again to really remove {branch_display}"
            ));
            self.remove_worktree_confirm_armed = Some(worktree_path);
            cx.notify();
            return false;
        }
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.remove_worktree_confirm_armed = None;
        self.execute_discard_worktree_path(worktree_path, cx);
        true
    }

    /// The real, already-confirmed discard of one worktree - the single place
    /// `wt_core::undo::discard_worktree` is called from, whether the confirmation came from the
    /// Review footer's per-agent button or the rail's per-worktree menu row.
    ///
    /// Closes **every** agent open in that worktree on success, not just the one whose button
    /// started it: the directory all of their `cwd`s point at no longer exists, so any tab left
    /// behind is a shell running in a deleted path.
    pub(in crate::worktree_history) fn execute_discard_worktree_path(
        &mut self,
        worktree_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.worktree_history_op_in_flight.is_some() {
            return;
        }
        let repo_path = self.focused_repo_path();
        let branch_display = self.branch_display_for(&worktree_path);
        self.worktree_history_op_in_flight = Some(WorktreeHistoryOpKind::Discard);
        self.worktree_history_status = Some(format!("discarding {branch_display}\u{2026}"));
        cx.notify();

        let discarded_path = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::undo::discard_worktree(&repo_path, &worktree_path) })
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
                        // Every one of these agents' cwd no longer exists.
                        let orphaned: Vec<AgentId> = this
                            .agents
                            .iter_for_cwd(discarded_path.clone())
                            .map(|agent| agent.id)
                            .collect();
                        for id in orphaned {
                            this.close_agent(id, window, cx);
                        }
                        this.remove_worktree_confirm_armed = None;
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
}

/// Real-git-repo, real-`TestAppContext` regression coverage, mirroring
/// `merge::flow::merge_regression_tests`/`rail::render::prune_regression_tests`' own
/// established idiom.
#[cfg(test)]
mod worktree_history_regression_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::test_support::{temp_repo, TempRepo};
    use gpui::{Entity, TestAppContext};
    use std::fs;
    use tempfile::TempDir;
    use test_support::{git, git_output};

    fn init_repo() -> TempRepo {
        temp_repo_with(|root| {
            test_support::seed_empty_repo_at(root);
            test_support::commit(root, "base.txt", "base\n", "initial");
        })
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
                ProcessKind::Shell,
                cwd,
                app.settings.appearance.terminal_font_size,
                app.settings.terminal.shell_override(),
                None,
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn keep_all_changes_commits_a_dirty_worktree(cx: &mut TestAppContext) {
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
}
