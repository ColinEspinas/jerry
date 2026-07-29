use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

impl AdeApp {
    /// Cleanup for [`Self::close_session`] closing the session whose `Merge` click started
    /// [`Self::merge_flow`]. If a merge is still in progress in the base worktree at that
    /// moment (`Clean`/`Conflicted`, or an `Error` with `abortable_worktree`), this aborts it
    /// (`wt_core::merge::abort_merge`) rather than leaving the repository mid-merge with no UI
    /// left to finish or abort it.
    ///
    /// A `Running` attempt (the `git merge` child process itself) can't be cancelled from here -
    /// there is no cancellation token threaded through it. Clearing `merge_flow` regardless is
    /// still correct: [`Self::start_merge`]'s completion handler guards on `session_id` still
    /// matching, so a `Running` attempt that finishes after this point is a no-op there. If it
    /// left a `MERGE_HEAD` behind, the next `Merge` click hits a git failure and
    /// [`run_merge_attempt`]'s `find_in_progress_merge` fallback surfaces `Abort merge` for it
    /// then - never a silent dead end.
    ///
    /// If [`Self::merge_op_in_flight`] is `true`, [`Self::complete_merge_flow`]/
    /// [`Self::abort_merge_flow`] already own this flow's outcome on the background executor,
    /// so this spawns nothing and only clears the UI-facing `merge_flow` field - reaching into
    /// their shared [`Self::_merge_task`] slot here would drop (cancel) their in-flight
    /// operation. See [`Self::_merge_cleanup_task`]'s docs for why this method's own
    /// best-effort abort uses a separate field instead.
    pub(super) fn clear_merge_flow_for_closed_session(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.merge_flow.take() else {
            return;
        };
        if self.merge_op_in_flight {
            return;
        }
        let base_worktree_path = match flow.state {
            merge::MergeFlowState::Clean {
                base_worktree_path, ..
            }
            | merge::MergeFlowState::Conflicted {
                base_worktree_path, ..
            } => Some(base_worktree_path),
            merge::MergeFlowState::Error {
                abortable_worktree, ..
            } => abortable_worktree,
            merge::MergeFlowState::Running | merge::MergeFlowState::AlreadyUpToDate { .. } => None,
        };
        let Some(base_worktree_path) = base_worktree_path else {
            return;
        };
        let task = cx.spawn(async move |_this, cx| {
            // Fire-and-forget: the session tab is already gone, so there's no UI left to
            // report a failure to. Best-effort is the honest ceiling here - on failure the
            // repository is left in whatever state `git merge --abort` left it in,
            // inspectable/recoverable via a terminal.
            let _ = cx
                .background_executor()
                .spawn(async move { wt_core::merge::abort_merge(&base_worktree_path) })
                .await;
        });
        self._merge_cleanup_task = Some(task);
    }

