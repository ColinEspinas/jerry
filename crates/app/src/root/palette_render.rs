use super::*;
use crate::root::settings_widgets::ChoiceOption;
use crate::root::sidebar_render::RightSidebarView;
use crate::root::widgets::{render_hint_pair, render_hint_row, render_keycap_row, KeycapSize};

/// The real, live secondary line for one of the three `Window controls: …` palette commands
/// (`palette::PaletteCommand::WindowControlsSystem`/`WindowControlsMacos`/
/// `WindowControlsWindowsLinux`) - names which real chrome that option currently resolves to
/// (`⌘`-style dots vs. caption buttons), and flags the one that's already active, exactly like
/// `Self::build_palette_groups`'s other live-state secondaries (e.g.
/// `PaletteCommand::ToggleRailGrouping`'s "switch to {label}").
///
/// These three commands change [`WindowControlsStyle`] - a rendering-only preview of another
/// platform's title bar and keycap glyphs, never a rebinding of this session's real, globally-
/// bound shortcuts (fixed at compile time by the real OS - see `crate::keymap`'s own "This is a
/// cosmetic preview, not a rebinding" docs for the full reasoning). Picking "macOS" here on a
/// real Linux/Windows box makes every keycap in the app render `⌘`-style glyphs while the key
/// that actually works is still, and can only ever be, Ctrl - a deliberate, documented tradeoff,
/// not an oversight.
fn window_controls_secondary(current: WindowControlsStyle, option: WindowControlsStyle) -> String {
    let resolved = if option.is_macos() {
        "macOS dots"
    } else {
        "Windows/Linux caption buttons"
    };
    if current == option {
        format!("{resolved} - already active")
    } else {
        format!("switch to {resolved}")
    }
}

impl AdeApp {
    pub(super) fn handle_toggle_palette_action(
        &mut self,
        _action: &TogglePalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_open {
            self.close_palette(window, cx);
        } else {
            self.open_palette(window, cx);
        }
    }

    /// Builds the palette's real, live candidate lists from current app state and hands them to
    /// `crate::palette::build_groups` - the one real bridge between this app's live
    /// `crate::sessions::Sessions`/file tree/diff state and that module's pure matching/ranking
    /// logic. Called both by rendering ([`Self::render_palette`]) and by keyboard handling
    /// ([`Self::move_palette_selection`]/[`Self::run_selected_palette_entry`]), so what's drawn
    /// and what `⏎`/`↑`/`↓` act on can never disagree about the current result list - mirrors
    /// [`Self::build_session_rows`]'s own "built fresh every call, no separately cached copy
    /// that could drift" shape for the *result*.
    ///
    /// The `sessions`/`commands` candidate inputs are themselves built fresh here every call -
    /// they're cheap (bounded by open-tab count plus a fixed 10 commands) and a session's status
    /// dot is genuinely live per-render data with no stable point to cache against (see
    /// [`Self::palette_file_candidates`]'s docs). The `files` candidate input is the one
    /// genuinely expensive part (up to `file_tree::MAX_ENTRIES` = 5000 entries, each needing a
    /// `PathBuf` clone plus two `String` allocations) - it is *not* rebuilt here; this method
    /// just reads [`Self::palette_file_candidates`], which [`Self::rebuild_palette_file_candidates`]
    /// keeps current at its own two real mutation points.
    pub(super) fn build_palette_groups(&self, cx: &App) -> Vec<palette::PaletteGroup> {
        let sessions: Vec<palette::SessionCandidate> = self
            .sessions
            .iter()
            .map(|session| {
                let status = self.session_status(session, cx);
                let branch = self
                    .worktrees
                    .iter()
                    .find(|item| item.path == session.cwd)
                    .and_then(|item| item.branch.clone());
                let title = match session.cwd.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => session.cwd.display().to_string(),
                };
                palette::SessionCandidate {
                    id: session.id,
                    kind: session.kind,
                    title,
                    branch,
                    status,
                }
            })
            .collect();

