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
    pub(crate) fn clear_merge_flow_for_closed_session(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.merge_flow.take() else {
            return;
        };
        // The flow this hand-edit belonged to is gone (`self.merge_flow` was already
        // unconditionally taken above, regardless of `merge_op_in_flight` below - only the
        // best-effort abort *spawn* further down is skipped while an in-flight
        // complete/abort owns the outcome instead) - see `Self::clear_merge_edit_state`'s own
        // docs. The session tab (and thus any UI that could show this hand-edit) is genuinely
        // gone either way, so this always runs.
        self.clear_merge_edit_state();
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
    pub(crate) fn start_merge(&mut self, id: SessionId, cx: &mut Context<Self>) {
        if self.merge_flow.is_some() {
            return;
        }
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let repo_path = self.repo_path.clone();
        let worktree_path = session.cwd.clone();
        // A fresh attempt, even one reusing `id` (e.g. Abort then immediately Merge again on the
        // same session tab) - see `merge::MergeFlow::generation`'s own docs. Any hand-edit from a
        // *previous* attempt is unconditionally gone the moment a new one starts, since its
        // `files[]`/hunk indices belong to an attempt that no longer exists.
        self.merge_generation = self.merge_generation.wrapping_add(1);
        let generation = self.merge_generation;
        self.clear_merge_edit_state();
        self.merge_flow = Some(merge::MergeFlow {
            session_id: id,
            generation,
            state: merge::MergeFlowState::Running,
        });
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
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
                        generation,
                        state,
                    });
                    // The real state-transition point a fresh `Conflicted` state's active hunk
                    // (if any) needs its highlight cache filled - see
                    // `Self::ensure_active_merge_highlight_cache`'s docs for why this must never
                    // happen from `render()` instead.
                    this.ensure_active_merge_highlight_cache();
                }
                cx.notify();
            });
        });
        self._merge_task = Some(task);
    }

    /// Surface D's `Take left`/`Take right`/`Take both` action on the active hunk
    /// (`merge_flow.state`'s `active_file`/`active_hunk`) - mutates the in-memory
    /// [`wt_core::merge::ConflictedFile`] via `wt_core::merge::resolve_hunk`, then advances to
    /// the next unresolved hunk ([`crate::merge::state::first_unresolved`]). If that resolves the
    /// file's last conflict, the resolved content is written to disk and `git add`ed on the
    /// background executor (`wt_core::merge::write_resolved_file`).
    ///
    /// Only ever mutates a [`wt_core::merge::ConflictedPath::Text`] entry: `active_file`/
    /// `active_hunk` are only ever set from `crate::merge::state::first_unresolved`, which never
    /// points at an `Unmergeable` entry (see that function's docs).
    pub(in crate::merge) fn resolve_active_hunk(
        &mut self,
        choice: wt_core::merge::ConflictChoice,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
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
        // `session_id`/`generation` (plain `Copy` values) read out now, while `flow` (borrowed
        // from `self.merge_flow`) is still alive - `flow`/`files`/`active_file`/`active_hunk` are
        // not used again past this point, so this is the last real use of that borrow.
        let session_id = flow.session_id;
        let generation = flow.generation;
        cx.notify();
        // The real state-transition point the newly-active hunk (if the advance above landed on
        // a different one) needs its highlight cache filled - see
        // `Self::ensure_active_merge_highlight_cache`'s docs for why this must never happen from
        // `render()` instead. Safe to call now: the `flow`/`files` borrow above has ended.
        self.ensure_active_merge_highlight_cache();
        // A take-left/right/both click on the active hunk always targets whichever file is
        // currently active - if that's also the file a hand-edit buffer is open for, this quick-
        // pick action just resolved (or advanced past) it, so any open hand-edit for it is now
        // stale - see `Self::sync_merge_edit_to_active_file`'s own docs.
        self.sync_merge_edit_to_active_file();

        let Some((worktree_path, resolved_file)) = write_back else {
            return;
        };
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
                    // Real defense in depth (see `merge::MergeFlow::generation`'s own docs): a
                    // bare `session_id` match alone can't tell "this write's own attempt is still
                    // the live one" apart from "the same session started a *fresh* attempt while
                    // this write was still in flight" - both share `session_id`, only the newer
                    // one shares `generation` too.
                    let still_current = this.merge_flow.as_ref().is_some_and(|flow| {
                        flow.session_id == session_id && flow.generation == generation
                    });
                    if still_current {
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
    /// conflicted file is resolved ([`crate::merge::state::all_resolved`]). On success, clears the flow
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
    pub(in crate::merge) fn complete_merge_flow(&mut self, cx: &mut Context<Self>) {
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
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
                            this.clear_merge_edit_state();
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
    pub(in crate::merge) fn abort_merge_flow(&mut self, cx: &mut Context<Self>) {
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
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
                self.clear_merge_edit_state();
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
                        Ok(()) => {
                            this.merge_flow = None;
                            this.clear_merge_edit_state();
                        }
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
    pub(in crate::merge) fn dismiss_merge_error(&mut self, cx: &mut Context<Self>) {
        self.merge_flow = None;
        self.clear_merge_edit_state();
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// Real teardown for [`Self::merge_edit`] - clears the hand-edit slot itself. Does not, and
    /// need not, cancel [`Self::_merge_edit_save_task`] if a save happens to be in flight at the
    /// moment this runs: that background task's own completion handler
    /// ([`Self::apply_merge_edit_save_result`]) independently re-checks `(session_id,
    /// generation, relative_path)` before applying anything, so a stale completion after this
    /// call is already a safe no-op - the same "identify, don't cancel" discipline
    /// [`Self::_merge_write_tasks`] already establishes for the quick-pick resolution path (see
    /// [`Self::clear_merge_flow_for_closed_session`]'s own docs for why that path doesn't reach
    /// into its own task pool either).
    pub(in crate::merge) fn clear_merge_edit_state(&mut self) {
        self.merge_edit = None;
        self.merge_edit_save_error = None;
    }

    /// Keeps [`Self::merge_edit`] in sync whenever the merge flow's own active file/hunk pointer
    /// moves for any reason (a quick-pick resolve advancing it, or a hand-edit save
    /// re-deriving it) - a real state-transition hook, mirroring
    /// [`Self::ensure_active_merge_highlight_cache`]'s own "recompute only at real transition
    /// points, never from `render()`" discipline. Clears the hand-edit slot whenever it no
    /// longer matches whichever *unresolved* file is now active, by path (see
    /// [`merge::MergeEditState`]'s own docs for why path, not index) - including "the flow ended
    /// or restarted" and "the file this hand-edit was for is now fully resolved by any means".
    pub(in crate::merge) fn sync_merge_edit_to_active_file(&mut self) {
        let Some(edit) = self.merge_edit.as_ref() else {
            return;
        };
        let still_active = self.merge_flow.as_ref().is_some_and(|flow| {
            flow.session_id == edit.session_id
                && flow.generation == edit.generation
                && matches!(
                    &flow.state,
                    merge::MergeFlowState::Conflicted {
                        files,
                        active_file,
                        ..
                    } if matches!(
                        files.get(*active_file),
                        Some(ConflictedPath::Text(file))
                            if file.relative_path == edit.relative_path && !file.is_resolved()
                    )
                )
        });
        if !still_active {
            self.merge_edit = None;
            self.merge_edit_save_error = None;
        }
    }

    /// Toggles hand-edit mode *on* for the currently active conflicted file (see
    /// `crate::merge::render`'s own docs for the button this backs) - seeds a fresh
    /// [`crate::code_surface::edit_buffer::EditBuffer`] from [`wt_core::merge::ConflictedFile::render`]'s
    /// current in-memory content (which may already reflect quick-pick-resolved hunks not yet
    /// written to disk - see [`Self::resolve_active_hunk`]'s own docs for why a real disk read
    /// would show stale markers here), never raw disk bytes. `extension: None` deliberately -
    /// see `crate::merge::editing`'s own top docs for why this surface never runs the real
    /// `tree-sitter` highlighter at all.
    ///
    /// A no-op if there's no active `Conflicted` text hunk to edit (nothing to seed from), or if
    /// [`Self::merge_edit`] already matches the active file (re-clicking the toggle while already
    /// editing it does nothing destructive).
    pub(in crate::merge) fn start_merge_hand_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(flow) = self.merge_flow.as_ref() else {
            return;
        };
        let merge::MergeFlowState::Conflicted {
            base_worktree_path,
            files,
            active_file,
            ..
        } = &flow.state
        else {
            return;
        };
        let Some(ConflictedPath::Text(file)) = files.get(*active_file) else {
            return;
        };
        if self
            .merge_edit
            .as_ref()
            .is_some_and(|edit| edit.relative_path == file.relative_path)
        {
            window.focus(&self.merge_edit_focus_handle, cx);
            return;
        }
        let relative_path = file.relative_path.clone();
        let base_worktree_path = base_worktree_path.clone();
        let content = file.render();
        let len = content.len() as u64;
        let absolute_path = base_worktree_path.join(&relative_path);
        let buffer = edit_buffer::EditBuffer::new(absolute_path, content, None, None, len);
        // A genuinely fresh buffer - bumped here, not reused from a stale value, so a save
        // dispatched against the *previous* buffer for this same file (if any - see the re-click
        // early return above) can never be mistaken for one dispatched against this new one. See
        // `merge::MergeEditState::buffer_id`'s own docs for the real race this closes.
        self.merge_edit_buffer_id = self.merge_edit_buffer_id.wrapping_add(1);
        self.merge_edit = Some(merge::MergeEditState {
            session_id: flow.session_id,
            generation: flow.generation,
            buffer_id: self.merge_edit_buffer_id,
            base_worktree_path,
            relative_path,
            buffer,
        });
        self.merge_edit_save_error = None;
        window.focus(&self.merge_edit_focus_handle, cx);
        cx.notify();
    }

    /// Toggles hand-edit mode *off* without writing anything - discards the in-memory buffer
    /// (including any unsaved typing) and returns Surface D to the quick-pick two-column view for
    /// the active file's current (unaffected) state. The only real "undo" this phase offers - see
    /// `crate::code_surface::edit_buffer`'s own documented "no undo/redo" scope cut for why this is a coarse
    /// whole-buffer discard, not a step-by-step one.
    pub(in crate::merge) fn discard_merge_hand_edit(&mut self, cx: &mut Context<Self>) {
        self.clear_merge_edit_state();
        cx.notify();
    }

    /// The merge hand-edit editor's real explicit save (`secondary-s`, scoped to
    /// `"merge-editor"`) - see this module's own top docs for the real pipeline this runs:
    /// (a) a real `std::fs::write` of the buffer's current content to its real absolute path, off
    /// the foreground thread; (b) still off-thread, a real, git-free
    /// [`wt_core::merge::load_conflicted_file`] re-parse of what was just written; (c) if that
    /// re-parse reports zero remaining conflicts, a real [`wt_core::merge::write_resolved_file`]
    /// call (the `git add` staging piece - a harmless, byte-identical rewrite plus the real
    /// staging git's own index otherwise never gets); (d) back on the foreground thread, the
    /// matching `crate::merge::state::replace_conflicted_file` swap into `files[]`, a
    /// `crate::merge::state::first_unresolved` recompute, and [`Self::ensure_active_merge_highlight_cache`]
    /// - the same real hook the quick-pick path already uses.
    ///
    /// Mirrors [`Self::save_active_file`]/[`Self::spawn_file_save_loop`]'s serial-writer-loop
    /// discipline (see [`Self::merge_edit_save_pending`]/[`Self::merge_edit_save_running`]'s own
    /// docs) so two fast saves can't race each other's write, scoped to the single
    /// [`Self::merge_edit`] slot rather than per-path.
    pub(crate) fn save_merge_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.merge_edit.as_ref() else {
            return;
        };
        if !edit.buffer.is_dirty() {
            return;
        }
        self.merge_edit_save_pending = true;
        if self.merge_edit_save_running {
            // The loop below is already alive and re-checks `merge_edit_save_pending` before
            // writing or stopping - mirrors `Self::enqueue_save`'s identical discipline.
            return;
        }
        self.merge_edit_save_running = true;
        self.spawn_merge_edit_save_loop(cx);
    }

    /// The real serial writer loop backing [`Self::save_merge_edit`] - see that method's own docs
    /// for the pipeline each iteration runs. Reads [`Self::merge_edit`]'s *current* content fresh
    /// at each iteration (never a value captured once at dispatch time), so a keystroke landing
    /// while an earlier write is still in flight is picked up by this same loop's next pass.
    /// [`Self::merge_edit_save_running`] is cleared on every real exit path, mirroring
    /// [`Self::spawn_file_save_loop`]'s own documented fix for the analogous bug class.
    fn spawn_merge_edit_save_loop(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                let step = this.update(cx, |this, _cx| {
                    if !this.merge_edit_save_pending {
                        this.merge_edit_save_running = false;
                        return None;
                    }
                    this.merge_edit_save_pending = false;
                    match this.merge_edit.as_ref() {
                        Some(edit) => Some((
                            edit.session_id,
                            edit.generation,
                            edit.buffer_id,
                            edit.base_worktree_path.clone(),
                            edit.relative_path.clone(),
                            edit.buffer.path.clone(),
                            edit.buffer.content.clone(),
                        )),
                        None => {
                            // The hand-edit was discarded (or the whole flow torn down) while a
                            // save was still pending for it - nothing left to write, but the
                            // running flag must still be cleared here too, or this slot becomes
                            // permanently unsavable for any future hand-edit.
                            this.merge_edit_save_running = false;
                            None
                        }
                    }
                });
                let Ok(Some((
                    session_id,
                    generation,
                    buffer_id,
                    base_worktree_path,
                    relative_path,
                    real_path,
                    content,
                ))) = step
                else {
                    break;
                };

                // Test-only seam - see [`AdeApp::merge_edit_save_test_delay`]'s own docs. Mirrors
                // [`AdeApp::persist_settings`]'s own identical, established seam for the same
                // real reason: letting a test deterministically hold this exact save pending
                // (parked at this timer) while it synchronously mutates `Self::merge_edit`
                // underneath it (discard, then re-open hand-edit mode for the same file - a
                // genuinely fresh buffer), to really exercise the `buffer_id` identity guard
                // below rather than just asserting its predicate in isolation.
                #[cfg(test)]
                {
                    let delay = this.update(cx, |this, _cx| this.merge_edit_save_test_delay);
                    if let Ok(Some(delay)) = delay {
                        cx.background_executor().timer(delay).await;
                    }
                }

                let relative_path_for_write = relative_path.clone();
                // Real, deliberate separation of the *write* outcome (`Err` only for a genuine
                // `std::fs::write`/`std::fs::metadata` I/O failure - the write never happened, or
                // its real mtime/len couldn't be read back) from the *re-parse* outcome
                // (`MergeEditReparseOutcome`, always `Ok` once the write itself succeeded) - a
                // real, live-reproduced bug an audit caught in an earlier version of this method,
                // which used a single `?`-chained `Result` for both: a hand-edit that leaves
                // malformed conflict markers (e.g. deleting only the real `=======` line - a
                // real, easy real mistake in a view whose whole purpose is editing those markers)
                // made `wt_core::merge::load_conflicted_file`'s own real parse fail *after* the
                // real bytes were already written to disk, which the old code then treated
                // identically to "the write itself failed": `EditBuffer::mark_saved` never ran
                // (so the buffer kept reporting dirty even though the real on-disk bytes were
                // already exactly what it held), and `files[]` kept describing the pre-write
                // content - if the user then went back to the quick-pick view (via `Self::
                // discard_merge_hand_edit`) and resolved a hunk there, `Self::resolve_active_hunk`
                // would `wt_core::merge::write_resolved_file` a *stale*, pre-hand-edit render()
                // over whatever the real hand-edit had actually just written. Fixed by always
                // trusting the write's own real success (clearing dirty via `mark_saved`
                // regardless of the re-parse outcome, since the real bytes genuinely are what's on
                // disk now) and by *never* calling `Self::apply_merge_edit_save_result` (which is
                // the only thing that ever updates `files[]` or clears `Self::merge_edit`) on a
                // malformed re-parse - see [`MergeEditReparseOutcome::Malformed`]'s own docs for
                // why that keeps hand-edit mode structurally forced open for this file (the
                // quick-pick view's Take-left/right/both buttons stay absent from the render tree
                // for it - see `crate::merge::render`'s own docs) until either a clean
                // re-parse succeeds or the user explicitly discards, closing the stale-overwrite
                // risk at its actual source rather than papering over one symptom of it.
                let write_result: Result<
                    (
                        Option<std::time::SystemTime>,
                        u64,
                        String,
                        MergeEditReparseOutcome,
                    ),
                    wt_core::Error,
                > = cx
                    .background_executor()
                    .spawn(async move {
                        std::fs::write(&real_path, content.as_bytes())?;
                        let metadata = std::fs::metadata(&real_path)?;
                        let mtime = metadata.modified().ok();
                        let len = metadata.len();
                        let reparse = match wt_core::merge::load_conflicted_file(
                            &base_worktree_path,
                            &relative_path_for_write,
                        ) {
                            Ok(fresh) => {
                                // Only the real `git add` staging can fail independently here -
                                // `fresh` itself is already a real, valid, fully-parsed
                                // `ConflictedFile` that genuinely matches what's on disk right
                                // now regardless of whether staging succeeds, so it's still
                                // correct (and important - see this method's own docs) to apply
                                // it to `files[]` even when staging fails.
                                let stage_error = if fresh.remaining_conflicts() == 0 {
                                    wt_core::merge::write_resolved_file(&base_worktree_path, &fresh)
                                        .err()
                                        .map(|err| err.to_string())
                                } else {
                                    None
                                };
                                MergeEditReparseOutcome::Parsed { fresh, stage_error }
                            }
                            Err(err) => MergeEditReparseOutcome::Malformed(err.to_string()),
                        };
                        Ok((mtime, len, content, reparse))
                    })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    match write_result {
                        Ok((mtime, len, written_content, reparse)) => {
                            // Real identity re-check (see `merge::MergeFlow::generation`'s and
                            // `merge::MergeEditState::buffer_id`'s own docs) - a stale save whose
                            // hand-edit was discarded, or whose whole merge attempt was
                            // superseded, while this write was in flight must not resurrect
                            // `Self::merge_edit` or silently apply to the wrong attempt's/
                            // buffer's state. `buffer_id` is load-bearing on top of
                            // `session_id`/`generation`/`relative_path`: a discard followed by an
                            // immediate re-open of hand-edit mode for the *same* file keeps all
                            // three of those identical but seeds a genuinely new `EditBuffer`.
                            let still_current = this.merge_edit.as_ref().is_some_and(|edit| {
                                edit.session_id == session_id
                                    && edit.generation == generation
                                    && edit.buffer_id == buffer_id
                                    && edit.relative_path == relative_path
                            });
                            if still_current {
                                // The real write itself succeeded - the buffer's own dirty flag
                                // must reflect that regardless of the re-parse outcome below,
                                // since the real bytes genuinely are what's on disk now.
                                if let Some(edit) = this.merge_edit.as_mut() {
                                    edit.buffer.mark_saved(written_content, mtime, len);
                                }
                                match reparse {
                                    MergeEditReparseOutcome::Parsed { fresh, stage_error } => {
                                        this.merge_edit_save_error = stage_error.map(|err| {
                                            format!(
                                                "saved, but staging the resolved file with git \
                                                 failed: {err}"
                                            )
                                        });
                                        this.apply_merge_edit_save_result(
                                            session_id,
                                            generation,
                                            relative_path,
                                            fresh,
                                        );
                                    }
                                    MergeEditReparseOutcome::Malformed(message) => {
                                        this.merge_edit_save_error = Some(format!(
                                            "saved, but the conflict markers are malformed - fix \
                                             them and save again: {message}"
                                        ));
                                        // Deliberately does NOT call
                                        // `Self::apply_merge_edit_save_result` - see this
                                        // method's own top docs.
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if this.merge_edit.as_ref().is_some_and(|edit| {
                                edit.session_id == session_id
                                    && edit.generation == generation
                                    && edit.buffer_id == buffer_id
                            }) {
                                this.merge_edit_save_error = Some(format!("save failed: {err}"));
                            }
                        }
                    }
                    cx.notify();
                });
            }
        });
        self._merge_edit_save_task = Some(task);
    }

    /// Applies a hand-edit save's fresh, re-parsed [`wt_core::merge::ConflictedFile`] to the
    /// owning [`merge::MergeFlow`]'s `files[]` (via `crate::merge::state::replace_conflicted_file`,
    /// matched by path), recomputes `active_file`/`active_hunk`
    /// ([`crate::merge::state::first_unresolved`]), and refreshes the highlight cache - the real
    /// state-transition point mirroring [`Self::resolve_active_hunk`]'s own identical sequence
    /// for the quick-pick path. Only applies anything if `session_id`/`generation` still match
    /// the live flow - see [`Self::spawn_merge_edit_save_loop`]'s own docs for why the caller
    /// already re-checked this once (for [`Self::merge_edit`] itself) before calling here; this
    /// method independently re-checks against [`Self::merge_flow`] too, since those are two
    /// different pieces of state that could in principle have diverged.
    fn apply_merge_edit_save_result(
        &mut self,
        session_id: SessionId,
        generation: u64,
        relative_path: PathBuf,
        fresh: wt_core::merge::ConflictedFile,
    ) {
        let Some(flow) = self.merge_flow.as_mut() else {
            return;
        };
        if flow.session_id != session_id || flow.generation != generation {
            return;
        }
        let merge::MergeFlowState::Conflicted {
            files,
            active_file,
            active_hunk,
            ..
        } = &mut flow.state
        else {
            return;
        };
        if !merge::replace_conflicted_file(files, &relative_path, fresh) {
            return;
        }
        if let Some((next_file, next_hunk)) = merge::first_unresolved(files) {
            *active_file = next_file;
            *active_hunk = next_hunk;
        }
        self.ensure_active_merge_highlight_cache();
        self.sync_merge_edit_to_active_file();
    }

    /// Test-only seam - see [`AdeApp::merge_edit_save_test_delay`]'s own docs.
    #[cfg(test)]
    pub(crate) fn set_merge_edit_save_test_delay(&mut self, delay: Option<std::time::Duration>) {
        self.merge_edit_save_test_delay = delay;
    }
}

