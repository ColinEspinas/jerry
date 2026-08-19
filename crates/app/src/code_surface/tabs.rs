//! Opening, activating and closing Surface C's file and diff tabs, plus the background
//! loads behind them (`wt_core::diff` for a diff, `std::fs::read_to_string` +
//! tree-sitter for a file). No drawing happens here - see the sibling `render`/
//! `diff_view`/`file_view` modules for that.

use super::*;
#[cfg(test)]
use crate::code_surface::fixtures::temp_repo;
#[cfg(test)]
use crate::test_support::open_test_app;

impl AdeApp {
    /// Loads (or reloads) the diff of `root` against its detected base branch. Runs on
    /// `cx.background_executor()` since `diff_against_base` does blocking I/O (gix reads plus a
    /// spawned `git diff` process) and must not run on the GPUI foreground thread.
    pub(crate) fn load_diff(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.diff_root = root.clone();
        self.diff_state = DiffLoadState::Loading;
        // Cleared with the diff it was derived from: a change set outliving its diff would keep
        // rows' diffstats alive for a worktree that is no longer being shown.
        self.change_set = crate::provenance::change_set::ChangeSet::default();
        // GitHub issue #285: the Changes panel's other two git scopes reload with this one, from
        // this one call, so its four sections can never describe two different moments in git's
        // history. Reset to `Loading` synchronously here for the same reason `dirty_files` is
        // reset to `None` below - a stale answer must never outlive the diff it described.
        self.uncommitted_diff = crate::sidebar::sections::ScopeLoad::Loading;
        self.uncommitted_change_set = crate::provenance::change_set::ChangeSet::default();
        self.branch_commits = crate::sidebar::sections::ScopeLoad::Loading;
        self.diff_totals = None;
        self.dirty_files = None;
        cx.notify();
        self._load_diff_task = Some(self.spawn_diff_reload(root, cx));
    }

    /// Stashes [`Self::diff_root`]'s landed Changes content under its root, for
    /// [`Self::load_diff_for_switch`] to paint straight back on the next visit (GitHub issue
    /// #454). Refuses to record an in-flight `Loading` (nothing worth restoring) or a root that
    /// no longer exists on disk (a discarded worktree's content must not be resurrected by the
    /// switch away from it).
    pub(crate) fn stash_changes_snapshot(&mut self) {
        if matches!(self.diff_state, DiffLoadState::Loading) {
            return;
        }
        if !self.diff_root.is_dir() {
            return;
        }
        let snapshot = ChangesSnapshot {
            diff_state: std::mem::replace(&mut self.diff_state, DiffLoadState::Loading),
            diff_totals: self.diff_totals.take(),
            change_set: std::mem::take(&mut self.change_set),
            uncommitted_diff: std::mem::take(&mut self.uncommitted_diff),
            uncommitted_change_set: std::mem::take(&mut self.uncommitted_change_set),
            branch_commits: std::mem::take(&mut self.branch_commits),
            dirty_files: self.dirty_files.take(),
        };
        self.changes_by_worktree
            .insert(self.diff_root.clone(), snapshot);
    }

    /// [`Self::load_diff`] for a worktree switch (GitHub issue #454): when `root` has stashed
    /// last-known content, restore it and reload without blanking - [`Self::refresh_diff`]'s own
    /// no-blank shape - so the switch paints instantly. A first-ever visit falls back to the
    /// honest blanking load.
    pub(crate) fn load_diff_for_switch(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some(snapshot) = self.changes_by_worktree.remove(&root) else {
            self.load_diff(root, cx);
            return;
        };
        self.diff_root = root.clone();
        self.diff_state = snapshot.diff_state;
        self.diff_totals = snapshot.diff_totals;
        self.change_set = snapshot.change_set;
        self.uncommitted_diff = snapshot.uncommitted_diff;
        self.uncommitted_change_set = snapshot.uncommitted_change_set;
        self.branch_commits = snapshot.branch_commits;
        self.dirty_files = snapshot.dirty_files;
        cx.notify();
        self._load_diff_task = Some(self.spawn_diff_reload(root, cx));
    }

    /// Re-runs exactly the same real git reload [`Self::load_diff`] does, for the *same* worktree
    /// [`Self::diff_root`] already names - GitHub issue #399. The one difference is what it does
    /// **not** do: unlike `load_diff`, this never blanks [`Self::diff_state`]/
    /// [`Self::uncommitted_diff`]/[`Self::branch_commits`] to `Loading` first, so whatever the
    /// Changes panel is already showing keeps rendering, undisturbed, for the real duration of the
    /// reload - only flipping once the fresh answer actually lands.
    pub(crate) fn refresh_diff(&mut self, cx: &mut Context<Self>) {
        let root = self.diff_root.clone();
        self._load_diff_task = Some(self.spawn_diff_reload(root, cx));
    }

    /// Re-runs the same reload [`Self::refresh_diff`] does, but only when no reload is already in
    /// flight - GitHub issue #415. The watcher/poll loop driving this fires far faster than the
    /// five git queries take on a large worktree, so an ungated call would cancel its own
    /// still-running predecessor on every tick (assigning [`Self::_load_diff_task`] drops the
    /// previous task) and the pane could starve indefinitely while an agent keeps writing.
    /// Returns whether a reload really started - `false` means the skip-as-busy path ran, and a
    /// caller tracking "has the state I observed been reloaded" (the poll fingerprint,
    /// GitHub issue #473) must not mark it observed.
    pub(crate) fn refresh_diff_if_idle(&mut self, cx: &mut Context<Self>) -> bool {
        if self.diff_reload_in_flight {
            return false;
        }
        self.refresh_diff(cx);
        true
    }