        let active_cwd = self.active_session_cwd();
        let next_sidebar_view = match self.right_sidebar_view {
            RightSidebarView::Files => "Changes",
            RightSidebarView::Changes => "Files",
        };
        let commands = vec![
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewShell,
                secondary: format!("spawn a shell in {}", active_cwd.display()),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewClaudeSession,
                secondary: format!("spawn claude in {}", active_cwd.display()),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewCodexSession,
                secondary: format!("spawn codex in {}", active_cwd.display()),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::ToggleFilesChanges,
                secondary: format!("switch the right panel to {next_sidebar_view}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::ToggleRailGrouping,
                secondary: format!("switch to {}", self.rail_mode.toggled().label()),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::PruneWorktrees,
                secondary: format!(
                    "{} prunable worktree(s)",
                    self.prunable_worktree_paths().len()
                ),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::OpenSettings,
                secondary: "agents, worktrees, and the rest of the settings surface".to_string(),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::WindowControlsSystem,
                secondary: window_controls_secondary(
                    self.window_controls_style(),
                    WindowControlsStyle::System,
                ),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::WindowControlsMacos,
                secondary: window_controls_secondary(
                    self.window_controls_style(),
                    WindowControlsStyle::MacosStyle,
                ),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::WindowControlsWindowsLinux,
                secondary: window_controls_secondary(
                    self.window_controls_style(),
                    WindowControlsStyle::WindowsLinuxStyle,
                ),
            },
        ];

