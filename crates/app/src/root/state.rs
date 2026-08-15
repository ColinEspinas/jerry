use super::*;
use crate::code_surface::state::{DiffLoadState, FileLoadState};
use crate::palette::render as palette_render;
use crate::sidebar::render::RightSidebarView;
use std::sync::atomic::{AtomicU64, Ordering};

/// Backs [`AdeApp::tree_undo_instance_id`] - see that field's own docs for why per-process
/// uniqueness isn't enough.
static NEXT_TREE_UNDO_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

/// Why a `git worktree list --porcelain` fetch is happening - the one thing that differs between
/// [`AdeApp::load_worktrees`] and [`AdeApp::load_worktrees_for_opened_repo`], passed explicitly
/// rather than inferred from state once the fetch lands.
///
/// It has to be explicit: by the time the fetch resolves, "this repo was just opened" and "a
/// background poll tick fired for a repo that happens to have nothing selected" are
/// indistinguishable from [`AdeApp`]'s own fields alone, and they must behave in opposite ways -
/// the first *must* land on a real worktree (leaving it unselected is the reported bug), the
/// second must never select anything on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorktreeLoadIntent {
    /// A steady-state reload: the live watcher/poll loop, a post-prune or post-merge refresh, or
    /// [`AdeApp::checkout_repo_from_rail`]'s own follow-up fetch. Never changes *whether*
    /// something is selected, only re-anchors an existing selection by path
    /// ([`crate::rail::worktrees::recover_selection`]).
    Refresh,
    /// A repo is being opened for real work - a CLI launch or "Open Folder…". `opened_path` is
    /// the path the user actually named (which may be the repo root, a linked worktree, or a
    /// mere subdirectory of the repo - see
    /// [`crate::rail::worktrees::selection_for_opened_repo`] for how each resolves).
    Opening { opened_path: PathBuf },
}

impl AdeApp {
    /// Production entry point - loads `~/.config/jerry/settings.toml` (`Settings::load_or_init`)
    /// and delegates to [`Self::new_with_settings`]. Blocking the foreground thread here is a
    /// deliberate exception to this codebase's usual rule: it's a single tiny file read that
    /// runs exactly once, before a window even exists, not a per-render or per-poll cost.
    ///
    /// GitHub issue #90: `repo_path` is `None` for a fresh launch with no CLI argument (there is
    /// deliberately no `env::current_dir()` fallback anywhere upstream of this - see
    /// `crate::main`'s own docs) or for a brand-new window opened via `crate::title_bar::menu`'s
    /// "New Window" row. `use_remembered_repo` disambiguates those two `None` cases from each
    /// other - see [`Self::new_with_settings`]'s own docs for exactly what it controls; `Some`
    /// makes it irrelevant either way, since an explicit path always wins.
    pub fn new(
        repo_path: Option<PathBuf>,
        use_remembered_repo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings_path = settings_store::settings_toml_path();
        let settings = settings_store::Settings::load_or_init();
        Self::new_with_settings(
            repo_path,
            use_remembered_repo,
            settings,
            settings_path,
            window,
            cx,
        )
    }