    /// The real background git reload both [`Self::load_diff`] and [`Self::refresh_diff`] run -
    /// the five real `wt_core` queries (`diff_against_base`, `staged_paths`, `dirty_paths`,
    /// `diff_against_head`, `commits_since_base`), off the foreground thread, folded into exactly
    /// the same completion write-back either caller wants. The two callers differ only in what
    /// they do *before* this runs (see [`Self::refresh_diff`]'s own docs), never in what happens
    /// once the real answer comes back.
    fn spawn_diff_reload(&mut self, root: PathBuf, cx: &mut Context<Self>) -> gpui::Task<()> {
        // Set here rather than in the two callers so every spawn is covered by construction,
        // including the one that cancels a still-running predecessor: the replacement task owns
        // the flag from this point, and its own completion below is what clears it.
        self.diff_reload_in_flight = true;
        cx.spawn(async move |this, cx| {
            let (diff_result, staged_result, dirty_result, uncommitted_result, commits_result) = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move {
                        // Fold the +n/-n totals here, off the UI thread, rather than
                        // recomputing them on every render.
                        let diff_result = wt_core::diff::diff_against_base(&root).map(|base| {
                            let totals = base.diff().map(|diff| {
                                diff.files.iter().fold((0u32, 0u32), |(add, del), file| {
                                    let (file_add, file_del) = changes::diff_file_stats(file);
                                    (add + file_add, del + file_del)
                                })
                            });
                            (base, totals)
                        });
                        let staged_result = wt_core::stage::staged_paths(&root);
                        let dirty_result = wt_core::stage::dirty_paths(&root);
                        // GitHub issue #285's two new scopes, on the same background hop: the
                        // Changes panel's Uncommitted and Commits sections. Both are separate
                        // `git` calls because they are separate questions - see
                        // `wt_core::diff::diff_against_head`'s and
                        // `wt_core::diff::commits_since_base`' own docs.
                        let uncommitted_result = wt_core::diff::diff_against_head(&root);
                        let commits_result = wt_core::diff::commits_since_base(&root);
                        (
                            diff_result,
                            staged_result,
                            dirty_result,
                            uncommitted_result,
                            commits_result,
                        )
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.diff_reload_in_flight = false;
                match diff_result {
                    Ok((base, totals)) => {
                        this.diff_state = DiffLoadState::Loaded(base);
                        this.diff_totals = totals;
                    }
                    Err(err) => {
                        this.diff_state = DiffLoadState::Error(err.to_string());
                        this.diff_totals = None;
                    }
                }
                // A failed `staged_paths` query (an unreadable/non-git `root`, the same real
                // failure modes `diff_against_base` itself can hit) leaves `staged_files` as it
                // was rather than silently emptying a real staged set this call simply couldn't
                // confirm - `diff_state`'s own `Error` arm above already surfaces the underlying
                // problem honestly.
                if let Ok(staged) = staged_result {
                    this.staged_files = staged;
                }
                // A failed `dirty_paths` query leaves `dirty_files` exactly as it was - `None`
                // ("not known") for a `Self::load_diff` caller, which reset it synchronously
                // before this task started, or the last real answer for a `Self::refresh_diff`
                // caller, which did not. Either way every Changes row falls back to the ordinary
                // stageable presentation (or the last real one) rather than silently declaring
                // every file committed-clean off the back of an answer git never actually gave.
                if let Ok(dirty) = dirty_result {
                    this.dirty_files = Some(dirty);
                }
                // An unborn `HEAD` has no working-tree-against-`HEAD` answer at all
                // (`Ok(None)`), which is a real, distinct state from a clean checkout
                // (`Ok(Some(<empty diff>))`) - so it is surfaced as its own message rather than
                // rendered as "no uncommitted changes", which would be a claim git never made.
                this.uncommitted_diff = match uncommitted_result {
                    Ok(Some(diff)) => crate::sidebar::sections::ScopeLoad::Loaded(diff),
                    Ok(None) => crate::sidebar::sections::ScopeLoad::Error(
                        "no commit to compare the working tree against yet".to_string(),
                    ),
                    Err(err) => crate::sidebar::sections::ScopeLoad::Error(err.to_string()),
                };
                this.branch_commits = match commits_result {
                    Ok(commits) => crate::sidebar::sections::ScopeLoad::Loaded(commits),
                    Err(err) => crate::sidebar::sections::ScopeLoad::Error(err.to_string()),
                };
                // The reloaded diff may have changed whether `open_change`'s path has a
                // `DiffFile`, so refresh the cache immediately rather than leaving it stale.
                this.refresh_open_diff_file_cache();
                this.rebuild_palette_file_candidates();
                // GitHub issue #284: the Changes rows read their diffstat off the change set, so
                // it has to be rebuilt from the same write that replaced the diff it is derived
                // from - see `crate::provenance::flow::AdeApp::rebuild_change_set`.
                this.rebuild_change_set();
                cx.notify();
            });
        })
    }

    /// Appends `path` to [`Self::open_files`] if not already present. The shared entry point for
    /// every source that opens a file tab, so the tab list can't drift from what `open_change`
    /// points at.
    fn push_open_file(&mut self, path: &Path) {
        if !self.open_files().iter().any(|open| open.as_path() == path) {
            self.open_files_mut().push(path.to_path_buf());
        }
    }

    /// **The** "open this file and make it the thing the user is looking at and typing into"
    /// action - the single real code path behind every entry point that opens a file: a Files
    /// tree row click, a Changes row click, a command-palette file result, go-to-definition, a
    /// terminal path link, and a just-created file. [`Self::open_file_view`] and
    /// [`Self::open_change_diff`] are now thin `view`-choosing wrappers over this, not two
    /// parallel implementations.
    fn open_and_focus_file(
        &mut self,
        relative: PathBuf,
        absolute: PathBuf,
        view: code_view::CodeView,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Settings *replaces* the workspace body (`crate::root::AdeApp::render`), so focusing the
        // code surface while it is up would pin `Window::focus` to a handle that is not in the
        // rendered frame at all - the exact dangling-focus state `crate::root::OverlayFocus`'
        // docs describe, and one that leaves every context-scoped binding (Esc included) dead
        // until the next click. Found by this branch's own adversarial audit, which reproduced it
        // through the real palette. Closing Settings is the honest resolution rather than
        // declining to focus: the user asked to open a file, and leaving the Settings page on
        // screen would be showing them something else.
        if self.settings_open {
            self.close_settings(window, cx);
        }
        // The git graph tab, if showing, replaces this exact centre-pane slot (`Self::
        // render_center_pane`'s own `graph_tab_active` check) - leaving it *before*
        // `focus_code_surface` below moves real keyboard focus off `graph_focus_handle` first, so
        // its own `code_focus.capture()` never captures a handle that's about to stop being
        // rendered. See `crate::graph_view::render::AdeApp::leave_graph_tab`'s own docs.
        self.leave_graph_tab(window, cx);
        // GitHub issue #225: the review tab occupies the centre pane exactly as the graph tab
        // does, so it needs the identical teardown here. Without this, `review_tab_active` stayed
        // set, `render_center_pane` kept returning the review body, and the tab this call is
        // switching *to* never mounted at all - while real focus had already moved onto it. Found
        // by an adversarial audit; the review surface's own docs claimed to copy the graph tab's
        // discipline and, in exactly this way, did not.
        self.leave_review_tab(window, cx);
        // GitHub issue #227: the run-transcript tab occupies the centre pane exactly as the
        // graph and review tabs do, so it needs the identical teardown - see
        // `crate::run_history::tab::AdeApp::leave_run_tab`.
        self.leave_run_tab(window, cx);

        if focus_editor {
            self.focus_code_surface(window, cx);
        }
        self.push_open_file(&relative);
        self.open_change = Some(relative);
        self.code_view = view;
        self.markdown_view = markdown_preview::MarkdownView::Source;
        // Every "this file is now the selected tree row" path reveals it (GitHub issue #18 §5).
        // A click in the tree has its ancestors expanded already, so this is a no-op there - but
        // go-to-definition (`Self::open_file_at_line`), a palette result and a terminal link
        // all land on files in folders nobody has expanded, and now that the tree starts
        // collapsed, highlighting a row that isn't showing would be no highlight at all.
        //
        // Guarded, because `absolute` is not always inside the tree currently on screen: a
        // terminal link resolves against its agent's own cwd, and `Self::open_change_diff`
        // resolves against [`Self::diff_root`], which really can differ from
        // [`Self::file_tree_root`] (`crate::merge::flow` and `crate::worktree_history::flow` both
        // load the *main repo's* diff while a worktree is selected). Revealing and highlighting a
        // path this tree does not contain would write a junk fold-state entry to disk and
        // highlight a row that does not exist. `reveal_in_tree` already refuses such a path;
        // `selected_tree_path` did not, which is what makes the check live here rather than
        // there.
        if absolute.starts_with(&self.file_tree_root) {
            self.reveal_in_tree(&absolute, cx);
            self.selected_tree_path = Some(absolute);
            // GitHub issue #145: opening any single file - from the tree itself, the palette, a
            // terminal link, go-to-definition, wherever - collapses a stray multi-selection down
            // to just that file, the same way a plain click on a tree row already did before
            // multi-selection existed.
            self.additional_tree_selection.clear();
        }
        self.refresh_open_diff_file_cache();
        // A hover card is only valid for the file it was requested against - and so is a real
        // Completions popup (Revision R8.5b audit finding 3's fix for a real, live-reproduced
        // data-corruption bug: without this, a popup left open from switching away from a file
        // could resurrect and splice stale text into whatever's active when the same path
        // becomes active again - see `Self::dismiss_completions`'s own docs).
        self.dismiss_hover();
        self.dismiss_completions();
        self.close_tab_confirm_armed = None;
        // A newly opened file tab changes this worktree's real tab session, so it reopens on the
        // next launch - see `crate::work_surface::session`. A no-op when `push_open_file` above
        // found the tab already open (an already-open path is reused, never duplicated), since the
        // recorder compares against what is already on disk before writing.
        self.record_worktree_session(cx);
        cx.notify();
    }

    /// Opens `path`'s diff in the centre pane (the Changes row click handler, and a palette file
    /// result for a file that really is in the loaded diff).
    pub(crate) fn open_change_diff(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // GitHub issue #285: opening a file marks it **seen** and does not, cannot, stage it
        // - `seen_files` and `staged_files` are separate fields of
        // separate types, and this is the only writer of the first. The mark records the diffstat
        // the file has right now, so an agent that writes to it again puts it straight back to
        // unseen (see `crate::sidebar::sections::SeenFiles`' own docs).
        if let Some(entry) = self.uncommitted_change_set.entry(&path) {
            let stat = entry.stat();
            self.seen_files.mark_seen(&self.diff_root, &path, stat);
        }
        let absolute = self.diff_root.join(&path);
        self.open_and_focus_file(path, absolute, code_view::CodeView::Diff, true, window, cx);
    }

    /// Opens `path` (absolute) directly in Surface C's File view - go-to-definition, a terminal
    /// path link, a just-created file, and a palette file result with no diff to show. The Files
    /// tree's own row click uses [`Self::open_file_view_from_tree_click`] instead (GitHub issue
    /// #105) - see that function's own docs for why it needs different focus behavior than every
    /// other caller here.
    pub(crate) fn open_file_view(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_view_with_focus(path, true, window, cx);
    }

    /// The Files tree's own row click handler (GitHub issue #105) - opens `path` exactly like
    /// [`Self::open_file_view`], but never moves keyboard focus onto the editor. A file click is
    /// a real *selection* gesture, the same as a folder click - and a folder click has always
    /// kept focus on [`Self::tree_focus_handle`] rather than handing it away. Before this, only
    /// file rows broke that symmetry: clicking a file silently moved focus off the tree, so every
    /// `"file-tree"`-scoped shortcut (`Ctrl+C`/`X`/`V`, `F2`, `Shift+F10`) went dead the instant
    /// the single most common selection gesture happened. Never transiently focuses the editor at
    /// all (rather than focusing it and immediately handing focus back), so no editor-focus/-blur
    /// side effect ever fires for a plain tree click - `crate::sidebar::render`'s own row click
    /// handler is responsible for explicitly (re)confirming tree focus afterward, the same way
    /// the folder-click handler already does.
    pub(crate) fn open_file_view_from_tree_click(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_view_with_focus(path, false, window, cx);
    }

    fn open_file_view_with_focus(
        &mut self,
        path: PathBuf,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
        // Clear any stale pending cursor line from an abandoned navigation;
        // `open_file_at_line` re-sets it right after this if it has a target line.
        self.pending_cursor_line = None;
        let relative = path
            .strip_prefix(&self.file_tree_root)
            .map(|relative| relative.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        self.open_and_focus_file(
            relative,
            path,
            code_view::CodeView::File,
            focus_editor,
            window,
            cx,
        );
    }

    /// Activates a file tab (the tab strip's click handler), as opposed to
    /// [`Self::open_change_diff`]/[`Self::open_file_view`], which open a file that may not have a
    /// tab yet. Calls [`Self::push_open_file`] itself so `open_change` can never point at a path
    /// missing from `open_files`.
    pub(crate) fn activate_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_open_file(&path);

        if self.open_change.as_deref() == Some(path.as_path()) {
            self.code_view = code_view::CodeView::File;
            self.markdown_view = markdown_preview::MarkdownView::Source;
            cx.notify();
            return;
        }

        // See `Self::open_and_focus_file`'s identical call for why this must run before
        // `focus_code_surface` below.
        self.leave_graph_tab(window, cx);
        // GitHub issue #225: the review tab occupies the centre pane exactly as the graph tab
        // does, so it needs the identical teardown here. Without this, `review_tab_active` stayed
        // set, `render_center_pane` kept returning the review body, and the tab this call is
        // switching *to* never mounted at all - while real focus had already moved onto it. Found
        // by an adversarial audit; the review surface's own docs claimed to copy the graph tab's
        // discipline and, in exactly this way, did not.
        self.leave_review_tab(window, cx);
        // GitHub issue #227: the run-transcript tab occupies the centre pane exactly as the
        // graph and review tabs do, so it needs the identical teardown - see
        // `crate::run_history::tab::AdeApp::leave_run_tab`.
        self.leave_run_tab(window, cx);

        self.focus_code_surface(window, cx);
        let has_diff = self
            .current_diff()
            .is_some_and(|diff| diff.files.iter().any(|file| file.path == path));
        self.open_change = Some(path.clone());
        self.code_view = if has_diff {
            code_view::CodeView::Diff
        } else {
            code_view::CodeView::File
        };
        self.markdown_view = markdown_preview::MarkdownView::Source;
        self.refresh_open_diff_file_cache();
        self.dismiss_hover();
        // See `Self::open_and_focus_file`'s identical `dismiss_completions()` call for why
        // (Revision R8.5b audit finding 3).
        self.dismiss_completions();
        self.code_cursor = None;
        self.prune_confirm_armed = false;
        self.close_tab_confirm_armed = None;
        // `push_open_file` above can genuinely add a tab here (see this method's own docs on why
        // it calls it at all), so this is a real session change, not just an activation.
        self.record_worktree_session(cx);
        cx.notify();
    }

    /// Closes file tab `path`, removing it from [`Self::open_files`] (the tab's `×` hit box). If
    /// `path` was the active tab, activates the neighbor to its right, else the one to its left,
    /// else falls back to the active agent's terminal (restoring focus like
    /// [`Self::close_settings`] does). No-op if `path` isn't an open tab.
    pub(crate) fn request_close_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_dirty = self
            .edit_buffer(&path)
            .is_some_and(|buffer| buffer.is_dirty());
        if is_dirty && self.close_tab_confirm_armed.as_deref() != Some(path.as_path()) {
            self.close_tab_confirm_armed = Some(path);
            cx.notify();
            return;
        }
        self.close_tab_confirm_armed = None;
        self.close_file_tab(path, window, cx);
    }

    pub(crate) fn close_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.open_files().iter().position(|open| open == &path) else {
            return;
        };
        if self.close_tab_confirm_armed.as_deref() == Some(path.as_path()) {
            self.close_tab_confirm_armed = None;
        }
        self.open_files_mut().remove(index);
        self._lsp_sync_tasks.remove(&path);
        if self
            .completions
            .as_ref()
            .is_some_and(|entry| entry.path == path)
        {
            self.dismiss_completions();
        }
        let was_active = self.open_change.as_deref() == Some(path.as_path());
        if was_active {
            // After removal, the tab that was to the right (if any) has shifted into `index`;
            // fall back to `index - 1` only if there's nothing there.
            let neighbor = self.open_files().get(index).cloned().or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|left| self.open_files().get(left).cloned())
            });
            match neighbor {
                Some(next_path) => {
                    self.open_change = Some(next_path.clone());
                    self.refresh_open_diff_file_cache();
                    self.dismiss_hover();
                    self.dismiss_completions();
                }
                None => {
                    self.open_change = None;
                    self.refresh_open_diff_file_cache();
                    self.dismiss_hover();
                    self.dismiss_completions();
                    let fallback = self.focus_fallback_handle();
                    restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
                }
            }
        }
        self.prune_confirm_armed = false;
        // A closed file tab must stay closed across a relaunch - see
        // `crate::work_surface::session`.
        self.record_worktree_session(cx);
        cx.notify();
    }

    /// Closes the currently active file tab, if any (the code surface toolbar's own "× close",
    /// distinct from the tab strip's). Calls [`Self::close_file_tab`] directly - **not**
    /// [`Self::request_close_file_tab`]'s dirty-tab confirm-arm gate. GitHub issue #26 briefly
    /// routed this button through that gate too, but never gave it a matching visible "close
    /// without saving?" cue the way `crate::work_surface::render`'s tab-strip `×` has
    /// (`Self::close_tab_confirm_armed`'s own docs) - "the global `Ctrl+W` binding, the tab
    /// strip's own `×`, and middle-click" are the real affordances sharing the gate; this one
    /// isn't among them, and its own verification was
    /// scoped to `code_surface::`/`settings::`/`keymap*` tests, which never included
    /// `root::focus::text_undo_scoping_tests` - so a dirty file's first click here silently armed
    /// an invisible confirm state with zero on-screen feedback, unnoticed. Per
    /// [`Self::close_file_tab`]'s own docs, nothing is actually destroyed by closing either way -
    /// the edit buffer (and its whole undo history) stays alive in [`Self::edit_buffers`], and
    /// reopening the same path restores it - so skipping the confirm here trades a real but minor
    /// inconsistency (this button behaving differently from the tab strip's own `×`) for avoiding
    /// a real, silent, first-click-does-nothing regression, which this project's own bug-class
    /// history (this file's module docs) already treats as the worse failure mode.
    pub(crate) fn close_change_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.open_change.clone() {
            self.close_file_tab(path, window, cx);
        }
    }

    /// Recomputes [`Self::open_diff_file_cache`] (and [`Self::file_view_changed_lines`] with it)
    /// from [`Self::open_change`] and [`Self::current_diff`]. Called whenever either input
    /// changes; never from a render method, to avoid a per-render `DiffFile` clone - also the
    /// real hook [`Self::ensure_diff_highlight_cache`] recomputes real syntax highlighting from,
    /// for the same reason (see that method's own docs).
    pub(crate) fn refresh_open_diff_file_cache(&mut self) {
        self.open_diff_file_cache = match (&self.open_change, self.current_diff()) {
            (Some(open_path), Some(diff)) => diff
                .files
                .iter()
                .find(|file| &file.path == open_path)
                .cloned(),
            _ => None,
        };
        self.file_view_changed_lines = self
            .open_diff_file_cache
            .as_ref()
            .map(code_view::changed_line_set)
            .unwrap_or_default();
        self.ensure_diff_highlight_cache();
    }

    /// Dispatches a background `code_view::load_file` call for `path` (must never run inline
    /// during `render()`; see [`FileLoadState`]'s docs). Mirrors [`Self::load_diff`]'s shape: mark
    /// `Loading` and notify immediately, run the blocking work on `cx.background_executor()`, then
    /// write the outcome back into [`Self::file_view_cache`]/[`Self::file_load_state`].
    pub(in crate::code_surface) fn spawn_file_load(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.file_load_state = FileLoadState::Loading(path.clone());
        cx.notify();
        // The worktree-relative key `Self::edit_buffers` uses, matching `Self::open_files`' own
        // convention - computed synchronously here (a cheap prefix-strip, no I/O) so the
        // background closure below can move it in without needing `&self` later.
        let relative_path = path
            .strip_prefix(&self.file_tree_root)
            .map(|stripped| stripped.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        // The *worktree* half of `Self::edit_buffers`' composite key, captured now - before the
        // real `.await` below - rather than re-read from `self.file_tree_root` once this task
        // resumes. This load can genuinely outlive a worktree switch (a real background
        // `std::fs::read_to_string` + tree-sitter parse, not instant), and `self.file_tree_root`
        // would then already name whatever worktree the user switched *to*; resolving the key
        // from that at completion time would silently seed or reload the *new* worktree's buffer
        // for this same relative path instead of the one this load was actually for. See
        // `Self::edit_buffers`'s own docs for the stale-worktree bug class this prevents.
        let cwd = self.file_tree_root.clone();
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_string());
        // Read on the foreground thread, before the background read is spawned - the background
        // closure has no `self` to consult (see `spawn_file_load`'s own stale-worktree discipline
        // above, which captures everything it needs up front for the same reason).
        let highlight_options = self.highlight_options();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    let extension = extension.clone();
                    // GitHub issue #178: the File view breadcrumb's enclosing-symbol outline is a
                    // second real `tree_sitter::Parser::parse` of the same source, so it runs
                    // here - on the same background executor, off the back of the same decode -
                    // rather than on the foreground thread once this task resumes. See
                    // `code_surface::symbols`' own docs for why the outline is flattened to plain
                    // data at this point instead of a tree being carried across the await.
                    //
                    // GitHub issue #168: uses `load_file_with_options`, not `load_file_with_source`
                    // - the user's real bracket-pair-colorization setting (read on the foreground
                    // thread above) must govern this parse too, not silently fall back to
                    // `HighlightOptions::default()`.
                    async move {
                        code_view::load_file_with_options(&path, highlight_options).map(
                            |(parsed, source)| {
                                let symbols =
                                    symbols::symbol_outline(&source, extension.as_deref());
                                (parsed, source, symbols)
                            },
                        )
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((parsed, source, symbols)) => {
                        // Lazily seed a real edit buffer the first time this file is opened in
                        // File view - never overwrite an existing entry (which may hold real
                        // unsaved edits and a live cursor/selection); see `Self::edit_buffers`'
                        // own docs. A truncated file (see `code_view::MAX_FILE_BYTES`) is
                        // deliberately excluded: editing a partial view and saving it would
                        // silently discard everything past the cap - the same reasoning that
                        // already justifies the cap itself - so such a file stays read-only. A
                        // file whose real bytes aren't valid UTF-8 is excluded for the same real
                        // reason: `code_view::load_file_with_source`'s `source` is already a
                        // lossy `String::from_utf8_lossy` decode at that point, with every
                        // invalid byte sequence silently replaced by `U+FFFD` - seeding an
                        // editable buffer from it and later saving would overwrite the file's
                        // real original bytes with those replacement characters. `parsed.
                        // is_valid_utf8` is the exact real flag `code_view::load_file` already
                        // computes for this (already surfaced in the status bar's `UTF-8`
                        // label) - reused here rather than a second check.
                        // `Some(line)` only when an *existing*, clean buffer was reloaded in
                        // place below - see that arm's own docs.
                        let mut reloaded_cursor_line: Option<usize> = None;
                        // Set alongside it, so the language-server sync can run after the
                        // `edit_buffers` borrow above has ended.
                        let mut reloaded = false;
                        // A write this app itself issued for this exact path is still queued or
                        // running, so anything this background read saw is by definition not the
                        // final on-disk state - and the `(mtime, len)` guard below can't catch it
                        // if two of our own writes land inside one filesystem mtime granularity
                        // tick. Never adopt a read that raced our own writer; the next freshness
                        // tick re-reads once the writer has drained. Read before the
                        // `edit_buffers` borrow starts.
                        let save_in_flight = this.file_save_pending.contains(&relative_path)
                            || this.file_save_running.contains(&relative_path);
                        if !parsed.truncated && parsed.is_valid_utf8 {
                            match this.edit_buffer_at_mut(&cwd, &relative_path) {
                                // An existing buffer with **no** unsaved edits, whose file has
                                // since been rewritten on disk by someone else (an agent CLI
                                // running in this very worktree - this app's whole domain - a
                                // formatter, another editor). Adopting that content is the honest
                                // result: this buffer has nothing of the user's to lose, and
                                // leaving it showing bytes that no longer exist anywhere would be
                                // silently stale. Recorded as one single undoable step rather
                                // than a fresh buffer, per GitHub issue #17: an external rewrite
                                // must never be a silent history wipe mid-stack, and Ctrl+Z
                                // straight after one really does put the pre-reload content back.
                                // See `EditBuffer::reload_from_disk`'s own docs.
                                //
                                // The `(mtime, len)` guard is a real staleness check, not
                                // belt-and-braces: this read started before the `this.update`
                                // it is now inside, and a real, documented user action can land in
                                // between - `EditorSaveAnyway` (`force_save_active_file`), the
                                // explicit escape hatch for an external-change conflict, writes the
                                // user's own content and marks the buffer clean again. Without this
                                // check, the now-stale read would then be adopted over content the
                                // user had *just* deliberately force-saved, and would stamp the
                                // buffer's `saved_mtime`/`saved_len` to an on-disk identity the
                                // file no longer has. Refusing anything not strictly newer than
                                // what the buffer already believes about disk closes that window;
                                // the next freshness tick re-reads and gets it right.
                                Some(buffer)
                                    if !buffer.is_dirty()
                                        && (parsed.mtime, parsed.len)
                                            != (buffer.saved_mtime, buffer.saved_len)
                                        && parsed.mtime >= buffer.saved_mtime
                                        && !save_in_flight =>
                                {
                                    buffer.reload_from_disk(
                                        source,
                                        parsed.lines.clone(),
                                        symbols,
                                        parsed.mtime,
                                        parsed.len,
                                    );
                                    // The reloaded content is genuinely different text at genuinely
                                    // different offsets - every other content mutation in this
                                    // crate pairs with an LSP sync (see `Self::step_edit_history`),
                                    // and skipping it here would leave the language server
                                    // answering hover/diagnostics/goto about a document that is no
                                    // longer on screen until the user's next keystroke.
                                    reloaded = true;
                                    // `reload_from_disk` keeps the real caret (clamped into the
                                    // new content), so the caret-line indicator below must follow
                                    // it rather than snapping back to line 1 the way a genuinely
                                    // fresh load's does - the buffer's own caret is the truth
                                    // here, and an indicator disagreeing with it would be a real,
                                    // visible lie.
                                    let (line, _) =
                                        buffer.line_col_for_offset(buffer.cursor_offset());
                                    reloaded_cursor_line = Some(line + 1);
                                }
                                // A **dirty** buffer is deliberately left completely alone: its
                                // unsaved content is the user's, and this app already surfaces the
                                // real divergence as an explicit conflict
                                // (`AdeApp::file_external_conflict`, plus the save-time refusal in
                                // `save_active_file`) for the user to resolve, rather than picking
                                // a winner for them. Nothing touches the history in this case
                                // either - no wipe, no boundary, because no edit happened.
                                Some(_) => {}
                                // First open of this file in the editable File view - see this
                                // block's own docs above for why a truncated/invalid-UTF-8 file
                                // deliberately never reaches here at all.
                                None => {
                                    this.insert_edit_buffer_at(
                                        cwd.clone(),
                                        relative_path.clone(),
                                        edit_buffer::EditBuffer::from_highlighted(
                                            path.clone(),
                                            source,
                                            extension.clone(),
                                            parsed.lines.clone(),
                                            symbols,
                                            parsed.mtime,
                                            parsed.len,
                                        ),
                                    );
                                }
                            }
                        }
                        if reloaded {
                            // GitHub issue #202: fold state is plain line indices into content
                            // this reload just replaced wholesale (an agent CLI or formatter
                            // rewriting the file the user has open - this app's own core domain,
                            // not an edge case). A stale index can point at a *different*, larger
                            // region than the one the user actually folded, silently swallowing
                            // the caret's line with nothing left to expand it - the same
                            // `window.handle_input` breakage `Self::scroll_file_view_to_line`
                            // exists to prevent for every other caret-moving action. Dropping the
                            // folds here is the same "not persisted, self-healing" posture
                            // `AdeApp::file_view_folds` already documents for a restart; a real
                            // external rewrite is exactly as much a break in continuity as one.
                            this.file_view_folds.remove(&path);
                            this.schedule_lsp_sync(cwd.clone(), relative_path.clone(), cx);
                        }
                        // Whether this load is a *different* file arriving in the view, as
                        // opposed to a freshness re-read of the one already showing. Read before
                        // `file_view_cache` is overwritten below, since that is what it compares
                        // against. Only a genuine arrival is allowed to move the scroll position:
                        // a re-read of the current file must never yank the view away from
                        // wherever the user has scrolled it.
                        let newly_in_view = this
                            .file_view_cache
                            .as_ref()
                            .is_none_or(|cached| cached.path != path);
                        this.file_view_cache = Some(parsed);
                        // A pending go-to-definition target line applies only if it names the
                        // file that just finished loading, so an unrelated file's load can't
                        // apply a stale target left over from an abandoned navigation.
                        let target_line = match &this.pending_cursor_line {
                            Some((pending_path, line)) if pending_path == &path => Some(*line),
                            _ => None,
                        };
                        // GitHub issue #15: "reopening a previously visited file restores its last
                        // caret position when known; otherwise caret at 1:1, scrolled to top."
                        // The caret itself was already restored - `edit_buffers` deliberately
                        // survives a tab close (see that field's docs), so its `cursor_offset` is
                        // still the real one - but the two things that *show* it were not: the
                        // status bar's line indicator was hard-reset to 1, and nothing scrolled
                        // the caret's row back into view, so a caret restored to line 400 was
                        // invisible and misreported. Both now follow the buffer. A file being
                        // opened for the very first time has a fresh buffer at offset 0, so this
                        // reduces to exactly "line 1, scrolled to top" with no special case.
                        let buffer_cursor_line =
                            this.edit_buffer_at(&cwd, &relative_path).map(|buffer| {
                                let (line, _) = buffer.line_col_for_offset(buffer.cursor_offset());
                                line + 1
                            });
                        let cursor_line =
                            target_line.or(reloaded_cursor_line).or(buffer_cursor_line);
                        // GitHub issue #202: both of these now scroll to a *visual row*, not a
                        // line index (the two differ once anything in this file is collapsed -
                        // fold state is keyed by absolute path, so it survives a tab close and is
                        // still here when the file is reopened), expanding the destination first.
                        // See `AdeApp::scroll_file_view_to_line`.
                        if let Some(line) = target_line {
                            this.pending_cursor_line = None;
                            this.scroll_file_view_to_line(
                                &path,
                                line.saturating_sub(1),
                                ScrollStrategy::Center,
                            );
                        } else if newly_in_view {
                            if let Some(line) = buffer_cursor_line {
                                this.scroll_file_view_to_line(
                                    &path,
                                    line.saturating_sub(1),
                                    ScrollStrategy::Center,
                                );
                            }
                        }
                        this.code_cursor = Some(cursor_line.unwrap_or(1));
                        this.file_load_state = FileLoadState::Idle;
                    }
                    Err(error) => {
                        this.file_load_state =
                            FileLoadState::Error(path.clone(), error.to_string());
                        // Don't leave a stale target line to misapply onto the next file that
                        // loads successfully.
                        this.pending_cursor_line = None;
                    }
                }
                cx.notify();
            });
        });
        self._file_load_task = Some(task);
    }

    /// Handles `TerminalPaneEvent::OpenPath` - a mod-held click on a detected path/`path:line`
    /// link in an agent's terminal output. `path` is already resolved against the agent's cwd
    /// (see `crate::terminal::links::resolve`). Reuses [`Self::open_file_at_line`] when the
    /// link carried a line number, else [`Self::open_file_view`].
    pub(crate) fn open_terminal_link(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !path.is_file() {
            log::debug!(
                "terminal link click: {} does not exist on disk, ignoring",
                path.display()
            );
            return;
        }
        match line {
            Some(line) => self.open_file_at_line(path, line as usize, window, cx),
            None => self.open_file_view(path, window, cx),
        }
    }

    /// The currently loaded diff, if any - `None` only while loading/erroring, or when `HEAD`
    /// itself is unborn (see [`wt_core::diff::DiffBase::diff`]). A worktree on its default
    /// branch or with no detectable base still yields `Some` here, showing its real uncommitted
    /// changes instead (GitHub issue #108). The single source every view that shows diff state
    /// reads.
    pub(crate) fn current_diff(&self) -> Option<&WorktreeDiff> {
        match &self.diff_state {
            DiffLoadState::Loaded(base) => base.diff(),
            _ => None,
        }
    }

    /// The absolute, on-disk path [`Self::open_change`] really names, resolved against whichever
    /// root it's actually relative to - [`Self::diff_root`] while [`Self::code_view`] is `Diff`,
    /// [`Self::file_tree_root`] otherwise (`Self::open_and_focus_file`'s own two callers,
    /// [`Self::open_change_diff`]/[`Self::open_file_view`], resolve exactly this way). `None` iff
    /// no file tab is showing at all.
    pub(crate) fn open_change_absolute_path(&self) -> Option<PathBuf> {
        let relative = self.open_change.as_ref()?;
        let root = match self.code_view {
            code_view::CodeView::Diff => &self.diff_root,
            code_view::CodeView::File => &self.file_tree_root,
        };
        Some(root.join(relative))
    }
}