        palette::build_groups(
            self.palette_scope,
            &self.palette_query,
            &sessions,
            &commands,
            &self.palette_file_candidates,
        )
    }

    /// Rebuilds [`Self::palette_file_candidates`] from the current real [`Self::file_tree`]/
    /// [`Self::current_diff`] - called from the two real points either input can change
    /// ([`Self::load_file_tree`]'s and [`Self::load_diff`]'s completion handlers), never from
    /// [`Self::build_palette_groups`] itself. See [`Self::palette_file_candidates`]'s docs for
    /// the real per-render cost this avoids.
    pub(super) fn rebuild_palette_file_candidates(&mut self) {
        // Built once, not once per file - the same "no O(files * diff_files) rescan per row"
        // reasoning `Self::tree_change_marks` documents at its own use site.
        let diff_by_relative_path: HashMap<&std::path::Path, &DiffFile> = self
            .current_diff()
            .map(|diff| {
                diff.files
                    .iter()
                    .map(|file| (file.path.as_path(), file))
                    .collect()
            })
            .unwrap_or_default();

        self.palette_file_candidates = self
            .file_tree
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| {
                let relative = entry
                    .path
                    .strip_prefix(&self.file_tree_root)
                    .unwrap_or(entry.path.as_path());
                let (add, del, changed) = match diff_by_relative_path.get(relative) {
                    Some(file) => {
                        let (add, del) = changes::diff_file_stats(file);
                        let changed = match file.status {
                            FileChangeStatus::Added => Some(palette::FileChangeKind::Added),
                            FileChangeStatus::Deleted => Some(palette::FileChangeKind::Deleted),
                            FileChangeStatus::Modified | FileChangeStatus::Renamed => None,
                        };
                        (add, del, changed)
                    }
                    None => (0, 0, None),
                };
                let (dir, name) = changes::split_dir_name(relative);
                palette::FileCandidate {
                    path: entry.path.clone(),
                    name,
                    dir,
                    add,
                    del,
                    changed,
                }
            })
            .collect();
    }

    /// Moves the palette's real keyboard selection by `delta` rows (`↑`/`↓`), clamped to the
    /// current real result count - never wraps, and safely no-ops against zero results.
    pub(super) fn move_palette_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let groups = self.build_palette_groups(cx);
        let total = palette::flatten(&groups).len();
        if total == 0 {
            self.palette_selected = 0;
            return;
        }
        let next = (self.palette_selected as i32 + delta).clamp(0, total as i32 - 1);
        self.palette_selected = next as usize;
        cx.notify();
    }

    /// Runs whichever real command a [`palette::PaletteCommand`] names - dispatches to the
    /// exact same `AdeApp` method its existing, already-real UI affordance calls (see
    /// [`palette::PaletteCommand`]'s own per-variant docs for which one). Never a second,
    /// independent implementation of the action.
    ///
    /// Takes `window` (unlike every other palette-adjacent method that only needed `cx`) purely
    /// for [`palette::PaletteCommand::OpenSettings`]: [`Self::open_settings`] needs it to
    /// capture/move real keyboard focus, the same way [`Self::open_palette`] itself does.
    pub(super) fn execute_palette_command(
        &mut self,
        command: palette::PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            palette::PaletteCommand::NewShell => self.new_session(SessionKind::Shell, window, cx),
            palette::PaletteCommand::NewClaudeSession => {
                self.new_session(SessionKind::Claude, window, cx)
            }
            palette::PaletteCommand::NewCodexSession => {
                self.new_session(SessionKind::Codex, window, cx)
            }
            palette::PaletteCommand::ToggleFilesChanges => {
                // `Self::new_session`/`Self::toggle_rail_mode` (the other non-prune branches
                // here) already clear `prune_confirm_armed` themselves; this is the one
                // non-prune command with no other reason to touch it, so it's cleared
                // explicitly - see `Self::open_palette`'s docs for why any "did something
                // else in the palette" gesture must disarm a pending confirmation.
                self.prune_confirm_armed = false;
                let next = match self.right_sidebar_view {
                    RightSidebarView::Files => RightSidebarView::Changes,
                    RightSidebarView::Changes => RightSidebarView::Files,
                };
                self.set_right_sidebar_view(next, cx);
            }
            palette::PaletteCommand::ToggleRailGrouping => self.toggle_rail_mode(cx),
            palette::PaletteCommand::PruneWorktrees => self.request_prune(cx),
            palette::PaletteCommand::OpenSettings => self.open_settings(window, cx),
            // See `crate::keymap`'s module docs and `palette::PaletteCommand::
            // WindowControlsSystem`'s own docs for why these three still exist here even now
            // that the General settings page has its own real `Window controls` row: both
            // real entry points call the exact same `Self::set_window_controls_style`, which
            // mutates and persists `Self::settings.window.controls` for real (R3) - never two
            // independent copies.
            palette::PaletteCommand::WindowControlsSystem => {
                self.set_window_controls_style(WindowControlsStyle::System, cx);
            }
            palette::PaletteCommand::WindowControlsMacos => {
                self.set_window_controls_style(WindowControlsStyle::MacosStyle, cx);
            }
            palette::PaletteCommand::WindowControlsWindowsLinux => {
                self.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
            }
        }
    }

    /// Runs a real palette file result - `design_handoff_jerry_ade/README.md` leaves the exact
    /// choice between "open its diff" and "select it in the file tree" to this phase's own
    /// judgment call, documented here: a file that is a real changed file in the currently
    /// loaded diff opens its real diff in the centre, reusing the Changes list's own
    /// [`Self::open_change_diff`] verbatim (the same real transition a Changes-row click
    /// performs); a file with no diff to open (nothing to show in the centre) instead reveals it
    /// in the real Files tree - switches Zone 3 to `Files`, expands every real ancestor
    /// directory so the row is actually visible, and highlights it via
    /// [`Self::selected_tree_path`] (a real Files-tree row highlight - `design_handoff_jerry_ade/
    /// README.md`'s "Selected row bg `#1a1e21`" spec, previously unwired since Phase D never
    /// gave individual file rows a click handler of their own).
    pub(super) fn open_palette_file_result(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A palette file result runs via the same dispatch as a command/session result (see
        // `Self::run_selected_palette_entry`) but had no other reason to disarm a pending rail
        // prune confirmation the way `Self::select_session`/`Self::new_session` already do -
        // see `Self::open_palette`'s docs for why any palette selection must count as a fresh
        // gesture.
        self.prune_confirm_armed = false;
        let relative = path
            .strip_prefix(&self.file_tree_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        let has_diff = self
            .current_diff()
            .is_some_and(|diff| diff.files.iter().any(|file| file.path == relative));

        if has_diff {
            self.open_change_diff(relative, window, cx);
        } else {
            self.right_sidebar_view = RightSidebarView::Files;
            for ancestor in path.ancestors() {
                self.collapsed_dirs.remove(ancestor);
            }
            self.selected_tree_path = Some(path);
            cx.notify();
        }
    }

    /// Runs the currently highlighted real palette result (`⏎`) - looks it up fresh via
    /// [`Self::build_palette_groups`] (see that method's docs on why this is never a separately
    /// cached copy) and dispatches by its real [`palette::EntryTarget`], then closes the
    /// palette.
    pub(super) fn run_selected_palette_entry(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups = self.build_palette_groups(cx);
        let target = palette::flatten(&groups)
            .get(self.palette_selected)
            .map(|entry| entry.target.clone());
        if let Some(target) = target {
            match target {
                palette::EntryTarget::Command(command) => {
                    self.execute_palette_command(command, window, cx)
                }
                palette::EntryTarget::Session(id) => self.select_session(id, window, cx),
                palette::EntryTarget::File(path) => self.open_palette_file_result(path, window, cx),
            }
        }
        self.close_palette(window, cx);
    }

    /// The palette's real, deliberately minimal hand-rolled text field key handler - the same
    /// append/backspace shape as [`Self::handle_filter_key_down`], plus the palette's own real
    /// `Esc`/`⏎`/`↑`/`↓`/`⇥` affordances (`design_handoff_jerry_ade/README.md`'s palette
    /// footer: "↑↓ move · ⏎ run · ⇥ next scope · esc close"). Also implements the real "type
    /// the scope prefix" gesture (`crate::palette::typed_scope_prefix`) for the very first
    /// character typed into an empty query.
    pub(super) fn handle_palette_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.close_palette(window, cx);
                cx.stop_propagation();
            }
            "backspace" => {
                self.palette_query.pop();
                self.palette_selected = 0;
                cx.notify();
                cx.stop_propagation();
            }
            "enter" => {
                self.run_selected_palette_entry(window, cx);
                cx.stop_propagation();
            }
            "up" => {
                self.move_palette_selection(-1, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_palette_selection(1, cx);
                cx.stop_propagation();
            }
            "tab" => {
                self.palette_scope = self.palette_scope.cycle();
                self.palette_selected = 0;
                cx.notify();
                cx.stop_propagation();
            }
            _ => {
                let Some(text) = keystroke.key_char.as_deref() else {
                    return;
                };
                if text.is_empty() {
                    return;
                }
                if self.palette_query.is_empty() {
                    if let Some(first_char) = text.chars().next() {
                        if let Some(scope) = palette::typed_scope_prefix(first_char) {
                            self.palette_scope = scope;
                            self.palette_selected = 0;
                            cx.notify();
                            cx.stop_propagation();
                            return;
                        }
                    }
                }
                self.palette_query.push_str(text);
                self.palette_selected = 0;
                cx.notify();
                cx.stop_propagation();
            }
        }
    }

    /// The command palette overlay (`design_handoff_jerry_ade/README.md`'s "Command palette
    /// (⌘K)" section) - a real, absolutely-positioned scrim + panel painted as the last child
    /// of [`Render::render`]'s root div (so it paints on top of every other zone; verified
    /// real GPUI overlay pattern - see the module-level note on `crate::root`'s use of it below
    /// for why `deferred`/`anchored` weren't needed here). `top(theme::band::TITLE_BAR)` plus
    /// `bottom(0)` against the root div's own full-window box (`Position::Relative` is GPUI's
    /// own layout default - verified at `vendor/zed/crates/gpui/src/style.rs`'s `Style::
    /// default`, so the root div is already a valid containing block for this `.absolute()`
    /// child with no extra `.relative()` needed) means the scrim covers the body *and* the
    /// status bar - `Jerry.dc.html`'s own scrim div, `top:38px;bottom:0` against its full
    /// 1440×928 window container, does exactly the same.
    pub(super) fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = self.build_palette_groups(cx);
        let total: usize = groups.iter().map(|group| group.entries.len()).sum();
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PALETTE;

        div()
            .id("palette-scrim")
            .absolute()
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(theme::surface::SCRIM.opacity(0.62))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.0))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.close_palette(window, cx);
            }))
            .child(
                div()
                    .id("palette-panel")
                    .track_focus(&self.palette_focus_handle)
                    .on_key_down(cx.listener(Self::handle_palette_key_down))
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        // Only stops the click from bubbling to the scrim's own `on_click`
                        // (which would otherwise close the palette on every click inside it) -
                        // the same real `cx.stop_propagation()`-in-an-otherwise-no-op-handler
                        // pattern `Self::render_review_checkbox` already uses to keep its own
                        // click from also opening that row's diff.
                        cx.stop_propagation();
                    }))
                    .flex()
                    .flex_col()
                    .w(theme::zone::PALETTE_WIDTH)
                    .max_h(px(480.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::PANEL)
                    .overflow_hidden()
                    .shadow(vec![BoxShadow::new(
                        shadow_x,
                        shadow_y,
                        gpui::black().opacity(0.55),
                    )
                    .blur_radius(shadow_blur)])
                    .child(self.render_palette_input_row(cx))
                    .child(self.render_palette_groups(&groups, cx))
                    .child(self.render_palette_footer(total)),
            )
    }

    /// Input row 44 (`design_handoff_jerry_ade/README.md`): the real scope-prefix glyph, the
    /// real typed query (or its placeholder), a caret, and the real clickable segmented scope
    /// control.
    pub(super) fn render_palette_input_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = !self.palette_query.is_empty();

        div()
            .id("palette-input-row")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.0))
            .h(theme::band::PALETTE_INPUT)
            .px(px(12.0))
            .border_b_1()
            .border_color(theme::border::CARD)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(12.0))
                    .text_color(theme::palette::PREFIX)
                    .child(self.palette_scope.prefix_glyph()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(self.ui_text_size(13.0))
                            .text_color(if has_query {
                                theme::text::SELECTED
                            } else {
                                theme::text::GHOST
                            })
                            .child(if has_query {
                                self.palette_query.clone()
                            } else {
                                "Type a command, file or session\u{2026}".to_string()
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .ml(px(1.0))
                            .w(px(1.5))
                            .h(px(16.0))
                            .bg(theme::term::CURSOR),
                    ),
            )
            .child(self.render_palette_scope_control(cx))
    }

    /// The `All ⇥ / Commands › / Files @` segmented scope control - reachable by clicking here
    /// or by typing a scope's prefix character (`crate::palette::typed_scope_prefix`, handled
    /// in [`Self::handle_palette_key_down`]).
    pub(super) fn render_palette_scope_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_choice_control(
            "palette-scope",
            &[
                ChoiceOption::with_hint(
                    palette::PaletteScope::All.label(),
                    palette::PaletteScope::All.segment_key(),
                ),
                ChoiceOption::with_hint(
                    palette::PaletteScope::Commands.label(),
                    palette::PaletteScope::Commands.segment_key(),
                ),
                ChoiceOption::with_hint(
                    palette::PaletteScope::Files.label(),
                    palette::PaletteScope::Files.segment_key(),
                ),
            ],
            self.palette_scope.label().to_string(),
            cx,
            |this, index, cx| {
                // Structural, not a label re-match: index 0 is `All`, index 1 is `Commands`,
                // index 2 is `Files`, per the `options` array literal right above - see
                // `Self::render_choice_control`'s own docs for why dispatch is index-based.
                this.palette_scope = match index {
                    1 => palette::PaletteScope::Commands,
                    2 => palette::PaletteScope::Files,
                    _ => palette::PaletteScope::All,
                };
                this.palette_selected = 0;
                cx.notify();
            },
        )
    }

    /// The real, grouped, scrollable result list - `crate::palette::build_groups`'s output,
    /// rendered top to bottom in the same order [`Self::run_selected_palette_entry`] flattens
    /// it in, so the visual row a user sees at index N is always the row `⏎` would actually run
    /// at index N.
    pub(super) fn render_palette_groups(
        &self,
        groups: &[palette::PaletteGroup],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if groups.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .py(px(24.0))
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(10.5))
                .text_color(theme::text::FAINT)
                .child("no results")
                .into_any_element();
        }

        let mut container = div()
            .id("palette-groups")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .py(px(4.0));

        let mut flat_index = 0usize;
        for group in groups {
            container = container.child(self.render_palette_group(group, &mut flat_index, cx));
        }
        container.into_any_element()
    }

    pub(super) fn render_palette_group(
        &self,
        group: &palette::PaletteGroup,
        flat_index: &mut usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut el = div()
            .id(format!("palette-group-{}", group.label))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(12.0))
                    .pt(px(7.0))
                    .pb(px(4.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::palette::GROUP_HEADER)
                            .child(group.label.to_uppercase()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOSTER)
                            .child(group.entries.len().to_string()),
                    ),
            );

        for entry in &group.entries {
            let index = *flat_index;
            *flat_index += 1;
            el = el.child(self.render_palette_row(entry, index, cx));
        }
        el
    }

    /// One real result row: a real kind chip (command/agent-badge/language, per
    /// [`palette::EntryTarget`]), the real matched-substring label, real secondary text, an
    /// optional real status/change dot, and an optional real shortcut keycap - clicking (or
    /// hitting `⏎` while it's the selected row) runs it via
    /// [`Self::run_selected_palette_entry`]'s same dispatch.
    pub(super) fn render_palette_row(
        &self,
        entry: &palette::PaletteEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = index == self.palette_selected;
        let label_fg = if selected {
            theme::palette::LABEL_SELECTED
        } else {
            theme::text::STRONG
        };
        let mono = matches!(entry.target, palette::EntryTarget::File(_));

        let chip = match &entry.target {
            palette::EntryTarget::Command(_) => render_palette_command_chip().into_any_element(),
            palette::EntryTarget::Session(_) => {
                let kind = entry.session_kind.unwrap_or(SessionKind::Shell);
                render_palette_session_chip(kind).into_any_element()
            }
            palette::EntryTarget::File(path) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                render_palette_file_chip(file_tree::lang_chip_for_name(&name)).into_any_element()
            }
        };

        let mut row = div()
            .id(("palette-row", index as u64))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(9.0))
            .h(theme::band::PALETTE_ROW)
            .pl(px(10.0))
            .pr(px(12.0))
            .border_l(px(2.0))
            .border_color(if selected {
                theme::border::SELECTED_EDGE
            } else {
                work_surface::TRANSPARENT
            })
            .when(selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::palette::ROW_HOVER))
            })
            .child(chip)
            .child(render_palette_label(&entry.label, mono, label_fg))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::FAINTER)
                    .child(entry.secondary.clone()),
            );

        if let Some(status) = entry.status {
            row = row.child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded(px(2.5))
                    .bg(status.color()),
            );
        }
        if let Some(change) = entry.file_change {
            let color = match change {
                palette::FileChangeKind::Added => theme::diff::STAT_ADD,
                palette::FileChangeKind::Deleted => theme::diff::STAT_DEL,
            };
            row = row.child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded(px(2.5))
                    .bg(color),
            );
        }
        if let Some(spec) = entry.shortcut {
            row = row.child(render_keycap_row(
                &keymap::resolve_combo(spec, self.window_controls_style().is_macos()),
                KeycapSize::Standard,
            ));
        }

        row.on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
            this.palette_selected = index;
            this.run_selected_palette_entry(window, cx);
        }))
    }

    /// Footer 29 (`design_handoff_jerry_ade/README.md`: "↑↓ move · ⏎ run · ⇥ next scope · esc
    /// close, plus the result count") - `total` is exactly how many rows are actually rendered
    /// (post [`palette::MAX_ENTRIES_PER_GROUP`]-style capping inside `crate::palette::
    /// build_groups`), so this count can never overstate what's really on screen.
    ///
    /// The hint list used to be one bare-text string with `⏎`/`⇥` baked in as literal glyphs -
    /// exactly what `design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29 entry (change 2) names
    /// the palette footer as one of the real, explicitly converted call sites for. It's now four
    /// real `[keycap] label` pairs (`crate::root::widgets::render_hint_pair`/`render_hint_row`),
    /// each keycap resolved through `crate::keymap::resolve_combo` at the hint size - `↑↓` has
    /// no real modifier/key token behind it (not one of the eight `mod`/`alt`/.../`bksp` spec
    /// names), so it passes through `resolve_combo` unchanged, the same real "unrecognized token"
    /// path a bare letter like `N` takes.
    pub(super) fn render_palette_footer(&self, total: usize) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        let hints = [
            ("\u{2191}\u{2193}", "move"),
            ("enter", "run"),
            ("tab", "next scope"),
            ("esc", "close"),
        ]
        .into_iter()
        .map(|(spec, label)| {
            render_hint_pair(&keymap::resolve_combo(spec, macos), label).into_any_element()
        });

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(29.0))
            .px(px(12.0))
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::CARD)
            .child(div().flex_1().child(render_hint_row(hints)))
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::HINT)
                    .child(format!(
                        "{total} result{}",
                        if total == 1 { "" } else { "s" }
                    )),
            )
    }
}