    /// The context bar's `Merge` action (see `render_merge_button`) - starts
    /// `wt_core::merge::attempt_merge` of `id`'s worktree branch into the repository's detected
    /// base branch, on the background executor (a `gix` open, a `git status` dirty-check, and a
    /// spawned `git merge` child process - see that function's own docs).
    ///
    /// Only one merge flow is tracked at a time; a click while one is already in progress for
    /// any session is a no-op, since two concurrent `git merge` invocations would race over the
    /// same base worktree.
    pub(super) fn start_merge(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if self.merge_flow.is_some() {
            return;
        }
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let repo_path = self.repo_path.clone();
        let worktree_path = session.cwd.clone();
        self.merge_flow = Some(merge::MergeFlow {
            session_id: id,
            state: merge::MergeFlowState::Running,
        });
        self.prune_confirm_armed = false;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let state = cx
                .background_executor()
                .spawn(async move { run_merge_attempt(&repo_path, &worktree_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.merge_flow.as_ref().map(|flow| flow.session_id) == Some(id) {
                    this.merge_flow = Some(merge::MergeFlow {
                        session_id: id,
                        state,
                    });
                }
                cx.notify();
            });
        });
        self._merge_task = Some(task);
    }

    /// Surface D's `Take left`/`Take right`/`Take both` action on the active hunk
    /// (`merge_flow.state`'s `active_file`/`active_hunk`) - mutates the in-memory
    /// [`wt_core::merge::ConflictedFile`] via `wt_core::merge::resolve_hunk`, then advances to
    /// the next unresolved hunk ([`crate::merge::first_unresolved`]). If that resolves the
    /// file's last conflict, the resolved content is written to disk and `git add`ed on the
    /// background executor (`wt_core::merge::write_resolved_file`).
    ///
    /// Only ever mutates a [`wt_core::merge::ConflictedPath::Text`] entry: `active_file`/
    /// `active_hunk` are only ever set from `crate::merge::first_unresolved`, which never
    /// points at an `Unmergeable` entry (see that function's docs).
    pub(super) fn resolve_active_hunk(
        &mut self,
        choice: wt_core::merge::ConflictChoice,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
        let Some(flow) = self.merge_flow.as_mut() else {
            return;
        };
        let merge::MergeFlowState::Conflicted {
            base_worktree_path,
            files,
            active_file,
            active_hunk,
            ..
        } = &mut flow.state
        else {
            return;
        };
        let Some(wt_core::merge::ConflictedPath::Text(file)) = files.get_mut(*active_file) else {
            return;
        };
        if wt_core::merge::resolve_hunk(file, *active_hunk, choice).is_err() {
            // A stale index (shouldn't happen) - nothing sensible to do but ignore the click
            // rather than panicking.
            return;
        }
        let write_back = if file.is_resolved() {
            Some((base_worktree_path.clone(), file.clone()))
        } else {
            None
        };
        if let Some((next_file, next_hunk)) = merge::first_unresolved(files) {
            *active_file = next_file;
            *active_hunk = next_hunk;
        }
        cx.notify();

        let Some((worktree_path, resolved_file)) = write_back else {
            return;
        };
        let session_id = flow.session_id;
        let worktree_path_for_check = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    wt_core::merge::write_resolved_file(&worktree_path, &resolved_file)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = result {
                    if this.merge_flow.as_ref().map(|flow| flow.session_id) == Some(session_id) {
                        // Re-check MERGE_HEAD so `Abort merge` stays offered rather than
                        // silently vanishing - see `merge::MergeFlowState::Error`'s docs.
                        let abortable_worktree =
                            wt_core::merge::merge_head_exists(&worktree_path_for_check)
                                .ok()
                                .filter(|present| *present)
                                .map(|_| worktree_path_for_check.clone());
                        if let Some(flow) = this.merge_flow.as_mut() {
                            flow.state = merge::MergeFlowState::Error {
                                message: format!("failed to write resolved file: {err}"),
                                abortable_worktree,
                            };
                        }
                    }
                }
                cx.notify();
            });
        });
        // Writes to distinct files are independent in-flight operations - see `TaskPool`'s own
        // docs for why this can't be a single `Option<Task<()>>` slot.
        self._merge_write_tasks.push(task);
    }

    /// Surface D's `Complete merge` action - a `git commit` finishing the in-progress merge
    /// (`wt_core::merge::complete_merge`), valid once a clean merge is staged or every
    /// conflicted file is resolved ([`crate::merge::all_resolved`]). On success, clears the flow
    /// and refreshes worktree/diff state to reflect the merge that just happened.
    ///
    /// Guarded by [`Self::merge_op_in_flight`] for the duration of the background commit: a
    /// second click while the first is still in flight (e.g. a fast Abort-right-after-Complete)
    /// would otherwise spawn a second git operation, overwriting [`Self::_merge_task`] and
    /// dropping the first one's completion handler mid-commit.
    /// [`Self::clear_merge_flow_for_closed_session`] respects the same flag so closing the
    /// session mid-commit can't cancel this operation either.
    ///
    /// The success arm only clears [`Self::merge_flow`] when it still belongs to this
    /// `session_id`, matching the error arm below it: a session close no longer blocks this
    /// commit from running to completion, so a merge for a *different* session could
    /// legitimately be in `merge_flow` by the time this closure runs.
    pub(super) fn complete_merge_flow(&mut self, cx: &mut Context<Self>) {
        self.prune_confirm_armed = false;
        if self.merge_op_in_flight {
            return;
        }
        let Some(flow) = self.merge_flow.as_ref() else {
            return;
        };
        let base_worktree_path = match &flow.state {
            merge::MergeFlowState::Clean {
                base_worktree_path, ..
            } => base_worktree_path.clone(),
            merge::MergeFlowState::Conflicted {
                base_worktree_path,
                files,
                ..
            } if merge::all_resolved(files) => base_worktree_path.clone(),
            _ => return,
        };
        self.merge_op_in_flight = true;
        cx.notify();
        let session_id = flow.session_id;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::merge::complete_merge(&base_worktree_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.merge_op_in_flight = false;
                match result {
                    Ok(()) => {
                        if this.merge_flow.as_ref().map(|flow| flow.session_id) == Some(session_id)
                        {
                            this.merge_flow = None;
                        }
                        let repo_path = this.repo_path.clone();
                        this.load_worktrees(cx);
                        this.load_diff(repo_path, cx);
                    }
                    Err(err) => {
                        if this.merge_flow.as_ref().map(|flow| flow.session_id) == Some(session_id)
                        {
                            // MERGE_HEAD is still present when `complete_merge`'s defense in
                            // depth is what failed, so `Abort merge` stays offered.
                            let abortable_worktree =
                                wt_core::merge::find_in_progress_merge(&this.repo_path)
                                    .ok()
                                    .flatten();
                            if let Some(flow) = this.merge_flow.as_mut() {
                                flow.state = merge::MergeFlowState::Error {
                                    message: format!("commit failed: {err}"),
                                    abortable_worktree,
                                };
                            }
                        }
                    }
                }
                cx.notify();
            });
        });
        self._merge_task = Some(task);
    }

    /// Surface D's `Abort merge` action - `git merge --abort` (`wt_core::merge::abort_merge`),
    /// restoring the base worktree to its pre-merge state. If the abort itself fails, the flow
    /// is left in an `Error` state describing that rather than pretending it succeeded.
    ///
    /// Guarded by [`Self::merge_op_in_flight`] - see [`Self::complete_merge_flow`]'s docs for
    /// the Complete-vs-Abort race this (and the matching guard there) prevents.
    pub(super) fn abort_merge_flow(&mut self, cx: &mut Context<Self>) {
        self.prune_confirm_armed = false;
        if self.merge_op_in_flight {
            return;
        }
        let Some(flow) = self.merge_flow.as_ref() else {
            return;
        };
        let base_worktree_path = match &flow.state {
            merge::MergeFlowState::Clean {
                base_worktree_path, ..
            }
            | merge::MergeFlowState::Conflicted {
                base_worktree_path, ..
            } => base_worktree_path.clone(),
            merge::MergeFlowState::Error {
                abortable_worktree: Some(path),
                ..
            } => path.clone(),
            merge::MergeFlowState::Running
            | merge::MergeFlowState::AlreadyUpToDate { .. }
            | merge::MergeFlowState::Error {
                abortable_worktree: None,
                ..
            } => {
                self.merge_flow = None;
                cx.notify();
                return;
            }
        };
        self.merge_op_in_flight = true;
        cx.notify();
        let session_id = flow.session_id;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::merge::abort_merge(&base_worktree_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.merge_op_in_flight = false;
                if this.merge_flow.as_ref().map(|flow| flow.session_id) == Some(session_id) {
                    match result {
                        Ok(()) => this.merge_flow = None,
                        Err(err) => {
                            let abortable_worktree =
                                wt_core::merge::find_in_progress_merge(&this.repo_path).ok().flatten();
                            if let Some(flow) = this.merge_flow.as_mut() {
                                flow.state = merge::MergeFlowState::Error {
                                    message: format!(
                                        "abort failed - the repository may still be mid-merge: {err}"
                                    ),
                                    abortable_worktree,
                                };
                            }
                        }
                    }
                }
                cx.notify();
            });
        });
        self._merge_task = Some(task);
    }

    /// Surface D's `Dismiss` action on an `Error` state - UI-only, clears [`Self::merge_flow`]
    /// without running any git command. When a merge is still in progress
    /// (`abortable_worktree: Some(_)`), Surface D also offers `Abort merge`
    /// ([`Self::abort_merge_flow`]) right next to this one.
    pub(super) fn dismiss_merge_error(&mut self, cx: &mut Context<Self>) {
        self.merge_flow = None;
        self.prune_confirm_armed = false;
        cx.notify();
    }
}