/// Regression coverage for `AdeApp::render_file_view`'s cache: re-running an expensive parse on
/// every render.
#[cfg(test)]
mod code_view_cache_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opening_a_large_real_file_does_not_block_render_on_the_full_parse(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("large.rs");
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lsp/client.rs"))
                .expect("read this crate's own lsp/client.rs as a real, large .rs fixture");
        std::fs::write(&file_path, &source).expect("write large.rs");

        let baseline_start = std::time::Instant::now();
        code_view::load_file(&file_path).expect("load_file baseline");
        let baseline_duration = baseline_start.elapsed();

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });

        let render_start = std::time::Instant::now();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let render_duration = render_start.elapsed();

        assert!(
            app.read_with(cx, |app, _| app.file_view_cache.is_none()),
            "the real parse must not have completed synchronously inside this render call"
        );
        assert!(
            render_duration < baseline_duration,
            "render_center_pane's own foreground-thread duration ({render_duration:?}) should \
             be far less than a real, direct, synchronous code_view::load_file call on the same \
             file ({baseline_duration:?}) - render only dispatches the load to the background \
             executor and returns immediately, it does not run the parse inline"
        );

        cx.run_until_parked();
        let cached_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated once the background load completed")
                .lines
                .len()
        });
        assert!(
            cached_line_count > 1000,
            "sanity check: this should have really been a large, multi-thousand-line file, not \
             an accidentally-empty fixture"
        );
    }

    #[gpui::test]
    fn repeated_renders_of_the_same_open_file_reuse_the_cached_parse(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(
            &file_path,
            "fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n",
        )
        .expect("write sample.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.file_view_cache.is_none()),
            "file_view_cache must still be empty immediately after the render that dispatched \
             the load - the real parse has not run yet, since it only runs on the background \
             executor; if this is already populated here, the parse ran synchronously inline \
             during render() again"
        );

        cx.run_until_parked();

        let first_render_ptr = app.update(cx, |app, cx| {
            app.render_center_pane(cx);
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated once the background load completed")
                .lines
                .as_ptr()
        });

        for render_index in 1..=3 {
            let ptr = app.update(cx, |app, cx| {
                app.render_center_pane(cx);
                app.file_view_cache
                    .as_ref()
                    .expect("file_view_cache should stay populated across re-renders")
                    .lines
                    .as_ptr()
            });
            assert_eq!(
                ptr, first_render_ptr,
                "render #{render_index} of the same, unchanged open file rebuilt the cached \
                 parse instead of reusing it (a fresh heap allocation for `ParsedFile::lines` \
                 means `code_view::load_file` ran again)"
            );
        }
    }

    #[gpui::test]
    fn a_real_on_disk_change_to_the_open_file_invalidates_the_cache(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add() -> i32 {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the first real load")
                .lines
                .len()
        });

        std::fs::write(
            &file_path,
            "fn add() -> i32 {\n    1\n}\n\nfn subtract() -> i32 {\n    -1\n}\n",
        )
        .expect("rewrite sample.rs");

        // Clear the throttle so the next freshness check runs, rather than waiting out its
        // real wall-clock interval.
        app.update(cx, |app, _| {
            app.file_view_last_freshness_check = None;
        });

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let updated_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the second real load")
                .lines
                .len()
        });

        assert!(
            updated_line_count > original_line_count,
            "a real on-disk change to the open file should have invalidated the cache and \
             reloaded its real, larger content"
        );
    }

    #[gpui::test]
    fn renders_within_the_throttle_window_do_not_pick_up_a_fresh_on_disk_change(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add() -> i32 {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the first real load")
                .lines
                .len()
        });

        std::fs::write(
            &file_path,
            "fn add() -> i32 {\n    1\n}\n\nfn subtract() -> i32 {\n    -1\n}\n",
        )
        .expect("rewrite sample.rs");

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        let still_original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("cache must still be populated - no reload should have been dispatched")
                .lines
                .len()
        });
        assert_eq!(
            still_original_line_count, original_line_count,
            "a render inside the throttle window re-`stat`'d the file and picked up the on-disk \
             change early - the freshness check should have been skipped this render"
        );
        assert!(
            app.read_with(cx, |app, _| matches!(
                app.file_load_state,
                FileLoadState::Idle
            )),
            "no reload should have been dispatched while the freshness check was throttled"
        );

        // Clear the throttle - the change is now observed.
        app.update(cx, |app, _| {
            app.file_view_last_freshness_check = None;
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let updated_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the throttled-then-real load")
                .lines
                .len()
        });
        assert!(
            updated_line_count > original_line_count,
            "once the throttle window passed, the same real on-disk change should finally have \
             been picked up"
        );
    }
}