/// The palette row's real 15×15 command chip (`design_handoff_jerry_ade/README.md`: "commands ›
/// in `#7f9ad4` on `#1d2532`") - every command result gets the same generic `›` chip, since
/// (unlike sessions/files) a command has no per-instance colour of its own to inherit.
pub(super) fn render_palette_command_chip() -> impl IntoElement {
    let (fg, bg) = theme::palette::COMMAND_CHIP;
    div()
        .flex_none()
        .w(px(15.0))
        .h(px(15.0))
        .rounded(theme::radius::CHIP)
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .font(font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(8.0))
        .text_color(fg)
        .child("\u{203A}")
}

/// The palette row's real 15×15 session chip - the exact same agent badge/tint
/// `crate::work_surface::agent_tint`/`agent_initial` already gives the rail's own session rows,
/// reused verbatim here (`design_handoff_jerry_ade/README.md`: "sessions the agent badge - so
/// the palette inherits the rail's colour coding"), never a second, independently-drifting
/// colour mapping.
pub(super) fn render_palette_session_chip(kind: SessionKind) -> impl IntoElement {
    let (fg, bg) = work_surface::agent_tint(kind);
    div()
        .flex_none()
        .w(px(15.0))
        .h(px(15.0))
        .rounded(theme::radius::CHIP)
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .font(font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(8.5))
        .text_color(fg)
        .child(work_surface::agent_initial(kind))
}

