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
            file_view_last_freshness_check: None,
            file_load_state: FileLoadState::Idle,
            file_view_changed_lines: HashSet::new(),
            code_cursor: None,
            code_zoom_percent: AdeApp::ZOOM_DEFAULT_PERCENT,
            file_zoom_percent: HashMap::new(),
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
            disk_usage: None,
            worktree_disk_usage: HashMap::new(),
            prune_status: None,
            prune_confirm_armed: false,
            prune_in_flight: false,
            settings_open: false,
            settings_page: settings::SettingsPage::General,
            settings_focus_handle: cx.focus_handle(),
            settings_focus: OverlayFocus::default(),
            agent_rows: Vec::new(),
            merge_flow: None,
            merge_op_in_flight: false,
            _load_worktrees_task: None,
            _load_file_tree_task: None,
            _load_diff_task: None,
            _file_load_task: None,
            _status_poll_task: None,
            _disk_usage_task: None,
            _prune_task: None,
            _agent_rows_task: None,
            _merge_task: None,
            _merge_cleanup_task: None,
            _merge_write_tasks: TaskPool::new(),
            lsp_clients: HashMap::new(),
            lsp_opened_files: HashSet::new(),
            file_view_diagnostics: HashMap::new(),
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
            plus_menu_open: false,
            plus_button_bounds: gpui::Bounds::default(),
            _new_agent_pane_task: TaskPool::new(),
        };
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

    pub(super) fn select_worktree(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.worktrees.get(index) else {
            return;
        };
        if item.error.is_some() {
            // An unreadable entry has no usable path; nothing to select into.
            return;
        }
        let path = item.path.clone();
        self.selected = Some(index);
        // Browsing to a different worktree disarms a pending prune confirmation - see
        // `Self::request_prune`'s docs.
        self.prune_confirm_armed = false;
        // Reset per-worktree UI state (see `reset_per_worktree_ui_state`'s docs) so switching
        // worktrees never leaks a "reviewed" checkbox, open diff, or collapsed-dir entry from
        // the worktree just left.
        reset_per_worktree_ui_state(
            &mut self.reviewed_files,
            &mut self.open_files,
            &mut self.open_change,
            &mut self.collapsed_dirs,
            &mut self.selected_tree_path,
            &mut self.file_zoom_percent,
        );
        // The File view's own per-worktree state (a cached parse and diff lookup that are about
        // to belong to a different `file_tree_root`) - reset for the same reason as above.
        // Dropping `_file_load_task` cancels any in-flight load for the worktree just left.
        self.code_view = code_view::CodeView::Diff;
        self.file_view_cache = None;
        self.file_load_state = FileLoadState::Idle;
        self.file_view_changed_lines = HashSet::new();
        self.code_cursor = None;
        self.open_diff_file_cache = None;
        self._file_load_task = None;
        self.code_zoom_percent = AdeApp::ZOOM_DEFAULT_PERCENT;
        // The hover cache is per-file - clear it too, or a hover card from the worktree just
        // left could reappear the instant a same-named file opens in the new one.
        self.hover = None;
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
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.worktrees.iter().position(|item| item.path == path) {
            self.select_worktree(index, cx);
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
    file_zoom_percent: &mut HashMap<PathBuf, u16>,
) {
    reviewed_files.clear();
    open_files.clear();
    *open_change = None;
    collapsed_dirs.clear();
    *selected_tree_path = None;
    file_zoom_percent.clear();
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
        let mut file_zoom_percent = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
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
        let mut file_zoom_percent = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
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
        let mut file_zoom_percent = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
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
        let mut file_zoom_percent = HashMap::new();
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/src"));
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/tests"));

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
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
        let mut file_zoom_percent = HashMap::new();

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
        );

        assert_eq!(selected_tree_path, None);
    }

    /// `file_zoom_percent` is keyed the same way as `open_files`; without this reset a zoom
    /// level remembered for `src/main.rs` in one worktree would apply to a same-named file in a
    /// different one.
    #[test]
    fn reset_per_worktree_ui_state_clears_file_zoom_percent() {
        let mut reviewed_files = HashSet::new();
        let mut open_files = Vec::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        let mut file_zoom_percent = HashMap::new();
        file_zoom_percent.insert(PathBuf::from("src/main.rs"), 150u16);
        file_zoom_percent.insert(PathBuf::from("Cargo.toml"), 80u16);

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
            &mut file_zoom_percent,
        );

        assert!(file_zoom_percent.is_empty());
    }
}