    /// The real constructor - takes an already-resolved [`Settings`] and its optional source
    /// path rather than resolving them itself, so [`Self::new`] (production) and
    /// `root::focus::palette_focus_tests::open_test_app` (every GPUI test's shared entry point)
    /// can each supply their own. Test instances get in-memory-only defaults and a `None` path,
    /// so [`Self::persist_settings`] is a genuine no-op for them, never a write to whatever
    /// machine happens to run `cargo test`.
    ///
    /// ## `repo_path`/`use_remembered_repo` (GitHub issue #90)
    ///
    /// `repo_path`:
    /// - `Some(path)`: behaves exactly as this app always has - `path` is added and focused,
    ///   unconditionally. `use_remembered_repo` has no effect in this case.
    /// - `None`, `use_remembered_repo == true` (the real process-launch path, `Self::new`'s own
    ///   `run` caller): looks at whatever [`repo::RepoState`] was just loaded into `repos` below
    ///   for a real, still-existing last-focused repo
    ///   ([`repo::RepoState::last_focused_existing_path`]) and focuses that one if there is one -
    ///   "the app remembers the last-opened folder and reopens it automatically next launch". If
    ///   there is no such repo (nothing was ever persisted, or the remembered directory has since
    ///   been deleted/moved), the window opens in a genuinely empty state instead: no crash, no
    ///   silent fallback to a hardcoded path, just [`Self::focused_repo`] staying `None`.
    /// - `None`, `use_remembered_repo == false` (`crate::title_bar::menu`'s "New Window" row):
    ///   always a genuinely empty state, even if a real last-focused repo *is* on record - the
    ///   issue's own words are explicit that a new window opens empty, "not" whatever folder the
    ///   window it was opened from happens to have open.
    ///
    /// A genuinely empty window ([`Self::focused_repo`] left `None`) skips every single-repo-
    /// scoped piece of startup work below that would otherwise need a real path to run against -
    /// no initial shell agent is spawned, and `Self::load_worktrees`/`load_file_tree`/`load_diff`/
    /// `start_status_polling`/`start_worktree_watch` are never called at all. [`Render`]'s own
    /// `AdeApp` impl (`crate::root::mod`) renders a dedicated empty-state view instead of the
    /// three-zone workspace body whenever [`Self::focused_repo`] is `None` - see
    /// `Self::render_empty_state`'s own docs.
    pub(crate) fn new_with_settings(
        repo_path: Option<PathBuf>,
        use_remembered_repo: bool,
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

        // GitHub issue #225's review baselines resolve the same way, for the same reasons - see
        // `crate::review::baseline_state`'s module docs, including the honest note that nothing
        // can reconnect a loaded baseline to a live agent yet (agent ids don't survive a restart).
        // Loaded anyway so the file is merged into rather than clobbered on the first save.
        let review_baseline_path = settings_path
            .as_deref()
            .map(crate::review::baseline_state::review_baseline_path_for);
        let review_baseline_state = review_baseline_path
            .as_deref()
            .map(crate::review::baseline_state::ReviewBaselineState::load_at)
            .unwrap_or_default();

        // GitHub issue #239 phase 2's hook-learned agent statuses resolve identically, and for
        // the identical reason (see `crate::hooks::store`'s module docs, including the honest
        // note that no UI reads these back yet - that is issue #227's job).
        let agent_status_path = settings_path
            .as_deref()
            .map(crate::hooks::store::agent_status_path_for);
        let agent_status_state = agent_status_path
            .as_deref()
            .map(crate::hooks::store::AgentStatusState::load_at)
            .unwrap_or_default();

        // GitHub issue #284's per-agent line provenance resolves the same way. Unlike the two
        // above it is genuinely read back at startup - `restore_line_provenance`, called once
        // this literal exists, re-reads each recorded file to prove the spans still describe it.
        let line_provenance_path = settings_path
            .as_deref()
            .map(crate::provenance::persist_state::line_provenance_path_for);

        // The hook side-channel starts out absent and is brought up lazily, on the first Claude
        // agent this instance spawns - see `AdeApp::hook_injection_for`. Still exactly one
        // listener per instance, shared by every Claude agent it ever spawns; just not one opened
        // by a window that may never run an agent at all.
        let hook_runtime = None;

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
                worktrees_loaded: false,
            });
            next_repo_id += 1;
        }
        // The live mirror of every restored repo's own last-selected worktree - see
        // `Self::selected_worktree_by_repo`'s own docs for why this has to be a whole map rather
        // than a single value, and `Self::restore_worktree_session` for what a selection then
        // reopens. Deliberately seeded from the raw records rather than through
        // `RepoState::remembered_worktree`'s "still exists on disk" filter: this map's job is to
        // faithfully mirror what is on disk so a save doesn't blank another window's entry, and
        // the existence check belongs at the two points that actually *act* on a remembered
        // worktree (the startup selection below, and `selection_for_opened_repo`'s own check that
        // it is still a real worktree of the repo).
        let selected_worktree_by_repo: HashMap<String, PathBuf> = loaded_repo_state
            .repos
            .iter()
            .filter_map(|(key, record)| {
                let worktree = record.selected_worktree.as_ref()?;
                Some((key.clone(), PathBuf::from(worktree)))
            })
            .collect();

        // GitHub issue #90: the one real resolution point for "what repo (if any) should this
        // window start focused on" - see `Self::new_with_settings`'s own docs for the full
        // decision table. An explicit `repo_path` always wins outright; otherwise, only the real
        // process-launch path (`use_remembered_repo == true`) ever consults the just-loaded
        // `RepoState`'s own last-focused marker, and only if it still names a real, existing
        // directory - `RepoState::last_focused_existing_path` is the one place that "still
        // exists" check happens, so a deleted/moved remembered folder can never surface as a
        // broken/error startup state here, only as a genuinely empty one.
        //
        // The CLI argument is normalized here (`repo::canonical_repo_path`), before anything in
        // this constructor uses it - it becomes `Self::file_tree_root`, the startup shell's real
        // cwd, and (via `Self::add_repo`) `Repo::path`, and every one of those is compared by
        // exact path against git's own fully-resolved worktree paths later. `jerry .` and
        // `jerry ~/link-to-repo` are the ordinary ways to launch this app, and both used to
        // produce a repo whose agents matched no worktree row at all; see that function's own
        // docs. The remembered-repo branch needs no such call: `RepoState`'s keys are already
        // `repo::repo_key` output, which is canonical by construction.
        //
        // `opened_from_memory` is the second half of that same decision, and exists only for the
        // worktree-level restore below: an explicitly named path (a CLI argument) is a real
        // statement about *where to work* and is honoured literally, while a repo resolved purely
        // from memory carries no such statement and can therefore honour the finer-grained memory
        // of which worktree of it was last worked in. See the `opened_target` note further down.
        let opened_from_memory = repo_path.is_none() && use_remembered_repo;
        let resolved_repo_path: Option<PathBuf> = match repo_path {
            Some(path) => Some(repo::canonical_repo_path(&path)),
            None if use_remembered_repo => loaded_repo_state.last_focused_existing_path(),
            None => None,
        };

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
        let theme_seed_focus_handle = cx.focus_handle();
        let shell_focus_handle = cx.focus_handle();
        let caret_blink_subscriptions = AdeApp::wire_caret_blink(
            &[
                &code_focus_handle,
                &merge_edit_focus_handle,
                &palette_focus_handle,
                &tree_focus_handle,
                &filter_focus_handle,
                &settings_keymap_filter_focus_handle,
                &theme_seed_focus_handle,
                &shell_focus_handle,
            ],
            window,
            cx,
        );

        // GitHub issue #213: the Settings field starts out holding whatever the real file says,
        // so opening Settings shows the shell that is actually in force rather than a blank
        // field next to a running `fish`. Built here, before `settings` is moved into the
        // literal below.
        let shell_input =
            text_history::TextField::seeded(settings.terminal.shell_override().unwrap_or(""));

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

        // GitHub issue #226: the sound library, same "load once at startup, before anything
        // needs to resolve a `settings.toml` id against it" seam as `custom_themes` just above -
        // built-in sounds always come first (`crate::sound::library::builtin_sounds`), whatever
        // the user has imported into `~/.config/jerry/sounds/` is appended after.
        let (sound_library, sound_load_errors) = {
            let mut library = crate::sound::library::builtin_sounds();
            let mut errors = Vec::new();
            if let Some(path) = settings_path.as_deref() {
                let (user_sounds, user_errors) = crate::sound::library::load_user_sounds_from_dir(
                    &crate::sound::library::sounds_dir_for(path),
                );
                library.extend(user_sounds);
                errors = user_errors;
            }
            (library, errors)
        };

        // A genuinely empty startup (no `resolved_repo_path`) has no real root to point these
        // at yet - an empty `PathBuf` is an honest placeholder, never read by anything: every
        // consumer of `file_tree_root`/`diff_root` lives inside `Self::render_workspace_body`,
        // which `Render`'s own `AdeApp` impl never calls while `Self::focused_repo` is `None`
        // (see `Self::render_empty_state`'s own docs).
        let initial_root = resolved_repo_path.clone().unwrap_or_default();
        let mut this = Self {
            file_tree_root: initial_root.clone(),
            diff_root: initial_root,
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
            file_tree: file_tree::FileTree::default(),
            file_tree_error: None,
            right_sidebar_view: RightSidebarView::Files,
            file_tree_scroll_handle: UniformListScrollHandle::new(),
            diff_state: DiffLoadState::Loading,
            diff_totals: None,
            agent_reviews: HashMap::new(),
            review_baseline_state,
            review_baseline_path,
            review_baselines_owned: std::collections::BTreeSet::new(),
            review_mark_in_flight: None,
            hook_runtime,
            hook_runtime_tried: false,
            agent_status_state,
            agent_status_path,
            agent_status_owned: std::collections::BTreeSet::new(),
            line_provenance: crate::provenance::store::ProvenanceStore::default(),
            line_provenance_path,
            line_provenance_owned: std::collections::BTreeSet::new(),
            change_set: crate::provenance::change_set::ChangeSet::default(),
            uncommitted_diff: crate::sidebar::sections::ScopeLoad::Loading,
            uncommitted_change_set: crate::provenance::change_set::ChangeSet::default(),
            branch_commits: crate::sidebar::sections::ScopeLoad::Loading,
            changes_sections: crate::sidebar::sections::SectionCollapse::default(),
            // Starts empty and is reset to the real row count from the one place that builds the
            // rows (`crate::sidebar::render::AdeApp::render_changes_sections`) - never seeded with
            // a guessed count, which `gpui::list` would then measure against nothing.
            changes_sections_list: gpui::ListState::new(
                0,
                gpui::ListAlignment::Top,
                crate::sidebar::render::CHANGES_LIST_OVERDRAW,
            ),
            seen_files: crate::sidebar::sections::SeenFiles::default(),
            review_tab_open: None,
            review_tab_active: false,
            review_focus_handle: cx.focus_handle(),
            review_focus: OverlayFocus::default(),
            review_scroll_handle: UniformListScrollHandle::new(),
            review_highlight_cache: None,
            _review_baseline_tasks: HashMap::new(),
            _review_load_task: None,
            _review_mark_task: None,
            _review_release_tasks: HashMap::new(),
            _review_persist_task: None,
            _agent_status_persist_task: None,
            _line_provenance_persist_task: None,
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
            // Nothing has been walked yet, so nothing may be pruned yet either.
            file_tree_complete: false,
            rail_row_menu: None,
            sidebar_view: crate::rail::strip::SidebarView::default(),
            rail_overflow_menu: None,
            rail_overflow_button_bounds: gpui::Bounds::default(),
            remove_worktree_confirm_armed: None,
            tree_context_menu: None,
            tree_inline_edit: None,
            tree_clipboard: None,
            tree_drag_hover_target: None,
            tree_undo_stack: Vec::new(),
            tree_redo_stack: Vec::new(),
            tree_undo_backup_counter: 0,
            tree_undo_instance_id: NEXT_TREE_UNDO_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            tree_op_error: None,
            tree_focus_handle,
            file_tree_bounds: gpui::Bounds::default(),
            _tree_delete_tasks: Vec::new(),
            _tree_copy_task: None,
            staged_files: HashSet::new(),
            dirty_files: None,
            changes_row_error: None,
            _stage_tasks: TaskPool::new(),
            change_row_hover: None,
            change_row_actions_hover: None,
            change_row_discard_armed: None,
            _discard_tasks: TaskPool::new(),
            commit_menu_open: false,
            commit_composer_bounds: gpui::Bounds::default(),
            open_files_by_worktree: HashMap::new(),
            open_change: None,
            close_tab_confirm_armed: None,
            open_diff_file_cache: None,
            selected_tree_path: None,
            additional_tree_selection: HashSet::new(),
            code_view: code_view::CodeView::Diff,
            markdown_view: markdown_preview::MarkdownView::Source,
            markdown_preview_scroll_handle: gpui::ScrollHandle::new(),
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
            graph_view_focused: false,
            graph_focus: OverlayFocus::default(),
            graph_state: graph_view::state::GraphTabState::new(cx),
            _load_graph_task: None,
            _load_commit_files_task: None,
            file_view_scroll_handle: UniformListScrollHandle::new(),
            diff_view_scroll_handle: UniformListScrollHandle::new(),
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
            file_view_folds: HashMap::new(),
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
            palette_step: palette::PaletteStep::default(),
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
            commit_message: text_history::TextField::new(),
            commit_message_focus_handle: cx.focus_handle(),
            rail_focus_handle: cx.focus_handle(),
            empty_state_focus_handle: cx.focus_handle(),
            diff_cache: HashMap::new(),
            worktree_notes: HashMap::new(),
            ahead_behind_cache: HashMap::new(),
            process_stats: HashMap::new(),
            process_stats_sampled_at: None,
            resources_popover_open: false,
            resources_readout_bounds: gpui::Bounds::default(),
            disk_usage: None,
            worktree_disk_usage: HashMap::new(),
            prune_status: None,
            prune_confirm_armed: false,
            prune_in_flight: false,
            worktree_history_op_in_flight: None,
            worktree_history_status: None,
            update_state: updater::state::UpdateState::Idle,
            update_check_in_flight: false,
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
            _file_tree_watcher: None,
            _file_tree_watch_task: None,
            _load_diff_task: None,
            _file_load_task: None,
            _status_poll_task: None,
            _repo_worktrees_tasks: TaskPool::new(),
            _repo_worktrees_poll_task: None,
            _disk_usage_task: None,
            _prune_task: None,
            _worktree_history_task: None,
            _update_check_task: None,
            _update_download_task: None,
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
            _completions_resolve_task: None,
            completions_resolve_in_flight: None,
            completions_resolved: std::collections::HashSet::new(),
            completions_resolved_items: std::collections::HashMap::new(),
            completions: None,
            completions_scroll_handle: UniformListScrollHandle::new(),
            completions_detail_scroll_handle: gpui::ScrollHandle::new(),
            completions_generation: 0,
            completions_suppress_next_trigger: false,
            file_view_diagnostics: HashMap::new(),
            file_view_error_count: None,
            _lsp_tasks: TaskPool::new(),
            _lsp_poll_task: None,
            hover: None,
            hover_pending: None,
            _hover_debounce_task: None,
            _hover_hide_task: None,
            hover_card_bounds: None,
            diagnostic_card_bounds: None,
            diagnostic_copy_confirmed: None,
            _diagnostic_copy_confirm_task: None,
            hover_card_scroll_handle: gpui::ScrollHandle::new(),
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
            theme_seed_input: text_history::TextField::new(),
            theme_seed_focus_handle,
            shell_input,
            shell_focus_handle,
            // Left un-probed at construction: resolving it walks `$PATH` (or stats a file), and
            // nothing shows it until Settings is opened, which recomputes it - see
            // `AdeApp::refresh_shell_status`.
            shell_status: settings::ShellStatus::SystemDefault,
            // Same reasoning, and more so: detecting the installed shells reads `/etc/shells` and
            // walks `$PATH` several times over. Left genuinely empty until a real gesture asks
            // for it (`AdeApp::refresh_shell_suggestions`), never guessed at startup.
            shell_suggestions: Vec::new(),
            shell_suggestions_open: false,
            shell_field_bounds: gpui::Bounds::default(),
            _theme_generate_task: None,
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
            // GitHub issue #176. Fires on both edges (activate *and* deactivate), so the callback
            // has to ask `Window::is_window_active` which one it is rather than assuming - the
            // same shape `vendor/zed/crates/workspace/src/workspace.rs`'s own
            // `on_window_activation_changed` uses.
            //
            // GitHub issue #226 reuses this exact subscription for `Self::window_active` rather
            // than adding a second one: both are "something needs to know this window's real
            // focus edge", and a second `cx.observe_window_activation` here would just be two
            // independent callbacks racing to read the same `Window::is_window_active` on every
            // fire for no benefit.
            _window_activation_subscription: cx.observe_window_activation(
                window,
                |this, window, cx| {
                    let active = window.is_window_active();
                    this.window_active = active;
                    if !active {
                        this.close_all_menu_surfaces(cx);
                    }
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
            session_restored: HashSet::new(),
            session_restore_notices: Vec::new(),
            selected_worktree_by_repo,
            tab_drag_insertion: None,
            dragging_tab: None,
            dropped_tab_settle: None,
            next_tab_settle_id: 0,
            tab_bounds: HashMap::new(),
            tab_slide: HashMap::new(),
            custom_themes,
            custom_theme_load_errors,
            custom_theme_status: None,
            _custom_theme_import_task: None,
            _vscode_theme_import_task: None,
            _custom_theme_export_task: None,
            _custom_theme_remove_task: None,
            _custom_theme_create_task: None,
            custom_theme_remove_armed: None,
            icon_pack_status: None,
            _icon_pack_choose_task: None,
            _repo_folder_choose_task: None,
            sound_library,
            sound_load_errors,
            sound_import_status: None,
            _sound_import_task: None,
            sound_picker_open: None,
            sound_event_button_bounds: HashMap::new(),
            prev_agent_sound_states: HashMap::new(),
            agent_sound_seeded: false,
            last_sound_at: None,
            // A window is real and focused the instant its own constructor finishes - see this
            // field's own docs for why the subscription below only ever *updates* it after this.
            window_active: true,
            sound_player: crate::sound::player::SoundPlayer::new(),
        };
        // GitHub issue #45 ("Input blink only on focused input or file") / a live follow-up
        // report of missing carets: `graph_state.branches_filter_focus_handle` (added later, in
        // Revision R12's git graph tab), `new_file_focus_handle`, and (GitHub issue #241)
        // `graph_state.branch_prompt_focus_handle` are three more genuine caret-bearing
        // `FocusHandle`s - real, hand-rolled `text_history::TextField` inputs, the same shape as
        // the six wired above - that never got threaded through `Self::wire_caret_blink`. They
        // can't join that earlier call: it runs before `this` exists, and every one of these
        // handles lives *inside* `this` (two nested in `graph_state`, built by this same
        // literal). Wired here instead, appending onto the very same `_caret_blink_subscriptions`
        // vec so there's still exactly one place holding every real caret subscription this app
        // has, not two.
        let extra_caret_blink_subscriptions = AdeApp::wire_caret_blink(
            &[
                &this.graph_state.branches_filter_focus_handle,
                &this.new_file_focus_handle,
                // GitHub issue #241: the row menu's "Create branch here" prompt is a third real
                // hand-rolled `TextField` input built after this same `graph_state` literal, so
                // it joins this later call for the same reason `branches_filter_focus_handle`
                // does - see this call's own docs just above.
                &this.graph_state.branch_prompt_focus_handle,
                // GitHub issue #285's commit-message field is a fourth: another real
                // `text_history::TextField` input built inside this same `Self` literal, live-
                // reported missing its blink exactly like the three above once did - focusing it
                // left `caret_blink_visible` frozen at whatever it last was (never toggling, since
                // no subscription ever called `start_caret_blink` for this handle), so the caret
                // rendered as a permanently solid bar instead of blinking.
                &this.commit_message_focus_handle,
            ],
            window,
            cx,
        );
        this._caret_blink_subscriptions
            .extend(extra_caret_blink_subscriptions);
        // Real "add this one repo on startup" (Revision R12 Phase 0, extended by GitHub issue
        // #90 to a genuinely optional repo): `resolved_repo_path` - the CLI argument, the
        // remembered last-focused repo, or nothing at all, per `Self::new_with_settings`'s own
        // decision table above - becomes the first, focused entry of `Self::repos` rather than a
        // separate field whenever it's `Some`. `Self::add_repo` is idempotent against whatever
        // `loaded_repo_state` above already restored, so a repeat launch of the same path (or
        // reopening the same remembered repo) never duplicates it. Every other single-repo-
        // scoped piece of startup work below (`set_file_tree_root`/the initial shell's cwd/
        // `load_worktrees`/`load_file_tree`/`load_diff`/the watchers) only runs at all when there
        // is a real path to run it against - a genuinely empty startup does none of it, and
        // `Self::focused_repo` simply stays `None`.
        if let Some(path) = resolved_repo_path.clone() {
            let focused_repo_id = this.add_repo(path.clone(), cx);
            this.focus_repo(focused_repo_id, cx);
            // See the `expanded_dirs`/`fold_state_root_key` note in the literal above: resolving
            // the worktree key here, through the one function that ever resolves it, is what
            // keeps the startup path and every later worktree switch structurally identical.
            this.set_file_tree_root(path.clone(), cx);
            this.reload_expanded_dirs_from_fold_state();
        }
        // Applies `this.settings.keymap.overrides` on top of `crate::default_key_bindings()` -
        // see `Self::apply_effective_key_bindings`'s own docs. Must run before this constructor
        // returns and the entity's first render, so a persisted rebind is live from the very
        // first frame, not just after the next settings change - real "apply overrides at
        // startup", not only "apply overrides when later edited". Unconditional: a rebound
        // keymap applies just as much to a genuinely empty window as a focused one.
        this.apply_effective_key_bindings(cx);
        // GitHub issue #284: reads back the previous run's per-agent line attribution, re-reading
        // each recorded file to prove the spans still describe it (see
        // `crate::provenance::persist_state`). Unconditional and cheap - it does nothing at all
        // when nothing was ever recorded - and deliberately before the first render, so a restored
        // worktree's attribution is live from the first frame rather than appearing a poll later.
        this.restore_line_provenance();
        // Applies the real, persisted theme selection at startup (`Self::apply_theme_selection`)
        // - if `follow_system` is also on, the real, current OS appearance takes priority over
        // whatever `theme.name` was last persisted as, matching `Self::sync_theme_to_system_
        // appearance`'s own live behavior (see that method's docs); `apply_theme_selection`'s own
        // call at the end is always run regardless, since `apply_follow_system_appearance` is a
        // no-op (and doesn't itself apply anything) when the resolved name already matches. Also
        // unconditional, for the same reason as the keybindings above.
        if this.settings.theme.follow_system {
            let appearance = window.appearance();
            this.apply_follow_system_appearance(appearance, cx);
        }
        this.apply_theme_selection(cx);
        match resolved_repo_path {
            Some(path) => {
                // A fresh window starts with one shell - but in the repo's real, genuinely
                // *selected* worktree, not in the bare repo path. `load_worktrees_for_opened_repo`
                // owns that whole sequence (resolve the real worktree list, select the right
                // worktree of it, spawn the initial shell into that worktree, focus it); see its
                // own docs for why the spawn is deferred until that real fetch lands rather than
                // done synchronously right here, as it used to be.
                //
                // This constructor previously duplicated `Self::open_repo_in_current_window`'s
                // spawn/baseline/focus block almost verbatim; both now funnel through that one
                // method, so a CLI launch and "Open Folder…" can no longer drift apart on which
                // worktree the window lands in.
                //
                // `opened_target` is what that sequence resolves its selection against
                // (`crate::rail::worktrees::selection_for_opened_repo`: an exact match on this
                // path, else the main checkout). For a repo resolved purely from memory - the
                // plain "relaunch Jerry" gesture - that is the remembered worktree, so relaunching
                // genuinely lands back in the worktree you were last in, and
                // `Self::restore_worktree_session` then reopens its tabs. For an explicitly named
                // path it stays the path itself, unchanged: `jerry ~/repo` is a real statement
                // about where to work, and silently opening some other worktree of that repo
                // because it was the last one visited would be overriding the user, not helping
                // them. `selection_for_opened_repo` needs no change either way - a remembered
                // worktree that has since been removed simply matches nothing and falls back to
                // the main checkout, exactly as an unrecognised path already does.
                let opened_target = match opened_from_memory {
                    true => repo::repo_key(&path)
                        .and_then(|key| loaded_repo_state.remembered_worktree(&key))
                        .unwrap_or_else(|| path.clone()),
                    false => path.clone(),
                };
                this.load_worktrees_for_opened_repo(opened_target, cx);
                // Keyboard focus while that fetch is in flight. Without this the window would
                // render its first frames with `Window::focus == None` - the dangling-focus bug
                // class this crate's `OverlayFocus`/`restore_focus` machinery exists to prevent -
                // since the agent that focus used to land on doesn't exist yet. The rail's own
                // root container is part of the rendered tree for exactly this state (a focused
                // repo showing the workspace body), and `spawn_initial_shell_for_opened_repo`
                // moves focus onto the real terminal the moment there is one.
                window.focus(&this.rail_focus_handle, cx);
                this.load_file_tree(path.clone(), cx);
                this.load_diff(path, cx);
                this.start_status_polling(cx);
                this.start_worktree_watch(cx);
            }
            None => {
                // A genuinely empty window (GitHub issue #90) has no code surface/rail/tab strip
                // in its rendered tree at all (`Self::render_empty_state`) - nothing above moved
                // real keyboard focus anywhere, so this is the same "never leave `Window::focus`
                // dangling" discipline `Self::select_worktree`'s own fallback branch already
                // uses when a worktree switch leaves no agent to focus: the rail's own root
                // container, which - unlike a genuinely empty window's own body - is *not* part
                // of the rendered tree here, so `Self::render_empty_state`'s own focus handle is
                // used instead. See that method's docs.
                window.focus(&this.empty_state_focus_handle, cx);
            }
        }
        // Every repo restored from `repos.toml` (`loaded_repo_state`, above) besides whichever
        // one just became focused (already covered by `load_worktrees` in the `Some` branch
        // above) needs its own real first `Self::load_repo_worktrees` fetch too - unlike a repo
        // added later via `Self::add_repo` (which triggers this itself), these were pushed
        // straight into `repos` during construction, before `self` existed to call it against.
        // Run unconditionally (not only inside the `Some(path)` branch above): a genuinely empty-
        // focus startup can still have real, previously-added repos sitting in `Self::repos` from
        // a past session, and they deserve real rail data too, not just the one that happens to
        // be focused this time.
        let non_focused_repo_ids: Vec<RepoId> = this
            .repos
            .iter()
            .filter(|repo| Some(repo.id) != this.focused_repo)
            .map(|repo| repo.id)
            .collect();
        for repo_id in non_focused_repo_ids {
            this.load_repo_worktrees(repo_id, cx);
        }
        this.start_repo_worktrees_polling(cx);
        // GitHub issue #87: a real startup check, plus the periodic loop that keeps re-checking
        // for as long as the app runs - see `crate::updater::flow::AdeApp::
        // start_update_check_loop`'s own docs. Unconditional for the same reason the keybindings/
        // theme setup above is: it has nothing to do with which (if any) repo is focused.
        this.start_update_check_loop(cx);
        // GitHub issue #226: the app-start sound. `crate::sound::claim_app_start_sound` is the
        // real "once per process" gate - a second window (`File > New Window`,
        // `crate::title_bar::menu`) constructs a second `AdeApp` and reaches this same line, but
        // finds the process-global flag already claimed and skips straight past. Only once that
        // gate is won does `Self::maybe_play_app_start_sound` even check whether the user wants
        // this sound at all (`settings.sound.enabled` + its own toggle) - order matters here:
        // checking the settings gate first would let a *disabled* first window's early return
        // leave the process-global flag unclaimed, and a second window opened moments later with
        // the setting since turned on would then wrongly play it.
        if crate::sound::claim_app_start_sound() {
            this.maybe_play_app_start_sound();
        }
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
        self.load_worktrees_with_intent(WorktreeLoadIntent::Refresh, cx);
    }

    /// Gives a just-opened repo its guaranteed initial shell, in the worktree that was *genuinely
    /// selected* a moment earlier - the second half of [`Self::load_worktrees_for_opened_repo`]'s
    /// own contract, split out so the "which worktree" decision and the "spawn into it" step are
    /// visibly separate steps rather than one tangled block.
    ///
    /// Spawns into [`Self::current_worktree_path`] and refuses outright when that is `None`, which is
    /// the whole point: there is no such thing as a tab attributed to a repo rather than to a
    /// worktree, so if nothing real is selected there is nothing legitimate to spawn. (Today the
    /// caller has already either selected a real worktree or fallen into
    /// `current_worktree_path`'s one documented last resort, so `None` here means the repo stopped
    /// being focused entirely between the fetch being issued and it landing - a real race, worth
    /// refusing rather than guessing through.)
    ///
    /// Idempotent against a worktree that already has a real agent open - a repo revisited after
    /// being unfocused keeps whatever agents it already had running (see
    /// [`crate::root::AdeApp::open_repo_in_current_window`]'s cross-repo persistence docs), so
    /// this must never stack a redundant second shell onto one that is already there.
    fn spawn_initial_shell_for_opened_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cwd) = self.current_worktree_path() else {
            return;
        };
        // Deliberately *before* the guaranteed-shell check below, not after: this worktree may
        // have had a whole tab session last time, terminal included, and reopening it first is
        // what lets the check below see a worktree that already has its own real agent and decline
        // to stack a redundant extra shell on top. See
        // `crate::work_surface::session::AdeApp::restore_worktree_session`'s own docs. A no-op
        // whenever `Self::select_worktree` already restored this worktree a moment ago (the
        // ordinary path), and whenever there is nothing recorded at all.
        self.restore_worktree_session(cwd.clone(), window, cx);
        if self.agents.iter_for_cwd(cwd.clone()).next().is_none() {
            let startup_agent = self.agents.spawn(
                ProcessKind::Shell,
                cwd.clone(),
                self.settings.appearance.terminal_font_size,
                self.settings.terminal.shell_override(),
                // A shell, so no hook injection - `Agents::spawn` would discard one anyway.
                None,
                window,
                cx,
            );
            // GitHub issue #225: the startup shell is a real agent like any other and needs a
            // real review baseline too - see `crate::review::flow::AdeApp::
            // capture_review_baseline`'s docs for why this is hooked at the `Agents::spawn` call
            // site rather than inside `Agents` itself. Missing this would leave exactly one agent
            // - the one every window starts with - permanently without a review.
            self.capture_review_baseline(startup_agent, cx);
            // The guaranteed startup shell is a real tab like any other, so it is part of this
            // worktree's persisted session too - see `crate::work_surface::session`.
            self.record_worktree_session(cx);
        }
        // Makes this worktree's own tab the globally active one, whether it was just spawned
        // above or was already running from an earlier visit.
        self.agents.activate_for_worktree(&cwd, cx);
        // `focus_newly_spawned_agent`, not a bare `Agents::focus_active`: a focused window must
        // never be left with `Window::focus == None` (see this crate's `OverlayFocus`/
        // `restore_focus` docs), but it must equally never point focus at a terminal pane that
        // isn't in the rendered tree. "Open Folder…" is reachable *with Settings open* (the File
        // menu is an unconditional sibling of the Settings/workspace-body swap), and Settings
        // replaces the entire workspace body - so that guard is a real one here, not a formality.
        self.focus_newly_spawned_agent(window, cx);
        cx.notify();
    }

    /// [`Self::load_worktrees`] for the two real "this repo is being opened for genuine work"
    /// gestures - a CLI launch ([`Self::new_with_settings`]) and GitHub issue #90's "Open
    /// Folder…" ([`crate::root::AdeApp::open_repo_in_current_window`]).
    ///
    /// Both used to spawn their guaranteed initial shell *immediately*, into the bare repo path,
    /// and leave [`Self::selected`] at `None` - so the tab that shell produced belonged to no
    /// worktree at all and only rendered because [`Self::current_worktree_path`]'s old repo-root
    /// fallback happened to coincide with the main worktree's own path. That is the reported bug
    /// ("at the start of the program you select something and a tab bar has a terminal; then I
    /// select a worktree and this is lost"), and its fix is not to patch the fallback but to make
    /// the opening gesture do the real thing: resolve the repo's actual worktree list, genuinely
    /// select the right worktree of it ([`crate::rail::worktrees::selection_for_opened_repo`]),
    /// and spawn the initial shell into *that concretely-selected worktree*.
    ///
    /// The initial shell is therefore deliberately deferred until this real fetch lands rather
    /// than spawned synchronously beforehand. `git worktree list --porcelain` is a background
    /// call (this method never blocks the main thread on git - see [`Self::load_worktrees`]'s own
    /// task), and there is genuinely nothing correct to spawn *into* until it answers: a
    /// synchronous spawn would have to guess a cwd, and guessing the repo root is precisely the
    /// bug being removed. The interim frame or two is a real, honest "focused repo, nothing
    /// selected yet, empty tab strip" - not a fabricated tab - and [`Self::rail_focus_handle`]
    /// holds keyboard focus meanwhile so no frame ever renders with `Window::focus` dangling.
    ///
    /// Seeding synchronously from already-known data (the trick
    /// [`Self::select_worktree_by_path`]'s cross-repo case uses) is deliberately *not* used here:
    /// that case can only work because the row the user clicked was itself rendered from a
    /// [`crate::rail::repo::Repo::worktrees`] list that had already been fetched. A repo being
    /// opened for the first time has no such list by definition, so there would be nothing real
    /// to seed from in exactly the case that matters.
    pub(crate) fn load_worktrees_for_opened_repo(
        &mut self,
        opened_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.load_worktrees_with_intent(WorktreeLoadIntent::Opening { opened_path }, cx);
    }

    /// The shared body of [`Self::load_worktrees`]/[`Self::load_worktrees_for_opened_repo`] -
    /// one real `git worktree list --porcelain` fetch, with `intent` deciding only what happens
    /// to [`Self::selected`] once it lands. Kept as one function (rather than two similar ones)
    /// so the fetch, the [`crate::rail::repo::Repo::worktrees`] mirror, and the
    /// [`crate::rail::worktrees::recover_selection`] state machine can never drift apart between
    /// the opening path and the steady-state refresh path.
    fn load_worktrees_with_intent(&mut self, intent: WorktreeLoadIntent, cx: &mut Context<Self>) {
        let repo_path = self.focused_repo_path();
        // Captured now, not re-read via `this.focused_repo` once the fetch below completes: a
        // rapid double repo-switch could otherwise land this fetch's *old* result on whatever
        // repo happens to be focused *later*, rather than the one it was actually a fetch for.
        let focused_id = self.focused_repo;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::list_worktrees_porcelain(&repo_path) })
                .await;
            // `update_in`, not `update`: the `Opening` intent below performs a real worktree
            // selection and spawns this window's initial shell, both of which need a genuine
            // `&mut Window` (moving keyboard focus onto the terminal that results). The same
            // real pattern `Self::start_choose_repo_folder`'s own picker continuation uses.
            let _ = this.update_in(cx, |this, window, cx| {
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

                // Mirrors this same real fetch into the focused repo's own `Repo::worktrees` -
                // the rail's per-repo group listing (`crate::rail::render::AdeApp::
                // build_repo_groups`) reads that, not `Self::worktrees` directly, so the focused
                // repo's own rail group must never fall behind what this method just loaded. A
                // mirror, not a second independent fetch: `Self::start_repo_worktrees_polling`
                // deliberately skips the focused repo for exactly this reason - see its own docs.
                if let Some(id) = focused_id {
                    if let Some(repo) = this.repos.iter_mut().find(|repo| repo.id == id) {
                        repo.worktrees = this.worktrees.clone();
                        repo.worktrees_loaded = true;
                    }
                }

                match worktrees::recover_selection(previously_selected.as_ref(), &this.worktrees) {
                    // Nothing was selected before this fetch. What that *means* depends entirely
                    // on why the fetch happened, which is why `intent` is an explicit parameter
                    // rather than something guessed at from state here:
                    //
                    // - `Opening`: this repo is being opened for real work, and leaving it
                    //   unselected is exactly the reported bug - a focused repo with a live
                    //   startup terminal that belongs to no worktree. Land on a real worktree
                    //   (`selection_for_opened_repo`) and spawn the initial shell into it.
                    // - `Refresh`: a background watcher/poll tick, a post-prune reload, or
                    //   `checkout_repo_from_rail`'s own follow-up fetch. None of these are a
                    //   user asking to work somewhere, so none may select on the user's behalf -
                    //   see `crate::root::AdeApp::checkout_repo_from_rail`'s own docs. This is
                    //   now genuinely inert rather than merely *looking* inert: with
                    //   `Self::current_worktree_path`'s repo-root fallback gone, an unselected repo
                    //   renders an honestly empty tab strip and no selected rail row, instead of
                    //   the three-way rail/tab-strip/centre-pane disagreement it used to.
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
                        this.set_file_tree_root(new_root.clone(), cx);
                        this.file_tree = file_tree::FileTree::default();
                        this.reload_expanded_dirs_from_fold_state();
                        this.load_file_tree(new_root.clone(), cx);
                        this.load_diff(new_root, cx);
                    }
                }

                // Deliberately *after* the recovery match rather than inside its
                // `NoPriorSelection` arm, and keyed off `this.selected` rather than off which arm
                // ran. Both matter, for the same real race: this fetch is asynchronous, so the
                // user can click a worktree row while it is still in flight. That click sets
                // `Self::selected`, which makes `recover_selection` report `Unchanged`/
                // `FellBackToMain` here instead of `NoPriorSelection` - and an `Opening` handler
                // living inside that one arm would then silently skip the window's *guaranteed*
                // initial shell entirely, leaving a freshly opened repo with no terminal at all.
                //
                // Written this way, a raced click is simply respected: the worktree the user
                // actually chose stays selected, and the initial shell is spawned into that, not
                // into whatever this fetch would have picked on its own.
                if let WorktreeLoadIntent::Opening { opened_path } = &intent {
                    if this.selected.is_none() {
                        if let Some(index) =
                            worktrees::selection_for_opened_repo(opened_path, &this.worktrees)
                        {
                            // The one real selection entry point, deliberately - it is what
                            // re-roots the file tree/diff, activates the worktree's own remembered
                            // tab, and moves focus. Reimplementing a partial copy here is exactly
                            // how the opening path would drift away from an ordinary rail click.
                            this.select_worktree(index, window, cx);
                        }
                    }
                    // Runs whether or not a worktree was selectable: a repo with no usable
                    // worktree at all (an unreadable path, or a directory that is not a git
                    // repository) still gets its guaranteed initial shell, via
                    // `Self::current_worktree_path`'s one documented last resort. See its own docs.
                    this.spawn_initial_shell_for_opened_repo(window, cx);
                }

                this.load_disk_usage(cx);
                cx.notify();
            });
        });
        self._load_worktrees_task = Some(task);
    }

    /// A real, one-shot `wt_core::list_worktrees_porcelain` fetch for a single repo, writing the
    /// result straight into that repo's own [`Repo::worktrees`]/[`Repo::worktrees_loaded`] - the
    /// rail-display counterpart to [`Self::load_worktrees`], which stays scoped to [`Self::
    /// focused_repo`] and the single-slot [`Self::worktrees`] field the file tree/diff/agent-
    /// spawn machinery reads (see that field's own docs for why the two are deliberately kept
    /// separate rather than merged into one).
    ///
    /// Called once per newly [`Self::add_repo`]-ed repo (so a freshly added repo shows a real
    /// count within moments, rather than waiting for [`Self::start_repo_worktrees_polling`]'s own
    /// [`REPO_WORKTREES_POLL_INTERVAL`] tick) and once per repo restored from `repos.toml` at
    /// startup (`Self::new_with_settings`) - both real, one-time "get this repo a first real
    /// answer promptly" calls, not part of the steady-state keep-fresh cadence itself.
    ///
    /// A no-op if `repo_id` isn't (or is no longer) a known repo - defensive, matching every
    /// other `RepoId`-keyed lookup in this crate. On a genuine fetch failure (an inaccessible or
    /// since-deleted path - `wt_core::list_worktrees_porcelain` itself already reports that as a
    /// real `Err`, never a panic), this still marks the repo [`Repo::worktrees_loaded`] `true`
    /// with an empty list: the identical "attempted, got a definitive (if disappointing) answer"
    /// contract [`Self::load_worktrees`] already applies to the focused repo's own [`Self::
    /// worktrees_error`] case, so a broken repo shows a real, honest "no worktrees" rather than
    /// spinning on "not loaded yet" forever.
    pub(crate) fn load_repo_worktrees(&mut self, repo_id: RepoId, cx: &mut Context<Self>) {
        let Some(path) = self
            .repos
            .iter()
            .find(|repo| repo.id == repo_id)
            .map(|repo| repo.path.clone())
        else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::list_worktrees_porcelain(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(repo) = this.repos.iter_mut().find(|repo| repo.id == repo_id) {
                    repo.worktrees = match result {
                        Ok(results) => worktrees::build_worktree_items(results),
                        Err(_) => Vec::new(),
                    };
                    repo.worktrees_loaded = true;
                }
                cx.notify();
            });
        });
        self._repo_worktrees_tasks.push(task);
    }

    /// The steady-state keep-fresh sweep for every *non-focused* [`Self::repos`] entry's own
    /// [`Repo::worktrees`] - the currently focused repo is deliberately excluded here, since it
    /// already gets kept fresh at a faster cadence by [`Self::load_worktrees`]'s own mirror
    /// (see that method's docs) plus the real filesystem watcher/poll fallback ([`Self::
    /// start_worktree_watch`]); fetching it a second time here would be genuine duplicate `git`
    /// subprocess work for data this app already has.
    ///
    /// Started once, at startup (`Self::new_with_settings`) - unlike [`Self::
    /// start_worktree_watch`]/[`Self::start_status_polling`], this loop is never restarted on a
    /// repo switch, since it reads [`Self::repos`]/[`Self::focused_repo`] fresh on every tick
    /// rather than closing over one repo's path at spawn time; one instance already serves
    /// however many repos are added, for the whole life of the window.
    ///
    /// ## Cadence
    ///
    /// Ticks every [`REPO_WORKTREES_POLL_INTERVAL`] - see that constant's own docs for why it is
    /// deliberately slower than [`STATUS_POLL_INTERVAL`].
    ///
    /// ## Concurrency cap
    ///
    /// Firing one real `git worktree list` subprocess per non-focused repo *simultaneously* on
    /// every tick would be unbounded process-spawn cost for a user with many repos added. Each
    /// tick instead splits the due repos into fixed-size batches of [`REPO_WORKTREES_FETCH_CONCURRENCY`]
    /// (`crate::rail::repo::batch_repos_for_refresh` - see its own docs/tests for the exact
    /// chunking), spawning one batch's real subprocesses concurrently on the background executor
    /// and fully awaiting all of them before starting the next batch - so no more than
    /// [`REPO_WORKTREES_FETCH_CONCURRENCY`] of this sweep's own `git` processes are ever in
    /// flight at once, regardless of how many repos are due.
    pub(crate) fn start_repo_worktrees_polling(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            'outer: loop {
                cx.background_executor()
                    .timer(REPO_WORKTREES_POLL_INTERVAL)
                    .await;

                let Ok(due_ids) = this.update(cx, |this, _cx| {
                    let focused = this.focused_repo;
                    this.repos
                        .iter()
                        .filter(|repo| Some(repo.id) != focused)
                        .map(|repo| repo.id)
                        .collect::<Vec<_>>()
                }) else {
                    break;
                };

                for batch in
                    repo::batch_repos_for_refresh(&due_ids, REPO_WORKTREES_FETCH_CONCURRENCY)
                {
                    let Ok(paths) = this.update(cx, |this, _cx| {
                        batch
                            .iter()
                            .filter_map(|id| {
                                this.repos
                                    .iter()
                                    .find(|repo| repo.id == *id)
                                    .map(|repo| (*id, repo.path.clone()))
                            })
                            .collect::<Vec<_>>()
                    }) else {
                        break 'outer;
                    };

                    // Spawned up front (not one at a time inside the loop below) so every
                    // subprocess in this batch genuinely starts running concurrently on the
                    // background executor - awaiting them afterward just collects results, it
                    // doesn't gate when each one's real `git` call begins.
                    let fetches: Vec<_> = paths
                        .into_iter()
                        .map(|(id, path)| {
                            cx.background_executor().spawn(async move {
                                (id, wt_core::list_worktrees_porcelain(&path))
                            })
                        })
                        .collect();

                    for fetch in fetches {
                        let (id, result) = fetch.await;
                        let updated = this.update(cx, |this, cx| {
                            if let Some(repo) = this.repos.iter_mut().find(|repo| repo.id == id) {
                                repo.worktrees = match result {
                                    Ok(results) => worktrees::build_worktree_items(results),
                                    Err(_) => Vec::new(),
                                };
                                repo.worktrees_loaded = true;
                            }
                            cx.notify();
                        });
                        if updated.is_err() {
                            break 'outer;
                        }
                    }
                }
            }
        });
        self._repo_worktrees_poll_task = Some(task);
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

    pub(crate) fn set_file_tree_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.file_tree_root == root && self.fold_state_root_key.is_some() {
            return;
        }
        self.fold_state_root_key = fold_state::worktree_key(&root);
        self.file_tree_root = root;
        // Re-arms the real filesystem watch on the *new* root (GitHub issue #13) - see
        // `Self::start_file_tree_watch`'s own docs on why this only happens on a genuine root
        // change, guarded by the early return above, rather than on every `load_file_tree` call.
        self.start_file_tree_watch(cx);
    }

    /// Walks [`Self::file_tree_root`] and applies the result, off the foreground thread.
    ///
    /// GitHub issue #160 removed this walk's entry cap ("File tree should load all folders and
    /// files"), so the amount of work here is now bounded only by what is actually on disk. Two
    /// steps run on `gpui::BackgroundExecutor` because of it, not one:
    ///
    /// 1. the walk itself (`file_tree::build_file_tree`), as it always has; and
    /// 2. the palette's file-candidate list, which allocates a
    ///    `crate::palette::state::FileCandidate` per file and used to be built by
    ///    [`Self::rebuild_palette_file_candidates`] right here in the completion handler - on the
    ///    foreground thread, which was one of the two real costs the old cap existed to bound.
    ///
    /// The diff marks those candidates carry are snapshotted between the two
    /// (`palette::render::file_diff_marks`), on the foreground, because they live on `self`. That
    /// snapshot is bounded by the *diff's* size (tens of files), not the tree's, so it is the one
    /// piece of this that stays on the foreground thread and is genuinely cheap there.
    pub(crate) fn load_file_tree(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.set_file_tree_root(root.clone(), cx);
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { file_tree::build_file_tree(&root) }
                })
                .await;

            // Identity guard, applied at *every* point this task touches the app: a worktree
            // switch that lands while the walk is still running replaces `_load_file_tree_task`
            // (cancelling it) *and* moves `file_tree_root` - but a task already past an `await`
            // point can still reach here. Applying a stale walk would show one worktree's tree
            // under another's root, and - far worse - prune the *new* worktree's fold state
            // against the *old* worktree's directory list.
            let listing = match result {
                Ok(listing) => listing,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        if this.file_tree_root != root {
                            return;
                        }
                        this.file_tree = file_tree::FileTree::default();
                        this.file_tree_complete = false;
                        this.file_tree_error = Some(err.to_string());
                        this.rebuild_palette_file_candidates();
                        cx.notify();
                    });
                    return;
                }
            };

            let Ok(Some(marks)) = this.update(cx, |this, _| {
                (this.file_tree_root == root)
                    .then(|| palette_render::file_diff_marks(this.current_diff()))
            }) else {
                return;
            };

            let complete = listing.is_complete();
            let (tree, candidates) = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    let marks = marks.clone();
                    async move {
                        let candidates =
                            palette_render::build_file_candidates(&listing.tree, &root, &marks);
                        (listing.tree, candidates)
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if this.file_tree_root != root {
                    return;
                }
                this.file_tree_complete = complete;
                this.file_tree = tree;
                this.file_tree_error = None;
                // Applied in the same update as the tree they were built from, so the palette can
                // never be showing candidates for one walk while the sidebar shows another's rows.
                this.palette_file_candidates = candidates;
                // A `load_diff` that landed during the background hop above built its candidates
                // from the *old* tree, and the ones just applied carry the marks as they were
                // before it. Re-deriving here is the O(loaded files) foreground pass this whole
                // arrangement exists to avoid, so it is gated on the marks having genuinely moved
                // - which needs one cheap comparison over the diff's files, not the tree's.
                if palette_render::file_diff_marks(this.current_diff()) != marks {
                    this.rebuild_palette_file_candidates();
                }
                this.prune_stale_fold_state(cx);
                cx.notify();
            });
        });
        self._load_file_tree_task = Some(task);
    }

    /// Starts (or, via [`Self::set_file_tree_root`], re-starts on a new worktree) the real
    /// filesystem-watch-plus-poll-fallback loop behind GitHub issue #13's "the file list...
    /// drifts out of sync with the actual state on disk" - the same debounced-watcher-plus-
    /// poll-fallback shape `Self::start_worktree_watch` already established for the worktree
    /// list, just scoped to [`Self::file_tree_root`] and reloading via [`Self::load_file_tree`]
    /// instead. Assigning fresh values to [`Self::_file_tree_watcher`]/
    /// [`Self::_file_tree_watch_task`] drops (and so silently stops) whatever watcher/loop was
    /// previously watching the worktree just left - `Self::set_file_tree_root`'s own early
    /// return is what keeps this from happening on every unrelated `load_file_tree` call, not
    /// this method itself re-checking anything.
    ///
    /// A no-op (clearing both fields rather than starting anything) unless both:
    /// - `root` is really part of a git worktree (production never has a non-git
    ///   [`Self::file_tree_root`] - see
    ///   `crate::sidebar::file_tree_watch::spawn_file_tree_watcher`'s own docs on that gate), and
    /// - [`Self::settings_path`] is real (`Some`) - the same "this is a real, persisted session,
    ///   not a throwaway test instance" signal [`Self::persist_settings`] already gates its own
    ///   real disk write on (see that method's own docs).
    ///
    /// Both checks matter operationally, not just semantically: a real, reproduced regression
    /// found while building this - a real `notify::RecommendedWatcher` OS thread/instance spun
    /// up for essentially every one of this crate's own GPUI tests (the overwhelming majority
    /// construct an `AdeApp` against a real git repo purely for unrelated reasons, e.g. `wt_core`
    /// diff/merge/undo coverage, with a `None` settings path per `root::focus::palette_focus_
    /// tests::open_test_app`'s own docs) - was enough cumulative resource pressure, across a full
    /// `cargo test` run, to start starving `crate::rail::worktree_watch`'s own real-OS-thread-
    /// driven tests past their real-time budget. The `settings_path` check alone would already
    /// fix that (it's `None` for effectively every test but the handful that deliberately opt
    /// into a real settings path to test real persistence), but the git-repo check is kept too
    /// since it's independently correct for production, matching
    /// `crate::rail::worktree_watch::spawn_worktree_watcher`'s own identical gate.
    pub(crate) fn start_file_tree_watch(&mut self, cx: &mut Context<Self>) {
        let root = self.file_tree_root.clone();
        if self.settings_path.is_none() || wt_core::git_common_dir(&root).is_err() {
            self._file_tree_watcher = None;
            self._file_tree_watch_task = None;
            return;
        }
        let dirty: crate::rail::worktree_watch::DirtyFlag =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self._file_tree_watcher =
            crate::sidebar::file_tree_watch::spawn_file_tree_watcher(&root, dirty.clone());

        let task = cx.spawn(async move |this, cx| {
            let mut last_refresh = Instant::now();
            loop {
                cx.background_executor().timer(FILE_TREE_WATCH_TICK).await;

                let watcher_fired = dirty.load(std::sync::atomic::Ordering::SeqCst);
                if watcher_fired {
                    // Let a burst of events from one save/build/checkout settle before acting,
                    // then clear whatever accumulated during the settle window too - see
                    // `Self::start_worktree_watch`'s identical own reasoning.
                    cx.background_executor().timer(FILE_TREE_WATCH_SETTLE).await;
                    dirty.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                let poll_due = last_refresh.elapsed() >= FILE_TREE_WATCH_POLL_INTERVAL;

                if !watcher_fired && !poll_due {
                    continue;
                }
                last_refresh = Instant::now();

                let updated = this.update(cx, |this, cx| {
                    // The active worktree may have changed since this loop's own last tick (a
                    // fresh loop for the new root already started - see this method's own docs -
                    // so this one only needs to stop cleanly rather than reload the wrong root).
                    if this.file_tree_root != root {
                        return false;
                    }
                    this.load_file_tree(root.clone(), cx);
                    true
                });
                match updated {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        });
        self._file_tree_watch_task = Some(task);
    }

    /// The **one** real answer to "which worktree is the app currently working in" - the single
    /// path the tab strip is scoped to, a new agent spawns into, and the rail draws its selected
    /// row from. See the module docs' "Agents/tabs" section for why this is resolved on demand
    /// rather than tracked as a per-tab "current worktree".
    ///
    /// ## Why this returns `Option`
    ///
    /// This used to be infallible, falling back to [`Self::focused_repo_path`] whenever
    /// [`Self::selected`] was `None`. That fallback was the shared root cause of a family of
    /// reported bugs - the same underlying flaw as the three already fixed on this branch, not a
    /// separate one - because *a repo root is not a worktree*. Concretely, live-reproduced:
    ///
    /// - **A real tab that no rail row claims.** Launching against a subdirectory of a repo
    ///   (`jerry ./crates` - an entirely ordinary invocation) made this resolve to
    ///   `<repo>/crates`, which `git worktree list --porcelain` reports as no worktree at all.
    ///   The startup shell spawned there rendered a real tab in the strip while *every* rail row
    ///   read as unselected, and the instant any worktree row was clicked the tab vanished with
    ///   no path back - its `cwd` could never again equal any row's path, so the live PTY was
    ///   permanently orphaned.
    /// - **The rail, the tab strip, and the centre pane disagreeing three ways.** With
    ///   [`Self::selected`] `None` but a real agent already open in the repo root, the rail drew
    ///   its main-worktree row as selected (this fallback matched it), the tab strip drew that
    ///   agent's tab (`Self::combined_tab_order` scoped to this same fallback), and the centre
    ///   pane rendered nothing at all (`Agents::active` having been genuinely cleared).
    /// - **The reported "I select a worktree and the startup terminal is lost."** The startup
    ///   shell's tab showed only because this fallback happened to coincide with the main
    ///   worktree's own path while nothing was selected - so the user never performed the
    ///   selection that owned it, and had no model of where it went or that clicking the main
    ///   row brings it back.
    ///
    /// `None` is therefore a real, honest state now - "no worktree is genuinely selected" - and
    /// every caller renders it as such (an empty tab strip, an empty centre pane, no rail row
    /// reading as selected, and no spawn) rather than silently substituting the repo root.
    ///
    /// ## The one documented last resort
    ///
    /// A focused repo whose worktree list has genuinely *landed* and contains nothing usable -
    /// an unreadable path, or a directory that isn't a git repository at all, both of which
    /// `wt_core::list_worktrees_porcelain` reports as a real `Err` that
    /// [`Self::load_worktrees`] turns into an empty [`Self::worktrees`] - still resolves to the
    /// repo root. This is a real error state, not the common path, and it is self-consistent in
    /// a way the old blanket fallback never was: there are no worktree rows at all, so there is
    /// no row that could disagree with it, and the repo root is the only honest place left to
    /// work in. Gated on [`crate::rail::repo::Repo::worktrees_loaded`] specifically so the
    /// window *before* the first fetch lands - when [`Self::worktrees`] is empty merely because
    /// nothing has been asked yet - reports the honest `None` instead of briefly reintroducing
    /// the very fallback this removes.
    pub(crate) fn current_worktree_path(&self) -> Option<PathBuf> {
        if let Some(item) = self.selected.and_then(|index| self.worktrees.get(index)) {
            if item.error.is_none() {
                return Some(item.path.clone());
            }
        }
        // See "The one documented last resort" above.
        let list_has_landed = self
            .focused_repo()
            .is_some_and(|repo| repo.worktrees_loaded);
        let nothing_usable = !self.worktrees.iter().any(|item| item.error.is_none());
        if list_has_landed && nothing_usable {
            return Some(self.focused_repo_path());
        }
        None
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
        // The safety net for the tab session of whatever is being switched *away* from
        // (`crate::work_surface::session`): every real tab mutation records as it happens, so this
        // is normally a no-op that compares an unchanged snapshot and returns - but it means a
        // future mutation path that forgets to record still can't lose a user's tabs, since
        // leaving a worktree is something every such path is eventually followed by. Must run
        // before `self.selected` moves below, while `Self::current_worktree_path` still resolves to the
        // worktree being left.
        self.record_worktree_session(cx);
        self.selected = Some(index);
        // A real, explicit user selection supersedes any stale "fell back to main" notice a
        // previous refresh may have left up - see `Self::worktree_selection_notice`'s own docs.
        self.worktree_selection_notice = None;
        // "Which worktree of this repo was I last in" - the per-repo memory a later launch reads
        // back to land here again (`crate::rail::repo::RepoRecord::selected_worktree`, resolved at
        // startup in `Self::new_with_settings`). Recorded for the *focused* repo only, which is
        // the only repo `self.worktrees`/`self.selected` ever describe.
        if let Some(key) = self.focused_repo().and_then(|r| repo::repo_key(&r.path)) {
            if self.selected_worktree_by_repo.get(&key) != Some(&path) {
                self.selected_worktree_by_repo.insert(key, path.clone());
                self.persist_repo_state(cx);
            }
        }
        // Makes this worktree's own last-active tab (or its first agent, or none) the
        // globally active one - see `Agents::activate_for_worktree`'s own docs for why this
        // invariant ("the active agent always belongs to the selected worktree") is the real
        // fix this revision makes: before it, selecting a worktree never touched `self.agents`
        // at all, so the centre pane could keep showing a completely different worktree's
        // terminal after a rail click.
        self.agents.activate_for_worktree(&path, cx);
        self.reset_repo_scoped_state(path.clone(), window, cx);
        // Last, and deliberately so: `reset_repo_scoped_state` is what re-roots
        // `Self::file_tree_root` onto this worktree, and restoring file tabs before that would
        // file them under whichever worktree was just left (`Self::open_files_mut` is keyed by
        // that root). It also moves focus, which the restore then re-does for whatever it spawned.
        // A no-op for a worktree already restored in this window, or with nothing recorded.
        self.restore_worktree_session(path, window, cx);
    }

    /// The shared core of "something completely different is now the single-repo-scoped root
    /// this window revolves around" - every piece of per-worktree/per-repo transient UI state
    /// this app has, reset, plus the real reload ([`Self::load_file_tree`]/[`Self::load_diff`]/
    /// [`Self::evict_stale_lsp_clients`]/a graph-tab refresh if open) against `new_root`.
    ///
    /// Two real callers, extracted here (an independent audit's own finding) so they can never
    /// drift apart on which state gets reset: [`Self::select_worktree`] (switching worktrees
    /// *within* the same repo) and [`Self::open_repo_in_current_window`] (GitHub issue #90's
    /// "Open Folder", switching to an entirely different repo, or out of a genuinely empty
    /// window). Both are the same real invariant - "nothing here may still point at whatever was
    /// open before" - reached through different UI gestures; before this extraction,
    /// `open_repo_in_current_window` only reset four of these fields
    /// (`staged_files`/`open_change`/`expanded_dirs`/`selected_tree_path`), leaving every other
    /// one - `tree_undo_stack`/`tree_redo_stack`, `tree_clipboard`, `tree_context_menu`, `tree_inline_edit`,
    /// `discard_confirm_armed`, `prune_confirm_armed`, `commit_menu_open`, every file/LSP/blame
    /// cache and in-flight task - armed against whatever repo was open *before* the folder was
    /// switched. Concretely: arming a delete confirmation on `<old repo>/x`, opening a different
    /// folder, and confirming the still-rendered modal used to delete inside the *old* repo.
    ///
    /// Callers are responsible for whatever is genuinely different between "switching worktrees"
    /// and "switching repos" themselves: `self.selected`/`self.agents.activate_for_worktree`
    /// (`select_worktree`) vs. `self.focused_repo`/spawning an initial agent
    /// (`open_repo_in_current_window`) - this only owns the part identical to both.
    pub(crate) fn reset_repo_scoped_state(
        &mut self,
        new_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Browsing away disarms a pending prune confirmation - see `Self::request_prune`'s docs.
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        // Same reasoning for every floating menu (`crate::root::menus`): the commit composer's
        // split-button popover (Revision R12 §5) names the previously selected worktree's staged
        // set, the file tree's context menu targets a row that is about to stop existing, and the
        // git graph's row menu names a commit in the repo being left - none may stay open pointed
        // at the wrong one. This used to clear only `commit_menu_open` here and
        // `tree_context_menu` further down, leaving the graph's two menus behind (GitHub issue
        // #176's own audit of who closes what).
        let _ = self.close_menu_surfaces_except(None);
        // GitHub issue #242 phase B fix: a real, independently reproduced bug - interactive
        // rebase mode names a real worktree (`GraphTabState::rebase`'s own `worktree_root`) that
        // this switch is about to move `self.diff_root` away from. Left alone, every subsequent
        // rebase-mode click (`Continue`, `Start`, ...) would silently keep operating on the
        // *original* worktree via its own stored root while the graph pane visibly showed a
        // different one - or, worse, since every worktree of the same repository shares one real
        // object database, a plan's commit ids can resolve fine in the *new* worktree too,
        // letting a `Start`/`Continue` genuinely rewrite the wrong branch. Leaving rebase mode
        // outright here (real agent resume included, via `Self::leave_rebase_mode`) is the real
        // primary defense; `RebaseModeState::worktree_root`'s own stored-root check is the
        // backstop in case some other path ever reaches a rebase op without going through this.
        if self.graph_state.rebase.is_some() {
            self.leave_rebase_mode(cx);
        }
        // Reset per-worktree UI state (see `reset_per_worktree_ui_state`'s docs) so switching
        // never leaks a staged checkbox, open diff, or collapsed-dir entry from whatever was open
        // before. Deliberately runs *before* the focus-fallback block below: both
        // `focus_newly_spawned_agent` and the fallback branch after it key off
        // `self.open_change`, and until this call runs, `open_change` can still reflect the
        // worktree/repo just *left* (a file tab open there) rather than the real post-switch
        // state - evaluating either guard first (a real, live-reproduced bug found in this
        // revision's own self-audit) made both branches see a stale `Some` and skip moving focus
        // at all, leaving `Window::focus` dangling on `code_focus_handle` once `open_change` was
        // cleared one statement later.
        reset_per_worktree_ui_state(
            &mut self.staged_files,
            &mut self.open_change,
            &mut self.expanded_dirs,
            &mut self.selected_tree_path,
            &mut self.additional_tree_selection,
        );
        // The tree's fold state is per-worktree *persisted* state, not merely per-worktree
        // transient state: the reset above clears the live set, and this re-derives it from
        // whatever is genuinely being switched *to*. A worktree/repo with no recorded state (a
        // freshly created or freshly opened one) gets an empty set and so opens fully collapsed -
        // the issue's own suggested answer to "does a fresh worktree inherit anything".
        //
        // `file_tree_root`/`file_tree` move in the *same* step, deliberately: `load_file_tree`
        // below sets the root too, but its walk is asynchronous, so without this the frames
        // between here and the walk landing would render whatever was left's rows against the
        // new root's expanded set - and a click on one of those stale rows would reach
        // `set_dir_expanded` with a path from an entirely different worktree/repo. Clearing
        // `file_tree` makes that window render an honestly empty tree instead.
        self.set_file_tree_root(new_root.clone(), cx);
        self.file_tree = file_tree::FileTree::default();
        self.reload_expanded_dirs_from_fold_state();
        // Every one of these holds an absolute path in whatever was just left (GitHub issue #19):
        // a half-typed name for a folder in the old tree, a cut/copied entry a paste here would
        // move across worktrees/repos, and an armed delete for a path in the old tree. Cleared
        // together, in the same step as `file_tree`/`expanded_dirs` above and for the same reason
        // - the window between here and the new walk landing must not leave a control pointing at
        // the old worktree/repo. The tree's *context menu* belongs to that same list but is closed
        // by the `close_menu_surfaces_except` sweep at the top of this method instead, so there is
        // exactly one place that closes it.
        self.tree_inline_edit = None;
        self.tree_clipboard = None;
        // Every entry names an absolute path in whatever was just left, same reasoning as the
        // fields just above - undoing/redoing after a switch must never reach into a different
        // worktree/repo. `clear_tree_undo_history` also best-effort cleans up any orphaned
        // delete backups this leaves unreachable.
        self.clear_tree_undo_history();
        self.tree_op_error = None;
        self.file_tree_complete = false;
        // `focus_newly_spawned_agent` (despite its name - its body has no "newly spawned" logic
        // in it, just the shared "move focus unless a file tab/Settings is showing" guard) closes
        // the dangling-focus risk this switch creates: the previously-active agent's pane may
        // no longer be part of the rendered tree at all once the tab strip's own per-worktree
        // filter (`Self::render_tab_strip`) applies, so keyboard focus left pointing at it would
        // silently break every keybinding until the next click - the same "focus left pointing at
        // something no longer rendered" bug class this project's own `OverlayFocus`/
        // `restore_focus` mechanism exists to prevent, applied here to a plain worktree/repo
        // switch rather than an overlay open/close. `open_change` above is already this switch's
        // real post-reset value by the time this runs, so the guard it checks is genuine.
        self.focus_newly_spawned_agent(window, cx);
        // `focus_newly_spawned_agent` is a real no-op when whatever was just switched to has no
        // open agent at all (`Agents::focus_active` has nothing to focus) - so if a
        // previously-focused agent's pane belonged to whatever was left, it's now exactly as
        // dangling as the case the comment above already covers, just with no agent to redirect
        // *onto*. Fall back to the rail's own root container (`Self::rail_focus_handle`), which
        // is part of the rendered tree whenever the workspace body is showing (never while
        // Settings has replaced it - `!self.settings_open` guards that the same way
        // `focus_newly_spawned_agent` itself does, and never while a genuinely empty window is
        // showing `Self::render_empty_state` instead, which has no rail at all - callers switching
        // *out of* empty state are responsible for their own focus, same as
        // `Self::open_repo_in_current_window` already is). Deliberately the rail's root, not its
        // filter field, which this used to target - see `Self::rail_focus_handle`'s own docs for
        // the real, audit-found keystroke-swallowing bug that became once the filter field
        // started carrying a `"text-input"` key context. It keeps the focused `FocusId` genuinely
        // findable in the next rendered frame, which is the actual invariant this exists to
        // protect: a dangling `FocusId` makes GPUI's action dispatch fall back to a disconnected
        // root with no real `on_action` handlers at all, not just this worktree's own missing
        // ones - silently breaking every global keybinding (⌘P included) until the next click.
        if self.agents.active_id().is_none() && self.open_change.is_none() && !self.settings_open {
            window.focus(&self.rail_focus_handle, cx);
        }
        // The File view's own per-worktree state (a cached parse and diff lookup that are about
        // to belong to a different `file_tree_root`) - reset for the same reason as above.
        // Dropping `_file_load_task` cancels any in-flight load for whatever was left.
        self.code_view = code_view::CodeView::Diff;
        self.markdown_view = markdown_preview::MarkdownView::Source;
        self.file_view_cache = None;
        self.file_load_state = FileLoadState::Idle;
        self.file_view_changed_lines = HashSet::new();
        self.code_cursor = None;
        self.file_view_error_count = None;
        self.open_diff_file_cache = None;
        // The real text-editing state above (`edit_buffers`) is per-worktree-reset via the shared
        // helper; its own transient/task-shaped siblings - which don't fit that helper's plain
        // free-function signature - are reset directly here for the same reason. Dropping the
        // task maps cancels every in-flight debounced re-highlight/save for whatever was left.
        self.file_view_row_layout = HashMap::new();
        self.file_view_last_layout = None;
        self.file_view_last_bounds = None;
        self.file_view_last_layout_for = None;
        self._rehighlight_tasks = HashMap::new();
        // Real live LSP sync/completions state (Revision R8.5b) is worktree-relative-path-keyed
        // (or entirely path-scoped) the same way `edit_buffers` above is - reset alongside it so
        // a switch can't leak a stale "already synced this content" record or a dangling popup
        // from whatever was left. `lsp_document_versions`/`lsp_uri_cache` are keyed by *absolute*
        // path (see their own docs), so neither ever actually collides across worktrees/repos and
        // both are left to `evict_stale_lsp_clients`'s own root-scoped pruning instead of a
        // blanket reset here.
        self._lsp_sync_tasks = HashMap::new();
        self._completions_request_task = None;
        self._completions_resolve_task = None;
        self.completions_resolve_in_flight = None;
        self.completions_resolved = std::collections::HashSet::new();
        self.completions_resolved_items = std::collections::HashMap::new();
        self.completions_suppress_next_trigger = false;
        self.lsp_last_synced_content = HashMap::new();
        self.lsp_synced_version = HashMap::new();
        self.lsp_diagnostics_confirmed_version = HashMap::new();
        self.dismiss_completions();
        self._file_save_tasks = HashMap::new();
        self.file_save_pending = HashSet::new();
        self.file_save_running = HashSet::new();
        self.file_save_error = None;
        self.file_external_conflict = HashSet::new();
        // The Diff view's syntax-highlight cache is keyed on a whole `DiffFile` from whatever was
        // left - reset alongside `open_diff_file_cache` above for the same reason (and so it
        // can't retain a full file's highlighting from a worktree/repo that's no longer active).
        self.diff_highlight_cache = None;
        self._file_load_task = None;
        // Editor zoom (`Settings.appearance.editor_zoom_percent`) is a real, globally-persisted
        // Settings field now - see `settings_store`'s "Editor zoom is one global, persisted
        // number now" docs - so it deliberately does *not* get reset here anymore.
        //
        // The hover cache is per-file - clear it too, or a hover card from whatever was left
        // could reappear the instant a same-named file opens in the new one. The real
        // Completions popup is already dropped above (alongside `_lsp_sync_tasks`/
        // `lsp_last_synced_content`) via `Self::dismiss_completions()` - repeated here,
        // idempotently, right next to `hover`'s own reset for the same reason every other real
        // `self.hover = None` site in this codebase now pairs the two (Revision R8.5b audit
        // finding 3), rather than relying solely on it having already run earlier in this
        // function.
        self.dismiss_hover();
        self.dismiss_completions();
        self.pending_cursor_line = None;
        // Real blame state (GitHub issue #29) is absolute-path-keyed - cleared alongside the
        // hover cache above for the identical reason: without this, a same-named file's blame
        // from whatever was left could reappear (wrongly attributed) the instant a same-named
        // file opens in the new one, and a stale `Loading`/in-flight task from the old worktree/
        // repo has no reason to keep running once it's no longer active.
        self.blame_cache.clear();
        self.blame_state.clear();
        self._blame_tasks.clear();
        self.blame_last_freshness_check = None;
        // Commit messages are sha-keyed, not path-keyed - a sha means the same real commit
        // regardless of which worktree/repo it's viewed from, so this cache deliberately survives
        // a switch (the same "safe to keep, sha is a real global identity" reasoning
        // `AdeApp::lsp_uri_cache`'s own root-scoped-only pruning already applies elsewhere).
        self.load_file_tree(new_root.clone(), cx);
        // `load_file_tree` above already set `self.file_tree_root = new_root` synchronously, so
        // `new_root` is the active root by the time eviction runs.
        self.evict_stale_lsp_clients(&new_root, cx);
        self.load_diff(new_root, cx);
        // The graph tab is repo-scoped, not worktree-scoped (design spec §1: "the graph is
        // repo-scoped"), but the toolbar's `HEAD` chip/upstream counts and the Worktrees scope
        // (`wt_core::graph::GraphScope::Worktrees`, driven by real worktree HEADs) are real facts
        // about whatever is genuinely current now - without this, switching while the graph tab
        // is open silently left it showing stale data (a real, adversarial-audit-found gap), and
        // the Commit panel's cached "Files changed" could even fail against the wrong repo path.
        // `load_diff` above already updated `Self::diff_root` synchronously, so `load_graph`
        // (which reads it) picks up the new root correctly.
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

    /// Selects a worktree by its real path (rather than an index into [`Self::worktrees`], which
    /// the rail's rows don't carry) - the click handler behind every worktree row in the rail
    /// (`crate::rail::render::AdeApp::render_worktree_row`).
    ///
    /// ## Cross-repo
    ///
    /// [`Self::worktrees`] only ever holds the **focused** repo's own live list, so the plain
    /// index lookup below can only ever find a worktree of the repo already showing. That used to
    /// be this method's whole body, and it was fine right up until the multi-repo rail landed: a
    /// non-focused repo's worktrees had no rendered rows to click (its group showed "not loaded
    /// yet" instead), so the "path isn't in the list" branch was genuinely only ever a stale click
    /// racing a reload. Now every added repo renders its own real, clickable worktree rows from
    /// its own [`crate::rail::repo::Repo::worktrees`], and clicking one belonging to a *different*
    /// repo silently did nothing at all - the reported "I can't switch from a worktree to another
    /// repo's worktree".
    ///
    /// So a path not in [`Self::worktrees`] is now searched for across every added repo's own
    /// worktree list, and finding it means this is a real cross-repo switch:
    /// [`Self::checkout_repo_from_rail`] does the entire repo switch (cross-repo agent
    /// persistence, [`Self::reset_repo_scoped_state`], the watchers, `Agents::activate_for_
    /// worktree`) - nothing of it is reimplemented here - and then the specific worktree is
    /// selected within it.
    ///
    /// That second step needs [`Self::worktrees`] to already contain the target, and it does not:
    /// `checkout_repo_from_rail`'s own [`Self::load_worktrees`] call is a real *background* `git
    /// worktree list --porcelain` fetch, so [`Self::worktrees`] still holds the repo just **left**
    /// when it returns. Rather than chaining onto that fetch's completion, this seeds
    /// [`Self::worktrees`] synchronously from the target repo's own already-fetched
    /// [`crate::rail::repo::Repo::worktrees`] - the exact same data, from the exact same
    /// `list_worktrees_porcelain` call, kept fresh in the background by
    /// [`Self::load_repo_worktrees`]/[`Self::start_repo_worktrees_polling`], and the very list the
    /// row that was just clicked was rendered from in the first place. Selecting against it is
    /// therefore selecting against precisely what the user saw and clicked, which a continuation
    /// racing a fresh fetch would not guarantee. The in-flight fetch is not wasted or cancelled:
    /// it lands moments later and overwrites this seed with an equally real, slightly newer list,
    /// and because the selection below is recorded *before* it does,
    /// `crate::rail::worktrees::recover_selection` re-anchors it by path - so a worktree that
    /// really did vanish between the seed and the fetch falls back to main with a real notice,
    /// exactly as it would for any other refresh, instead of leaving a dangling index.
    ///
    /// Still does nothing at all when `path` isn't found in *any* repo - a stale click racing a
    /// reload, or a worktree removed on disk since the last render - which is the same documented
    /// fallback the focused-repo-only version already had.
    pub(crate) fn select_worktree_by_path(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.worktrees.iter().position(|item| item.path == path) {
            self.select_worktree(index, window, cx);
            return;
        }

        let Some((repo_id, items)) = self.repos.iter().find_map(|repo| {
            repo.worktrees
                .iter()
                .any(|item| item.path == path)
                .then(|| (repo.id, repo.worktrees.clone()))
        }) else {
            return;
        };

        self.checkout_repo_from_rail(repo_id, window, cx);
        self.worktrees = items;
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
    additional_tree_selection: &mut HashSet<PathBuf>,
) {
    staged_files.clear();
    *open_change = None;
    expanded_dirs.clear();
    *selected_tree_path = None;
    additional_tree_selection.clear();
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
        let mut additional_tree_selection = HashSet::new();

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
            &mut additional_tree_selection,
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
        let mut additional_tree_selection = HashSet::new();

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
            &mut additional_tree_selection,
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
        let mut additional_tree_selection = HashSet::new();
        expanded_dirs.insert(PathBuf::from("/repo/worktree-a/src"));
        expanded_dirs.insert(PathBuf::from("/repo/worktree-a/tests"));

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
            &mut additional_tree_selection,
        );

        assert!(expanded_dirs.is_empty());
    }

    #[test]
    fn reset_per_worktree_ui_state_clears_selected_tree_path() {
        let mut staged_files = HashSet::new();
        let mut open_change = None;
        let mut expanded_dirs = HashSet::new();
        let mut selected_tree_path = Some(PathBuf::from("/repo/worktree-a/src/main.rs"));
        let mut additional_tree_selection = HashSet::new();
        additional_tree_selection.insert(PathBuf::from("/repo/worktree-a/src/lib.rs"));

        reset_per_worktree_ui_state(
            &mut staged_files,
            &mut open_change,
            &mut expanded_dirs,
            &mut selected_tree_path,
            &mut additional_tree_selection,
        );

        assert_eq!(selected_tree_path, None);
        assert!(additional_tree_selection.is_empty());
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

/// Real, end-to-end proof that every added repo's own worktree list loads and stays live in the
/// rail, not just the currently focused repo's - the fix for a real live-user report: the rail
/// grouped worktrees under a repo header for every added repo, but only the *focused* one ever
/// showed a real count or its rows; every other one showed an honest but permanent "not loaded
/// yet" placeholder even though nothing stopped it from being fetched too. `AdeApp::
/// load_repo_worktrees` (a one-shot fetch on `AdeApp::add_repo`/at startup) and `AdeApp::
/// start_repo_worktrees_polling` (the periodic keep-fresh sweep for every non-focused repo) are
/// what closes that gap - see both methods' own docs. Real repos, real `git` subprocesses, no
/// mocks, mirroring `load_worktrees_integration_tests`'s own established discipline just above.
#[cfg(test)]
mod multi_repo_worktree_loading_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
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
        std::fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    fn add_worktree(repo_path: &Path, branch: &str, name: &str) {
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
    }

    /// The core ask: repo B is added but never focused, yet its own real worktree list still
    /// loads in the background and lands in `AdeApp::repos` once the fetch resolves - not just
    /// the currently focused repo A's.
    #[gpui::test]
    fn a_non_focused_repos_worktrees_load_in_the_background(cx: &mut TestAppContext) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));

        // Immediately after `add_repo`, in this same synchronous step, the real background fetch
        // it just kicked off has had no chance to run yet - proving the count that appears later
        // is real data, not a race that was always going to read `1` regardless.
        app.read_with(cx, |app, _| {
            let entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_b_id)
                .expect("repo B is a known repo");
            assert!(
                !entry.worktrees_loaded,
                "repo B's real fetch hasn't resolved yet - worktrees_loaded must still be false"
            );
        });

        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let repo_a_id = app.focused_repo().expect("repo A is focused").id;
            let repo_a_entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_a_id)
                .expect("repo A is a known repo");
            assert!(
                repo_a_entry.worktrees_loaded,
                "the focused repo's own data path must remain loaded exactly as before"
            );
            assert_eq!(repo_a_entry.worktrees.len(), 1);

            let repo_b_entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_b_id)
                .expect("repo B is a known repo");
            assert!(
                repo_b_entry.worktrees_loaded,
                "repo B's real background fetch must have resolved by now"
            );
            assert_eq!(
                repo_b_entry.worktrees.len(),
                1,
                "a real git repo always has at least its own main checkout as a worktree - this \
                 must read as a real 1, never a 0 that's actually just a race with \"not loaded \
                 yet\""
            );
        });
    }

    /// A repo whose path becomes inaccessible before its first real fetch resolves must not
    /// panic, and must still land on a real, definitive answer (`worktrees_loaded: true`, an
    /// empty list) rather than spinning on "not loaded yet" forever - the identical honest
    /// "attempted, got a disappointing but real answer" contract `AdeApp::load_worktrees` already
    /// gives the focused repo's own `AdeApp::worktrees_error` case on a real fetch failure.
    #[gpui::test]
    fn a_repo_whose_path_disappears_before_its_first_load_does_not_panic(cx: &mut TestAppContext) {
        let repo_a = init_repo();
        let doomed = TempDir::new().expect("tempdir");
        let doomed_path = doomed.path().to_path_buf();
        git(&doomed_path, &["init", "-b", "main"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let doomed_id = app.update(cx, |app, cx| app.add_repo(doomed_path.clone(), cx));

        // Deleted before the real background fetch this same `add_repo` call kicked off ever
        // gets a chance to run.
        std::fs::remove_dir_all(&doomed_path).expect("remove doomed repo directory");

        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let entry = app
                .repos
                .iter()
                .find(|repo| repo.id == doomed_id)
                .expect("still a known repo - a failed fetch must never remove it");
            assert!(
                entry.worktrees_loaded,
                "a failed fetch is still a real, definitive answer, not a forever-pending one"
            );
            assert!(
                entry.worktrees.is_empty(),
                "an inaccessible path must report a real empty list, never stale/fabricated data"
            );
        });
    }

    /// The cadence contract: a non-focused repo's worktree list must not be refetched before it
    /// has genuinely gone stale (`REPO_WORKTREES_POLL_INTERVAL`), and must be refetched once that
    /// interval elapses - proven with a real `git worktree add` landing between the two checks,
    /// so "unchanged" and "changed" both mean something real rather than an unobservable no-op.
    #[gpui::test]
    fn a_non_focused_repos_worktrees_are_not_refetched_before_the_poll_interval_elapses(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_b_id)
                .expect("repo B is a known repo");
            assert_eq!(
                entry.worktrees.len(),
                1,
                "sanity check: repo B starts out with just its own main checkout"
            );
        });

        // A real new worktree lands in repo B's real git metadata...
        add_worktree(repo_b.path(), "feature", "added-wt");

        // ...but less than a full poll interval has elapsed since repo B's last real fetch, so
        // the periodic sweep must not have refetched it yet.
        cx.background_executor
            .advance_clock(REPO_WORKTREES_POLL_INTERVAL - Duration::from_secs(1));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_b_id)
                .expect("repo B is a known repo");
            assert_eq!(
                entry.worktrees.len(),
                1,
                "repo B's data hasn't gone stale yet (less than REPO_WORKTREES_POLL_INTERVAL has \
                 elapsed since its last real fetch) - the periodic sweep must not have refetched \
                 it, so the real new worktree must not be visible yet"
            );
        });

        // Now a full interval has elapsed since repo B's last real fetch - the periodic sweep
        // must pick up the real change.
        cx.background_executor
            .advance_clock(Duration::from_secs(1) + Duration::from_millis(500));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let entry = app
                .repos
                .iter()
                .find(|repo| repo.id == repo_b_id)
                .expect("repo B is a known repo");
            assert_eq!(
                entry.worktrees.len(),
                2,
                "once the poll interval has elapsed, the periodic sweep must refetch repo B and \
                 pick up the real new worktree"
            );
        });
    }

    /// The real concurrency cap end to end, with more due repos than
    /// `REPO_WORKTREES_FETCH_CONCURRENCY`: one real sweep still drains every batch to completion
    /// - not just the first cap-sized chunk - proven with a real change landing in *every* extra
    /// repo, which only the periodic sweep (not each repo's own already-completed, one-shot
    /// `add_repo`-time fetch) can ever observe. `crate::rail::repo::batch_repos_for_refresh`'s own
    /// unit tests prove the exact chunk sizes the cap produces; this proves the sweep built on
    /// top of it actually processes every one of those chunks, not just the first.
    #[gpui::test]
    fn the_periodic_sweep_refreshes_every_repo_even_with_more_than_the_concurrency_cap(
        cx: &mut TestAppContext,
    ) {
        let repo_focused = init_repo();
        let extra_count = REPO_WORKTREES_FETCH_CONCURRENCY * 2 + 1;
        let extra_repos: Vec<TempDir> = (0..extra_count).map(|_| init_repo()).collect();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_focused.path().to_path_buf());
        cx.run_until_parked();

        let extra_ids: Vec<RepoId> = extra_repos
            .iter()
            .map(|repo| app.update(cx, |app, cx| app.add_repo(repo.path().to_path_buf(), cx)))
            .collect();
        cx.run_until_parked();

        // Sanity check: every repo's own one-shot `add_repo`-time fetch (not the periodic sweep
        // under test below) has already given it a real baseline of exactly one worktree.
        app.read_with(cx, |app, _| {
            for id in &extra_ids {
                let entry = app
                    .repos
                    .iter()
                    .find(|repo| repo.id == *id)
                    .expect("repo is known");
                assert_eq!(entry.worktrees.len(), 1);
            }
        });

        // A real new worktree lands in *every* extra repo - only a real periodic sweep tick can
        // ever observe this, since each repo's own one-shot add-time fetch already ran above.
        for repo in &extra_repos {
            add_worktree(repo.path(), "feature", "added-wt");
        }

        cx.background_executor
            .advance_clock(REPO_WORKTREES_POLL_INTERVAL + Duration::from_millis(500));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            for id in &extra_ids {
                let entry = app
                    .repos
                    .iter()
                    .find(|repo| repo.id == *id)
                    .expect("repo is known");
                assert_eq!(
                    entry.worktrees.len(),
                    2,
                    "every non-focused repo must be refreshed within one real sweep, not just \
                     the first REPO_WORKTREES_FETCH_CONCURRENCY of them - the batching loop must \
                     drain every batch, not stop after the first"
                );
            }
        });
    }
}