/// The palette row's real 15×15 file chip - the exact same language chip
/// `crate::file_tree::lang_chip_for_name` already gives the Files tree (`design_handoff_jerry_
/// ade/README.md`: "files the language chip"), just at the palette's own 15×15 size rather than
/// the tree row's 13×13 (see [`render_lang_chip`]).
pub(super) fn render_palette_file_chip(chip: LangChip) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(15.0))
        .h(px(15.0))
        .rounded(theme::radius::CHIP)
        .bg(chip.bg)
        .flex()
        .items_center()
        .justify_center()
        .font(font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(7.0))
        .text_color(chip.fg)
        .child(chip.label)
}

/// A result row's real matched-substring label (`design_handoff_jerry_ade/README.md`: "the
/// matched substring in `#8fbde6`") - three adjacent spans (`pre`/`mid`/`post`), the middle one
/// tinted, matching `Jerry.dc.html`'s own row template exactly. `#8fbde6` needs no separate
/// token here: it's the exact same value already ported as `theme::term::PROMPT` (the same
/// documented "reuse when the hex is genuinely identical" precedent
/// `theme::button::GREEN_KEYCAP_FG`'s own docs describe for the blue keycap glyph colour).
/// `mono` selects between the design's two label fonts (mono for a file result, sans for a
/// command/session result).
pub(super) fn render_palette_label(
    matched: &palette::MatchedText,
    mono: bool,
    fg: gpui::Rgba,
) -> impl IntoElement {
    let family = if mono {
        theme::font::MONO
    } else {
        theme::font::SANS
    };
    let size = if mono { px(11.5) } else { px(12.0) };

    div()
        .flex_none()
        .max_w(px(340.0))
        .overflow_hidden()
        .flex()
        .font(font(family))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(size)
        .child(div().text_color(fg).child(matched.pre.clone()))
        .child(
            div()
                .text_color(theme::term::PROMPT)
                .child(matched.mid.clone()),
        )
        .child(div().text_color(fg).child(matched.post.clone()))
}
