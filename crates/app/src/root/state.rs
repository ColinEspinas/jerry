use super::*;
use crate::root::code_surface::{DiffLoadState, FileLoadState};
use crate::root::sidebar_render::RightSidebarView;

impl AdeApp {
    /// Real, production entry point - loads `~/.config/jerry/settings.toml` for real
    /// (`Settings::load_or_init`, see that method's own docs for why this is one of the rare
    /// deliberate exceptions to this codebase's "never block the foreground thread" rule: it's
    /// a single, tiny file read that must complete before the very first frame renders anyway -
    /// unlike every other blocking-I/O call this project's audits have repeatedly caught, this
    /// one is not a per-render or per-poll cost, it runs exactly once, before a window even
    /// exists) and delegates to [`Self::new_with_settings`].
    pub fn new(repo_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_path = settings_store::settings_toml_path();
        let settings = settings_store::Settings::load_or_init();
        Self::new_with_settings(repo_path, settings, settings_path, window, cx)
    }

    /// The real constructor - takes an already-resolved [`Settings`] value and its real
    /// (optional) source path rather than resolving them itself, so [`Self::new`] (the real
    /// `~/.config/jerry/settings.toml`-backed production path) and
    /// `root::focus::palette_focus_tests::open_test_app` (every GPUI regression test in this
    /// crate's shared entry point) can each supply their own - test app instances get real,
    /// in-memory-only defaults and a `None` path, so [`Self::persist_settings`] is a genuine,
    /// honest no-op for them (the same real code path a genuinely `$HOME`-less production
    /// environment already exercises - see [`settings_store::settings_toml_path`]'s own docs),
    /// never a write to whatever real machine happens to run `cargo test`.
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
            open_change: None,
            open_diff_file_cache: None,
            selected_tree_path: None,
            code_view: code_view::CodeView::Diff,
            code_focus_handle: cx.focus_handle(),
            code_return_focus: None,
            code_opened_session: None,
            file_view_scroll_handle: UniformListScrollHandle::new(),
            file_view_cache: None,
            file_view_last_freshness_check: None,
            file_load_state: FileLoadState::Idle,
            file_view_changed_lines: HashSet::new(),
            code_cursor: None,
            palette_open: false,
            palette_scope: palette::PaletteScope::default(),
            palette_query: String::new(),
            palette_selected: 0,
            palette_focus_handle: cx.focus_handle(),
            palette_return_focus: None,
            palette_opened_session: None,
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
            settings_open: false,
            settings_page: settings::SettingsPage::General,
            settings_focus_handle: cx.focus_handle(),
            settings_return_focus: None,
            settings_opened_session: None,
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
            _merge_write_tasks: Vec::new(),
            lsp_clients: HashMap::new(),
            lsp_opened_files: HashSet::new(),
            file_view_diagnostics: HashMap::new(),
            _lsp_tasks: Vec::new(),
            _lsp_poll_task: None,
            hover: None,
            _hover_request_task: None,
            _goto_definition_tasks: Vec::new(),
            pending_cursor_line: None,
            settings,
            settings_path,
            _settings_save_task: None,
            settings_cfg_format: settings_store::CfgFormat::default(),
            lsp_rows: Vec::new(),
            _lsp_rows_task: None,
            settings_keymap_filter: String::new(),
            settings_keymap_filter_focus_handle: cx.focus_handle(),
        };
        // A fresh window shouldn't open with zero tabs and no way to see anything running -
        // start with one real shell in the repo root, exactly like step 3's single terminal
        // did, except now it's a tab like any other rather than the only pane that can
        // exist.
        this.sessions
            .spawn(SessionKind::Shell, repo_path.clone(), cx);
        // A freshly opened window starts with `Window::focus == None` - nothing is focused
        // until the user clicks something. Left alone, that means every bound action
        // (⌘K/⌘N) falls back to dispatch against the root node, which has no `on_action`
        // handler of its own registered (see `Self::render`'s docs on why those handlers
        // live where they do), so neither works until the user manually clicks into the
        // terminal first. Focusing the initial session's real terminal pane here closes that
        // gap the same way a click into it would.
        if let Some(session) = this.sessions.active() {
            window.focus(&session.pane.focus_handle(cx), cx);
        }
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

    /// Recomputes [`Self::disk_usage`] *and* [`Self::worktree_disk_usage`] from the current
    /// real worktree list, offloaded to the background executor - see
    /// `crate::rail::disk_usage_bytes`'s docs for the real, bounded `std::fs` walk this runs
    /// once per readable worktree. Run once per worktree-list load (not on the 3s status-poll
    /// cadence - a `std::fs` walk is real per-file I/O, and re-walking every worktree's entire
    /// tree every 3s would be needless cost for numbers that only meaningfully change when a
    /// worktree is added, removed, or its files change).
    ///
    /// [`Self::disk_usage`] (the rail footer's aggregate) is always derived from the same
    /// per-path map the Settings › Worktrees page reads - one real computation, two real
    /// consumers, never two separately-run walks that could disagree.
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
        // Any other rail interaction disarms a pending prune confirmation - see
        // `Self::request_prune`'s docs. Browsing to a different worktree is exactly the kind
        // of "I did something else" that must not let a stale armed click land later.
        self.prune_confirm_armed = false;
        // Review/collapse state is per-worktree in spirit but keyed only by repo-relative (or,
        // for `collapsed_dirs`, absolute-but-never-pruned) path (see
        // `reset_per_worktree_ui_state`'s docs) - reset it here so switching worktrees never
        // leaks a "reviewed" checkbox or an open diff from the worktree just left, and never
        // lets `collapsed_dirs` grow forever across however many worktrees get browsed.
        reset_per_worktree_ui_state(
            &mut self.reviewed_files,
            &mut self.open_change,
            &mut self.collapsed_dirs,
            &mut self.selected_tree_path,
        );
        // `code_view`/`file_view_cache`/`file_load_state`/`file_view_changed_lines`/
        // `code_cursor`/`open_diff_file_cache` are the File view's own equivalent per-worktree
        // UI state (a cached parse of a file, and a cached diff lookup, that are both about to
        // belong to a whole different worktree's `file_tree_root`) - reset for exactly the same
        // reason `reset_per_worktree_ui_state`'s own docs give for `open_change`/
        // `reviewed_files`. Dropping `_file_load_task` (rather than leaving it to finish)
        // cancels any real, in-flight load for the worktree just left - see that field's docs.
        self.code_view = code_view::CodeView::Diff;
        self.file_view_cache = None;
        self.file_load_state = FileLoadState::Idle;
        self.file_view_changed_lines = HashSet::new();
        self.code_cursor = None;
        self.open_diff_file_cache = None;
        self._file_load_task = None;
        // The Hover-state cache is per-file (see `Self::hover`'s own docs) - clear it here too,
        // for the same real reason: without this, a hover card from the worktree just left could
        // render again the instant a same-named file happened to be opened in the new one.
        self.hover = None;
        self.pending_cursor_line = None;
        self.load_file_tree(path.clone(), cx);
        // `load_file_tree` (just called, above) synchronously sets `self.file_tree_root = path`
        // before its own background task even starts (see that method's own docs) - so `path`
        // is already the real, active root by the time eviction runs; nothing here races the
        // still-in-flight file-tree load itself.
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

