use super::*;
use crate::root::code_surface::{DiffLoadState, FileLoadState};
use crate::root::sidebar_render::RightSidebarView;

impl AdeApp {
    /// Production entry point - loads `~/.config/jerry/settings.toml` (`Settings::load_or_init`)
    /// and delegates to [`Self::new_with_settings`]. Blocking the foreground thread here is a
    /// deliberate exception to this codebase's usual rule: it's a single tiny file read that
    /// runs exactly once, before a window even exists, not a per-render or per-poll cost.
    pub fn new(repo_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_path = settings_store::settings_toml_path();
        let settings = settings_store::Settings::load_or_init();
        Self::new_with_settings(repo_path, settings, settings_path, window, cx)
    }

    /// The real constructor - takes an already-resolved [`Settings`] and its optional source
    /// path rather than resolving them itself, so [`Self::new`] (production) and
    /// `root::focus::palette_focus_tests::open_test_app` (every GPUI test's shared entry point)
    /// can each supply their own. Test instances get in-memory-only defaults and a `None` path,
    /// so [`Self::persist_settings`] is a genuine no-op for them, never a write to whatever
    /// machine happens to run `cargo test`.
    pub(super) fn new_with_settings(
        repo_path: PathBuf,
        settings: settings_store::Settings,
        settings_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            file_tree_root: repo_path.clone(),
            diff_root: repo_path.clone(),
            repo_path: repo_path.clone(),
            worktrees: Vec::new(),
            worktrees_error: None,
            selected: None,
            sessions: Sessions::new(),
            file_tree: Vec::new(),
            file_tree_error: None,
            right_sidebar_view: RightSidebarView::Files,
            diff_state: DiffLoadState::Loading,
            diff_totals: None,
            collapsed_dirs: HashSet::new(),
            reviewed_files: HashSet::new(),
            open_files: Vec::new(),
            open_change: None,
            open_diff_file_cache: None,
            selected_tree_path: None,
            code_view: code_view::CodeView::Diff,
            code_focus_handle: cx.focus_handle(),
            code_focus: OverlayFocus::default(),
            file_view_scroll_handle: UniformListScrollHandle::new(),
            file_view_cache: None,
            diff_highlight_cache: None,
            file_view_last_freshness_check: None,
            file_load_state: FileLoadState::Idle,
            file_view_changed_lines: HashSet::new(),
            code_cursor: None,
            edit_buffers: HashMap::new(),
            file_view_row_layout: HashMap::new(),
            file_view_last_layout: None,
            file_view_last_bounds: None,
            file_view_last_layout_for: None,
            _rehighlight_tasks: HashMap::new(),
            _file_save_tasks: HashMap::new(),
            file_save_pending: HashSet::new(),
            file_save_running: HashSet::new(),
            file_save_error: None,
            file_external_conflict: HashSet::new(),
            palette_open: false,
            palette_scope: palette::PaletteScope::default(),
            palette_query: String::new(),
            palette_selected: 0,
            palette_focus_handle: cx.focus_handle(),
            palette_focus: OverlayFocus::default(),
            palette_file_candidates: Vec::new(),
            rail_width: px(layout::RAIL_DEFAULT),
            panel_width: px(layout::PANEL_DEFAULT),
            body_bounds: gpui::Bounds::default(),
            title_bar_move_armed: false,
            rail_mode: RailMode::default(),
            filter_query: String::new(),
            filter_focus_handle: cx.focus_handle(),
            diff_cache: HashMap::new(),
            worktree_notes: HashMap::new(),
            ahead_behind_cache: HashMap::new(),
            process_stats: HashMap::new(),
            disk_usage: None,
            worktree_disk_usage: HashMap::new(),
            prune_status: None,
            prune_confirm_armed: false,
            prune_in_flight: false,
            undo_stack: undo::UndoStack::new(),
            worktree_history_op_in_flight: None,
            worktree_history_status: None,
            discard_confirm_armed: None,
            settings_open: false,
            settings_page: settings::SettingsPage::General,
            settings_focus_handle: cx.focus_handle(),
            settings_focus: OverlayFocus::default(),
            agent_rows: Vec::new(),
            merge_flow: None,
            merge_op_in_flight: false,
            merge_highlight_cache: None,
            merge_edit: None,
            merge_edit_focus_handle: cx.focus_handle(),
            merge_edit_scroll_handle: UniformListScrollHandle::new(),
            merge_edit_row_layout: HashMap::new(),
            merge_edit_last_layout: None,
            merge_edit_last_bounds: None,
            merge_edit_last_layout_for: None,
            merge_edit_save_pending: false,
            merge_edit_save_running: false,
            merge_edit_save_error: None,
            _merge_edit_save_task: None,
            merge_generation: 0,
            merge_edit_buffer_id: 0,
            #[cfg(test)]
            merge_edit_save_test_delay: None,
            _load_worktrees_task: None,
            _load_file_tree_task: None,
            _load_diff_task: None,
            _file_load_task: None,
            _status_poll_task: None,
            _disk_usage_task: None,
            _prune_task: None,
            _worktree_history_task: None,
            _agent_rows_task: None,
            _merge_task: None,
            _merge_cleanup_task: None,
            _merge_write_tasks: TaskPool::new(),
            lsp_clients: HashMap::new(),
            lsp_opened_files: HashSet::new(),
            lsp_document_versions: HashMap::new(),
            lsp_last_synced_content: HashMap::new(),
            lsp_synced_version: HashMap::new(),
            lsp_diagnostics_confirmed_version: HashMap::new(),
            lsp_uri_cache: HashMap::new(),
            _lsp_sync_tasks: HashMap::new(),
            _completions_request_task: None,
            completions: None,
            completions_generation: 0,
            file_view_diagnostics: HashMap::new(),
            file_view_error_count: None,
            _lsp_tasks: TaskPool::new(),
            _lsp_poll_task: None,
            hover: None,
            _hover_request_task: None,
            _goto_definition_tasks: TaskPool::new(),
            pending_cursor_line: None,
            settings,
            settings_path,
            _settings_save_task: None,
            settings_save_pending: false,
            settings_save_running: false,
            #[cfg(test)]
            settings_save_test_delay: None,
            settings_cfg_format: settings_store::CfgFormat::default(),
            lsp_rows: Vec::new(),
            _lsp_rows_task: None,
            settings_keymap_filter: String::new(),
            settings_keymap_filter_focus_handle: cx.focus_handle(),
            keymap_recording: None,
            _keymap_intercept: None,
            keymap_rebind_error: None,
            // Set up once, regardless of whether `follow_system` starts on - see
            // `Self::sync_theme_to_system_appearance`'s own docs for why the callback checks the
            // live setting itself on every real fire rather than this subscription being
            // conditionally created.
            _window_appearance_subscription: cx.observe_window_appearance(
                window,
                |this, window, cx| {
                    this.sync_theme_to_system_appearance(window, cx);
                },
            ),
            plus_menu_open: false,
            plus_button_bounds: gpui::Bounds::default(),
            title_menu_open: None,
            title_menu_button_bounds: [gpui::Bounds::default(); title_bar::TitleMenu::ALL.len()],
            _new_agent_pane_task: TaskPool::new(),
            new_file_input: None,
            new_file_focus_handle: cx.focus_handle(),
            new_file_error: None,
        };
        // Applies `this.settings.keymap.overrides` on top of `crate::default_key_bindings()` -
        // see `Self::apply_effective_key_bindings`'s own docs. Must run before this constructor
        // returns and the entity's first render, so a persisted rebind is live from the very
        // first frame, not just after the next settings change - real "apply overrides at
        // startup", not only "apply overrides when later edited".
        this.apply_effective_key_bindings(cx);
        // Applies the real, persisted theme selection at startup (`Self::apply_theme_selection`)
        // - if `follow_system` is also on, the real, current OS appearance takes priority over
        // whatever `theme.name` was last persisted as, matching `Self::sync_theme_to_system_
        // appearance`'s own live behavior (see that method's docs); `apply_theme_selection`'s own
        // call at the end is always run regardless, since `apply_follow_system_appearance` is a
        // no-op (and doesn't itself apply anything) when the resolved name already matches.
        if this.settings.theme.follow_system {
            let appearance = window.appearance();
            this.apply_follow_system_appearance(appearance, cx);
        }
        this.apply_theme_selection(cx);
        // A fresh window starts with one shell in the repo root, as a tab like any other.
        // `focus_active` below moves real keyboard focus onto it - see `Sessions::focus_active`'s
        // docs and this crate's `OverlayFocus`/`restore_focus` docs for why a fresh window must
        // never start with `Window::focus == None`.
        this.sessions.spawn(
            SessionKind::Shell,
            repo_path.clone(),
            this.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        this.sessions.focus_active(window, cx);
        this.load_worktrees(cx);
        this.load_file_tree(repo_path.clone(), cx);
        this.load_diff(repo_path, cx);
        this.start_status_polling(cx);
        this
    }