/// The real outcome of re-parsing a hand-edit save's just-written content
/// (`wt_core::merge::load_conflicted_file`), *after* the real `std::fs::write` itself already
/// succeeded - see [`AdeApp::spawn_merge_edit_save_loop`]'s own docs for why this is kept
/// structurally separate from the write's own success/failure.
enum MergeEditReparseOutcome {
    /// The written content parsed as a real, valid [`wt_core::merge::ConflictedFile`] - genuinely
    /// safe to apply to `files[]` (via [`AdeApp::apply_merge_edit_save_result`]) regardless of
    /// `stage_error`, since `fresh` itself already matches what's on disk right now either way.
    /// `stage_error` is `Some` only when `fresh.remaining_conflicts() == 0` *and* the real `git
    /// add` inside `wt_core::merge::write_resolved_file` itself failed - a real, independent
    /// failure mode from parsing, surfaced as its own distinct, non-blocking warning rather than
    /// discarding a perfectly good `fresh` over it. `wt_core::merge::complete_merge`'s own
    /// defense-in-depth `git diff --name-only --diff-filter=U` check still correctly refuses to
    /// commit while a path stays genuinely unstaged, regardless of what this warning says.
    Parsed {
        fresh: wt_core::merge::ConflictedFile,
        stage_error: Option<String>,
    },
    /// The written content's own conflict markers are genuinely malformed (e.g. a real
    /// `Error::MergeMalformedConflictMarkers` - deleting only a hunk's `=======` line while
    /// keeping its `<<<<<<<`/`>>>>>>>` is a real, easy way to reach this in a view whose whole
    /// purpose is hand-editing those exact markers) - there is no real `ConflictedFile` to apply
    /// to `files[]` at all, so the caller must leave `files[]`/`Self::merge_edit` untouched here.
    Malformed(String),
}