/// Builds a [`merge::MergeFlowState::Error`] for `message`, best-effort populating
/// `abortable_worktree` via [`wt_core::merge::find_in_progress_merge`] rather than assuming a
/// merge is or isn't in progress just because this call failed. If that lookup itself fails,
/// `abortable_worktree` is `None`.
pub(super) fn merge_error_state(
    repo_path: &std::path::Path,
    message: String,
) -> merge::MergeFlowState {
    let abortable_worktree = wt_core::merge::find_in_progress_merge(repo_path)
        .ok()
        .flatten();
    merge::MergeFlowState::Error {
        message,
        abortable_worktree,
    }
}

/// Runs `wt_core::merge::attempt_merge` and folds its result into a [`merge::MergeFlowState`] -
/// a free function (not an `AdeApp` method) so it can run entirely inside
/// `cx.background_executor().spawn`, matching this crate's `load_diff`/`load_worktrees`
/// convention of doing blocking I/O and result-shaping together, off the GPUI foreground
/// thread. For a [`wt_core::merge::MergeOutcome::Conflicted`], this also classifies every
/// conflicted path (`wt_core::merge::classify_conflicted_file`) here, still off-thread, rather
/// than leaving that as a second round-trip.
pub(super) fn run_merge_attempt(
    repo_path: &std::path::Path,
    worktree_path: &std::path::Path,
) -> merge::MergeFlowState {
    let (start, outcome) = match wt_core::merge::attempt_merge(repo_path, worktree_path) {
        Ok(result) => result,
        Err(err) => return merge_error_state(repo_path, err.to_string()),
    };
    match outcome {
        wt_core::merge::MergeOutcome::AlreadyUpToDate => merge::MergeFlowState::AlreadyUpToDate {
            base_branch: start.base_branch,
        },
        wt_core::merge::MergeOutcome::Clean { files } => merge::MergeFlowState::Clean {
            base_branch: start.base_branch,
            base_worktree_path: start.base_worktree_path,
            files,
        },
        wt_core::merge::MergeOutcome::Conflicted {
            conflicted_files,
            clean_files,
        } => {
            let mut files = Vec::with_capacity(conflicted_files.len());
            for path in &conflicted_files {
                match wt_core::merge::classify_conflicted_file(&start.base_worktree_path, path) {
                    Ok(classified) => files.push(classified),
                    Err(err) => return merge_error_state(repo_path, err.to_string()),
                }
            }
            let (active_file, active_hunk) = merge::first_unresolved(&files).unwrap_or((0, 0));
            merge::MergeFlowState::Conflicted {
                base_branch: start.base_branch,
                base_worktree_path: start.base_worktree_path,
                clean_files,
                files,
                active_file,
                active_hunk,
            }
        }
    }
}