    pub(super) fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.repo_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::list_worktrees(&repo_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(results) => {
                        this.worktrees = worktrees::build_worktree_items(results);
                        this.worktrees_error = None;
                    }
                    Err(err) => {
                        this.worktrees = Vec::new();
                        this.worktrees_error = Some(err.to_string());
                    }
                }
                this.load_disk_usage(cx);
                cx.notify();
            });
        });
        self._load_worktrees_task = Some(task);
    }

    /// Recomputes [`Self::disk_usage`] and [`Self::worktree_disk_usage`] from the current
    /// worktree list, offloaded to the background executor (`crate::rail::disk_usage_bytes`).
    /// Run once per worktree-list load, not on the 3s status-poll cadence - a `std::fs` walk per
    /// worktree every 3s would be needless cost for numbers that rarely change.
    ///
    /// [`Self::disk_usage`] (the rail footer's aggregate) is always derived from the same
    /// per-path map the Settings › Worktrees page reads - one computation, two consumers.
    pub(super) fn load_disk_usage(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .worktrees
            .iter()
            .filter(|item| item.error.is_none())
            .map(|item| item.path.clone())
            .collect();

        let task = cx.spawn(async move |this, cx| {
            let per_path = cx
                .background_executor()
                .spawn(async move {
                    let mut per_path = HashMap::with_capacity(paths.len());
                    for path in paths {
                        let usage = rail::disk_usage_bytes(&path);
                        per_path.insert(path, usage);
                    }
                    per_path
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let total: u64 = per_path.values().map(|(bytes, _)| bytes).sum();
                let truncated = per_path.values().any(|(_, truncated)| *truncated);
                this.disk_usage = Some((total, truncated));
                this.worktree_disk_usage = per_path;
                cx.notify();
            });
        });
        self._disk_usage_task = Some(task);
    }

    pub(super) fn load_file_tree(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.file_tree_root = root.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { file_tree::build_file_tree(&root) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.file_tree = entries;
                        this.file_tree_error = None;
                    }
                    Err(err) => {
                        this.file_tree = Vec::new();
                        this.file_tree_error = Some(err.to_string());
                    }
                }
                // The palette's cached file-candidate list (`Self::palette_file_candidates`) is
                // derived from `file_tree` - refresh it here, the real point that input changes,
                // rather than leaving it stale until some unrelated `load_diff` happens to
                // refresh it too.
                this.rebuild_palette_file_candidates();
                cx.notify();
            });
        });
        self._load_file_tree_task = Some(task);
    }

    /// The worktree a *new* session should be spawned into: the selected worktree's real
    /// path if one is selected and readable, otherwise the repo root - see the module docs'
    /// "Sessions/tabs" section for why this is resolved at spawn time rather than tracked as
    /// a per-tab "current worktree".
    pub(super) fn active_session_cwd(&self) -> PathBuf {
        match self.selected.and_then(|index| self.worktrees.get(index)) {
            Some(item) if item.error.is_none() => item.path.clone(),
            _ => self.repo_path.clone(),
        }
    }

    pub(super) fn select_worktree(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.worktrees.get(index) else {
            return;
        };
        if item.error.is_some() {
            // An unreadable entry has no usable path; nothing to select into.
            return;
        }
        let path = item.path.clone();
        self.selected = Some(index);
        // Makes this worktree's own last-active tab (or its first session, or none) the
        // globally active one - see `Sessions::activate_for_worktree`'s own docs for why this
        // invariant ("the active session always belongs to the selected worktree") is the real
        // fix this revision makes: before it, selecting a worktree never touched `self.sessions`
        // at all, so the centre pane could keep showing a completely different worktree's
        // terminal after a rail click.
        self.sessions.activate_for_worktree(&path);
        // Browsing to a different worktree disarms a pending prune confirmation - see
        // `Self::request_prune`'s docs.
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        // Reset per-worktree UI state (see `reset_per_worktree_ui_state`'s docs) so switching
        // worktrees never leaks a "reviewed" checkbox, open diff, or collapsed-dir entry from
        // the worktree just left. Deliberately runs *before* the focus-fallback block below: both
        // `focus_newly_spawned_session` and the `filter_focus_handle` fallback branch on
        // `self.open_change`, and until this call runs, `open_change` can still reflect the
        // worktree just *left* (a file tab open there) rather than the real post-switch state -
        // evaluating either guard first (a real, live-reproduced bug found in this revision's own
        // self-audit) made both branches see a stale `Some` and skip moving focus at all, leaving
        // `Window::focus` dangling on `code_focus_handle` once `open_change` was cleared one
        // statement later.
        reset_per_worktree_ui_state(
            &mut self.reviewed_files,
            &mut self.open_files,
            &mut self.open_change,
            &mut self.collapsed_dirs,
            &mut self.selected_tree_path,
            &mut self.edit_buffers,
        );
        // `focus_newly_spawned_session` (despite its name - its body has no "newly spawned" logic
        // in it, just the shared "move focus unless a file tab/Settings is showing" guard) closes
        // the dangling-focus risk this switch creates: the previously-active session's pane may
        // no longer be part of the rendered tree at all once the tab strip's own per-worktree
        // filter (`Self::render_tab_strip`) applies, so keyboard focus left pointing at it would
        // silently break every keybinding until the next click - the same "focus left pointing at
        // something no longer rendered" bug class this project's own `OverlayFocus`/
        // `restore_focus` mechanism exists to prevent, applied here to a plain worktree switch
        // rather than an overlay open/close. `open_change` above is already this switch's real
        // post-reset value by the time this runs, so the guard it checks is genuine.
        self.focus_newly_spawned_session(window, cx);
        // `focus_newly_spawned_session` is a real no-op when the newly selected worktree has no
        // open session at all (`Sessions::focus_active` has nothing to focus) - so if a
        // previously-focused session's pane belonged to the worktree just left, it's now exactly
        // as dangling as the case the comment above already covers, just with no session to
        // redirect *onto*. Fall back to the rail's own filter field
        // (`Self::filter_focus_handle`), which is part of the rendered tree whenever the
        // workspace body is showing (never while Settings has replaced it - `!self.settings_open`
        // guards that the same way `focus_newly_spawned_session` itself does) - real, if
        // imperfect, UX (it makes the filter box look focused without being asked to), but it
        // keeps the focused `FocusId` genuinely findable in the next rendered frame, which is the
        // actual invariant this exists to protect: a dangling `FocusId` makes GPUI's action
        // dispatch fall back to a disconnected root with no real `on_action` handlers at all, not
        // just this worktree's own missing ones - silently breaking every global keybinding (⌘K
        // included) until the next click.
        if self.sessions.active_id().is_none() && self.open_change.is_none() && !self.settings_open
        {
            window.focus(&self.filter_focus_handle, cx);
        }
        // The File view's own per-worktree state (a cached parse and diff lookup that are about
        // to belong to a different `file_tree_root`) - reset for the same reason as above.
        // Dropping `_file_load_task` cancels any in-flight load for the worktree just left.
        self.code_view = code_view::CodeView::Diff;
        self.file_view_cache = None;
        self.file_load_state = FileLoadState::Idle;
        self.file_view_changed_lines = HashSet::new();
        self.code_cursor = None;
        self.file_view_error_count = None;
        self.open_diff_file_cache = None;
        // The real text-editing state above (`edit_buffers`) is per-worktree-reset via the shared
        // helper; its own transient/task-shaped siblings - which don't fit that helper's plain
        // free-function signature - are reset directly here for the same reason. Dropping the
        // task maps cancels every in-flight debounced re-highlight/save for the worktree just
        // left, matching `_file_load_task`'s own reset above.
        self.file_view_row_layout = HashMap::new();
        self.file_view_last_layout = None;
        self.file_view_last_bounds = None;
        self.file_view_last_layout_for = None;
        self._rehighlight_tasks = HashMap::new();
        // Real live LSP sync/completions state (Revision R8.5b) is worktree-relative-path-keyed
        // (or entirely path-scoped) the same way `edit_buffers` above is - reset alongside it so
        // a worktree switch can't leak a stale "already synced this content" record or a dangling
        // popup from the worktree just left. `lsp_document_versions`/`lsp_uri_cache` are keyed by
        // *absolute* path (see their own docs), so neither ever actually collides across
        // worktrees and both are left to `evict_stale_lsp_clients`'s own root-scoped pruning
        // instead of a blanket reset here.
        self._lsp_sync_tasks = HashMap::new();
        self._completions_request_task = None;
        self.lsp_last_synced_content = HashMap::new();
        self.lsp_synced_version = HashMap::new();
        self.lsp_diagnostics_confirmed_version = HashMap::new();
        self.dismiss_completions();
        self._file_save_tasks = HashMap::new();
        self.file_save_pending = HashSet::new();
        self.file_save_running = HashSet::new();
        self.file_save_error = None;
        self.file_external_conflict = HashSet::new();
        // The Diff view's syntax-highlight cache is keyed on a whole `DiffFile` from the
        // worktree just left - reset alongside `open_diff_file_cache` above for the same reason
        // (and so it can't retain a full file's highlighting from a worktree that's no longer
        // active).
        self.diff_highlight_cache = None;
        self._file_load_task = None;
        // Editor zoom (`Settings.appearance.editor_zoom_percent`) is a real, globally-persisted
        // Settings field now - see `settings_store`'s "Editor zoom is one global, persisted
        // number now" docs - so it deliberately does *not* get reset here anymore.
        //
        // The hover cache is per-file - clear it too, or a hover card from the worktree just
        // left could reappear the instant a same-named file opens in the new one. The real
        // Completions popup is already dropped above (alongside `_lsp_sync_tasks`/
        // `lsp_last_synced_content`) via `Self::dismiss_completions()` - repeated here,
        // idempotently, right next to `hover`'s own reset for the same reason every other real
        // `self.hover = None` site in this codebase now pairs the two (Revision R8.5b audit
        // finding 3), rather than relying solely on it having already run earlier in this
        // function.
        self.hover = None;
        self.dismiss_completions();
        self.pending_cursor_line = None;
        self.load_file_tree(path.clone(), cx);
        // `load_file_tree` above already set `self.file_tree_root = path` synchronously, so
        // `path` is the active root by the time eviction runs.
        self.evict_stale_lsp_clients(&path, cx);
        self.load_diff(path, cx);
        cx.notify();
    }

    /// Selects a worktree by its real path (rather than an index into
    /// [`Self::worktrees`], which project-mode rows don't carry) - used by a plain worktree
    /// row's click handler in "by project" mode. Falls back to doing nothing if the path
    /// isn't currently in the loaded worktree list (e.g. a stale click racing a reload).
    pub(super) fn select_worktree_by_path(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.worktrees.iter().position(|item| item.path == path) {
            self.select_worktree(index, window, cx);
        }
    }
}

