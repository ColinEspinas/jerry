use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

impl AdeApp {
    /// Real cleanup for [`Self::close_session`] closing the very session whose `Merge` click
    /// started [`Self::merge_flow`]. If a real merge is genuinely still in progress in the
    /// base worktree at that moment (`Clean`/`Conflicted` - both real "`MERGE_HEAD` present,
    /// uncommitted" states - or an `Error` with a real `abortable_worktree`), this really
    /// aborts it (`wt_core::merge::abort_merge`) rather than just dropping the UI's own state
    /// and silently leaving the repository mid-merge with no UI left to finish or abort it.
    ///
    /// A merge attempt still `Running` (the `git merge` child process itself, in flight on the
    /// background executor) can't be cancelled from here - there is no cancellation token
    /// threaded through it. Clearing `merge_flow` regardless is still correct: `Self::
    /// start_merge`'s own completion handler already guards on `merge_flow`'s `session_id`
    /// still matching before applying its result (see that method), so a `Running` attempt
    /// that finishes after this point is a no-op here, not a resurrected stale flow. In the
    /// rare case that in-flight attempt *did* leave a real `MERGE_HEAD` behind before this
    /// runs, it's a real, narrow, self-healing race: the next `Merge` click will hit a real
    /// git failure, and `Self::run_merge_attempt`'s `find_in_progress_merge` fallback (see its
    /// docs) surfaces a real `Abort merge` action for it then - never a silent, permanent
    /// dead end.
    ///
    /// If [`Self::merge_op_in_flight`] is `true`, a real `Self::complete_merge_flow`/
    /// `Self::abort_merge_flow` background git operation already owns this flow's outcome, so
    /// this deliberately spawns nothing here and returns after only clearing the UI-facing
    /// `merge_flow` field. This was a verified real bug: this method used to unconditionally
    /// spawn its own best-effort abort into the *same* [`Self::_merge_task`] slot
    /// `complete_merge_flow`/`abort_merge_flow` use, and dropping a GPUI `Task` cancels it
    /// immediately - so closing/archiving a session while a real `Complete merge` commit was
    /// still in flight silently cancelled that commit (discarding already-resolved conflict
    /// work to a `git merge --abort` that won the resulting race) *and* permanently stranded
    /// `merge_op_in_flight` at `true` forever, since the reset lives inside the very completion
    /// closure that got cancelled - wedging the repository mid-merge with no working recovery
    /// action anywhere in the UI. Leaving that already-running operation alone and letting its
    /// own completion handler finish naturally is the real fix; see [`Self::_merge_cleanup_task`]
    /// for why this method's own best-effort abort (the non-in-flight case below) now lives in
    /// a separate field instead.
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
            // Fire-and-forget: the session tab (and any UI to show a further error) is
            // already gone by the time this real abort even starts. Best-effort is the
            // honest ceiling here - if it genuinely fails, the repository is left in
            // whatever real state `git merge --abort` left it in, inspectable/recoverable
            // via a real terminal, exactly like every other real-error path in this module.
            let _ = cx
                .background_executor()
                .spawn(async move { wt_core::merge::abort_merge(&base_worktree_path) })
                .await;
        });
        self._merge_cleanup_task = Some(task);
    }

    /// The context bar's real `Merge` action (`render_merge_button`'s docs) - starts a real
    /// `wt_core::merge::attempt_merge` of `id`'s worktree branch into the repository's
    /// detected base branch, on the background executor (this performs real, possibly-slow
    /// blocking I/O: a `gix` open, a `git status` dirty-check, and a spawned `git merge`
    /// child process - see that function's own docs for the full plumbing and why it's safe).
    ///
    /// Only one merge flow is tracked at a time (`Self::merge_flow`); a click here while one
    /// is already in progress for *any* session is a no-op - the design's own `Merge` button
    /// has no concept of queuing a second merge behind a first one, and doing two at once
    /// would mean two real, concurrent `git merge` invocations racing over the same base
    /// worktree.
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

    /// Surface D's real `Take left`/`Take right`/`Take both` action on the currently active
    /// hunk (`merge_flow.state`'s `active_file`/`active_hunk`) - mutates the real, in-memory
    /// [`wt_core::merge::ConflictedFile`] via `wt_core::merge::resolve_hunk`, then advances to
    /// the next real unresolved hunk (`crate::merge::first_unresolved`). If that resolves the
    /// file's very last conflict, the real, now-fully-resolved content is written back to disk
    /// and `git add`ed on the background executor (`wt_core::merge::write_resolved_file`) -
    /// never left resolved only in memory.
    ///
    /// Only ever mutates a [`wt_core::merge::ConflictedPath::Text`] entry - `active_file`/
    /// `active_hunk` are only ever set from `crate::merge::first_unresolved`'s own real
    /// output, which never points at an `Unmergeable` entry (it has no hunk to point at - see
    /// that function's docs).
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
                        // Best-effort: re-check real `MERGE_HEAD` presence in this same
                        // worktree so a real `Abort merge` stays offered rather than
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
        // Prune already-finished entries rather than replacing a single slot: dropping a GPUI
        // `Task` cancels it immediately, so a single `Option<Task<()>>` here was a verified
        // real bug - resolving a *different* file's last hunk while this write was still in
        // flight would cancel it, leaving real conflict markers on disk while the in-memory
        // model already reported that file resolved. Writes to distinct files are independent,
        // so nothing in-flight is ever dropped here, only tasks that have already completed.
        self._merge_write_tasks.retain(|task| !task.is_ready());
        self._merge_write_tasks.push(task);
    }

    /// Surface D's real `Complete merge` action - a real `git commit` finishing the
    /// in-progress merge (`wt_core::merge::complete_merge`'s docs), valid once a clean merge
    /// is staged or every conflicted file is resolved (`crate::merge::all_resolved`). On real
    /// success, clears the flow and refreshes the real worktree/diff state so the rest of the
    /// UI reflects the merge that actually just happened.
    ///
    /// Guarded by [`Self::merge_op_in_flight`] (set for the duration of the real background
    /// commit): without this, the button stayed clickable while a first click's real `git
    /// commit` was still in flight, and a second click (e.g. a fast Abort-right-after-Complete
    /// double-click) could spawn a second real git operation racing the first, overwriting
    /// [`Self::_merge_task`] and dropping the first one's own completion handler - verified to
    /// let a real `git merge --abort` win the race and discard real, already-resolved conflict
    /// work `git commit` was mid-writing. [`Self::clear_merge_flow_for_closed_session`] respects
    /// this same flag (see its docs) so closing/archiving the session mid-commit can no longer
    /// reach into [`Self::_merge_task`] and cancel this operation out from under itself either.
    ///
    /// The success arm only clears [`Self::merge_flow`] when it still belongs to this same
    /// `session_id` - matching the error arm right below it - since a session close no longer
    /// blocks this real background commit from running to completion (see
    /// `clear_merge_flow_for_closed_session`'s docs); a real merge for a *different* session
    /// could legitimately have started and be in `merge_flow` by the time this closure runs.
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
                            // Real defense in depth (`wt_core::merge::complete_merge`'s own
                            // docs) can be exactly what failed here (e.g. a real modify/
                            // delete or binary conflict this app has no resolution action
                            // for) - `MERGE_HEAD` is still genuinely present in that case, so
                            // a real `Abort merge` stays offered.
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

    /// Surface D's real `Abort merge` action - a real `git merge --abort`
    /// (`wt_core::merge::abort_merge`'s docs), restoring the base worktree to exactly its
    /// pre-merge state. If the abort itself genuinely fails (rare - e.g. no merge was actually
    /// in progress any more), the flow is left in a real `Error` state describing that
    /// (`merge::MergeFlowState::Error`'s own docs on why this never silently drops the UI back
    /// to "nothing happening" while git might still be mid-merge) rather than pretending the
    /// abort succeeded.
    ///
    /// Guarded by [`Self::merge_op_in_flight`] - see [`Self::complete_merge_flow`]'s docs for
    /// the real Complete-vs-Abort race this (and the matching guard there) prevents.
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

    /// Surface D's `Dismiss` action on a real `Error` state - UI-only: clears
    /// [`Self::merge_flow`] without running any further git command, since the real
    /// repository state at that point is exactly whatever the last real `wt_core::merge` call
    /// left it as (see `merge::MergeFlowState::Error`'s docs) and remains inspectable/
    /// recoverable through a real terminal in that worktree either way. When a real merge is
    /// still genuinely in progress (`abortable_worktree: Some(_)`), Surface D also offers a
    /// real `Abort merge` action right next to this one (`Self::abort_merge_flow`) - `Dismiss`
    /// itself deliberately never runs a git command on its own.
    pub(super) fn dismiss_merge_error(&mut self, cx: &mut Context<Self>) {
        self.merge_flow = None;
        self.prune_confirm_armed = false;
        cx.notify();
    }
}