/// Regression coverage for the cross-file cursor leak [`AdeApp::pending_cursor_line`] describes:
/// [`AdeApp::open_file_at_line`] to a file B that isn't cached yet leaves a one-shot target
/// line waiting for B's background load; opening a different file C before that resolves must
/// not let C's load misapply B's stale target line.
#[cfg(test)]
mod cross_file_navigation_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn navigating_to_an_uncached_file_then_opening_a_different_file_first_does_not_leak_the_cursor(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let file_b = repo.path().join("b.txt");
        let file_c = repo.path().join("c.txt");
        std::fs::write(&file_b, "one\ntwo\nthree\nfour\nfive\n").expect("write b.txt");
        std::fs::write(&file_c, "hello\nworld\n").expect("write c.txt");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        // Landing on B's line 5 - B isn't cached yet, so this sets `pending_cursor_line` rather
        // than `code_cursor` directly.
        app.update_in(cx, |app, window, cx| {
            app.open_file_at_line(file_b.clone(), 5, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.pending_cursor_line.clone()),
            Some((file_b.clone(), 5))
        );
        app.update(cx, |app, cx| {
            // Dispatches B's background load; not yet driven to completion, so it's still
            // in flight.
            app.render_center_pane(cx);
        });

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_c.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let (loaded_path, cursor) = app.read_with(cx, |app, _| {
            (
                app.file_view_cache
                    .as_ref()
                    .map(|parsed| parsed.path.clone()),
                app.code_cursor,
            )
        });
        assert_eq!(
            loaded_path,
            Some(file_c.clone()),
            "sanity check: C's own real load should have completed"
        );
        assert_eq!(
            cursor,
            Some(1),
            "C is a fresh file open with no navigation target of its own - its cursor must start \
             at real line 1, not leak B's stale target line 5"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.pending_cursor_line.clone()),
            None,
            "AdeApp::open_file_view must have cleared the stale entry outright when C was opened, \
             so it can never leak onto a later, unrelated file's load either"
        );
    }
}