/// Builds a [`merge::MergeFlowState::Error`] for `message`, best-effort populating
/// `abortable_worktree` via [`wt_core::merge::find_in_progress_merge`] rather than assuming a
/// merge is or isn't in progress just because this call failed. If that lookup itself fails,
/// `abortable_worktree` is `None`.
pub(in crate::merge) fn merge_error_state(
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
pub(in crate::merge) fn run_merge_attempt(
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
    use gpui::{EntityInputHandler, TestAppContext};
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

    /// Real parent-commit count for `rev` - the same real check `wt_core::merge`'s own test
    /// module uses to confirm a merge commit is genuinely a merge commit (two parents), not a
    /// fabricated stand-in.
    fn parent_count(dir: &std::path::Path, rev: &str) -> usize {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["cat-file", "-p", rev])
            .output()
            .expect("git cat-file");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .take_while(|line| !line.is_empty())
            .filter(|line| line.starts_with("parent "))
            .count()
    }

    fn merge_head_exists(dir: &std::path::Path) -> bool {
        wt_core::merge::merge_head_exists(dir).expect("merge_head_exists")
    }

    /// Real, bound keystroke bindings - see `crate::code_surface::editing::editing_tests::
    /// bind_real_keys`'s identical own precedent (a separate module, so not directly reusable -
    /// this is a real, deliberate duplication of one line, not a second, drifting
    /// implementation).
    fn bind_real_keys(cx: &mut gpui::VisualTestContext) {
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
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

    /// Real merge-surface zoom: the active hunk's code rows must genuinely grow with
    /// `Settings.appearance.editor_zoom_percent`, mirroring `code_surface::zoom::code_zoom_tests::
    /// zoom_scales_text_but_not_the_gutter_width`'s real-bounds-measurement shape - see
    /// `crate::merge::render::AdeApp::render_conflict_columns`'s `zoom_scoped` wrap. Reaches a real
    /// `Conflicted` state through a real `git merge` (base and feature branches each edit line 2
    /// of the same real `.rs` file differently), not a fabricated conflict string - the same
    /// real-worktree harness this module's other regression tests use.
    #[gpui::test]
    fn merge_conflict_code_rows_genuinely_grow_with_zoom(cx: &mut TestAppContext) {
        let repo = init_repo();
        fs::write(
            repo.path().join("value.rs"),
            "fn value() -> i32 {\n    1\n}\n",
        )
        .expect("write");
        git(repo.path(), &["add", "value.rs"]);
        git(repo.path(), &["commit", "-m", "seed value.rs"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("value.rs"),
            "fn value() -> i32 {\n    2\n}\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes value.rs"]);

        fs::write(feature.join("value.rs"), "fn value() -> i32 {\n    3\n}\n").expect("write");
        git(&feature, &["commit", "-am", "feature changes value.rs"]);

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

        // Hand-verified real line numbers for the conflict git produces from this fixture:
        // "fn value() -> i32 {"=1, "<<<<<<< HEAD"=2, "    2"=3 (ours_start_line), "======="=4,
        // "    3"=5 (theirs_start_line), ">>>>>>> feature"=6, "}"=7.
        let ours_start_line = app.read_with(cx, |app, _| {
            let flow = app
                .merge_flow
                .as_ref()
                .expect("merge_flow after start_merge");
            let merge::MergeFlowState::Conflicted {
                files,
                active_file,
                active_hunk,
                ..
            } = &flow.state
            else {
                panic!(
                    "expected a conflicted merge - base and feature both changed line 2 of \
                     value.rs"
                );
            };
            let ConflictedPath::Text(file) = &files[*active_file] else {
                panic!("expected a real text conflict for value.rs");
            };
            let ConflictSegment::Conflict(hunk) = &file.segments[*active_hunk] else {
                panic!("expected the active segment to still be a real conflict hunk");
            };
            hunk.ours_start_line
        });
        assert_eq!(
            ours_start_line, 3,
            "hand-verified real position - see this test's own doc comment"
        );

        cx.run_until_parked();
        let bounds_at_100 = cx.debug_bounds("merge-ours-code-row-3").expect(
            "the active hunk's real ours-side code row should have really painted at 100% zoom",
        );

        app.update(cx, |app, cx| {
            for _ in 0..10 {
                app.zoom_in(cx); // 100% -> 200%
            }
        });
        cx.run_until_parked();

        let bounds_at_200 = cx.debug_bounds("merge-ours-code-row-3").expect(
            "the active hunk's real ours-side code row should have really painted at 200% zoom",
        );

        assert!(
            bounds_at_200.size.height > bounds_at_100.size.height,
            "the real, rems()-sized merge conflict code row must genuinely grow taller at 200% \
             zoom (line-height is rems(1.6), scoped through the same zoom_scoped mechanism the \
             Diff/File views use) - got {:?} at 100% vs {:?} at 200%",
            bounds_at_100.size,
            bounds_at_200.size,
        );
    }

    /// Proves `AdeApp::merge_highlight_cache` is genuinely *reused*, not silently recomputed
    /// every time `Self::ensure_active_merge_highlight_cache` runs - pointer identity of the
    /// cached `ours` `Vec`, since a fresh recompute would allocate a new one (mirrors
    /// `code_surface::diff_view::diff_render_tests`' identical technique for `diff_highlight_cache`, itself
    /// mirroring `code_view_cache_tests`' original for `file_view_cache`). If the
    /// `(relative_path, ConflictHunk)` freshness check were ever removed from
    /// `ensure_merge_highlight_cache`, this would fail.
    #[gpui::test]
    fn repeated_cache_fills_of_the_same_active_hunk_reuse_the_cached_highlighting(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(
            repo.path().join("value.rs"),
            "fn value() -> i32 {\n    1\n}\n",
        )
        .expect("write");
        git(repo.path(), &["add", "value.rs"]);
        git(repo.path(), &["commit", "-m", "seed value.rs"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("value.rs"),
            "fn value() -> i32 {\n    2\n}\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes value.rs"]);
        fs::write(feature.join("value.rs"), "fn value() -> i32 {\n    3\n}\n").expect("write");
        git(&feature, &["commit", "-am", "feature changes value.rs"]);

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

        let first_ptr = app.read_with(cx, |app, _| {
            app.merge_highlight_cache
                .as_ref()
                .expect("merge_highlight_cache should be populated after a real conflicted merge")
                .2
                .as_ptr()
        });

        // The real hook this cache is recomputed from, called again with nothing changed.
        app.update(cx, |app, _cx| {
            app.ensure_active_merge_highlight_cache();
        });
        let second_ptr = app.read_with(cx, |app, _| {
            app.merge_highlight_cache
                .as_ref()
                .expect("merge_highlight_cache should still be populated")
                .2
                .as_ptr()
        });
        assert_eq!(
            first_ptr, second_ptr,
            "a second cache-fill call for the same, still-active hunk must reuse the cached \
             highlighting, not rebuild it (a fresh heap allocation means highlight_block ran \
             again for content that hadn't changed)"
        );
    }

    /// The other half of the same cache's correctness: advancing to a genuinely different active
    /// hunk (a different conflicted file, real two-file conflict) must recompute - not a cache
    /// that never refreshes, and not stale content from the file just resolved.
    #[gpui::test]
    fn resolving_a_hunk_and_advancing_recomputes_the_merge_highlight_cache_for_the_new_file(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    1\n}\n").expect("write");
        fs::write(repo.path().join("b.py"), "def b():\n    return 1\n").expect("write");
        git(repo.path(), &["add", "a.rs", "b.py"]);
        git(repo.path(), &["commit", "-m", "seed a.rs and b.py"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    2\n}\n").expect("write");
        fs::write(repo.path().join("b.py"), "def b():\n    return 2\n").expect("write");
        git(
            repo.path(),
            &["commit", "-am", "base changes a.rs and b.py"],
        );
        fs::write(feature.join("a.rs"), "fn a() -> i32 {\n    3\n}\n").expect("write");
        fs::write(feature.join("b.py"), "def b():\n    return 3\n").expect("write");
        git(
            &feature,
            &["commit", "-am", "feature changes a.rs and b.py"],
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

        let first_path = app.read_with(cx, |app, _| {
            app.merge_highlight_cache
                .as_ref()
                .expect("merge_highlight_cache should be populated after a real conflicted merge")
                .0
                .clone()
        });

        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Left, cx);
        });

        app.read_with(cx, |app, _| {
            let (cached_path, _hunk, ours, _theirs) = app
                .merge_highlight_cache
                .as_ref()
                .expect("merge_highlight_cache should be populated for the newly active hunk");
            assert_ne!(
                cached_path, &first_path,
                "advancing to the next conflicted file's hunk must recompute the cache for that \
                 file, not keep serving the resolved file's stale highlighting"
            );
            let has_real_content = ours
                .iter()
                .any(|line| !line.runs.is_empty() || !line.text.is_empty());
            assert!(
                has_real_content,
                "the newly active file's real conflict content should be genuinely highlighted, \
                 not an empty leftover cache entry"
            );
        });
    }

    fn secondary(key: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("cmd-{key}")
        } else {
            format!("ctrl-{key}")
        }
    }

    /// Required regression test (Revision R8.5c): hand-editing a real conflicted file's markers
    /// away through the real `EntityInputHandler`/action-handler path - a real `secondary-a`
    /// select-all keystroke, a real `replace_text_in_range` call (the same real trait method the
    /// platform text-input layer itself calls - matching `crate::code_surface::editing::editing_tests`'
    /// own established idiom for "type this text", not a private shortcut), and a real
    /// `secondary-s` save keystroke - never by mutating `EditBuffer::content` directly. Confirms
    /// the real on-disk content is marker-free, git's own unmerged-paths listing no longer
    /// includes it, and the app's own `files[]` entry reports resolved.
    #[gpui::test]
    fn hand_editing_markers_away_and_saving_through_real_keystrokes_resolves_the_file(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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
            assert!(matches!(
                flow.state,
                merge::MergeFlowState::Conflicted { .. }
            ));
        });

        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.merge_edit.is_some()),
            "hand-edit mode should be genuinely on for the active conflicted file"
        );

        cx.simulate_keystrokes(&secondary("a"));
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "line1\nBASE CHANGED\nline3\n", window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app
                .merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .content
                .clone()),
            "line1\nBASE CHANGED\nline3\n",
            "the real select-all plus replace_text_in_range should have replaced the whole real \
             buffer, markers included"
        );

        cx.simulate_keystrokes(&secondary("s"));
        cx.run_until_parked();

        let on_disk = fs::read_to_string(repo.path().join("shared.txt")).expect("read shared.txt");
        assert_eq!(
            on_disk, "line1\nBASE CHANGED\nline3\n",
            "the real on-disk content must be marker-free and hold the real hand-edited \
             resolution"
        );
        assert!(
            !on_disk.contains("<<<<<<<"),
            "no leftover conflict markers on disk"
        );
        let real_status = status(repo.path());
        assert!(
            !real_status.contains('U'),
            "git's own unmerged-paths listing must no longer include the real hand-resolved \
             file: {real_status:?}"
        );
        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow still present");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected a conflicted merge state");
            };
            assert!(
                merge::all_resolved(files),
                "the app's own files[] entry must report the file as resolved after a real \
                 hand-edit save"
            );
            assert!(
                app.merge_edit.is_none(),
                "hand-edit mode should have cleared itself once the file became fully resolved"
            );
        });
    }

    /// Required regression test: mixing a quick-pick Take-left on one hunk of a multi-hunk file
    /// with a real hand-edit-and-save resolving a *different* hunk of the *same* file - confirms
    /// both the final on-disk content and the app's own resolution state are correct for both
    /// resolution paths applied to one file.
    #[gpui::test]
    fn mixing_take_left_on_one_hunk_with_a_hand_edit_save_on_another_hunk_of_the_same_file(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        // A generous real gap (10 unchanged context lines) between the two changed lines, so
        // git's own merge genuinely produces two separate conflict hunks rather than coalescing
        // them into one (empirically verified: a 3-line gap was not enough).
        let context = "ctx\n".repeat(10);
        let original = format!("l1\nl2\n{context}l6\nl7\n");
        fs::write(repo.path().join("multi.txt"), &original).expect("write");
        git(repo.path(), &["add", "multi.txt"]);
        git(repo.path(), &["commit", "-m", "seed multi.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("multi.txt"),
            format!("l1\nBASE2\n{context}BASE6\nl7\n"),
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes multi.txt"]);
        fs::write(
            feature.join("multi.txt"),
            format!("l1\nFEATURE2\n{context}FEATURE6\nl7\n"),
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes multi.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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

        let hunk_count = app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            assert_eq!(files.len(), 1, "one conflicted file expected");
            let ConflictedPath::Text(file) = &files[0] else {
                panic!("expected a text conflict");
            };
            merge::hunk_count(file)
        });
        assert_eq!(
            hunk_count, 2,
            "the fixture must produce two real, separate conflict hunks"
        );

        // Quick-pick resolve the first hunk via Take-left.
        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Left, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            let ConflictedPath::Text(file) = &files[0] else {
                panic!("expected text");
            };
            assert_eq!(
                file.remaining_conflicts(),
                1,
                "exactly one hunk should remain after Take-left resolved the first one"
            );
        });

        // Hand-edit the remaining hunk - seeded from the real, live, partially-resolved
        // in-memory state (hunk 1 already resolved via Take-left, hunk 2 still real markers).
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let seeded = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .content
                .clone()
        });
        assert!(
            seeded.starts_with(&format!("l1\nBASE2\n{context}")),
            "the hand-edit buffer must be seeded from the real in-memory \
             ConflictedFile::render() - hunk 1 already resolved via Take-left: {seeded:?}"
        );
        assert!(
            seeded.contains("<<<<<<<") && seeded.contains("BASE6") && seeded.contains("FEATURE6"),
            "hunk 2 must still show its real, unresolved markers: {seeded:?}"
        );

        let resolved = format!("l1\nBASE2\n{context}FEATURE6\nl7\n");
        cx.simulate_keystrokes(&secondary("a"));
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, &resolved, window, cx);
        });
        cx.simulate_keystrokes(&secondary("s"));
        cx.run_until_parked();

        let on_disk = fs::read_to_string(repo.path().join("multi.txt")).expect("read multi.txt");
        assert_eq!(
            on_disk, resolved,
            "the real final on-disk content must reflect both resolutions - Take-left for hunk \
             1, the real hand-edit for hunk 2"
        );
        assert!(!on_disk.contains("<<<<<<<"));
        let real_status = status(repo.path());
        assert!(
            !real_status.contains('U'),
            "no remaining unmerged marker: {real_status:?}"
        );
        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            assert!(merge::all_resolved(files));
            assert!(app.merge_edit.is_none());
        });
    }

    /// Required end-to-end regression test: a real conflicting `git merge`, resolved through a
    /// genuine mix of both resolution paths (one file via quick-pick Take-both, the other via a
    /// real hand-edit save), then a real `Complete merge` producing a real merge commit with the
    /// correct parent count and correct final content on disk.
    #[gpui::test]
    fn real_end_to_end_merge_resolved_through_a_mix_of_both_paths_produces_a_real_merge_commit(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("a.txt"), "a1\na2\na3\n").expect("write");
        fs::write(repo.path().join("b.txt"), "b1\nb2\nb3\n").expect("write");
        git(repo.path(), &["add", "a.txt", "b.txt"]);
        git(repo.path(), &["commit", "-m", "seed a.txt and b.txt"]);

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(repo.path().join("a.txt"), "a1\nBASE A\na3\n").expect("write");
        fs::write(repo.path().join("b.txt"), "b1\nBASE B\nb3\n").expect("write");
        git(
            repo.path(),
            &["commit", "-am", "base changes a.txt and b.txt"],
        );
        fs::write(feature.join("a.txt"), "a1\nFEATURE A\na3\n").expect("write");
        fs::write(feature.join("b.txt"), "b1\nFEATURE B\nb3\n").expect("write");
        git(
            &feature,
            &["commit", "-am", "feature changes a.txt and b.txt"],
        );

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            assert_eq!(files.len(), 2, "both a.txt and b.txt should be conflicted");
        });

        // a.txt resolved entirely through the quick-pick Take-both path.
        app.update(cx, |app, cx| {
            app.resolve_active_hunk(wt_core::merge::ConflictChoice::Both, cx);
        });
        cx.run_until_parked();

        // b.txt resolved entirely by hand.
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let hand_edit_path = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit")
                .relative_path
                .clone()
        });
        assert_eq!(hand_edit_path, PathBuf::from("b.txt"));
        cx.simulate_keystrokes(&secondary("a"));
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "b1\nBASE B\nFEATURE B\nb3\n", window, cx);
        });
        cx.simulate_keystrokes(&secondary("s"));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            assert!(
                merge::all_resolved(files),
                "both files should be genuinely resolved now"
            );
        });

        app.update(cx, |app, cx| app.complete_merge_flow(cx));
        cx.run_until_parked();

        assert!(!merge_head_exists(repo.path()));
        assert_eq!(status(repo.path()), "");
        assert_eq!(
            parent_count(repo.path(), "HEAD"),
            2,
            "a real merge commit must have two parents"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "a1\nBASE A\nFEATURE A\na3\n"
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("b.txt")).expect("read b.txt"),
            "b1\nBASE B\nFEATURE B\nb3\n"
        );
        assert!(app.read_with(cx, |app, _| app.merge_flow.is_none()));
        assert!(app.read_with(cx, |app, _| app.merge_edit.is_none()));
    }

    /// Required regression test: this exact bug class ("a shortcut steals a keystroke a text
    /// field needed") has shipped six separate times in this codebase per BUILD-LOG (Revisions
    /// R2, R4a, R4b, R8.5a, R8.5b) - proves the new `"merge-editor"` key context does not swallow
    /// a keystroke while a *different* surface (here, the File view) is what's actually focused,
    /// even while a merge hand-edit is genuinely still alive in the background for a different
    /// session.
    #[gpui::test]
    fn merge_editor_context_does_not_swallow_keystrokes_meant_for_a_different_focused_surface(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("notes.txt"), "hello\n").expect("write");
        git(repo.path(), &["add", "notes.txt"]);
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "seed notes.txt and shared.txt"],
        );

        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
        let notes_path = repo.path().join("notes.txt");
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
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let merge_edit_content_before = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .content
                .clone()
        });
        assert!(app.read_with(cx, |app, _| app.merge_edit.is_some()));

        // Switch away entirely - open a plain file in File view (Surface C), which per
        // `crate::work_surface::render::AdeApp::render_center_pane`'s own visibility rule
        // takes over the whole center pane regardless of `merge_flow`.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(notes_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        // A real, ordinary keystroke dispatched through the *real* window/key-context pipeline
        // (`cx.simulate_input`, not a direct `AdeApp::replace_text_in_range` call) - this is the
        // real property that matters: if `"merge-editor"`'s own context/bindings ever leaked
        // onto this now-focused File-view surface's own node (or bound globally by mistake),
        // this is the dispatch path that would actually expose it - a direct method call proves
        // only the routing *function's* logic, never the real key-context wiring itself.
        cx.simulate_input("X");

        let relative_notes = PathBuf::from("notes.txt");
        let file_content = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative_notes)
                .expect("notes.txt buffer")
                .content
                .clone()
        });
        assert_eq!(
            file_content, "Xhello\n",
            "the real keystroke must land in the real, currently-focused File-view buffer"
        );
        let merge_edit_content_after = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit still present in the background")
                .buffer
                .content
                .clone()
        });
        assert_eq!(
            merge_edit_content_after, merge_edit_content_before,
            "the backgrounded merge hand-edit buffer must be completely untouched by a \
             keystroke meant for the now-focused File view"
        );

        // The other real, reachable direction (the exact state Revision R8.5c's own audit found
        // a real, live bug in - `crate::code_surface::editing::AdeApp::active_edit_target`'s docs): a
        // "stale active tab" - `open_change` still `Some`, no diff to show it, `code_view` left
        // on `Diff` - under which Surface C is genuinely *not* shown at all and the merge
        // hand-edit (still open the whole time, above) is what's genuinely on screen. A real
        // dispatched keystroke here must reach the merge buffer, not be swallowed.
        app.update(cx, |app, cx| {
            app.open_change = Some(PathBuf::from("some/stale/tab.txt"));
            app.code_view = code_view::CodeView::Diff;
            app.open_diff_file_cache = None;
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        // Real window focus is its own, separate concept from render state in GPUI - switching
        // which surface *renders* does not, by itself, move a focus that was left on a node no
        // longer part of the tree (`code_focus_handle`, from opening the File view above) back
        // onto anything real. A real user reaches this exact state by clicking back into the
        // merge editor (the same real `window.focus(&self.merge_edit_focus_handle, cx)` call
        // `crate::merge::editing`'s own row click handler makes) - simulated directly here
        // for the same reason `crate::code_surface`'s own click handlers are the real
        // mechanism, not a render-time side effect.
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.merge_edit_focus_handle, cx);
        });

        cx.simulate_input("Y");

        let merge_edit_content_final = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit still present")
                .buffer
                .content
                .clone()
        });
        assert_eq!(
            merge_edit_content_final,
            format!("Y{merge_edit_content_after}"),
            "a real dispatched keystroke must reach the genuinely on-screen merge hand-edit \
             buffer once `open_change` is Some but Surface C itself is not actually showing - \
             got {merge_edit_content_final:?}"
        );
        let file_content_after_stale_tab = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative_notes)
                .expect("notes.txt buffer")
                .content
                .clone()
        });
        assert_eq!(
            file_content_after_stale_tab, file_content,
            "the File view's own buffer must stay completely untouched once it is no longer \
             the genuinely on-screen surface"
        );
    }

    /// Required identity/staleness regression test: starting a hand-edit, then ending the merge
    /// flow (here, abort), must genuinely tear down [`AdeApp::merge_edit`] (not merely hide it),
    /// and a fresh, unrelated merge started afterward must neither resurrect it nor be corruptible
    /// by a stale result for the old attempt - even one carrying the old attempt's own real
    /// `session_id`/`generation` identity, directly proving `Self::apply_merge_edit_save_result`'s
    /// own guard rather than relying on timing to reproduce the race.
    #[gpui::test]
    fn ending_a_merge_flow_tears_down_hand_edit_state_and_a_fresh_merge_cannot_be_polluted_by_it(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let (stale_session_id, stale_generation, stale_relative_path) =
            app.read_with(cx, |app, _| {
                let edit = app.merge_edit.as_ref().expect("merge_edit");
                (edit.session_id, edit.generation, edit.relative_path.clone())
            });

        // Abort the merge - a real exit point.
        app.update(cx, |app, cx| app.abort_merge_flow(cx));
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.merge_flow.is_none()));
        assert!(
            app.read_with(cx, |app, _| app.merge_edit.is_none()),
            "the hand-edit slot must be genuinely gone, not merely hidden, once the flow ends"
        );
        assert!(!merge_head_exists(repo.path()));
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read"),
            "line1\nBASE CHANGED\nline3\n",
            "a real `git merge --abort` must have restored the real pre-merge content"
        );

        // A fresh, unrelated (also conflicting) merge attempt against the same repository must
        // succeed normally and must never resurrect the old hand-edit state.
        fs::write(repo.path().join("second.txt"), "s1\ns2\ns3\n").expect("write");
        git(repo.path(), &["add", "second.txt"]);
        git(repo.path(), &["commit", "-m", "seed second.txt"]);
        let second_feature = add_worktree(repo.path(), "second-feature", "second-feature-wt");
        fs::write(repo.path().join("second.txt"), "s1\nBASE SECOND\ns3\n").expect("write");
        git(repo.path(), &["commit", "-am", "base changes second.txt"]);
        fs::write(
            second_feature.join("second.txt"),
            "s1\nFEATURE SECOND\ns3\n",
        )
        .expect("write");
        git(
            &second_feature,
            &["commit", "-am", "second feature changes second.txt"],
        );
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
                matches!(flow.state, merge::MergeFlowState::Conflicted { .. }),
                "the fresh, independent merge must succeed (conflicted, as this fixture \
                 produces), unaffected by the earlier aborted attempt"
            );
            assert_ne!(
                flow.generation, stale_generation,
                "a fresh attempt must have a genuinely new generation, never reusing the old one"
            );
        });
        assert!(
            app.read_with(cx, |app, _| app.merge_edit.is_none()),
            "the fresh merge must not have resurrected the old hand-edit state"
        );

        // Direct identity-guard proof: a stale hand-edit save result carrying the *old*
        // attempt's own real `session_id`/`generation` must be a real, verified no-op against
        // the live, fresh flow - never silently applied to it.
        let injected = wt_core::merge::load_conflicted_file(repo.path(), Path::new("shared.txt"))
            .expect("load_conflicted_file");
        app.update(cx, |app, _cx| {
            app.apply_merge_edit_save_result(
                stale_session_id,
                stale_generation,
                stale_relative_path,
                injected,
            );
        });
        app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow still present");
            assert_eq!(
                flow.session_id, second_session_id,
                "the stale call must not have touched the live, fresh flow's own identity"
            );
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected the live flow to still be conflicted");
            };
            assert_eq!(
                files.len(),
                1,
                "the stale call must be a real no-op - the live flow's own files[] must be \
                 exactly what the fresh attempt produced, untouched by the injected stale result"
            );
        });
    }

    /// Regression test for a real, live-reproduced bug an audit caught: an earlier version of
    /// `crate::code_surface::editing::AdeApp::active_edit_target` returned `None` whenever
    /// `AdeApp::open_change` was `Some`, but `crate::work_surface::render::AdeApp::
    /// render_center_pane`'s own real Surface-C-visibility predicate is stronger than that -
    /// `open_change.is_some() && (open_diff_file_cache.is_some() || code_view ==
    /// CodeView::File)`. A real, reachable state falls outside that predicate while `open_change`
    /// is still `Some`: `crate::code_surface::tabs::AdeApp::refresh_open_diff_file_cache`
    /// recomputes `open_diff_file_cache` from whatever `open_change` names against the *current*
    /// diff - if that file's diff has since disappeared (e.g. reverted externally, then
    /// `Self::load_diff` reruns) while `code_view` is still `Diff` (never switched to `File`),
    /// `open_diff_file_cache` goes back to `None` with `open_change` untouched - exactly the state
    /// `crate::code_surface::tabs::AdeApp::activate_file_tab`'s own doc comment describes ("the
    /// tab can be active without being shown"). `render_center_pane` then genuinely falls through
    /// to the session/merge surface with `open_change` still `Some` the whole time. Set directly
    /// here (the same established precedent `status_bar::render`'s own tests already use for this
    /// exact field pair) rather than chasing the full multi-step live path, for a deterministic
    /// reproduction of the *state* the routing bug actually depends on - what matters for this
    /// test is that `open_change`/`code_view`/`open_diff_file_cache` hold exactly the real values
    /// `refresh_open_diff_file_cache` can genuinely produce, not which sequence of clicks got
    /// there. Confirms a real keystroke (`EntityInputHandler::replace_text_in_range`) still
    /// reaches the genuinely on-screen merge hand-edit buffer in that state.
    #[gpui::test]
    fn merge_hand_edit_keystrokes_reach_the_buffer_even_while_open_change_is_some_but_surface_c_is_not_shown(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let before = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .content
                .clone()
        });

        // Reproduce the exact real field state described above: a "stale active tab" with no
        // diff to show it and `code_view` left on `Diff` - the state that must NOT be mistaken
        // for "Surface C is showing".
        app.update(cx, |app, cx| {
            app.open_change = Some(PathBuf::from("some/stale/tab.txt"));
            app.code_view = code_view::CodeView::Diff;
            app.open_diff_file_cache = None;
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        // Real key dispatch (`cx.simulate_input`, not a direct `AdeApp::replace_text_in_range`
        // call) - the real property under test is that the platform's real window/key-context
        // dispatch pipeline itself routes here, not just that the routing function would say so
        // if asked directly.
        cx.simulate_input("X");

        let after = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit still present")
                .buffer
                .content
                .clone()
        });
        assert_eq!(
            after,
            format!("X{before}"),
            "a real keystroke must reach the genuinely on-screen merge hand-edit buffer even \
             while `open_change` is Some but Surface C itself is not actually showing - got \
             {after:?}, expected the keystroke prepended to {before:?}"
        );
    }

    /// Regression test for a real, live-reproduced bug an audit caught: saving a hand-edit whose
    /// own markers are genuinely malformed (here: deleting only the real `=======` line, keeping
    /// `<<<<<<<`/`>>>>>>>` - a real, easy mistake) must still record the real, successful
    /// `std::fs::write` (clearing the buffer's own dirty flag, since the real bytes on disk now
    /// genuinely match it) while leaving `files[]`/`Self::merge_edit` untouched - never silently
    /// treating "the write succeeded but the re-parse failed" the same as "the write itself
    /// failed" (which an earlier version of `Self::spawn_merge_edit_save_loop` did, leaving the
    /// buffer wrongly dirty forever and `files[]` describing stale, pre-write content).
    #[gpui::test]
    fn saving_malformed_markers_still_records_the_real_write_without_corrupting_files(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();

        let files_before = app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            files.clone()
        });

        // Real markers with only the `=======` line deleted - genuinely malformed, per
        // `wt_core::merge::parse_conflict_segments`'s own real parser.
        let malformed =
            "line1\n<<<<<<< HEAD\nBASE CHANGED\nFEATURE CHANGED\n>>>>>>> feature\nline3\n";
        cx.simulate_keystrokes(&secondary("a"));
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, malformed, window, cx);
        });
        cx.simulate_keystrokes(&secondary("s"));
        cx.run_until_parked();

        let on_disk = fs::read_to_string(repo.path().join("shared.txt")).expect("read shared.txt");
        assert_eq!(
            on_disk, malformed,
            "the real write must have genuinely happened - the malformed markers are real \
             content the user asked to save, not something this pipeline may silently refuse to \
             write"
        );

        app.read_with(cx, |app, _| {
            let edit = app.merge_edit.as_ref().expect(
                "hand-edit mode must stay genuinely open - a malformed re-parse must \
                          not clear it",
            );
            assert!(
                !edit.buffer.is_dirty(),
                "the buffer's own dirty flag must be cleared - the real on-disk bytes now \
                 genuinely match the buffer's content, regardless of the re-parse outcome"
            );
            let error = app
                .merge_edit_save_error
                .as_ref()
                .expect("a real, distinct error must be surfaced for the malformed re-parse");
            assert!(
                error.contains("malformed"),
                "the error must describe a malformed re-parse, not a generic write failure: \
                 {error:?}"
            );
        });

        let files_after = app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow still present");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            files.clone()
        });
        assert_eq!(
            files_after, files_before,
            "files[] must be completely untouched by a malformed re-parse - there is no real, \
             valid ConflictedFile to apply, so the pre-save state (still describing what's \
             genuinely known-valid) must be preserved exactly, not silently left half-updated"
        );
    }

    /// Regression test for the real, narrow race `merge::MergeEditState::buffer_id` exists to
    /// close (found by reading, then genuinely reproduced here using the same real, established
    /// test-only-delay seam `AdeApp::persist_settings`'s own tests already use for the analogous
    /// settings-save race - `AdeApp::set_merge_edit_save_test_delay`): a save dispatched against
    /// one hand-edit buffer, discarded and immediately replaced by a genuinely fresh buffer for
    /// the *same* file (same session/generation/relative_path) *before* the first save's real
    /// background write lands, must not let that stale completion apply itself to the new
    /// buffer/state at all - not `EditBuffer::mark_saved` (which would wrongly stamp the new,
    /// untouched buffer as having saved the old buffer's content, corrupting its own dirty-state
    /// bookkeeping) and not `AdeApp::apply_merge_edit_save_result` (which would even wrongly
    /// resolve `files[]` and silently clear the new hand-edit out from under the user, since the
    /// stale write's own content happens to have fully resolved the file).
    #[gpui::test]
    fn a_stale_save_completing_after_discard_and_a_fresh_reopen_does_not_corrupt_the_new_buffer(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        fs::write(repo.path().join("shared.txt"), "line1\nline2\nline3\n").expect("write");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "-m", "seed shared.txt"]);
        let feature = add_worktree(repo.path(), "feature", "feature-wt");
        fs::write(
            repo.path().join("shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
        )
        .expect("write");
        git(repo.path(), &["commit", "-am", "base changes shared.txt"]);
        fs::write(
            feature.join("shared.txt"),
            "line1\nFEATURE CHANGED\nline3\n",
        )
        .expect("write");
        git(&feature, &["commit", "-am", "feature changes shared.txt"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        bind_real_keys(cx);
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

        // Buffer A: hand-edit, resolve it fully, and dispatch a save with a long artificial
        // delay before its real write - so the save is genuinely still pending (parked at the
        // delay timer) once this test synchronously discards and reopens below.
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        cx.run_until_parked();
        let buffer_id_a = app.read_with(cx, |app, _| {
            app.merge_edit.as_ref().expect("merge_edit").buffer_id
        });
        cx.simulate_keystrokes(&secondary("a"));
        app.update_in(cx, |app, window, cx| {
            app.replace_text_in_range(None, "line1\nBASE CHANGED\nline3\n", window, cx);
        });
        app.update(cx, |app, cx| {
            app.set_merge_edit_save_test_delay(Some(std::time::Duration::from_millis(200)));
            app.save_merge_edit(cx);
        });
        // Parks at the delay timer - buffer A's own identity/content has already been captured
        // by the save loop's first step, but its real write hasn't happened yet.
        cx.run_until_parked();

        // Discard buffer A, then immediately reopen hand-edit mode for the *same* file - a
        // genuinely fresh buffer B, still fully synchronous (no yield since the discard), so
        // buffer A's still-parked save cannot have observed any of this yet.
        app.update(cx, |app, cx| {
            app.discard_merge_hand_edit(cx);
        });
        assert!(app.read_with(cx, |app, _| app.merge_edit.is_none()));
        app.update_in(cx, |app, window, cx| {
            app.start_merge_hand_edit(window, cx);
        });
        let buffer_id_b = app.read_with(cx, |app, _| {
            app.merge_edit.as_ref().expect("merge_edit").buffer_id
        });
        assert_ne!(
            buffer_id_a, buffer_id_b,
            "the reopened hand-edit must be a genuinely fresh buffer, not the same identity as \
             the one whose save is still pending"
        );
        let content_b_before = app.read_with(cx, |app, _| {
            app.merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .content
                .clone()
        });
        assert!(
            !app.read_with(cx, |app, _| app
                .merge_edit
                .as_ref()
                .expect("merge_edit")
                .buffer
                .is_dirty()),
            "a freshly (re-)opened hand-edit buffer must start out genuinely clean"
        );

        // Release buffer A's delayed save and let it run to completion.
        for _ in 0..60 {
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(10));
            cx.run_until_parked();
        }

        // Buffer A's own write is still a real, unconditional side effect (there is no way to
        // cancel a write already dispatched to the background executor) - the real bytes on
        // disk now hold buffer A's real resolved content.
        assert_eq!(
            fs::read_to_string(repo.path().join("shared.txt")).expect("read shared.txt"),
            "line1\nBASE CHANGED\nline3\n",
            "buffer A's own real write must still have genuinely happened"
        );

        // The buffer-identity guard's own real job: buffer B must be completely untouched by
        // buffer A's stale completion.
        app.read_with(cx, |app, _| {
            let edit = app.merge_edit.as_ref().expect(
                "the fresh hand-edit (buffer B) must still be genuinely open - a stale \
                 completion for a different buffer must not silently clear it out from under \
                 the user",
            );
            assert_eq!(
                edit.buffer_id, buffer_id_b,
                "merge_edit must still be buffer B, not resurrected/replaced by anything from \
                 buffer A's stale completion"
            );
            assert_eq!(
                edit.buffer.content, content_b_before,
                "buffer B's own content must be completely untouched"
            );
            assert!(
                !edit.buffer.is_dirty(),
                "buffer B must still report itself as genuinely clean - a real bug this guard \
                 fixes let a stale completion stamp buffer A's own written content as buffer \
                 B's saved_content while B's real content was still the original, unedited \
                 seed, which would have made B wrongly report itself as dirty against content \
                 it never actually held"
            );
        });
        let files_after = app.read_with(cx, |app, _| {
            let flow = app.merge_flow.as_ref().expect("merge_flow still present");
            let merge::MergeFlowState::Conflicted { files, .. } = &flow.state else {
                panic!("expected conflicted");
            };
            files.clone()
        });
        assert_eq!(
            files_after.len(),
            1,
            "files[] must still describe the real, original unresolved hunk - buffer A's stale \
             completion must never have been allowed to apply its own resolved content to it"
        );
        let ConflictedPath::Text(file) = &files_after[0] else {
            panic!("expected text");
        };
        assert_eq!(
            file.remaining_conflicts(),
            1,
            "the real hunk must still be genuinely unresolved in files[]"
        );
    }
}