/// Builds a real [`merge::MergeFlowState::Error`] for `message`, best-effort populating
/// `abortable_worktree` via [`wt_core::merge::find_in_progress_merge`] - real ground truth
/// ("does the repository's base worktree genuinely have `MERGE_HEAD` set right now"), not an
/// assumption that a merge is (or isn't) in progress just because *this* call happened to
/// fail. If `find_in_progress_merge` itself also fails, `abortable_worktree` is `None` (no
/// worse than not offering the abort action at all - never compounds one real error into a
/// second, confusing one).
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

/// Runs one real `wt_core::merge::attempt_merge` and folds its `Result<(MergeStart,
/// MergeOutcome), Error>` into a [`merge::MergeFlowState`] - a free function (not an `AdeApp`
/// method) so it can run entirely inside `cx.background_executor().spawn`, per this crate's
/// own established `load_diff`/`load_worktrees` convention of doing the real blocking I/O and
/// its result-shaping together, off the GPUI foreground thread. For a real
/// [`wt_core::merge::MergeOutcome::Conflicted`], this also classifies every conflicted path's
/// real state (`wt_core::merge::classify_conflicted_file` - real text conflict vs. a real
/// modify/delete or binary conflict this app has no text-hunk resolution for, see that
/// function's docs) here, still off-thread, rather than leaving that as a second round-trip.
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