/// Regression coverage for the unbounded busy-loop [`FileLoadState::Error`] describes: a read
/// failure must settle into a stable error state, never respawn a doomed load every render.
#[cfg(test)]
mod unreadable_file_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn an_unreadable_file_settles_into_a_stable_error_state_without_respawning(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        // A deterministic, cross-platform read failure: no such path exists, so `load_file`'s
        // `fs::metadata`/`fs::read` fail every time, with no platform-specific setup needed.
        let missing_path = repo.path().join("does-not-exist.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(missing_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let message = app.read_with(cx, |app, _| match &app.file_load_state {
            FileLoadState::Error(path, message) => {
                assert_eq!(path, &missing_path);
                message.clone()
            }
            other => panic!("expected a real Error state after the failed load, got {other:?}"),
        });

        // The regression this guards against: before the fix, this next render alone (no
        // further `run_until_parked()`) would flip `file_load_state` back to `Loading`, because
        // the respawn guard only checked `FileLoadState::Loading`, never `Error` - an unbounded
        // busy-loop instead of a stable error state.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        app.read_with(cx, |app, _| match &app.file_load_state {
            FileLoadState::Error(path, message_after) => {
                assert_eq!(path, &missing_path);
                assert_eq!(
                    message_after, &message,
                    "the same real, already-observed error - not a freshly respawned attempt"
                );
            }
            other => panic!(
                "a real, already-observed read failure for this exact path must stay a stable \
                 Error state across further renders, not respawn back into Loading - got \
                 {other:?}"
            ),
        });
    }
}