/// Regression coverage against real git repositories in tempdirs (`init_repo`/`add_worktree`,
/// the same idiom `wt_core::merge`'s own test module uses), through a real `AdeApp` in a test
/// GPUI window. `cx.run_until_parked()` is only called where the test wants a pending background
/// task to actually finish, so calling a second `AdeApp` method between two `run_until_parked()`
/// calls reliably lands while the first task is still in flight, deterministically reproducing a
/// task-cancellation race rather than relying on wall-clock timing (dropping a GPUI `Task`
/// cancels it immediately - `vendor/zed/crates/scheduler/src/executor.rs`).
#[cfg(test)]
mod merge_regression_tests {
    use super::*;
    use gpui::TestAppContext;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) {
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

    /// Same real-linked-worktree idiom as `wt_core::merge`'s own test module.
    fn add_worktree(repo_path: &std::path::Path, branch: &str, name: &str) -> PathBuf {
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

    fn status(dir: &std::path::Path) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Regression test: closing/archiving the session mid-`Complete merge` must not cancel the
    /// in-flight `git commit` or strand `merge_op_in_flight` at `true` - see
    /// `AdeApp::clear_merge_flow_for_closed_session`'s docs for the mechanism this guards.
    #[gpui::test]
    fn close_session_during_in_flight_complete_merge_lets_the_commit_finish(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let feature_session_id = app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                feature.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });

        app.update(cx, |app, cx| app.start_merge(feature_session_id, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let flow = app
                .merge_flow
                .as_ref()
                .expect("merge_flow after start_merge");
            assert_eq!(flow.session_id, feature_session_id);
            assert!(
                matches!(flow.state, merge::MergeFlowState::Clean { .. }),
                "seed setup should produce a clean (no-conflict) merge, ready for Complete"
            );
        });

        // Click Complete - sets `merge_op_in_flight` synchronously and spawns the commit, but
        // the test executor won't run it until `run_until_parked()` below.
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        assert!(
            app.read_with(cx, |app, _| app.merge_op_in_flight),
            "merge_op_in_flight should be set synchronously by complete_merge_flow"
        );

        // Close (archive) the session before that commit has run - the "click Complete, then
        // immediately click Archive" race.
        app.update_in(cx, |app, window, cx| {
            app.close_session(feature_session_id, window, cx)
        });

        // Let the pending commit and its completion handler run.
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.merge_op_in_flight),
            "merge_op_in_flight must not be permanently stranded at true - the real commit's \
             own completion handler must still run to reset it, since closing the session must \
             not cancel that in-flight task"
        );

        assert!(
            !wt_core::merge::merge_head_exists(repo.path()).expect("merge_head_exists"),
            "MERGE_HEAD must be gone - the merge must have genuinely completed (committed), not \
             been discarded by a competing abort"
        );
        assert_eq!(
            status(repo.path()),
            "",
            "the base worktree must be clean after a real, completed commit"
        );
        assert!(
            repo.path().join("new.txt").is_file(),
            "the resolved/merged content must genuinely be present on disk - not discarded"
        );
    }

    /// Same setup, asserting the repository is left clean and usable: a brand-new merge started
    /// right after the close-during-complete race must succeed, not hit a wedged repo.
    #[gpui::test]
    fn close_session_during_in_flight_complete_merge_leaves_repo_usable_for_a_new_merge(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(feature.join("new.txt"), "from feature\n").expect("write");
        git(&feature, &["add", "new.txt"]);
        git(&feature, &["commit", "-m", "feature commit"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let feature_session_id = app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                feature.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });

        app.update(cx, |app, cx| app.start_merge(feature_session_id, cx));
        cx.run_until_parked();
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        app.update_in(cx, |app, window, cx| {
            app.close_session(feature_session_id, window, cx)
        });
        cx.run_until_parked();

        // A second, independent merge against the same base repo must work normally.
        let second_feature = add_worktree(repo.path(), "second-feature", "second-feature-wt");
        fs::write(second_feature.join("more.txt"), "more work\n").expect("write");
        git(&second_feature, &["add", "more.txt"]);
        git(&second_feature, &["commit", "-m", "second feature commit"]);

        let second_session_id = app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                second_feature.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });
        app.update(cx, |app, cx| app.start_merge(second_session_id, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let flow = app
                .merge_flow
                .as_ref()
                .expect("merge_flow after second start_merge");
            assert_eq!(flow.session_id, second_session_id);
            assert!(
                matches!(flow.state, merge::MergeFlowState::Clean { .. }),
                "a real, independent merge must succeed cleanly on the now-clean repo, not hit \
                 a stale MERGE_HEAD left behind by the earlier race"
            );
        });
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        cx.run_until_parked();
        assert!(!app.read_with(cx, |app, _| app.merge_op_in_flight));
        assert!(!wt_core::merge::merge_head_exists(repo.path()).expect("merge_head_exists"));
        assert!(repo.path().join("more.txt").is_file());
    }

    /// Regression test: resolving two different conflicted files' last hunk back-to-back (e.g.
    /// via Take-both) must not cancel the first file's background write - see
    /// `AdeApp::resolve_active_hunk`'s docs for the mechanism this guards.
    #[gpui::test]
    fn resolving_two_files_back_to_back_writes_both_to_disk_without_cancelling_either(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("a.txt"), "line1\nline2\nline3\n").expect("write");
        fs::write(repo.path().join("b.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "a.txt", "b.txt"]);
        git(repo.path(), &["commit", "-m", "seed a.txt and b.txt"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(repo.path().join("a.txt"), "line1\nBASE CHANGED A\nline3\n").expect("write");
        fs::write(repo.path().join("b.txt"), "line1\nBASE CHANGED B\nline3\n").expect("write");
        git(
            repo.path(),
            &["commit", "-am", "base changes a.txt and b.txt"],
        );

        fs::write(feature.join("a.txt"), "line1\nFEATURE CHANGED A\nline3\n").expect("write");
        fs::write(feature.join("b.txt"), "line1\nFEATURE CHANGED B\nline3\n").expect("write");
        git(
            &feature,
            &["commit", "-am", "feature changes a.txt and b.txt"],
        );

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let feature_session_id = app.update_in(cx, |app, window, cx| {
            app.sessions.spawn(
                SessionKind::Shell,
                feature.clone(),
                app.settings.appearance.terminal_font_size,
                window,
                cx,
            )
        });

        app.update(cx, |app, cx| app.start_merge(feature_session_id, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let flow = app
                .merge_flow
                .as_ref()
                .expect("merge_flow after start_merge");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected a conflicted merge");
            };
            assert_eq!(files.len(), 2, "both a.txt and b.txt should be conflicted");
        });

        // Resolve the first file's only hunk via Take-both - spawns a background write, held
        // pending until the next `run_until_parked()`.
        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Both, cx);
        });

        // Resolve the second file's only hunk too before the first write has run - this must
        // not cancel the first file's still-pending write.
        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Both, cx);
        });

        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow still present");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected a conflicted merge");
            };
            assert!(
                merge::all_resolved(files),
                "both files should be fully resolved in-memory after two Take-both clicks"
            );
        });

        // Let both pending background writes run.
        cx.run_until_parked();

        let a_on_disk = fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt");
        let b_on_disk = fs::read_to_string(repo.path().join("b.txt")).expect("read b.txt");
        assert!(
            !a_on_disk.contains("<<<<<<<"),
            "a.txt must be genuinely marker-free on disk, not left mid-conflict by a cancelled \
             write: {a_on_disk:?}"
        );
        assert!(
            !b_on_disk.contains("<<<<<<<"),
            "b.txt must be genuinely marker-free on disk, not left mid-conflict by a cancelled \
             write: {b_on_disk:?}"
        );

        let real_status = status(repo.path());
        assert!(
            !real_status.contains('U'),
            "git status must show no remaining unmerged (U) entries for either file: \
             {real_status:?}"
        );

        // Both files must now be genuinely staged and resolved on disk, so the merge can
        // actually complete.
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        cx.run_until_parked();
        assert!(
            !wt_core::merge::merge_head_exists(repo.path()).expect("merge_head_exists"),
            "the merge should have completed successfully now that both files are genuinely \
             resolved on disk"
        );
    }
}
