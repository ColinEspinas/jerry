//! Real backing for `Keep all changes` and `Discard worktree` and, since Revision R12 §5, the
//! Changes panel commit composer's "commit staged files"
//! (`crate::sidebar::render::AdeApp::commit_staged_files`).

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
    pub(crate) fn branch_display_for(&self, path: &Path) -> String {
        self.worktrees
            .iter()
            .find(|item| item.path == path)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// Whether `path` is a repository's own main checkout - the one worktree
    /// [`Self::execute_discard_worktree_path`] can never succeed on.
    pub(crate) fn is_main_worktree_path(&self, path: &Path) -> bool {
        // Every added repo, not just the focused one: a rail row menu can be opened on a worktree
        // belonging to a repo `Self::worktrees` says nothing about. An unknown path is not main,
        // so a list that has not loaded yet cannot disable a real action.
        self.worktrees
            .iter()
            .chain(self.repos.iter().flat_map(|repo| repo.worktrees.iter()))
            .any(|item| item.path == path && item.is_main)
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
    /// [`Self::request_discard_worktree_path`]): it's non-destructive.
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

    /// The rail's `Remove worktree…` row, keyed by worktree path - two clicks, and the only door
    /// onto [`Self::execute_discard_worktree_path`]. `true` once it really ran.
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
        self.remove_worktree_confirm_armed = None;
        self.execute_discard_worktree_path(worktree_path, cx);
        true
    }

    /// The real, already-confirmed discard of one worktree - the single place
    /// `wt_core::undo::discard_worktree` is called from, whether the confirmation came from the
    /// Review footer's per-agent button or the rail's per-worktree menu row.
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

        // GitHub issue #470: every process this app started inside the worktree must be dead
        // *before* the directory is deleted - on Windows a live child's cwd holds an open handle
        // that makes `git worktree remove` half-fail, and on unix a surviving process keeps
        // executing (and recreating files) in an unlinked cwd. Agent PTY sessions and the
        // worktree's language servers are both taken here and shut down inside the same
        // background hop that then runs the discard, so the ordering holds by construction; the
        // one spawn source left is the user starting something new mid-delete, which
        // `Self::discarding_worktree` (set below) makes `new_agent`/`respawn_agent` refuse.
        // The panes render as exited; the tabs themselves still close only in the success arm
        // below (and deliberately stay, showing the dead pane, when the discard fails).
        self.discarding_worktree = Some(worktree_path.clone());
        let doomed_panes: Vec<gpui::Entity<crate::terminal::pane::TerminalPane>> = self
            .agents
            .iter_for_cwd(worktree_path.clone())
            .map(|agent| agent.pane.clone())
            .collect();
        let mut doomed_sessions = Vec::with_capacity(doomed_panes.len());
        for pane in doomed_panes {
            if let Some(session) = pane.update(cx, |pane, cx| pane.take_session_for_teardown(cx)) {
                doomed_sessions.push(session);
            }
        }
        let doomed_lsp_clients = self.take_lsp_clients_for_root(&worktree_path);

        let discarded_path = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    for mut session in doomed_sessions {
                        if let Err(err) = session.shutdown() {
                            log::warn!("failed to shut down a doomed agent session: {err}");
                        }
                    }
                    for client in doomed_lsp_clients {
                        // The same try_unwrap-then-shutdown-else-drop rule as
                        // `AdeApp::shutdown_lsp_client_off_thread`, run here so the server's
                        // process tree is down before the delete rather than on a detached task.
                        match std::sync::Arc::try_unwrap(client) {
                            Ok(mut client) => {
                                if let Err(err) = client.shutdown() {
                                    log::warn!("failed to shut down a doomed server: {err}");
                                }
                            }
                            Err(client) => drop(client),
                        }
                    }
                    wt_core::undo::discard_worktree(&repo_path, &worktree_path)
                })
                .await;
            // `update_in` (not plain `update`): a successful discard closes the now-cwd-less
            // agent tab, and `Self::close_agent` needs a real `Window` to move focus off it -
            // see `vendor/zed/crates/gpui/src/app/async_context.rs`'s `AsyncApp::with_window`,
            // the same mechanism `Self::trigger_goto_definition`'s own completion handler already
            // relies on for the identical reason.
            let _ = this.update_in(cx, |this, window, cx| {
                this.worktree_history_op_in_flight = None;
                this.discarding_worktree = None;
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
    use crate::test_support::{temp_repo_with, TempRoot};
    use gpui::{Entity, TestAppContext};
    use std::fs;
    use tempfile::TempDir;
    use test_support::{git, git_output};

    fn init_repo() -> TempRoot {
        temp_repo_with(|root| {
            test_support::seed_empty_repo_at(root);
            test_support::commit(root, "base.txt", "base\n", "initial");
        })
    }

    /// Same linked-worktree idiom `merge::flow`/`rail::render`'s own test modules use.
    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        // `keep()`, not `drop()`: dropping the container deleted the directory and freed its
        // random name for reuse, so under the parallel suite another fixture could be handed the
        // same name and create `<name>` inside it first - `git worktree add` then failed with
        // "already exists". Reproduced directly: 1 run in ~7 of the merge module under load.
        // Keeping the (empty) directory costs nothing the old code did not already leak, since
        // git recreated the path afterwards and nothing ever removed it.
        let root = container.keep();
        let path = root.join(name);
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

        app.update(cx, |app, cx| {
            app.request_discard_worktree_path(feature.clone(), cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.remove_worktree_confirm_armed.clone()),
            Some(feature.clone())
        );
        assert!(feature.exists(), "the first click must not touch anything");

        app.update(cx, |app, cx| {
            app.request_discard_worktree_path(feature.clone(), cx)
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
        let _id = spawn_agent(&app, cx, feature.clone());
        app.update(cx, |app, cx| {
            app.request_discard_worktree_path(feature.clone(), cx)
        });
        app.update(cx, |app, cx| {
            app.request_discard_worktree_path(feature.clone(), cx)
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