/// Regression coverage for the multi-file tab strip's `open_files` state transitions:
/// [`AdeApp::open_change_diff`]/[`AdeApp::open_file_view`] no longer discard a file when the user
/// navigates elsewhere, [`AdeApp::activate_file_tab`] switches which one is showing without
/// touching the list, and [`AdeApp::close_file_tab`] is the only place a tab leaves the list.
/// GitHub issue #15's "reopening a previously visited file restores its last caret/scroll
/// position when known; otherwise caret at 1:1, scrolled to top."
#[cfg(test)]
mod reopened_file_caret_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Drives the real background load/render cycle `edit_buffers` is seeded from - the same
    /// shape `code_view_cache_tests` and `editing_tests::open_file_for_editing` use.
    fn open_and_settle(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
    ) {
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(path, window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    #[gpui::test]
    fn reopening_a_file_reports_its_restored_caret_line_not_line_one(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let target = repo.path().join("many.txt");
        // Long enough that the caret's row genuinely cannot be on screen at scroll offset 0 -
        // otherwise "the view scrolled to the caret" and "the view never moved" look identical.
        let long_file: String = (1..=400).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&target, &long_file).expect("write");
        let other = repo.path().join("other.txt");
        std::fs::write(&other, "x\n").expect("write");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        open_and_settle(&app, cx, target.clone());
        let relative = PathBuf::from("many.txt");

        assert_eq!(
            app.read_with(cx, |app, _| app.code_cursor),
            Some(1),
            "a first open is caret at 1:1"
        );

        assert_eq!(
            app.read_with(cx, |app, _| f32::from(
                app.file_view_scroll_handle
                    .0
                    .borrow()
                    .base_handle
                    .offset()
                    .y
            )),
            0.0,
            "premise: a first open really is scrolled to the top"
        );

        app.update_in(cx, |app, window, cx| {
            for _ in 0..200 {
                app.handle_editor_down_action(&crate::root::EditorDown, window, cx);
            }
        });
        cx.run_until_parked();
        let moved = app.read_with(cx, |app, _| {
            let buffer = app.edit_buffer(&relative).expect("buffer");
            buffer.line_col_for_offset(buffer.cursor_offset()).0
        });
        assert_eq!(
            moved, 200,
            "premise: the caret really moved to buffer line 200"
        );
        assert_eq!(app.read_with(cx, |app, _| app.code_cursor), Some(201));

        open_and_settle(&app, cx, other);
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(target, window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let buffer = app.edit_buffer(&relative).expect("buffer");
            assert_eq!(
                buffer.line_col_for_offset(buffer.cursor_offset()).0,
                200,
                "premise: the buffer's own caret survived the round trip"
            );
            assert_eq!(
                app.code_cursor,
                Some(201),
                "and the indicator must follow it rather than snapping back to line 1"
            );
            // The other half of the same fix, read off the real list's own resolved scroll
            // offset (`vendor/zed/crates/gpui/src/elements/uniform_list.rs`'s `base_handle`,
            // which is what `scroll_to_item`'s deferred target resolves into on the next
            // layout): the caret's row must have been scrolled back into view, or a caret
            // restored to line 200 of a 400-line file would be correct and invisible.
            assert!(
                f32::from(
                    app.file_view_scroll_handle
                        .0
                        .borrow()
                        .base_handle
                        .offset()
                        .y
                ) < 0.0,
                "reopening must scroll the restored caret's row into view, not sit at the top"
            );
        });
    }

    #[gpui::test]
    fn a_first_open_is_caret_at_line_one(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let target = repo.path().join("fresh.txt");
        std::fs::write(&target, "a\nb\nc\n").expect("write");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        open_and_settle(&app, cx, target);

        app.read_with(cx, |app, _| {
            assert_eq!(app.code_cursor, Some(1));
            assert_eq!(
                app.edit_buffer(Path::new("fresh.txt"))
                    .expect("buffer")
                    .cursor_offset(),
                0
            );
        });
    }
}

#[cfg(test)]
mod multi_file_tab_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Writes three files under `dir` and returns both their absolute paths (what
    /// `open_file_view` takes) and their repo-relative paths (what `open_change`/`open_files`/
    /// `activate_file_tab`/`close_file_tab` key by, via `open_file_view`'s own `strip_prefix`).
    fn write_three_files(
        dir: &std::path::Path,
    ) -> ((PathBuf, PathBuf, PathBuf), (PathBuf, PathBuf, PathBuf)) {
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, "a\n").expect("write a.txt");
        std::fs::write(&b, "b\n").expect("write b.txt");
        std::fs::write(&c, "c\n").expect("write c.txt");
        let rel = (
            PathBuf::from("a.txt"),
            PathBuf::from("b.txt"),
            PathBuf::from("c.txt"),
        );
        ((a, b, c), rel)
    }

    #[gpui::test]
    fn opening_the_same_file_twice_does_not_duplicate_its_tab(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let ((a, _b, _c), (a_rel, _, _)) = write_three_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().len()),
            1,
            "opening an already-open file a second time must activate the existing tab, not \
             append a duplicate"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel)
        );
    }

    #[gpui::test]
    fn closing_the_active_tab_activates_the_tab_that_was_to_its_right(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let ((a, b, c), (a_rel, b_rel, c_rel)) = write_three_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        for path in [a, b, c] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        // Opening a, then b, then c in order leaves `open_files == [a, b, c]` with `c` (the
        // most recently opened) active - reactivate the middle one (`b`) so there's a real
        // right *and* left neighbor to prove the "prefer right" order against.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(b_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(b_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(b_rel, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![a_rel.clone(), c_rel.clone()],
            "b should be gone from the tab list"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(c_rel),
            "closing the active middle tab should activate the tab that was to its right, not \
             the one to its left"
        );

        // Closing the now-active last tab (`c`) has no tab to its right left, so it should fall
        // back to the one on its left (`a`).
        app.update_in(cx, |app, window, cx| {
            let active = app.open_change.clone().expect("c should be active");
            app.close_file_tab(active, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel)
        );

        // Closing the last remaining tab falls all the way back to no active file at all (the
        // active agent shows through instead).
        app.update_in(cx, |app, window, cx| {
            let active = app.open_change.clone().expect("a should be active");
            app.close_file_tab(active, window, cx);
        });
        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);
        assert!(app.read_with(cx, |app, _| app.open_files().is_empty()));
    }

    #[gpui::test]
    fn closing_a_non_active_tab_does_not_change_what_is_active(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let ((a, b, _c), (a_rel, b_rel, _)) = write_three_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b, window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(b_rel, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel),
            "closing a tab that wasn't active must leave the active tab unchanged"
        );
        assert_eq!(app.read_with(cx, |app, _| app.open_files().len()), 1);
    }

    #[gpui::test]
    fn selecting_a_agent_deactivates_the_open_file_tab_without_closing_it(cx: &mut TestAppContext) {
        let repo = temp_repo();
        let ((a, _b, _c), (a_rel, _, _)) = write_three_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        let agent_id = app.read_with(cx, |app, _| {
            app.agents
                .active_id()
                .expect("a fresh window has one real agent")
        });

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            None,
            "selecting an agent tab should deactivate the file tab"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![a_rel],
            "the file tab must still be in open_files, just not active"
        );

        // The centre pane must actually render the agent again, not silently panic/show
        // nothing - a real smoke check on top of the state assertions above.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    #[gpui::test]
    fn next_changed_file_advances_through_every_changed_file_and_wraps_around(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\n").expect("write b.txt");
        std::fs::write(repo.path().join("c.txt"), "1\n").expect("write c.txt");
        test_support::git(repo.path(), &["add", "."]);
        test_support::git(repo.path(), &["commit", "-m", "initial"]);
        test_support::git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\nchanged\n").expect("rewrite b.txt");
        std::fs::write(repo.path().join("c.txt"), "1\nchanged\n").expect("rewrite c.txt");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let order: Vec<PathBuf> = app.read_with(cx, |app, _| {
            app.current_diff()
                .expect("a real diff against main should have loaded")
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect()
        });
        assert_eq!(
            order.len(),
            3,
            "sanity check: all three files should be changed"
        );

        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[0].clone()),
            "with nothing active, the first real changed file should open"
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[1].clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[2].clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[0].clone()),
            "advancing past the last changed file should wrap around to the first"
        );
    }

    /// Regression test for "switching tabs shows the wrong Diff/File view": `code_view` is a
    /// single global field, not per-tab, so switching from a tab left in `File` view to a
    /// different tab with a diff used to incorrectly stay in `File` view instead of forcing
    /// `Diff` back.
    #[gpui::test]
    fn switching_to_a_tab_with_a_real_diff_shows_the_diff_not_the_last_view_mode(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(repo.path().join("changed.txt"), "1\n").expect("write changed.txt");
        std::fs::write(repo.path().join("plain.txt"), "plain\n").expect("write plain.txt");
        test_support::git(repo.path(), &["add", "."]);
        test_support::git(repo.path(), &["commit", "-m", "initial"]);
        test_support::git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("changed.txt"), "1\nchanged\n")
            .expect("rewrite changed.txt");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let changed_rel = PathBuf::from("changed.txt");
        let plain_abs = repo.path().join("plain.txt");

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(changed_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(plain_abs, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File
        );

        // Switch back to the changed file's tab - it has a diff, so it must show that again, not
        // inherit the File view the plain file left `code_view` in.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(changed_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(changed_rel)
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "switching back to a tab with a real diff must show that diff, not the File view \
             the previously active tab happened to leave code_view in"
        );
    }

    #[gpui::test]
    fn reactivating_a_tab_whose_diff_disappeared_shows_real_content_again(cx: &mut TestAppContext) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        test_support::git(repo.path(), &["add", "."]);
        test_support::git(repo.path(), &["commit", "-m", "initial"]);
        test_support::git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let rel = PathBuf::from("a.txt");
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff
        );

        // Revert the change on disk and reload the diff - the file's `DiffFile` disappears from
        // the loaded diff, but `open_change` still names it (nothing closed the tab).
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("revert a.txt");
        app.update(cx, |app, cx| app.load_diff(repo.path().to_path_buf(), cx));
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app
                .current_diff()
                .is_some_and(|diff| diff.files.iter().any(|file| file.path == rel))),
            "sanity: the file should no longer be in the loaded diff"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(rel.clone()),
            "the tab is still 'active' by name, even though the surface has nothing real left to \
             show for it"
        );

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(rel, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File,
            "re-activating a tab that's active-but-not-really-showing must fall back to File \
             view, which always has real content, instead of staying a dead no-op"
        );
    }

    #[gpui::test]
    fn switching_worktrees_and_back_preserves_each_worktree_s_open_file_tabs(
        cx: &mut TestAppContext,
    ) {
        let repo_a = temp_repo();
        let repo_b = temp_repo();
        let ((a1, a2, _), (a1_rel, a2_rel, _)) = write_three_files(repo_a.path());
        let b1 = repo_b.path().join("notes.txt");
        std::fs::write(&b1, "notes\n").expect("write notes.txt");
        let b1_rel = PathBuf::from("notes.txt");

        let (app, cx) = open_test_app(cx, repo_a.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                crate::rail::worktrees::WorktreeItem {
                    path: repo_a.path().to_path_buf(),
                    label: "wt-a".to_string(),
                    branch: Some("wt-a".to_string()),
                    is_main: true,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
                crate::rail::worktrees::WorktreeItem {
                    path: repo_b.path().to_path_buf(),
                    label: "wt-b".to_string(),
                    branch: Some("wt-b".to_string()),
                    is_main: false,
                    is_bare: false,
                    is_detached: false,
                    short_sha: None,
                    is_locked: false,
                    lock_reason: None,
                    is_broken: false,
                    broken_reason: None,
                    error: None,
                },
            ];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a1, window, cx);
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a2, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![a1_rel.clone(), a2_rel.clone()],
            "sanity check: both real tabs should be open in worktree A"
        );

        // Switching to worktree B - a worktree that has never opened anything - must show a
        // real, honest empty tab strip, never worktree A's tabs.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_files().is_empty()),
            "a worktree that has never opened a file must show a real empty tab strip, not \
             worktree A's leftover tabs"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b1, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![b1_rel.clone()],
            "sanity check: worktree B's own real tab should be open"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![a1_rel, a2_rel],
            "switching back to worktree A must restore exactly the tabs it was left with, not \
             lose them to the switch away and back"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![b1_rel],
            "worktree B's own tab must survive being switched away from and back to as well"
        );
    }
}

