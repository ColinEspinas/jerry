use super::*;
use crate::code_surface::state::{DiffLoadState, FileLoadState};
use crate::sidebar::render::RightSidebarView;

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
    pub(crate) fn new_with_settings(
        repo_path: PathBuf,
        settings: settings_store::Settings,
        settings_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The fold-state file is resolved as a sibling of whatever settings path this instance
        // was given (`crate::sidebar::fold_state::fold_state_path_for`), so a test with a
        // temp-dir settings path gets real, isolated fold-state persistence and a test with
        // `None` gets none at all - the same seam, one decision.
        let fold_state_path = settings_path
            .as_deref()
            .map(fold_state::fold_state_path_for);
        let fold_state = fold_state_path
            .as_deref()
            .map(fold_state::FoldState::load_at)
            .unwrap_or_default();
        // Issue #18 §1/§2: the tree opens with exactly what this worktree had expanded last
        // time, which for a worktree this file has never seen (including a freshly created one)
        // is nothing at all.

        // The tab-order file mirrors the fold-state file's own resolution immediately above -
        // see `work_surface::tab_order_state`'s own module docs for the identical pattern this
        // copies. Issue #16's own "persists per session/worktree and restores on relaunch".
        let tab_order_path = settings_path
            .as_deref()
            .map(crate::work_surface::tab_order_state::tab_order_path_for);
        let tab_order_state = tab_order_path
            .as_deref()
            .map(crate::work_surface::tab_order_state::TabOrderState::load_at)
            .unwrap_or_default();

        // The repo-list file mirrors the fold-state file's own resolution one line up (see
        // `rail::repo`'s module docs for why it's the identical pattern): a sibling of whatever
        // settings path this instance was given, `None` (and so no persistence at all) for a test
        // that doesn't opt into a real one. Every repo this user has previously added is restored
        // here - not just the one `repo_path` names below - so "which repos are added" genuinely
        // survives a restart; nothing yet *renders* more than the focused one (see
        // `Self::worktrees`'s own docs), so a returning user with several repos added in a
        // previous agent sees exactly the same single-repo view they always have, just with the
        // rest of their repos already known to `Self::repos` for the rail-rendering phase that
        // will actually show them.
        let repo_state_path = settings_path.as_deref().map(repo::repo_state_path_for);
        let loaded_repo_state = repo_state_path
            .as_deref()
            .map(repo::RepoState::load_at)
            .unwrap_or_default();
        let mut repos: Vec<Repo> = Vec::with_capacity(loaded_repo_state.repos.len());
        let mut next_repo_id: u64 = 0;
        for (key, record) in &loaded_repo_state.repos {
            repos.push(Repo {
                id: RepoId(next_repo_id),
                path: PathBuf::from(key),
                name: record.name.clone(),
                worktrees: Vec::new(),
            });
            next_repo_id += 1;
        }

        // Built before the `Self` literal below (rather than inline as `cx.focus_handle()` at
        // their own field positions, as every other focus handle in this literal is) because
        // `AdeApp::wire_caret_blink` needs real, already-constructed handles to subscribe to -
        // there is no `self` yet to read them back off of at this point in construction. Moved
        // into the literal below by their own field-init-shorthand once wiring is done.
        let code_focus_handle = cx.focus_handle();
        let merge_edit_focus_handle = cx.focus_handle();
        let palette_focus_handle = cx.focus_handle();
        let tree_focus_handle = cx.focus_handle();
        let filter_focus_handle = cx.focus_handle();
        let settings_keymap_filter_focus_handle = cx.focus_handle();
        let caret_blink_subscriptions = AdeApp::wire_caret_blink(
            &[
                &code_focus_handle,
                &merge_edit_focus_handle,
                &palette_focus_handle,
                &tree_focus_handle,
                &filter_focus_handle,
                &settings_keymap_filter_focus_handle,
            ],
            window,
            cx,
        );

        // GitHub issue #5: real, additional themes loaded from disk, before `apply_theme_selection`
        // (below) needs to resolve `settings.theme.name` against them. Derived from
        // `settings_path` (`custom_theme::custom_themes_dir_for`), not `$HOME` directly - the same
        // seam `fold_state_path`/`fold_state` just above use, so a test constructed with a `None`/
        // temp-dir settings path never touches the real developer machine's own
        // `~/.config/jerry/themes`. Same "block the foreground thread once, at startup" exception
        // `Settings::load_or_init` itself already takes - a small, one-time directory read, not a
        // per-render or per-poll cost.
        let (custom_themes, custom_theme_load_errors) = match settings_path.as_deref() {
            Some(path) => custom_theme::load_custom_themes_from_dir(
                &custom_theme::custom_themes_dir_for(path),
            ),
            None => (Vec::new(), Vec::new()),
        };

        let mut this = Self {
            file_tree_root: repo_path.clone(),
            diff_root: repo_path.clone(),
            repos,
            // Resolved just below the literal, via `Self::add_repo`/`Self::focus_repo` - there is
            // no `self` yet at this point in construction to call them against.
            focused_repo: None,
            next_repo_id,
            repo_state_path,
            repo_state_owned: std::collections::BTreeSet::new(),
            _repo_state_save_task: None,
            repo_state_save_pending: false,
            repo_state_save_running: false,
            worktrees: Vec::new(),
            worktrees_error: None,
            worktree_selection_notice: None,
            rail_scroll_handle: gpui::ScrollHandle::new(),
            selected: None,
            agents: Agents::new(),
            file_tree: Vec::new(),
            file_tree_error: None,
            right_sidebar_view: RightSidebarView::Files,
            file_tree_scroll_handle: UniformListScrollHandle::new(),
            changes_rows_scroll_handle: UniformListScrollHandle::new(),
            diff_state: DiffLoadState::Loading,
            diff_totals: None,
            file_authorship: changes::Authorship::default(),
            // Both are filled in immediately after this literal, through the same single
            // chokepoints every later change uses (`set_file_tree_root` +
            // `reload_expanded_dirs_from_fold_state`) - a second, constructor-only copy of that
            // resolution is exactly how the cached key and the root it describes drift apart.
            expanded_dirs: HashSet::new(),
            fold_state,
            fold_state_path,
            fold_state_root_key: None,
            fold_state_owned: std::collections::BTreeSet::new(),
            _fold_state_save_task: None,
            fold_state_save_pending: false,
            fold_state_save_running: false,
            file_tree_truncated: false,
            // Nothing has been walked yet, so nothing may be pruned yet either.
            file_tree_complete: false,
            file_tree_limit_override: None,
            tree_context_menu: None,
            tree_inline_edit: None,
            tree_clipboard: None,
            tree_delete_confirm: None,
            tree_op_error: None,
            tree_focus_handle,
            file_tree_bounds: gpui::Bounds::default(),
            _tree_delete_task: None,
            _tree_copy_task: None,
            staged_files: HashSet::new(),
            staging_error: None,
            _stage_tasks: TaskPool::new(),
            commit_menu_open: false,
            open_files_by_worktree: HashMap::new(),
            open_change: None,
            close_tab_confirm_armed: None,
            open_diff_file_cache: None,
            selected_tree_path: None,
            code_view: code_view::CodeView::Diff,
            code_focus_handle,
            code_focus: OverlayFocus::default(),
            // `true`/`Task::ready(())`: no blink loop is running yet (nothing is focused at
            // construction - a fresh window focuses the initial agent's terminal pane, not
            // the code editor, a few lines below), and `Self::start_caret_blink` will replace
            // this the moment a real caret-bearing handle is - see
            // `crate::root::caret_blink`'s module docs.
            caret_blink_visible: true,
            _caret_blink_task: Task::ready(()),
            _caret_blink_subscriptions: caret_blink_subscriptions,
            graph_tab_open: false,
            graph_tab_active: false,
            graph_focus_handle: cx.focus_handle(),
            graph_focus: OverlayFocus::default(),
            graph_state: graph_view::state::GraphTabState::new(cx),
            _load_graph_task: None,
            _load_commit_files_task: None,
            file_view_scroll_handle: UniformListScrollHandle::new(),
            diff_view_scroll_handle: gpui::ScrollHandle::new(),
            file_view_cache: None,
            diff_highlight_cache: None,
            file_view_last_freshness_check: None,
            file_load_state: FileLoadState::Idle,
            file_view_changed_lines: HashSet::new(),
            minimap_panel_bounds: gpui::Bounds::default(),
            code_cursor: None,
            blame_cache: HashMap::new(),
            blame_state: HashMap::new(),
            _blame_tasks: HashMap::new(),
            blame_last_freshness_check: None,
            blame_commit_messages: HashMap::new(),
            _blame_message_tasks: HashMap::new(),
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
            palette_results_scroll_handle: gpui::ScrollHandle::new(),
            palette_scope: palette::PaletteScope::default(),
            palette_query: text_history::TextField::new(),
            palette_selected: 0,
            palette_focus_handle,
            palette_focus: OverlayFocus::default(),
            palette_file_candidates: Vec::new(),
            rail_width: px(layout::RAIL_DEFAULT),
            panel_width: px(layout::PANEL_DEFAULT),
            body_bounds: gpui::Bounds::default(),
            title_bar_move_armed: false,
            filter_query: text_history::TextField::new(),
            rail_collapse_overrides: HashMap::new(),
            filter_focus_handle,
            rail_focus_handle: cx.focus_handle(),
            diff_cache: HashMap::new(),
            worktree_notes: HashMap::new(),
            ahead_behind_cache: HashMap::new(),
            process_stats: HashMap::new(),
            disk_usage: None,
            worktree_disk_usage: HashMap::new(),
            prune_status: None,
            prune_confirm_armed: false,
            prune_in_flight: false,
            worktree_history_op_in_flight: None,
            worktree_history_status: None,
            discard_confirm_armed: None,
            settings_open: false,
            settings_nav_scroll_handle: gpui::ScrollHandle::new(),
            settings_content_scroll_handle: gpui::ScrollHandle::new(),
            settings_page: settings::SettingsPage::General,
            settings_focus_handle: cx.focus_handle(),
            settings_focus: OverlayFocus::default(),
            agent_rows: Vec::new(),
            merge_flow: None,
            merge_op_in_flight: false,
            merge_highlight_cache: None,
            merge_edit: None,
            merge_edit_focus_handle,
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
            _worktree_watcher: None,
            _worktree_watch_task: None,
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
            settings_keymap_filter: text_history::TextField::new(),
            settings_keymap_filter_focus_handle,
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
            tab_order: HashMap::new(),
            tab_order_state,
            tab_order_path,
            tab_order_owned: std::collections::BTreeSet::new(),
            _tab_order_save_task: None,
            tab_drag_insertion: None,
            custom_themes,
            custom_theme_load_errors,
            custom_theme_status: None,
            _custom_theme_import_task: None,
            _custom_theme_export_task: None,
            _custom_theme_remove_task: None,
            _custom_theme_create_task: None,
            custom_theme_remove_armed: None,
        };
        // GitHub issue #45 ("Input blink only on focused input or file") / a live follow-up
        // report of missing carets: `graph_state.branches_filter_focus_handle` (added later, in
        // Revision R12's git graph tab) and `new_file_focus_handle` are two more genuine
        // caret-bearing `FocusHandle`s - real, hand-rolled `text_history::TextField` inputs, the
        // same shape as the six wired above - that never got threaded through
        // `Self::wire_caret_blink`. They can't join that earlier call: it runs before `this`
        // exists, and both handles live *inside* `this` (one nested in `graph_state`, built by
        // this same literal). Wired here instead, appending onto the very same
        // `_caret_blink_subscriptions` vec so there's still exactly one place holding every real
        // caret subscription this app has, not two.
        let extra_caret_blink_subscriptions = AdeApp::wire_caret_blink(
            &[
                &this.graph_state.branches_filter_focus_handle,
                &this.new_file_focus_handle,
            ],
            window,
            cx,
        );
        this._caret_blink_subscriptions
            .extend(extra_caret_blink_subscriptions);
        // Real "add this one repo on startup" (Revision R12 Phase 0): the CLI argument (or a
        // test's own `repo_path`) becomes the first, focused entry of `Self::repos` rather than a
        // separate field - `Self::add_repo` is idempotent against whatever `loaded_repo_state`
        // above already restored, so a repeat launch of the same path never duplicates it. Every
        // other single-repo-scoped field below (`file_tree_root`/`diff_root`/the initial shell's
        // cwd/`load_worktrees`) still reads `repo_path` directly rather than
        // `Self::focused_repo_path()` - they're the same value here by construction, and `Self::
        // focused_repo_path()` becomes the real accessor once a repo switch is more than "one repo
        // was ever set".
        let focused_repo_id = this.add_repo(repo_path.clone(), cx);
        this.focus_repo(focused_repo_id);
        // See the `expanded_dirs`/`fold_state_root_key` note in the literal above: resolving the
        // worktree key here, through the one function that ever resolves it, is what keeps the
        // startup path and every later worktree switch structurally identical.
        this.set_file_tree_root(repo_path.clone());
        this.reload_expanded_dirs_from_fold_state();
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
        // `focus_active` below moves real keyboard focus onto it - see `Agents::focus_active`'s
        // docs and this crate's `OverlayFocus`/`restore_focus` docs for why a fresh window must
        // never start with `Window::focus == None`.
        this.agents.spawn(
            AgentKind::Shell,
            repo_path.clone(),
            this.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        this.agents.focus_active(window, cx);
        this.load_worktrees(cx);
        this.load_file_tree(repo_path.clone(), cx);
        this.load_diff(repo_path, cx);
        this.start_status_polling(cx);
        this.start_worktree_watch(cx);
        this
    }

    /// The **one** real code path that (re)populates [`Self::worktrees`] - GitHub issue #12.
    /// Called on startup, after every explicit in-IDE worktree mutation
    /// (`Self::execute_prune`, the merge/undo flows' own `load_worktrees` calls), *and* by the
    /// live watcher/poll refresh loop (`crate::rail::worktree_watch`,
    /// `Self::start_worktree_watch`) - there is no separate "optimistic insert" that patches
    /// [`Self::worktrees`] directly anywhere else in this crate, so the panel can never diverge
    /// from a real `git worktree list --porcelain` re-parse.
    ///
    /// Also runs [`crate::rail::worktrees::recover_selection`] against the previously selected
    /// worktree (by path - the only stable identity a worktree has across a refresh) every time:
    /// still present and usable → [`Self::selected`] is remapped to its new index with no other
    /// effect; gone or newly broken → falls back to the main worktree and sets
    /// [`Self::worktree_selection_notice`]. See that function's docs for the full state machine.
    pub(crate) fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.focused_repo_path();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::list_worktrees_porcelain(&repo_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Captured *before* `this.worktrees` is overwritten below - `recover_selection`
                // needs the old entry's own path/label, which won't exist in the new list.
                let previously_selected = this
                    .selected
                    .and_then(|index| this.worktrees.get(index))
                    .cloned();

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

                match worktrees::recover_selection(previously_selected.as_ref(), &this.worktrees) {
                    worktrees::SelectionRecovery::NoPriorSelection => {}
                    worktrees::SelectionRecovery::Unchanged(index) => {
                        this.selected = Some(index);
                    }
                    worktrees::SelectionRecovery::FellBackToMain { new_index, notice } => {
                        this.selected = new_index;
                        this.worktree_selection_notice = Some(notice);
                        // The file tree/diff panes were pointed at the worktree that just
                        // vanished - re-root them at wherever selection landed (or the repo
                        // root, if even the main worktree is gone) rather than leaving them
                        // showing a directory that no longer resolves to anything real. This is
                        // deliberately *not* a full `Self::select_worktree` call: that also
                        // moves real keyboard focus, which needs a `&mut Window` this
                        // background-task callback doesn't have (see its own docs) - a refresh
                        // triggered by an external `git worktree remove`/a background poll tick
                        // has no user click to anchor a focus change to in the first place.
                        let new_root = new_index
                            .and_then(|index| this.worktrees.get(index))
                            .map(|item| item.path.clone())
                            .unwrap_or_else(|| this.focused_repo_path());
                        this.set_file_tree_root(new_root.clone());
                        this.file_tree = Vec::new();
                        this.reload_expanded_dirs_from_fold_state();
                        this.load_file_tree(new_root.clone(), cx);
                        this.load_diff(new_root, cx);
                    }
                }

                this.load_disk_usage(cx);
                cx.notify();
            });
        });
        self._load_worktrees_task = Some(task);
    }

    /// Recomputes [`Self::disk_usage`] and [`Self::worktree_disk_usage`] from the current
    /// worktree list, offloaded to the background executor (`crate::rail::state::disk_usage_bytes`).
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

    /// The **one** place [`Self::file_tree_root`] is ever assigned, so its cached
    /// `fold_state_root_key` can never drift out of step with it - the identity-guard discipline
    /// this codebase has been bitten by repeatedly, applied to a derived value rather than a
    /// borrowed one. `worktree_key`'s blocking `canonicalize` therefore runs once per real
    /// worktree change and never on a click or a render.
    /// Re-derives [`Self::expanded_dirs`] from the persisted fold state for whatever
    /// [`Self::file_tree_root`] currently is - the one place that mapping is made, shared by
    /// startup and by every worktree switch.
    pub(crate) fn reload_expanded_dirs_from_fold_state(&mut self) {
        self.expanded_dirs = match &self.fold_state_root_key {
            Some(key) => self
                .fold_state
                .expanded_dirs_with_key(key, &self.file_tree_root),
            None => HashSet::new(),
        };
    }

    pub(crate) fn set_file_tree_root(&mut self, root: PathBuf) {
        if self.file_tree_root == root && self.fold_state_root_key.is_some() {
            return;
        }
        self.fold_state_root_key = fold_state::worktree_key(&root);
        self.file_tree_root = root;
    }

    pub(crate) fn load_file_tree(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.set_file_tree_root(root.clone());
        // Always a real bound - see [`Self::file_tree_limit_override`] for why even the "load
        // more" escape hatch raises the cap rather than removing it.
        let limit = Some(
            self.file_tree_limit_override
                .unwrap_or(self.settings.file_tree.max_entries),
        );
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { file_tree::build_file_tree(&root, limit) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Identity guard: a worktree switch that lands while this walk is still running
                // replaces `_load_file_tree_task` (cancelling it) *and* moves
                // `file_tree_root` - but a task already past its `await` point can still reach
                // here. Applying a stale walk would show one worktree's tree under another's
                // root, and - far worse now - prune the *new* worktree's fold state against the
                // *old* worktree's directory list.
                if this.file_tree_root != root {
                    return;
                }
                match result {
                    Ok(listing) => {
                        this.file_tree_truncated = listing.truncated;
                        this.file_tree_complete = listing.is_complete();
                        this.file_tree = listing.entries;
                        this.file_tree_error = None;
                        this.prune_stale_fold_state(cx);
                    }
                    Err(err) => {
                        this.file_tree = Vec::new();
                        this.file_tree_truncated = false;
                        this.file_tree_complete = false;
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

    /// The worktree a *new* agent should be spawned into: the selected worktree's real
    /// path if one is selected and readable, otherwise the repo root - see the module docs'
    /// "Agents/tabs" section for why this is resolved at spawn time rather than tracked as
    /// a per-tab "current worktree".
    pub(crate) fn active_agent_cwd(&self) -> PathBuf {
        match self.selected.and_then(|index| self.worktrees.get(index)) {
            Some(item) if item.error.is_none() => item.path.clone(),
            _ => self.focused_repo_path(),
        }
    }

    pub(crate) fn select_worktree(
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
        // A real, explicit user selection supersedes any stale "fell back to main" notice a
        // previous refresh may have left up - see `Self::worktree_selection_notice`'s own docs.
        self.worktree_selection_notice = None;
        // Makes this worktree's own last-active tab (or its first agent, or none) the
        // globally active one - see `Agents::activate_for_worktree`'s own docs for why this
        // invariant ("the active agent always belongs to the selected worktree") is the real
        // fix this revision makes: before it, selecting a worktree never touched `self.agents`
        // at all, so the centre pane could keep showing a completely different worktree's
        // terminal after a rail click.
        self.agents.activate_for_worktree(&path, cx);
        // Browsing to a different worktree disarms a pending prune confirmation - see
        // `Self::request_prune`'s docs.
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        // Same reasoning for the commit composer's own split-button popover (Revision R12 §5) -
        // it targets the *previously* selected worktree's staged set, so it must not stay open
        // pointed at the wrong one.
        self.commit_menu_open = false;
        // Reset per-worktree UI state (see `reset_per_worktree_ui_state`'s docs) so switching
        // worktrees never leaks a staged checkbox, open diff, or collapsed-dir entry from
        // the worktree just left. Deliberately runs *before* the focus-fallback block below: both
        // `focus_newly_spawned_agent` and the `filter_focus_handle` fallback branch on
        // `self.open_change`, and until this call runs, `open_change` can still reflect the
        // worktree just *left* (a file tab open there) rather than the real post-switch state -
        // evaluating either guard first (a real, live-reproduced bug found in this revision's own
        // self-audit) made both branches see a stale `Some` and skip moving focus at all, leaving
        // `Window::focus` dangling on `code_focus_handle` once `open_change` was cleared one
        // statement later.
        reset_per_worktree_ui_state(
            &mut self.staged_files,
            &mut self.open_change,
            &mut self.expanded_dirs,
            &mut self.selected_tree_path,
        );
        // The tree's fold state is per-worktree *persisted* state, not merely per-worktree
        // transient state: the reset above clears the live set, and this re-derives it from the
        // worktree genuinely being switched *to*. A worktree with no recorded state (a freshly
        // created one, say) gets an empty set and so opens fully collapsed - the issue's own
        // suggested answer to "does a fresh worktree inherit anything".
        //
        // `file_tree_root`/`file_tree` move in the *same* step, deliberately: `load_file_tree`
        // below sets the root too, but its walk is asynchronous, so without this the frames
        // between here and the walk landing would render the worktree just left's rows against
        // the new worktree's expanded set - and a click on one of those stale rows would reach
        // `set_dir_expanded` with a path from a different worktree entirely. Clearing
        // `file_tree` makes that window render an honestly empty tree instead.
        self.set_file_tree_root(path.clone());
        self.file_tree = Vec::new();
        self.reload_expanded_dirs_from_fold_state();
        // Every one of these holds an absolute path in the worktree being *left* (GitHub issue
        // #19): an open context menu targeting a row that is about to stop existing, a
        // half-typed name for a folder in the old tree, a cut/copied entry a paste here would
        // move across worktrees, and an armed delete for a path in the old tree. Cleared
        // together, in the same step as `file_tree`/`expanded_dirs` above and for the same
        // reason - the window between here and the new walk landing must not leave a control
        // pointing at the old worktree.
        self.tree_context_menu = None;
        self.tree_inline_edit = None;
        self.tree_clipboard = None;
        self.tree_delete_confirm = None;
        self.tree_op_error = None;
        // "Show me the whole listing" was a decision about the worktree being left.
        self.file_tree_limit_override = None;
        self.file_tree_truncated = false;
        self.file_tree_complete = false;
        // `focus_newly_spawned_agent` (despite its name - its body has no "newly spawned" logic
        // in it, just the shared "move focus unless a file tab/Settings is showing" guard) closes
        // the dangling-focus risk this switch creates: the previously-active agent's pane may
        // no longer be part of the rendered tree at all once the tab strip's own per-worktree
        // filter (`Self::render_tab_strip`) applies, so keyboard focus left pointing at it would
        // silently break every keybinding until the next click - the same "focus left pointing at
        // something no longer rendered" bug class this project's own `OverlayFocus`/
        // `restore_focus` mechanism exists to prevent, applied here to a plain worktree switch
        // rather than an overlay open/close. `open_change` above is already this switch's real
        // post-reset value by the time this runs, so the guard it checks is genuine.
        self.focus_newly_spawned_agent(window, cx);
        // `focus_newly_spawned_agent` is a real no-op when the newly selected worktree has no
        // open agent at all (`Agents::focus_active` has nothing to focus) - so if a
        // previously-focused agent's pane belonged to the worktree just left, it's now exactly
        // as dangling as the case the comment above already covers, just with no agent to
        // redirect *onto*. Fall back to the rail's own root container
        // (`Self::rail_focus_handle`), which is part of the rendered tree whenever the
        // workspace body is showing (never while Settings has replaced it - `!self.settings_open`
        // guards that the same way `focus_newly_spawned_agent` itself does). Deliberately the
        // rail's root, not its filter field, which this used to target - see
        // `Self::rail_focus_handle`'s own docs for the real, audit-found keystroke-swallowing bug
        // that became once the filter field started carrying a `"text-input"` key context. It
        // keeps the focused `FocusId` genuinely findable in the next rendered frame, which is the
        // actual invariant this exists to protect: a dangling `FocusId` makes GPUI's action
        // dispatch fall back to a disconnected root with no real `on_action` handlers at all, not
        // just this worktree's own missing ones - silently breaking every global keybinding (⌘P
        // included) until the next click.
        if self.agents.active_id().is_none() && self.open_change.is_none() && !self.settings_open {
            window.focus(&self.rail_focus_handle, cx);
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
        // Real blame state (GitHub issue #29) is absolute-path-keyed - cleared alongside the
        // hover cache above for the identical reason: without this, a same-named file's blame
        // from the worktree just left could reappear (wrongly attributed) the instant a
        // same-named file opens in the new one, and a stale `Loading`/in-flight task from the
        // old worktree has no reason to keep running once its own worktree is no longer active.
        self.blame_cache.clear();
        self.blame_state.clear();
        self._blame_tasks.clear();
        self.blame_last_freshness_check = None;
        // Commit messages are sha-keyed, not path-keyed - a sha means the same real commit
        // regardless of which worktree it's viewed from, so this cache deliberately survives a
        // worktree switch (the same "safe to keep, sha is a real global identity" reasoning
        // `AdeApp::lsp_uri_cache`'s own root-scoped-only pruning already applies elsewhere).
        self.load_file_tree(path.clone(), cx);
        // `load_file_tree` above already set `self.file_tree_root = path` synchronously, so
        // `path` is the active root by the time eviction runs.
        self.evict_stale_lsp_clients(&path, cx);
        self.load_diff(path, cx);
        // The graph tab is repo-scoped, not worktree-scoped (design spec §1: "the graph is
        // repo-scoped"), but the toolbar's `HEAD` chip/upstream counts and the Worktrees scope
        // (`wt_core::graph::GraphScope::Worktrees`, driven by real worktree HEADs) are real facts
        // about *this* worktree - without this, switching worktrees while the graph tab is open
        // silently left it showing the previous worktree's data (a real, adversarial-audit-found
        // gap), and the Commit panel's cached "Files changed" could even fail against the wrong
        // repo path. `load_diff` above already updated `Self::diff_root` synchronously, so
        // `load_graph` (which reads it) picks up the new worktree correctly.
        if self.graph_tab_open {
            self.load_graph(cx);
        }
        cx.notify();
    }

    /// The current worktree's open file tabs (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §3) - an empty slice for a worktree that has never opened one,
    /// never a panic or a fabricated default. See [`Self::open_files_by_worktree`]'s own docs for
    /// why this is keyed by [`Self::file_tree_root`] rather than a flat, unscoped `Vec`.
    pub(crate) fn open_files(&self) -> &[PathBuf] {
        self.open_files_by_worktree
            .get(&self.file_tree_root)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The current worktree's open file tabs, mutably - creates an empty entry for a worktree
    /// that has never opened one rather than requiring a separate seeding step. Every real
    /// mutation of [`Self::open_files_by_worktree`] (push/remove/rename-remap) goes through this,
    /// matching [`Self::open_files`]'s own read-side resolution.
    pub(crate) fn open_files_mut(&mut self) -> &mut Vec<PathBuf> {
        self.open_files_by_worktree
            .entry(self.file_tree_root.clone())
            .or_default()
    }

    /// The `(worktree, worktree-relative path)` composite key [`Self::edit_buffers`] is keyed by,
    /// resolved from whatever [`Self::file_tree_root`] is *right now* - safe for any synchronous
    /// call site, but never for a real `cx.spawn` task resuming after an `.await` (see
    /// [`Self::edit_buffers`]'s own docs on why; use [`Self::edit_buffer_at`]/
    /// [`Self::edit_buffer_at_mut`] there instead, with a `cwd` captured before the await).
    fn edit_buffer_key(&self, path: &std::path::Path) -> (PathBuf, PathBuf) {
        (self.file_tree_root.clone(), path.to_path_buf())
    }

    /// The current worktree's edit buffer for `path`, if one exists. See [`Self::edit_buffer_key`]
    /// for why this is only safe from a synchronous call site.
    pub(crate) fn edit_buffer(&self, path: &std::path::Path) -> Option<&edit_buffer::EditBuffer> {
        self.edit_buffers.get(&self.edit_buffer_key(path))
    }

    /// Mutable sibling of [`Self::edit_buffer`] - same synchronous-only caveat.
    pub(crate) fn edit_buffer_mut(
        &mut self,
        path: &std::path::Path,
    ) -> Option<&mut edit_buffer::EditBuffer> {
        let key = self.edit_buffer_key(path);
        self.edit_buffers.get_mut(&key)
    }

    /// `true` iff the current worktree has a real edit buffer for `path`. Same synchronous-only
    /// caveat as [`Self::edit_buffer`].
    pub(crate) fn edit_buffer_contains(&self, path: &std::path::Path) -> bool {
        self.edit_buffers.contains_key(&self.edit_buffer_key(path))
    }

    /// Inserts (or replaces) `buffer` for `path` in the *current* worktree - the same
    /// synchronous-only caveat as [`Self::edit_buffer`] applies to which worktree "current" means.
    pub(crate) fn insert_edit_buffer(
        &mut self,
        path: PathBuf,
        buffer: edit_buffer::EditBuffer,
    ) -> Option<edit_buffer::EditBuffer> {
        let key = self.edit_buffer_key(&path);
        self.edit_buffers.insert(key, buffer)
    }

    /// Explicit-key read for a real `cx.spawn` task resuming after an `.await` - `cwd` must be
    /// captured **before** the await (typically alongside `path` itself, in the same closure
    /// capture list), never re-derived from [`Self::file_tree_root`] once the task resumes. See
    /// [`Self::edit_buffers`]'s own docs for the stale-worktree bug this exists to prevent.
    pub(crate) fn edit_buffer_at(
        &self,
        cwd: &std::path::Path,
        path: &std::path::Path,
    ) -> Option<&edit_buffer::EditBuffer> {
        self.edit_buffers
            .get(&(cwd.to_path_buf(), path.to_path_buf()))
    }

    /// Mutable sibling of [`Self::edit_buffer_at`] - same "capture `cwd` before the await" rule.
    pub(crate) fn edit_buffer_at_mut(
        &mut self,
        cwd: &std::path::Path,
        path: &std::path::Path,
    ) -> Option<&mut edit_buffer::EditBuffer> {
        self.edit_buffers
            .get_mut(&(cwd.to_path_buf(), path.to_path_buf()))
    }

    /// Removes and returns the current worktree's edit buffer for `path`, if any. Same
    /// synchronous-only caveat as [`Self::edit_buffer`]. Test-only seam (no real production call
    /// site removes a single buffer directly - see [`Self::edit_buffers`]'s own "deliberately not
    /// removed" docs for why): used to simulate a buffer vanishing mid-flight, e.g. in
    /// `crate::code_surface::editing::editing_tests`'s writer-loop coverage.
    #[cfg(test)]
    pub(crate) fn remove_edit_buffer(
        &mut self,
        path: &std::path::Path,
    ) -> Option<edit_buffer::EditBuffer> {
        let key = self.edit_buffer_key(path);
        self.edit_buffers.remove(&key)
    }

    /// Explicit-key insert, the [`Self::insert_edit_buffer`] sibling for a real `cx.spawn` task
    /// resuming after an `.await` - same "capture `cwd` before the await" rule as
    /// [`Self::edit_buffer_at`].
    pub(crate) fn insert_edit_buffer_at(
        &mut self,
        cwd: PathBuf,
        path: PathBuf,
        buffer: edit_buffer::EditBuffer,
    ) -> Option<edit_buffer::EditBuffer> {
        self.edit_buffers.insert((cwd, path), buffer)
    }

    /// Selects a worktree by its real path (rather than an index into
    /// [`Self::worktrees`], which project-mode rows don't carry) - used by a plain worktree
    /// row's click handler in "by project" mode. Falls back to doing nothing if the path
    /// isn't currently in the loaded worktree list (e.g. a stale click racing a reload).
    pub(crate) fn select_worktree_by_path(
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
/// called from [`AdeApp::select_worktree`] on every switch. `open_change` is keyed by
/// repo-relative paths with no per-worktree scoping of its own, so without this reset a file
/// opened in worktree A would reopen in worktree B if it shares the same relative path.
/// `expanded_dirs` is keyed by absolute path, so it doesn't bleed the same way - but it must
/// still be emptied here, because
/// [`AdeApp::select_worktree`] re-derives it from the *new* worktree's own persisted fold state
/// immediately afterwards, and a leftover entry from the worktree just left would otherwise
/// survive that (its absolute path simply never matches anything in the new tree, so nothing
/// would ever remove it). A free, `gpui`-free function so this is unit-testable without constructing an
/// `AdeApp`.
///
/// `staged_files` is cleared here too, but only as the *synchronous* half of a two-step reset:
/// this stops the worktree just left's staged set from flashing on screen for the frame or two
/// before the new worktree's own diff lands, but it is not itself where the new worktree's real
/// staged set comes from. `AdeApp::select_worktree` calls `AdeApp::load_diff` immediately after
/// this function returns, and `load_diff`'s own background task re-derives `staged_files` from a
/// real `git diff --cached --name-only` (`wt_core::stage::staged_paths`) against the worktree
/// being switched *to* - so a file already staged in the real index before Jerry ever opened this
/// worktree reads as staged once the load lands, rather than this reset leaving it looking
/// unstaged forever (see `load_diff`'s own docs for why this is a live re-query on every diff
/// load, not a per-worktree cache).
///
/// **Deliberately does *not* touch `open_files`/`edit_buffers` anymore.** Both moved to real,
/// per-worktree-keyed storage (`AdeApp::open_files_by_worktree`, keyed by
/// [`AdeApp::file_tree_root`]; `AdeApp::edit_buffers`, keyed by `(file_tree_root, path)`) -
/// `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §1/§3's explicit requirement that
/// each worktree remembers its own open files, and that an unsaved edit must survive fluidly
/// hopping between worktrees rather than being silently discarded. Clearing them here used to be
/// how this function kept one worktree's tabs/buffers from leaking into another's; now that both
/// are looked up *through* the worktree they belong to, the worktree switch happening around this
/// call (`AdeApp::select_worktree` reassigning `file_tree_root` a few lines below) is itself what
/// makes the old worktree's entries stop being "current" - nothing here needs to delete them, and
/// deleting them would be exactly the silent data loss this revision set out to fix.
pub(super) fn reset_per_worktree_ui_state(
    staged_files: &mut HashSet<PathBuf>,
    open_change: &mut Option<PathBuf>,
    expanded_dirs: &mut HashSet<PathBuf>,
    selected_tree_path: &mut Option<PathBuf>,
) {
    staged_files.clear();
    *open_change = None;
    expanded_dirs.clear();
    *selected_tree_path = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_per_worktree_ui_state_clears_staged_files_and_open_change() {
        let mut staged_files = HashSet::new();
        staged_files.insert(PathBuf::from("src/main.rs"));
        staged_files.insert(PathBuf::from("Cargo.toml"));
        let mut open_change = Some(PathBuf::from("src/main.rs"));
        let mut expanded_dirs = HashSet::new();
        let mut selected_tree_path = None;

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
        );

        assert!(staged_files.is_empty());
        assert_eq!(open_change, None);
    }

    #[test]
    fn reset_per_worktree_ui_state_is_a_no_op_when_already_empty() {
        let mut staged_files = HashSet::new();
        let mut open_change = None;
        let mut expanded_dirs = HashSet::new();
        let mut selected_tree_path = None;

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
        );

        assert!(staged_files.is_empty());
        assert_eq!(open_change, None);
        assert!(expanded_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_expanded_dirs_too() {
        let mut staged_files = HashSet::new();
        let mut open_change = None;
        let mut expanded_dirs = HashSet::new();
        let mut selected_tree_path = None;
        expanded_dirs.insert(PathBuf::from("/repo/worktree-a/src"));
        expanded_dirs.insert(PathBuf::from("/repo/worktree-a/tests"));

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
        );

        assert!(expanded_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_selected_tree_path() {
        let mut staged_files = HashSet::new();
        let mut open_change = None;
        let mut expanded_dirs = HashSet::new();
        let mut selected_tree_path = Some(PathBuf::from("/repo/worktree-a/src/main.rs"));

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
        );

        assert_eq!(selected_tree_path, None);
    }

    /// [`AdeApp::open_files`]/[`AdeApp::open_files_mut`] resolve through
    /// [`AdeApp::open_files_by_worktree`], keyed by [`AdeApp::file_tree_root`] - a worktree that
    /// has never opened a file reads as a real empty slice, and two different worktree keys never
    /// see each other's entries.
    #[test]
    fn open_files_by_worktree_keeps_each_worktree_s_tabs_independent() {
        let mut open_files_by_worktree: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let worktree_a = PathBuf::from("/repo/worktree-a");
        let worktree_b = PathBuf::from("/repo/worktree-b");

        open_files_by_worktree
            .entry(worktree_a.clone())
            .or_default()
            .push(PathBuf::from("src/main.rs"));

        assert_eq!(
            open_files_by_worktree
                .get(&worktree_a)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            &[PathBuf::from("src/main.rs")][..],
            "worktree A must see the tab it opened"
        );
        assert_eq!(
            open_files_by_worktree
                .get(&worktree_b)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            <&[PathBuf]>::default(),
            "an unrelated worktree that never opened anything must read as a real empty slice, \
             never worktree A's tabs"
        );
    }

    /// `edit_buffers` is keyed by `(worktree, worktree-relative path)` - a same-named file open
    /// with different unsaved content in two worktrees must never collide or merge.
    #[test]
    fn edit_buffers_composite_key_never_collides_across_worktrees() {
        let mut edit_buffers: HashMap<(PathBuf, PathBuf), edit_buffer::EditBuffer> = HashMap::new();
        let worktree_a = PathBuf::from("/repo/worktree-a");
        let worktree_b = PathBuf::from("/repo/worktree-b");
        let relative = PathBuf::from("src/main.rs");

        edit_buffers.insert(
            (worktree_a.clone(), relative.clone()),
            edit_buffer::EditBuffer::new(
                worktree_a.join(&relative),
                "fn main() { a() }".to_string(),
                Some("rs".to_string()),
                None,
                17,
            ),
        );
        edit_buffers.insert(
            (worktree_b.clone(), relative.clone()),
            edit_buffer::EditBuffer::new(
                worktree_b.join(&relative),
                "fn main() { b() }".to_string(),
                Some("rs".to_string()),
                None,
                17,
            ),
        );

        assert_eq!(
            edit_buffers
                .get(&(worktree_a.clone(), relative.clone()))
                .map(|buffer| buffer.content.as_str()),
            Some("fn main() { a() }"),
            "worktree A's buffer for this relative path must stay worktree A's own content"
        );
        assert_eq!(
            edit_buffers
                .get(&(worktree_b, relative))
                .map(|buffer| buffer.content.as_str()),
            Some("fn main() { b() }"),
            "worktree B's buffer for the identical relative path must never merge with or \
             overwrite worktree A's"
        );
    }
}

/// GitHub issue #12's real, end-to-end proof: `Self::load_worktrees` against a *real* temp git
/// repository, driven by *real* `git worktree add`/`remove`/`lock` commands (not a mocked-away
/// `wt_core`) - the panel really does converge to whatever `git` itself now reports, and
/// selection recovery really does fall back to the main worktree with a real notice when the
/// selected one really vanishes. The watcher's own OS-level event delivery has its own real,
/// non-`gpui` test coverage in `crate::rail::worktree_watch`'s test module (no deterministic/
/// simulated clock exists there to drive a real `notify` background thread through
/// `cx.run_until_parked()`); what's proven here is the other half - that a refresh, however
/// triggered, produces a correct in-app list.
#[cfg(test)]
mod load_worktrees_integration_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use std::path::Path;
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

    #[gpui::test]
    fn a_real_worktree_add_appears_after_a_refresh(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.worktrees.len()),
            1,
            "only the main worktree exists before the real `git worktree add`"
        );

        let feature = add_worktree(repo.path(), "feature", "added-wt");

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let worktrees = app.read_with(cx, |app, _| app.worktrees.clone());
        assert_eq!(worktrees.len(), 2, "the real new worktree must now appear");
        let added = worktrees
            .iter()
            .find(|item| item.path == feature)
            .expect("the added worktree's path must be present");
        assert_eq!(added.branch.as_deref(), Some("feature"));
        assert!(added.error.is_none());
    }

    #[gpui::test]
    fn a_real_worktree_remove_disappears_after_a_refresh(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "removed-wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        assert_eq!(app.read_with(cx, |app, _| app.worktrees.len()), 2);

        wt_core::remove_worktree(repo.path(), &feature, false).expect("remove_worktree");

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let worktrees = app.read_with(cx, |app, _| app.worktrees.clone());
        assert_eq!(
            worktrees.len(),
            1,
            "the removed worktree must be gone, not left as a phantom entry"
        );
        assert!(!worktrees.iter().any(|item| item.path == feature));
    }

    #[gpui::test]
    fn a_real_lock_with_reason_is_reflected_after_a_refresh(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "locked-wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        git(
            repo.path(),
            &[
                "worktree",
                "lock",
                "--reason",
                "on a USB drive",
                feature.to_str().expect("utf8 path"),
            ],
        );

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let worktrees = app.read_with(cx, |app, _| app.worktrees.clone());
        let locked = worktrees
            .iter()
            .find(|item| item.path == feature)
            .expect("the locked worktree must still be present");
        assert!(locked.is_locked);
        assert_eq!(locked.lock_reason.as_deref(), Some("on a USB drive"));
    }

    /// The real reproduction of the issue's "prunable / missing worktree" case: the working
    /// directory is deleted by hand, not via `git worktree remove` - `git` itself (not a guess
    /// on this app's side) is what flags it prunable, and a refresh must mark it broken rather
    /// than silently listing it as a healthy, selectable row.
    #[gpui::test]
    fn a_manually_deleted_worktree_directory_is_marked_broken_after_a_refresh(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "gone-wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        fs::remove_dir_all(&feature).expect("manually delete the worktree directory");

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let worktrees = app.read_with(cx, |app, _| app.worktrees.clone());
        let broken = worktrees
            .iter()
            .find(|item| item.path == feature)
            .expect("the now-broken entry must still be listed, not silently dropped");
        assert!(broken.is_broken);
        assert!(
            broken.error.is_some(),
            "a broken worktree must fail the usability gate every selection/spawn call site uses"
        );
    }

    #[gpui::test]
    fn selection_survives_a_refresh_when_the_selected_worktree_is_still_present(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "stays-wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let feature_index = app
            .read_with(cx, |app, _| {
                app.worktrees.iter().position(|item| item.path == feature)
            })
            .expect("the added worktree must be in the list");
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(feature_index, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(feature_index)
        );

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let (selected_path, notice) = app.read_with(cx, |app, _| {
            (
                app.selected
                    .and_then(|i| app.worktrees.get(i))
                    .map(|item| item.path.clone()),
                app.worktree_selection_notice.clone(),
            )
        });
        assert_eq!(
            selected_path,
            Some(feature),
            "selection must be remapped to the same real path after a refresh, not reset"
        );
        assert_eq!(
            notice, None,
            "no notice should appear when the selected worktree is still present"
        );
    }

    /// GitHub issue #12's own acceptance criterion: "the currently active worktree stays
    /// highlighted across refreshes; if it disappears, the user is notified and the selection
    /// falls back to the main worktree" - proven here against a *real* `wt_core::remove_worktree`
    /// call, not a directly-mutated `worktrees` vec.
    #[gpui::test]
    fn selecting_a_worktree_then_really_removing_it_falls_back_to_main_with_a_notice(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "vanishes-wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let feature_index = app
            .read_with(cx, |app, _| {
                app.worktrees.iter().position(|item| item.path == feature)
            })
            .expect("the added worktree must be in the list");
        let main_path = app.read_with(cx, |app, _| app.focused_repo_path());
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(feature_index, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(feature_index)
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_selection_notice.clone()),
            None
        );

        wt_core::remove_worktree(repo.path(), &feature, false).expect("remove_worktree");

        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();

        let (selected_path, notice) = app.read_with(cx, |app, _| {
            (
                app.selected
                    .and_then(|i| app.worktrees.get(i))
                    .map(|item| item.path.clone()),
                app.worktree_selection_notice.clone(),
            )
        });
        assert_eq!(
            selected_path,
            Some(main_path),
            "selection must fall back to the real main worktree once the selected one is gone"
        );
        let notice = notice.expect("a real notice must be shown when selection falls back");
        assert!(notice.to_lowercase().contains("main"));

        // A real, explicit re-selection clears the stale notice - `Self::select_worktree`'s own
        // documented contract.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_selection_notice.clone()),
            None,
            "a real user re-selection must clear the stale fallback notice"
        );
    }
}