/// GitHub issue #13's own real, end-to-end wiring proof: opening the app arms a real file-tree
/// watcher, and switching worktrees re-arms it onto the new root rather than leaking the old
/// one. The watcher's own OS-level event delivery/`.git/`-filtering already has real, non-`gpui`
/// coverage in `crate::sidebar::file_tree_watch`'s own test module - see
/// `load_worktrees_integration_tests`'s identical own docs for why the debounced polling loop
/// itself isn't driven end to end through a `gpui` test's deterministic clock.
#[cfg(test)]
mod file_tree_watch_integration_tests {
    use crate::root::AdeApp;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::fs;
    use std::path::{Path, PathBuf};
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
            "git {:?} failed in {:?}",
            args,
            dir
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

    /// `Self::start_file_tree_watch` only ever arms a real watcher for a real, `Some` settings
    /// path (see that method's own docs on why) - `root::focus::palette_focus_tests::
    /// open_test_app`'s own `None` path would make every assertion in this module vacuous, so
    /// these tests need their own real-settings-path open helper instead.
    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = config_dir.path().join("settings.toml");
        // Leaked deliberately: `config_dir` must outlive the returned `AdeApp`, and this helper
        // has no later point to drop it at - the OS reclaims it at process exit either way, the
        // same real tradeoff `tempfile::TempDir::into_path` documents for this exact situation.
        std::mem::forget(config_dir);
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                false,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    fn opening_the_app_arms_a_real_file_tree_watcher(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = open_test_app_with_real_settings_path(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app._file_tree_watcher.is_some()),
            "startup must arm a real watcher on the initial file tree root"
        );
    }

    #[gpui::test]
    fn selecting_a_different_worktree_re_arms_the_watcher_on_the_new_root(cx: &mut TestAppContext) {
        let repo = init_repo();
        let container = TempDir::new().expect("tempdir");
        let linked_path = container.path().join("added-wt");
        drop(container);
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                linked_path.to_str().expect("utf8 path"),
            ],
        );

        let (app, cx) = open_test_app_with_real_settings_path(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| app.load_worktrees(cx));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.worktrees.len()),
            2,
            "premise: the linked worktree above must really be there to select"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_root.clone()),
            linked_path,
            "premise: selecting the linked worktree must really move the file tree root"
        );
        assert!(
            app.read_with(cx, |app, _| app._file_tree_watcher.is_some()),
            "switching worktrees must re-arm a real watcher on the newly selected root, not \
             leave the previous worktree's watcher (or none at all) in place"
        );
    }
}