/// Revision R8.5b audit finding 3's direct regression coverage: a genuine data-corruption bug -
/// a stale [`AdeApp::completions`] popup surviving a tab switch/close and resurrecting later,
/// letting stale text be spliced into whatever file happens to become active again. The real
/// `Ready` popup state is seeded directly (no real LSP round trip needed to prove this real
/// navigation/bookkeeping bug - matching `multi_file_tab_tests`' own established precedent, and
/// `crate::lsp::client::lsp_diagnostics_wiring_tests` for the real, live end-to-end completions
/// proof this module doesn't duplicate).
#[cfg(test)]
mod stale_completions_popup_tests {
    use super::*;
    use crate::lsp::completion_popup::{CompletionsEntry, CompletionsStatus};
    use gpui::TestAppContext;

    fn write_two_files(dir: &std::path::Path) -> ((PathBuf, PathBuf), (PathBuf, PathBuf)) {
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        ((a, b), (PathBuf::from("a.rs"), PathBuf::from("b.rs")))
    }

    fn fake_ready_entry(path: PathBuf, label: &str) -> CompletionsEntry {
        CompletionsEntry {
            path,
            status: CompletionsStatus::ready(
                vec![lsp_core::lsp_types::CompletionItem {
                    label: label.to_string(),
                    ..Default::default()
                }],
                "",
            )
            .expect("a real, non-empty ready state"),
        }
    }

    #[gpui::test]
    fn switching_tabs_away_and_back_does_not_resurrect_a_stale_completions_popup(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let ((a, b), (a_rel, b_rel)) = write_two_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        for path in [a, b] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        // `open_file_view` activates whichever file was opened last (`b`) - reactivate `a` so a
        // real popup can be seeded "for" it.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.completions = Some(fake_ready_entry(a_rel.clone(), "stale_for_a"));
            cx.notify();
        });
        assert!(
            app.read_with(cx, |app, _| app.completions_open_for_active_path()),
            "sanity check: the seeded popup should genuinely be open for the active file, a"
        );

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(b_rel.clone(), window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "switching tabs away from a must drop the real completions popup that was open for \
             it, not merely hide it - a stale entry left behind is exactly the real, live-\
             reproduced bug this fix closes"
        );

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel, window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "switching back to a must not resurrect the stale popup that was open for it before \
             the switch away - accepting it would splice stale, wrong-context text into the \
             buffer, the real data-corruption bug this fix closes"
        );
    }

    #[gpui::test]
    fn closing_a_tab_with_an_open_popup_never_lets_it_resurface_for_another_file(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        let ((a, b), (a_rel, _b_rel)) = write_two_files(repo.path());
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        for path in [a.clone(), b] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.completions = Some(fake_ready_entry(a_rel.clone(), "stale_for_a"));
            cx.notify();
        });

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(a_rel.clone(), window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "closing a tab with an open real completions popup must drop it, not leave it \
             dangling for a file that's no longer open"
        );
        assert!(
            app.read_with(cx, |app, _| !app.open_files().contains(&a_rel)),
            "sanity check: a's tab should genuinely be closed"
        );

        // Reopening the exact same path later (the buffer itself survives a close - see
        // `AdeApp::close_file_tab`'s own docs) must not resurrect the popup either.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "reopening the same path after its tab was closed must not resurrect a stale popup \
             from before the close"
        );
    }
}

/// End-to-end proof of the "terminal is interactive" link-click-opens-a-file path:
/// `simulate_click` against the pane's own painted bounds drives `TerminalPane`'s `on_click`/
/// `cx.emit`, `Agents::spawn`'s `cx.subscribe_in`, and `AdeApp::open_terminal_link` - the same
/// chain a real mouse click goes through.
#[cfg(test)]
mod terminal_link_click_tests {
    use super::*;
    use gpui::{
        point, Bounds, Entity, Modifiers, MouseButton, Pixels, Size, TestAppContext,
        VisualTestContext,
    };