/// Clears every piece of per-worktree UI state that would otherwise survive a worktree switch
/// ([`AdeApp::reviewed_files`], [`AdeApp::open_change`], [`AdeApp::collapsed_dirs`]) - called
/// from [`AdeApp::select_worktree`] on every switch. `reviewed_files`/`open_change` are keyed by
/// repo-relative paths, so without this reset a file reviewed (or opened) in worktree A would
/// silently read as already-reviewed - or reopen a same-named file - in worktree B just because
/// it happens to share the same relative path; neither has any per-worktree scoping of its own.
/// `collapsed_dirs` is keyed by absolute path (so it never visually bleeds the same way - two
/// worktrees are different directories on disk), but nothing ever removed a past worktree's
/// entries either, so it grew unboundedly across however many worktrees got browsed in a
/// session; clearing it here on every switch is the same fix applied for the same reason.
/// Pulled out as a free, `gpui`-free function (rather than an `AdeApp` method) so this behavior
/// is directly unit-testable without needing a `Context<AdeApp>` to construct an `AdeApp` first.
pub(super) fn reset_per_worktree_ui_state(
    reviewed_files: &mut HashSet<PathBuf>,
    open_change: &mut Option<PathBuf>,
    collapsed_dirs: &mut HashSet<PathBuf>,
    selected_tree_path: &mut Option<PathBuf>,
) {
    reviewed_files.clear();
    *open_change = None;
    collapsed_dirs.clear();
    *selected_tree_path = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the cross-worktree state-bleed bug: `reviewed_files`/`open_change`
    /// are keyed only by repo-relative path, so without `reset_per_worktree_ui_state`'s call in
    /// `AdeApp::select_worktree`, a file reviewed (or opened) in one worktree would silently
    /// read as already-reviewed - or reopen a same-named file - in a different worktree that
    /// happens to share the same relative path.
    #[test]
    fn reset_per_worktree_ui_state_clears_reviewed_files_and_open_change() {
        let mut reviewed_files = HashSet::new();
        reviewed_files.insert(PathBuf::from("src/main.rs"));
        reviewed_files.insert(PathBuf::from("Cargo.toml"));
        let mut open_change = Some(PathBuf::from("src/main.rs"));
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
        );

        assert!(reviewed_files.is_empty());
        assert_eq!(open_change, None);
    }

    #[test]
    fn reset_per_worktree_ui_state_is_a_no_op_when_already_empty() {
        let mut reviewed_files = HashSet::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
        );

        assert!(reviewed_files.is_empty());
        assert_eq!(open_change, None);
        assert!(collapsed_dirs.is_empty());
    }

    /// Regression test for the "never pruned" half of the same bug (item f): `collapsed_dirs`
    /// is keyed by absolute path, so it doesn't visually bleed between worktrees the way
    /// `reviewed_files` does, but nothing removed a past worktree's entries either, so it grew
    /// unboundedly across however many worktrees got browsed in a session.
    #[test]
    fn reset_per_worktree_ui_state_clears_collapsed_dirs_too() {
        let mut reviewed_files = HashSet::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = None;
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/src"));
        collapsed_dirs.insert(PathBuf::from("/repo/worktree-a/tests"));

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
        );

        assert!(collapsed_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_selected_tree_path() {
        let mut reviewed_files = HashSet::new();
        let mut open_change = None;
        let mut collapsed_dirs = HashSet::new();
        let mut selected_tree_path = Some(PathBuf::from("/repo/worktree-a/src/main.rs"));

        reset_per_worktree_ui_state(
            &mut reviewed_files,
            &mut open_change,
            &mut collapsed_dirs,
            &mut selected_tree_path,
        );

        assert_eq!(selected_tree_path, None);
    }
}