/// Clears every piece of per-worktree UI state that would otherwise survive a worktree switch -
/// called from [`AdeApp::select_worktree`] on every switch. `reviewed_files`/`open_files`/
/// `open_change` are keyed by repo-relative paths with no per-worktree scoping of their own, so
/// without this reset a file reviewed or opened in worktree A would read as already-reviewed (or
/// reopen) in worktree B if it shares the same relative path. `collapsed_dirs` is keyed by
/// absolute path, so it doesn't bleed the same way, but nothing else ever pruned its entries
/// either. A free, `gpui`-free function so this is unit-testable without constructing an
/// `AdeApp`.
pub(super) fn reset_per_worktree_ui_state(
    reviewed_files: &mut HashSet<PathBuf>,
    open_files: &mut Vec<PathBuf>,
    open_change: &mut Option<PathBuf>,
    collapsed_dirs: &mut HashSet<PathBuf>,
    selected_tree_path: &mut Option<PathBuf>,
    edit_buffers: &mut HashMap<PathBuf, edit_buffer::EditBuffer>,
) {
    reviewed_files.clear();
    open_files.clear();
    *open_change = None;
    collapsed_dirs.clear();
    *selected_tree_path = None;
    // Real, live unsaved-edit state (Revision R8.5a) is just as worktree-relative-path-keyed as
    // `open_files` above - without this, a same-named file in a different worktree could
    // silently inherit another worktree's in-memory buffer/cursor/selection.
    edit_buffers.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_per_worktree_ui_state_clears_reviewed_files_and_open_change() {
        let mut reviewed_files = HashSet::new();
        reviewed_files.insert(PathBuf::from("src/main.rs"));
        reviewed_files.insert(PathBuf::from("Cargo.toml"));
        let mut open_files = Vec::new();
        let mut open_change = Some(PathBuf::from("src/main.rs"));
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut edit_buffers = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert!(reviewed_files.is_empty());
        assert_eq!(open_change, None);
    }

    /// Every open tab from the worktree just left must be gone, not just deactivated.
    #[test]
    fn reset_per_worktree_ui_state_clears_every_open_file_tab() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("Cargo.toml"),
            PathBuf::from("README.md"),
        ];
        let mut open_change = Some(PathBuf::from("Cargo.toml"));
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut edit_buffers = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert!(
            open_files.is_empty(),
            "every open file tab from the worktree just left must be cleared, not just \
             deactivated"
        );
    }

    #[test]
    fn reset_per_worktree_ui_state_is_a_no_op_when_already_empty() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = Vec::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut edit_buffers = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert!(reviewed_files.is_empty());
        assert_eq!(open_change, None);
        assert!(collapsed_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_collapsed_dirs_too() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = Vec::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut edit_buffers = HashMap::new();
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/src"));
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/tests"));

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert!(collapsed_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_selected_tree_path() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = Vec::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = Some(PathBuf::from("/repo/worktree-a/src/main.rs"));
        let mut edit_buffers = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert_eq!(selected_tree_path, None);
    }

    /// `edit_buffers` is keyed the same way as `open_files`; without this reset, a same-named
    /// file's real unsaved edits from one worktree could silently reappear (or be silently
    /// overwritten by) an unrelated file in a different worktree.
    #[test]
    fn reset_per_worktree_ui_state_clears_edit_buffers() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = Vec::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut edit_buffers = HashMap::new();
        edit_buffers.insert(
            PathBuf::from("src/main.rs"),
            edit_buffer::EditBuffer::new(
                PathBuf::from("/repo/worktree-a/src/main.rs"),
                "fn main() {}".to_string(),
                Some("rs".to_string()),
                None,
                13,
            ),
        );

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut edit_buffers,
        );

        assert!(edit_buffers.is_empty());
    }
}