    /// Injects a row of terminal text containing a `path:line` link on the third visible row
    /// (`"see src/main.rs:1 for it"`, 0-indexed row 2) into the active agent's pane, and
    /// returns the painted geometry (`content_bounds`, cell size) needed to compute a click
    /// position.
    fn inject_link_row_and_measure(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
    ) -> (Bounds<Pixels>, Size<Pixels>) {
        let pane = app
            .read_with(cx, |app, _| app.agents.active().map(|s| s.pane.clone()))
            .expect("a fresh test window has one real, active shell agent");

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(
                b"first line\r\nsecond line\r\nsee src/main.rs:1 for it",
                cx,
            );
        });
        // Lets the injected `cx.notify()` drive a real paint (populating `content_bounds`). The
        // agent's `$SHELL` spawn may also make background progress here, but only ever appends
        // after the injected bytes, so it can't touch the link characters this test's
        // click-position math depends on.
        cx.run_until_parked();

        let (bounds, cell_size) = pane.update_in(cx, |pane, window, _cx| {
            (
                pane.content_bounds_for_test(),
                pane.cell_size_for_test(window),
            )
        });
        (
            bounds.expect("the pane must have painted at least once after run_until_parked"),
            cell_size,
        )
    }

    /// The pixel position of the middle of `"src/main.rs:1"` on row 2 of
    /// [`inject_link_row_and_measure`]'s injected text (`"see "` is a 4-character prefix) -
    /// geometry math off the pane's measured padding/line-height/cell-width constants, mirroring
    /// `vendor/zed/crates/editor/src/editor_tests.rs`'s `simulate_click` call sites
    /// (`text_origin + em_width * column`, `line_height * row`).
    fn link_click_position(bounds: Bounds<Pixels>, cell_size: Size<Pixels>) -> gpui::Point<Pixels> {
        let link_text = "src/main.rs:1";
        let prefix_chars = 4.0; // "see "
        let x = bounds.origin.x
            + px(crate::terminal::pane::PANE_PADDING_PX)
            + cell_size.width * (prefix_chars + link_text.chars().count() as f32 / 2.0);
        let y = bounds.origin.y
            + px(crate::terminal::pane::PANE_PADDING_PX)
            + cell_size.height * 2.0
            + cell_size.height / 2.0;
        point(x, y)
    }

    #[gpui::test]
    fn mod_click_on_a_detected_link_opens_the_real_file_tab(cx: &mut TestAppContext) {
        let repo = temp_repo();
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(
            link_click_position(bounds, cell_size),
            Modifiers::secondary_key(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change,
                Some(PathBuf::from("src/main.rs")),
                "a real mod-held click on the detected link must open the real file as the \
                 active tab - got open_change = {:?}, open_files = {:?}",
                app.open_change,
                app.open_files(),
            );
            assert!(
                app.open_files().contains(&PathBuf::from("src/main.rs")),
                "the opened file must appear in the real tab list too"
            );
        });
    }

    #[gpui::test]
    fn a_bare_click_on_a_detected_link_does_not_open_anything(cx: &mut TestAppContext) {
        let repo = temp_repo();
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(link_click_position(bounds, cell_size), Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change, None,
                "a bare click (no mod held) on a detected link must not open anything"
            );
            assert!(
                app.open_files().is_empty(),
                "a bare click must not add a real tab either"
            );
        });
    }

    #[gpui::test]
    fn mod_held_only_during_mouse_down_still_opens_the_real_file_tab(cx: &mut TestAppContext) {
        let repo = temp_repo();
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);
        let position = link_click_position(bounds, cell_size);

        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::secondary_key());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change,
                Some(PathBuf::from("src/main.rs")),
                "a modifier held only during mouse-down (released before mouse-up) must still \
                 open the real file tab, not be silently dropped - got open_change = {:?}",
                app.open_change,
            );
        });
    }

    #[gpui::test]
    fn mod_click_on_a_link_to_a_nonexistent_path_does_not_open_a_tab(cx: &mut TestAppContext) {
        let repo = temp_repo();
        // Deliberately never created: `src/` itself doesn't exist in this repo at all.

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(
            link_click_position(bounds, cell_size),
            Modifiers::secondary_key(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change, None,
                "a mod-click on a link that resolves to a real, nonexistent path must not open \
                 anything - got open_change = {:?}",
                app.open_change,
            );
            assert!(
                app.open_files().is_empty(),
                "a link to a nonexistent path must not add a real tab either"
            );
        });
    }
}

#[cfg(test)]
mod diff_self_refresh_tests {
    use crate::root::focus::palette_focus_tests;
    use crate::root::{AdeApp, FILE_TREE_WATCH_SETTLE, FILE_TREE_WATCH_TICK};
    use crate::settings::store as settings_store;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn git_repo(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A repo on a feature branch with one committed file, so `diff_against_head` has a real base
    /// to answer against.
    fn repo_on_a_feature_branch() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        git_repo(repo.path(), &["init", "-b", "main"]);
        git_repo(repo.path(), &["config", "user.email", "test@example.com"]);
        git_repo(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        git_repo(repo.path(), &["add", "."]);
        git_repo(repo.path(), &["commit", "-m", "initial"]);
        git_repo(repo.path(), &["checkout", "-b", "feature"]);
        repo
    }

    /// Unlike `palette_focus_tests::open_test_app`, this passes a real settings path: without one
    /// `AdeApp::start_file_tree_watch` returns before arming anything, so the watch loop these
    /// tests exercise would never run at all.
    fn open_watched_app(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (Entity<AdeApp>, &mut VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    fn uncommitted_paths(app: &AdeApp) -> Vec<PathBuf> {
        app.uncommitted_diff
            .loaded()
            .map(|diff| diff.files.iter().map(|file| file.path.clone()).collect())
            .unwrap_or_default()
    }

    /// The watcher's callback is delivered by the OS on its own thread, so this cannot be a purely
    /// simulated clock - the same real-time bounded wait `crate::sidebar::file_tree_watch`'s own
    /// tests use, with the executor's timers advanced on each pass so the loop's tick/settle
    /// waits actually elapse.
    fn wait_until(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        mut done: impl FnMut(&AdeApp) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            cx.executor()
                .advance_clock(FILE_TREE_WATCH_TICK + FILE_TREE_WATCH_SETTLE);
            cx.run_until_parked();
            if app.read_with(cx, |app, _| done(app)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// GitHub issue #415's headline symptom: an agent writes a file and the Changes pane never
    /// hears about it, because the only steady-state reload was the user navigating *to* the
    /// Changes tab. Nothing here touches `set_right_sidebar_view`.
    #[gpui::test]
    fn a_write_after_load_reaches_the_changes_pane_without_visiting_the_tab(
        cx: &mut TestAppContext,
    ) {
        let repo = repo_on_a_feature_branch();
        let settings = tempfile::tempdir().expect("settings tempdir");
        let (app, cx) = open_watched_app(
            cx,
            repo.path().to_path_buf(),
            settings.path().join("settings.toml"),
        );
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| uncommitted_paths(app))
                .is_empty(),
            "sanity: a freshly opened clean worktree has no uncommitted files"
        );

        // What an agent does: write a file nothing in the app initiated.
        std::fs::write(repo.path().join("agent-wrote-this.txt"), "hello\n").expect("write");

        let target = PathBuf::from("agent-wrote-this.txt");
        assert!(
            wait_until(&app, cx, |app| uncommitted_paths(app).contains(&target)),
            "a file written behind the app's back must appear in the Changes pane on its own, \
             without the user switching to the Changes tab first"
        );
    }

    /// The other half of #415: the `+n`/`-n` totals live in the sidebar header band, which is
    /// rendered above all three views, so they have to self-update wherever the user is standing.
    #[gpui::test]
    fn the_header_totals_self_update_after_a_write(cx: &mut TestAppContext) {
        let repo = repo_on_a_feature_branch();
        let settings = tempfile::tempdir().expect("settings tempdir");
        let (app, cx) = open_watched_app(
            cx,
            repo.path().to_path_buf(),
            settings.path().join("settings.toml"),
        );
        cx.run_until_parked();

        std::fs::write(repo.path().join("a.txt"), "1\n2\n3\n").expect("rewrite a.txt");

        assert!(
            wait_until(&app, cx, |app| app
                .diff_totals
                .is_some_and(|(add, _)| add >= 2)),
            "the header's +n/-n totals must reflect a write the app did not initiate"
        );
    }

    /// The in-flight guard: the watch loop ticks far faster than five git queries take on a real
    /// worktree, and assigning `_load_diff_task` drops whatever was still running - so an ungated
    /// refresh would cancel its own predecessor every tick and could never finish.
    #[gpui::test]
    fn a_refresh_is_skipped_while_a_reload_is_still_in_flight(cx: &mut TestAppContext) {
        let repo = repo_on_a_feature_branch();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.diff_reload_in_flight),
            "a settled app has no reload running"
        );

        std::fs::write(repo.path().join("b.txt"), "b\n").expect("write b.txt");

        // Stand in for a reload that has been spawned but has not landed yet, then ask for the
        // refresh the watch loop would ask for on its next tick.
        app.update(cx, |app, cx| {
            app.diff_reload_in_flight = true;
            app.refresh_diff_if_idle(cx);
        });
        cx.run_until_parked();

        let target = PathBuf::from("b.txt");
        assert!(
            !app.read_with(cx, |app, _| uncommitted_paths(app))
                .contains(&target),
            "the guard must have skipped the refresh, so the new file is not picked up yet"
        );

        // Once the in-flight reload lands, the very next tick does the work.
        app.update(cx, |app, cx| {
            app.diff_reload_in_flight = false;
            app.refresh_diff_if_idle(cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| uncommitted_paths(app))
                .contains(&target),
            "with no reload in flight the same call must genuinely refresh"
        );
    }

    /// `spawn_diff_reload` owns the flag for every spawn, including the manual button's, so a
    /// cancelled-and-replaced reload can never strand it `true` and wedge the watcher refresh off
    /// for the rest of the session.
    #[gpui::test]
    fn a_replaced_reload_still_clears_the_in_flight_flag(cx: &mut TestAppContext) {
        let repo = repo_on_a_feature_branch();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // Two reloads back to back: the second drops the first's still-running task.
        app.update(cx, |app, cx| {
            app.refresh_diff(cx);
            app.refresh_diff(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.diff_reload_in_flight),
            "a spawned reload must mark itself in flight"
        );
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.diff_reload_in_flight),
            "the surviving reload's completion must clear the flag, or every later watcher-driven \
             refresh is silently skipped forever"
        );
    }
}