/// Real, interactive regression coverage for the two round-2-audit bugs the round-1 fix for
/// the Complete-vs-Abort race (`AdeApp::merge_op_in_flight`'s own docs) introduced: both
/// [`AdeApp::clear_merge_flow_for_closed_session`] and [`AdeApp::resolve_active_hunk`] used to
/// funnel their own background task into a field a *different* real, in-flight merge
/// background task also used ([`AdeApp::_merge_task`] and a since-removed single-slot
/// `_merge_write_task` respectively) - and dropping a GPUI `Task` cancels it immediately
/// (`vendor/zed/crates/scheduler/src/executor.rs`), so the second task to land silently
/// cancelled the first one's real git operation. Exercised against real git repositories in
/// tempdirs (`init_repo`/`add_worktree`, the same idiom `wt_core::merge`'s own test module
/// uses) through a real `AdeApp` in a real (test) GPUI window, driven by GPUI's deterministic
/// test executor: `cx.run_until_parked()` is called only where the test deliberately wants a
/// pending background task to actually finish, so that calling a second `AdeApp` method
/// in between two `run_until_parked()` calls reliably lands *while the first task is still
/// in flight* rather than racing it - reproducing the two bugs deterministically rather than
/// relying on real wall-clock timing.
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

    /// Regression test for Bug 1 (the critical one): closing/archiving the session mid-`Complete
    /// merge` used to cancel that real, in-flight `git commit` (via the shared `_merge_task`
    /// slot `clear_merge_flow_for_closed_session` also wrote to) and permanently strand
    /// `merge_op_in_flight` at `true` - see this module's own docs, and
    /// `AdeApp::clear_merge_flow_for_closed_session`'s docs, for the exact mechanism.
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
        let feature_session_id = app.update(cx, |app, cx| {
            app.sessions.spawn(SessionKind::Shell, feature.clone(), cx)
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

        // Click Complete - this synchronously sets `merge_op_in_flight` and spawns the real
        // `git commit` onto the background executor, but the deterministic test executor
        // doesn't run it until the next `run_until_parked()`/similar below.
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        assert!(
            app.read_with(cx, |app, _| app.merge_op_in_flight),
            "merge_op_in_flight should be set synchronously by complete_merge_flow"
        );

        // Before that commit has actually run, close (archive) the session it belongs to -
        // exactly the "click Complete, then immediately click Archive/the tab x" race from the
        // bug report.
        app.update_in(cx, |app, window, cx| {
            app.close_session(feature_session_id, window, cx)
        });

        // Now let both the pending real `git commit` and its completion handler actually run.
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

    /// The same regression, but asserting the *second* half explicitly: immediately starting a
    /// brand-new merge after the close-during-complete race must find a real, clean repository
    /// (not one still wedged mid-merge from a cancelled commit racing an abort).
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
        let feature_session_id = app.update(cx, |app, cx| {
            app.sessions.spawn(SessionKind::Shell, feature.clone(), cx)
        });

        app.update(cx, |app, cx| app.start_merge(feature_session_id, cx));
        cx.run_until_parked();
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        app.update_in(cx, |app, window, cx| {
            app.close_session(feature_session_id, window, cx)
        });
        cx.run_until_parked();

        // A second, independent worktree/session/merge against the same base repo must work
        // normally - proof the repo was left in a real, clean, usable state rather than wedged.
        let second_feature = add_worktree(repo.path(), "second-feature", "second-feature-wt");
        fs::write(second_feature.join("more.txt"), "more work\n").expect("write");
        git(&second_feature, &["add", "more.txt"]);
        git(&second_feature, &["commit", "-m", "second feature commit"]);

        let second_session_id = app.update(cx, |app, cx| {
            app.sessions
                .spawn(SessionKind::Shell, second_feature.clone(), cx)
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

    /// Regression test for Bug 2: resolving two different conflicted files' last hunk
    /// back-to-back (e.g. via Take-both) used to cancel the first file's real background write
    /// (`wt_core::merge::write_resolved_file`) via a shared single-slot `_merge_write_task`,
    /// leaving real conflict markers on disk for the first file while the in-memory model
    /// already reported it resolved. See `AdeApp::resolve_active_hunk`'s docs and this module's
    /// own docs for the exact mechanism.
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
        let feature_session_id = app.update(cx, |app, cx| {
            app.sessions.spawn(SessionKind::Shell, feature.clone(), cx)
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

        // Resolve the first active file's only hunk via Take-both - this spawns a real
        // background write for it, but the deterministic test executor holds it pending until
        // the next `run_until_parked()`.
        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Both, cx);
        });

        // Before that first write has actually run, resolve the *second* file's only hunk too
        // - exactly the back-to-back Take-both race from the bug report. This must not cancel
        // the first file's still-pending write.
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

        // Now let both pending real background writes actually run.
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

        // Real defense-in-depth proof: the merge can actually be completed now (both files are
        // genuinely staged and resolved on disk, not just in the in-memory model).
        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        cx.run_until_parked();
        assert!(
            !wt_core::merge::merge_head_exists(repo.path()).expect("merge_head_exists"),
            "the merge should have completed successfully now that both files are genuinely \
             resolved on disk"
        );
    }
}
