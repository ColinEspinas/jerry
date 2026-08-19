use super::*;
use crate::root::plural;
use crate::root::scrollbar;
use crate::root::widgets::{
    render_hint_pair, render_hint_row, render_keycap_row, KeycapSize, SimpleInput, SimpleInputCaret,
};
use crate::settings::widgets::ChoiceOption;
use crate::sidebar::render::{next_right_sidebar_view, RightSidebarView};

/// Live secondary line for one of the three `Window controls: …` palette commands - names which
/// chrome that option resolves to and flags the one that's already active, like
/// `Self::build_palette_groups`'s other live-state secondaries.
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

/// One changed file's contribution to a [`palette::FileCandidate`] - what a diff overlays onto a
/// row that otherwise comes purely from the file tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileDiffMark {
    pub add: u32,
    pub del: u32,
    pub changed: Option<palette::FileChangeKind>,
}

/// Every changed file's [`FileDiffMark`], keyed by the repo-relative path `wt_core::diff` reports
/// (see `crate::sidebar::changes::split_dir_name`'s docs on that shape).
pub(crate) type FileDiffMarks = HashMap<PathBuf, FileDiffMark>;

/// Snapshots the currently loaded diff as [`FileDiffMarks`].
pub(crate) fn file_diff_marks(diff: Option<&wt_core::diff::WorktreeDiff>) -> FileDiffMarks {
    let Some(diff) = diff else {
        return FileDiffMarks::default();
    };
    diff.files
        .iter()
        .map(|file: &DiffFile| {
            let (add, del) = changes::diff_file_stats(file);
            let changed = match file.status {
                FileChangeStatus::Added => Some(palette::FileChangeKind::Added),
                FileChangeStatus::Deleted => Some(palette::FileChangeKind::Deleted),
                FileChangeStatus::Modified | FileChangeStatus::Renamed => None,
            };
            (file.path.clone(), FileDiffMark { add, del, changed })
        })
        .collect()
}

/// Builds the palette's file-candidate list: one [`palette::FileCandidate`] per non-directory
/// entry in an already-walked file tree, with `marks` overlaid where the file has a diff.
pub(crate) fn build_file_candidates(
    entries: &[file_tree::FileTreeEntry],
    root: &std::path::Path,
    marks: &FileDiffMarks,
) -> Vec<palette::FileCandidate> {
    entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| {
            let relative = entry
                .path
                .strip_prefix(root)
                .unwrap_or(entry.path.as_path());
            let mark = marks.get(relative).copied();
            let (dir, name) = changes::split_dir_name(relative);
            palette::FileCandidate {
                path: entry.path.clone(),
                name,
                dir,
                add: mark.map(|mark| mark.add).unwrap_or(0),
                del: mark.map(|mark| mark.del).unwrap_or(0),
                changed: mark.and_then(|mark| mark.changed),
            }
        })
        .collect()
}

impl AdeApp {
    pub(crate) fn handle_toggle_palette_action(
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

    /// Builds the palette's live candidate lists from current app state and hands them to
    /// `crate::palette::state::build_groups`. Called both by rendering ([`Self::render_palette`]) and
    /// by keyboard handling ([`Self::move_palette_selection`]/[`Self::run_selected_palette_entry`]),
    /// built fresh every call so what's drawn and what `⏎`/`↑`/`↓` act on can never disagree.
    pub(crate) fn build_palette_groups(&self, cx: &App) -> Vec<palette::PaletteGroup> {
        // A drill-down step replaces the root list entirely rather than adding a group to it -
        // the palette is asking one question ("which server?") and every row on screen has to be
        // a real answer to it. Same builder shape, so typing/`↑`/`↓`/`⏎` behave identically.
        if self.palette_step == palette::PaletteStep::PickLanguageServer {
            return palette::build_language_server_groups(
                self.palette_query.as_str(),
                &self.language_server_candidates(),
            );
        }

        let agents: Vec<palette::AgentCandidate> = self
            .agents
            .iter()
            .map(|agent| {
                let status = self.agent_status(agent, cx);
                let branch = self
                    .worktrees
                    .iter()
                    .find(|item| item.path == agent.cwd)
                    .and_then(|item| item.branch.clone());
                let title = match agent.cwd.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => agent.cwd.display().to_string(),
                };
                palette::AgentCandidate {
                    id: agent.id,
                    kind: agent.kind,
                    title,
                    branch,
                    status,
                }
            })
            .collect();

        // The three spawn commands below name the real worktree they would spawn into. With no
        // worktree genuinely selected they would spawn nothing at all (`Self::new_agent`/
        // `Self::new_agent_pane` both refuse outright), so the honest thing to show is that -
        // rather than the repo root, which is what `Self::current_worktree_path` used to hand back and
        // which was never a place a tab could legitimately belong to. See that method's own docs.
        let spawn_target = match self.current_worktree_path() {
            Some(cwd) => format!("in {}", cwd.display()),
            None => "- select a worktree first".to_string(),
        };
        let next_sidebar_view = match next_right_sidebar_view(self.right_sidebar_view) {
            RightSidebarView::Files => "Files",
            RightSidebarView::Search => "Search",
            RightSidebarView::Changes => "Changes",
        };
        let mut commands = vec![
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewShell,
                secondary: format!("spawn a shell {spawn_target}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewClaudeAgent,
                secondary: format!("spawn claude {spawn_target}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewCodexAgent,
                secondary: format!("spawn codex {spawn_target}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::NewCursorAgent,
                secondary: format!("spawn cursor-agent {spawn_target}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::CycleRightPanel,
                secondary: format!("switch the right panel to {next_sidebar_view}"),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::PruneWorktrees,
                secondary: format!(
                    "{} prunable",
                    plural::count(self.prunable_worktree_paths().len(), "worktree", None)
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
            // Real, previously-reported bug: `PaletteCommand::RestartLanguageServers` and
            // `PaletteCommand::CheckForUpdates` have carried a real label, real search keywords,
            // and a real click handler (see the `handle_palette_command` match below) since the
            // commits that introduced them - but neither was ever pushed into this hand-built
            // list, so the palette never actually listed either one. `PaletteCommand::ALL`
            // (used by this module's own exhaustive-listing test) didn't catch the gap because
            // it enumerates the enum, not what actually reaches a user.
            palette::CommandCandidate {
                command: palette::PaletteCommand::RestartLanguageServers,
                secondary: "recover a language server that stopped responding".to_string(),
            },
            palette::CommandCandidate {
                command: palette::PaletteCommand::CheckForUpdates,
                secondary: "check GitHub for a newer release".to_string(),
            },
        ];
        // GitHub issue #90: a genuinely empty window has no real repo to graph -
        // `crate::graph_view::render::AdeApp::open_git_graph`'s own guard now refuses outright
        // with no focused repo, so this entry is dropped from the results entirely rather than
        // listed as a real command that would silently do nothing if picked (this app has no
        // existing "greyed out but still listed" convention for palette rows, unlike the title
        // bar's dropdown menus - dropping the entry is the simpler, equally honest fix).
        if self.focused_repo().is_some() {
            commands.push(palette::CommandCandidate {
                command: palette::PaletteCommand::OpenGitGraph,
                secondary: "commit history, branches, lanes".to_string(),
            });
        }
        // Listed only while there is genuinely something to pick - same reasoning as
        // `OpenGitGraph` right above: with no live client under this worktree, "Restart Language
        // Server" would open a step with nothing in it, which is a command that does nothing.
        // The secondary line says which real thing will happen, since with exactly one server
        // running this restarts it outright instead of asking (see
        // [`Self::begin_language_server_pick`]).
        let servers = self.restartable_language_servers();
        if let Some(secondary) = match servers.as_slice() {
            [] => None,
            [only] => Some(format!("restart {}", only.client_key)),
            many => Some(format!("pick one of {} running servers", many.len())),
        } {
            // Inserted beside the other recovery/utility commands rather than appended: with an
            // empty query every command ranks equally, so the group's own
            // `MAX_ENTRIES_PER_GROUP` cap silently drops whatever sits past the eighth row - a
            // recovery command a user can only reach by already knowing to type its name is the
            // same discoverability failure `PaletteCommand::RestartLanguageServers`' own docs
            // argue against. Positional rather than a magic index so reordering the list above
            // can't quietly move this somewhere else.
            let beside_settings = commands
                .iter()
                .position(|candidate| candidate.command == palette::PaletteCommand::OpenSettings)
                .map(|index| index + 1)
                .unwrap_or(commands.len());
            commands.insert(
                beside_settings,
                palette::CommandCandidate {
                    command: palette::PaletteCommand::RestartLanguageServer,
                    secondary,
                },
            );
        }

        palette::build_groups(
            self.palette_scope,
            self.palette_query.as_str(),
            &agents,
            &commands,
            &self.palette_file_candidates,
        )
    }

    /// The [`palette::PaletteStep::PickLanguageServer`] step's rows, built fresh from the real
    /// live clients [`Self::restartable_language_servers`] reports - never a fixed menu of the
    /// server names this app knows how to spawn, so a row exists exactly when a real process (or
    /// a real failed spawn) does.
    pub(in crate::palette) fn language_server_candidates(
        &self,
    ) -> Vec<palette::LanguageServerCandidate> {
        self.restartable_language_servers()
            .into_iter()
            .map(|server| {
                let language =
                    crate::language::language_for_lsp_client_key(server.client_key).map(|found| {
                        if found.is_companion {
                            // Vue runs two processes under two keys; without this both rows
                            // would read "Vue" and only the key would tell them apart.
                            format!("{} companion", found.display_name)
                        } else {
                            found.display_name.to_string()
                        }
                    });
                let (state, status) = match &server.state {
                    crate::lsp::client::LanguageServerRunState::Ready => {
                        ("ready".to_string(), crate::rail::status::Status::Run)
                    }
                    crate::lsp::client::LanguageServerRunState::Failed(reason) => {
                        (reason.clone(), crate::rail::status::Status::Fail)
                    }
                };
                palette::LanguageServerCandidate {
                    client_key: server.client_key,
                    secondary: match &language {
                        Some(language) => format!("{language} \u{b7} {state}"),
                        None => state,
                    },
                    keywords: language.unwrap_or_default(),
                    status,
                }
            })
            .collect()
    }

    /// Runs [`palette::PaletteCommand::RestartLanguageServer`]: either restarts the one server
    /// there is, or moves the palette into its "which server?" step.
    pub(in crate::palette) fn begin_language_server_pick(&mut self, cx: &mut Context<Self>) {
        let servers = self.restartable_language_servers();
        match servers.as_slice() {
            // Not reachable through the palette (the entry isn't listed with no servers), but a
            // real no-op rather than an empty step if it ever is.
            [] => {}
            [only] => {
                let client_key = only.client_key;
                self.restart_lsp_client(client_key, cx);
            }
            _ => {
                self.palette_step = palette::PaletteStep::PickLanguageServer;
                // The query that found "Restart Language Server" would otherwise filter the
                // server list it just opened, which is a different list of different words - a
                // step starts from the same clean slate a freshly opened palette does.
                self.palette_query.reset();
                self.palette_selected = 0;
                cx.notify();
            }
        }
    }

    /// Leaves a drill-down step for the root command list (`Esc` inside a step, which is
    /// deliberately *not* the same as `Esc` on the root list - that closes the palette). Returns
    /// `true` if there really was a step to leave.
    pub(in crate::palette) fn leave_palette_step(&mut self, cx: &mut Context<Self>) -> bool {
        if self.palette_step == palette::PaletteStep::Root {
            return false;
        }
        self.palette_step = palette::PaletteStep::Root;
        self.palette_query.reset();
        self.palette_selected = 0;
        cx.notify();
        true
    }

    /// Rebuilds [`Self::palette_file_candidates`] from the current real [`Self::file_tree`]/
    /// [`Self::current_diff`], on the calling (foreground) thread. This is the *diff* side's
    /// entry point - [`Self::load_diff`]'s completion handler, and `open_file_tab`'s - where the
    /// tree is unchanged and only the marks laid over it moved.
    pub(crate) fn rebuild_palette_file_candidates(&mut self) {
        self.palette_file_candidates = build_file_candidates(
            &self.file_tree,
            &self.file_tree_root,
            &file_diff_marks(self.current_diff()),
        );
    }

    /// Moves the palette's keyboard selection by `delta` rows (`↑`/`↓`), clamped to the current
    /// result count - never wraps, and no-ops against zero results.
    pub(in crate::palette) fn move_palette_selection(
        &mut self,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        let groups = self.build_palette_groups(cx);
        let total = palette::flatten(&groups).len();
        if total == 0 {
            self.palette_selected = 0;
            return;
        }
        let next = (self.palette_selected as i32 + delta).clamp(0, total as i32 - 1);
        self.palette_selected = next as usize;
        // Moving the highlight has to move the viewport with it, or the selection walks off the
        // bottom and `⏎` runs a row the user cannot see (GitHub issue #413). Resolved on the next
        // prepaint against real child bounds, and a no-op when the row is already fully visible.
        if let Some(child) = palette::row_child_index(&groups, self.palette_selected) {
            self.palette_results_scroll_handle.scroll_to_item(child);
        }
        cx.notify();
    }

    /// Dispatches a [`palette::PaletteCommand`] to the same `AdeApp` method its existing UI
    /// affordance calls (see [`palette::PaletteCommand`]'s per-variant docs) - never a second,
    /// independent implementation.
    pub(crate) fn execute_palette_command(
        &mut self,
        command: palette::PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            palette::PaletteCommand::NewShell => self.new_agent(ProcessKind::Shell, window, cx),
            palette::PaletteCommand::NewClaudeAgent => {
                self.new_agent(ProcessKind::claude(), window, cx)
            }
            palette::PaletteCommand::NewCodexAgent => {
                self.new_agent(ProcessKind::codex(), window, cx)
            }
            palette::PaletteCommand::NewCursorAgent => {
                self.new_agent(ProcessKind::cursor(), window, cx)
            }
            palette::PaletteCommand::CycleRightPanel => {
                // Any palette gesture must disarm a pending prune confirmation (see
                // `Self::open_palette`'s docs); the other branches already do this via the
                // methods they call, so this one clears it explicitly.
                self.prune_confirm_armed = false;
                self.set_right_sidebar_view(
                    next_right_sidebar_view(self.right_sidebar_view),
                    window,
                    cx,
                );
            }
            palette::PaletteCommand::PruneWorktrees => self.request_prune(cx),
            palette::PaletteCommand::OpenSettings => self.open_settings(window, cx),
            palette::PaletteCommand::RestartLanguageServers => self.restart_lsp_clients(cx),
            palette::PaletteCommand::RestartLanguageServer => self.begin_language_server_pick(cx),
            // Also reachable from the Settings "General" page - both entry points call
            // `Self::set_window_controls_style`, never two independent copies.
            palette::PaletteCommand::WindowControlsSystem => {
                self.set_window_controls_style(WindowControlsStyle::System, cx);
            }
            palette::PaletteCommand::WindowControlsMacos => {
                self.set_window_controls_style(WindowControlsStyle::MacosStyle, cx);
            }
            palette::PaletteCommand::WindowControlsWindowsLinux => {
                self.set_window_controls_style(WindowControlsStyle::WindowsLinuxStyle, cx);
            }
            palette::PaletteCommand::OpenGitGraph => self.open_git_graph(window, cx),
            palette::PaletteCommand::CheckForUpdates => self.check_for_update(cx),
        }
    }

    /// Runs a palette file result (GitHub issue #15): opens the file - its diff if it is changed
    /// in the currently loaded diff, otherwise the editable File view - moves real keyboard focus
    /// into it, and reveals and highlights it in the Files tree.
    // `pub(crate)`, not `pub(in crate::palette)`: `crate::sidebar::render`'s own
    // "reveal in tree" regression test drives this real flow rather than calling
    // `AdeApp::reveal_in_tree` directly, so that the wiring between the two is what's covered.
    pub(crate) fn open_palette_file_result(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Every palette selection counts as a fresh gesture that disarms a pending prune
        // confirmation (see `Self::open_palette`'s docs).
        self.prune_confirm_armed = false;
        let relative = path
            .strip_prefix(&self.file_tree_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        let has_diff = self
            .current_diff()
            .is_some_and(|diff| diff.files.iter().any(|file| file.path == relative));

        // The revealed row has to be on a tab that is actually showing, or the reveal is
        // invisible. Set before opening, so the one `cx.notify()` at the end of
        // `open_and_focus_file` covers this too.
        self.right_sidebar_view = RightSidebarView::Files;
        if has_diff {
            self.open_change_diff(relative, window, cx);
        } else {
            self.open_file_view(path, window, cx);
        }
    }

    /// Runs the currently highlighted palette result (`⏎`) - looks it up fresh via
    /// [`Self::build_palette_groups`], dispatches by its [`palette::EntryTarget`], then closes
    /// the palette.
    pub(in crate::palette) fn run_selected_palette_entry(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups = self.build_palette_groups(cx);
        let target = palette::flatten(&groups)
            .get(self.palette_selected)
            .map(|entry| entry.target.clone());
        let focus_before = window.focused(cx);
        if let Some(target) = target {
            match target {
                palette::EntryTarget::Command(command) => {
                    self.execute_palette_command(command, window, cx)
                }
                palette::EntryTarget::Agent(id) => self.select_agent(id, window, cx),
                palette::EntryTarget::File(path) => self.open_palette_file_result(path, window, cx),
                palette::EntryTarget::LanguageServer(client_key) => {
                    // Back to the root list *before* the restart, so the closing rule below sees
                    // an answered question rather than an open one.
                    self.palette_step = palette::PaletteStep::Root;
                    self.restart_lsp_client(client_key, cx);
                }
            }
        }
        // An entry that opened a drill-down step didn't run anything yet - it asked which thing
        // to run, and the answer is on screen in this same overlay. Closing here would throw that
        // question away the instant it was asked. Only a command that enters a step can leave
        // `palette_step` non-root at this point (every step row resets it above), so this is a
        // real observation of what just happened rather than a per-entry flag to keep in sync -
        // the same reasoning this method's own focus rule is built on.
        if self.palette_step != palette::PaletteStep::Root {
            cx.notify();
            return;
        }
        if window.focused(cx) == focus_before {
            self.close_palette(window, cx);
        } else {
            self.close_palette_keeping_result_focus(cx);
        }
    }

    /// `TextUndo` for the palette query (GitHub issue #17). Resets the highlighted row alongside
    /// the text, exactly like every other real mutation of the query does - a restored query with
    /// a stale selected index would point into a different result list than the one on screen.
    pub(in crate::palette) fn handle_palette_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_query.undo() {
            self.palette_selected = 0;
            cx.notify();
        }
    }

    pub(in crate::palette) fn handle_palette_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_query.redo() {
            self.palette_selected = 0;
            cx.notify();
        }
    }

    /// The palette's hand-rolled text field key handler - the same append/backspace shape as
    /// [`Self::handle_filter_key_down`], plus `Esc`/`⏎`/`↑`/`↓`/`⇥`. Also implements the "type
    /// the scope prefix" gesture ([`crate::palette::state::typed_scope_prefix`]) for the first
    /// character typed into an empty query.
    pub(in crate::palette) fn handle_palette_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) =
            crate::root::widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        // GitHub issue #27's "solid mid-keystroke" applies to every real caret-bearing input,
        // not only the code editor - reset unconditionally here, before dispatching on which key
        // this actually was, since every branch below is a real, live keystroke this input is
        // handling.
        self.reset_caret_blink(cx);
        match keystroke.key.as_str() {
            "escape" => {
                // Inside a drill-down step, `Esc` is "back to the command list" - what a real
                // nested menu does, and the reason the step is worth having in the same overlay
                // rather than as a second widget. On the root list it still closes the palette.
                if !self.leave_palette_step(cx) {
                    self.close_palette(window, cx);
                }
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
                // A step lists one kind of thing, so there are no scopes to cycle - swallowed
                // rather than left to bubble, since the palette owns the keyboard while it's up.
                if self.palette_step == palette::PaletteStep::Root {
                    self.palette_scope = self.palette_scope.cycle();
                    self.palette_selected = 0;
                    cx.notify();
                }
                cx.stop_propagation();
            }
            key => {
                // A scope prefix character typed into an *empty* root query switches scope
                // instead of being inserted - checked before any editing, exactly as before.
                if let Some(text) = keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty())
                {
                    if self.palette_query.is_empty()
                        && self.palette_step == palette::PaletteStep::Root
                    {
                        if let Some(scope) =
                            text.chars().next().and_then(palette::typed_scope_prefix)
                        {
                            self.palette_scope = scope;
                            self.palette_selected = 0;
                            cx.notify();
                            cx.stop_propagation();
                            return;
                        }
                    }
                }
                // GitHub issue #336: the whole `TextField` vocabulary through one entry point,
                // rather than the backspace/insert pair this palette used to hand-roll - which is
                // why its caret could never move off the end of the query.
                if self.palette_query.handle_editing_key(
                    key,
                    keystroke.key_char.as_deref(),
                    modifiers,
                    Instant::now(),
                ) {
                    self.palette_selected = 0;
                    cx.notify();
                    cx.stop_propagation();
                }
            }
        }
    }

    /// The palette query's own field handle - what click/drag selection and GitHub issue #336's
    /// four clipboard/select-all actions act on. Its `on_changed` resets the highlighted row to
    /// the top exactly as a keystroke into the field does: a pasted query describes a different
    /// result list, and leaving the old index selected would run whatever now happens to sit
    /// there.
    fn palette_query_handle() -> crate::root::widgets::TextFieldHandle {
        crate::root::widgets::TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.palette_query))
            .on_changed(|app: &mut AdeApp, _cx| app.palette_selected = 0)
    }

    /// The command palette overlay: an absolutely-positioned scrim + panel painted as the last
    /// child of [`Render::render`]'s root div, so it paints on top of every other zone.
    /// `top(theme::band::TITLE_BAR)` plus `bottom(0)` against the root div's own full-window box
    /// covers the body and the status bar - no extra `.relative()` is needed since
    /// `Position::Relative` is GPUI's layout default (`vendor/zed/crates/gpui/src/style.rs`'s
    /// `Style::default`), so the root div is already a valid containing block.
    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .bg(theme::surface::SCRIM.resolve().opacity(0.62))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(64.0))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.close_palette(window, cx);
            }))
            .child(
                self.wire_text_input_actions(
                    div()
                        .id("palette-panel")
                        .track_focus(&self.palette_focus_handle)
                        // The one shared context tag every real text-typing surface in this app
                        // carries (GitHub issue #17): it routes `secondary-z` to `TextUndo` here.
                        // Registering the listener on *this* node - the one
                        // `palette_focus_handle` is tracked on - is what makes the routing
                        // structural: GPUI only dispatches an action along the focused node's own
                        // ancestor path, so a palette query typed over an open file editor undoes
                        // the query, not the file. See `crate::default_key_bindings`' own docs.
                        .key_context("text-input")
                        .on_action(cx.listener(Self::handle_palette_text_undo))
                        .on_action(cx.listener(Self::handle_palette_text_redo))
                        .on_key_down(cx.listener(Self::handle_palette_key_down)),
                    Self::palette_query_handle(),
                    cx,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    // Stops the click from bubbling to the scrim's own `on_click`, which
                    // would otherwise close the palette on every click inside it.
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

    /// A 1.5×16 bar at the palette input's real insertion point - the design renders the
    /// identical two-position caret rather than a fixed one, so this isn't a new invention. `margin_right`/`margin_left` place it on whichever side of the text it sits
    /// on: before the placeholder (empty query) or after the real typed text (non-empty).
    /// The palette's input row: scope-prefix glyph, typed query (or its placeholder), a caret at
    /// the real insertion point, and the clickable segmented scope control.
    pub(in crate::palette) fn render_palette_input_row(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let placeholder = match self.palette_step {
            palette::PaletteStep::Root => "Type a command, file or agent\u{2026}",
            palette::PaletteStep::PickLanguageServer => "Which language server?",
        };

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
            // GitHub issue #336: through the one helper that owns this structure, like every
            // other simple input in the app. The palette keeps its own caret *metrics* (16px
            // tall, 3px before the placeholder, 2px after the typed text) through
            // `SimpleInputCaret`, which is exactly why that type exists. What it gains is a caret
            // that follows `TextField::caret` instead of sitting at the end of the query
            // regardless, a real selection highlight, and real click/drag/double-click selection.
            .child(self.render_simple_input_row(
                SimpleInput {
                    caret_selector: "palette-caret".into(),
                    text_selector: "palette-query-text".into(),
                    focus_handle: Some(&self.palette_focus_handle),
                    text: self.palette_query.as_str(),
                    caret_offset: self.palette_query.caret(),
                    selection: self.palette_query.selection(),
                    placeholder,
                    font: theme::font::SANS,
                    text_size: self.ui_text_size(13.0),
                    text_color: theme::text::SELECTED,
                    placeholder_color: theme::text::GHOST,
                    caret: SimpleInputCaret {
                        height: px(16.0),
                        gap_before_placeholder: px(3.0),
                        gap_after_text: px(2.0),
                    },
                    field: Some(Self::palette_query_handle()),
                },
                cx,
            ))
            .when(self.palette_step == palette::PaletteStep::Root, |el| {
                el.child(self.render_palette_scope_control(cx))
            })
    }

    /// The `All ⇥ / Commands › / Files @` segmented scope control - reachable by clicking here
    /// or by typing a scope's prefix character (`crate::palette::state::typed_scope_prefix`, handled
    /// in [`Self::handle_palette_key_down`]).
    pub(in crate::palette) fn render_palette_scope_control(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            |this, index, _window, cx| {
                // Index-based, matching the `options` array literal right above: 0 = `All`,
                // 1 = `Commands`, 2 = `Files`.
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

    /// The grouped, scrollable result list - `crate::palette::state::build_groups`'s output, rendered
    /// top to bottom in the same order [`Self::run_selected_palette_entry`] flattens it, so the
    /// row shown at index N is always the row `⏎` would run at index N.
    pub(in crate::palette) fn render_palette_groups(
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
            // Test-only, as above: the results viewport's own painted box, so a test can assert a
            // row is inside it rather than re-deriving the viewport from theme constants.
            .debug_selector(|| "palette-results".to_string())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.palette_results_scroll_handle)
            .flex()
            .flex_col()
            .py(px(4.0));

        // Headers and rows are children of the scroller itself rather than of a per-group wrapper,
        // so `palette_results_scroll_handle`'s child bounds line up with
        // `palette::row_child_index` and a row can actually be scrolled to. Both carry `flex_none`
        // for that to be safe: as direct children of a column flex container they would otherwise
        // shrink below their themed heights the moment the list overflows, compacting the list
        // rather than scrolling it (the per-group wrapper used to absorb that shrink).
        let mut flat_index = 0usize;
        for group in groups {
            container = container.child(self.render_palette_group_header(group));
            for entry in &group.entries {
                let index = flat_index;
                flat_index += 1;
                container = container.child(self.render_palette_row(entry, index, cx));
            }
        }

        // See `crate::sidebar::render::AdeApp::render_file_tree`'s own docs on why the scrollbar
        // must be a sibling of `container`, inside its own non-scrolling `.relative()` wrapper,
        // never a child of `container` itself (GitHub issue #30).
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(container)
            .children(scrollbar::render_vertical_scrollbar(
                "palette-results-scrollbar",
                &self.palette_results_scroll_handle,
                &[],
                cx,
            ))
            .into_any_element()
    }

    /// One group's header band - its label and how many rows it contributes. The rows themselves
    /// are siblings of this, not children; see [`Self::render_palette_groups`].
    pub(in crate::palette) fn render_palette_group_header(
        &self,
        group: &palette::PaletteGroup,
    ) -> impl IntoElement {
        div()
            .id(format!("palette-group-{}", group.label))
            // Same reason as [`Self::render_palette_row`]'s own: a direct child of the scrolling
            // column must not shrink when the list overflows.
            .flex_none()
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
            )
    }

    /// One result row: a kind chip (command/agent-badge/language, per [`palette::EntryTarget`]),
    /// the matched-substring label, secondary text, an optional status/change dot, and an
    /// optional shortcut keycap. Clicking (or `⏎` on the selected row) runs it via
    /// [`Self::run_selected_palette_entry`].
    pub(in crate::palette) fn render_palette_row(
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
        // Mono for a real on-disk identifier - a file name, or a server binary's own name.
        let mono = matches!(
            entry.target,
            palette::EntryTarget::File(_) | palette::EntryTarget::LanguageServer(_)
        );

        let chip = match &entry.target {
            palette::EntryTarget::Command(_) => render_palette_command_chip().into_any_element(),
            palette::EntryTarget::Agent(_) => {
                let kind = entry.process_kind.unwrap_or(ProcessKind::Shell);
                render_palette_agent_chip(kind).into_any_element()
            }
            palette::EntryTarget::File(path) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                render_palette_file_chip(file_tree::lang_chip_for_name(&name)).into_any_element()
            }
            // A step row is still an action the palette performs, so it wears the same generic
            // command chip - the row's own status dot carries the per-server information.
            palette::EntryTarget::LanguageServer(_) => {
                render_palette_command_chip().into_any_element()
            }
        };

        let mut row = div()
            .id(("palette-row", index as u64))
            // Test-only (a no-op outside test builds, like every other `debug_selector` in this
            // codebase) - lets a real render test read a row's own painted bounds back with
            // `VisualTestContext::debug_bounds` and prove the selected row really sits inside the
            // results viewport, rather than trusting that scrolling "probably" happened.
            .debug_selector(move || format!("palette-row-{index}"))
            .cursor_pointer()
            // A row is a direct child of the scrolling column, so it has to opt out of flex
            // shrinking or an overflowing list compacts its rows instead of scrolling them.
            .flex_none()
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
                theme::ColorToken::literal(work_surface::TRANSPARENT)
            })
            .when(selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::palette::ROW_HOVER))
            })
            .child(chip)
            .child(render_palette_label(&entry.label, mono, label_fg.into()))
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

    /// The palette footer: `↑↓ move · ⏎ run · ⇥ next scope · esc close`, plus the result count -
    /// `total` reflects rows actually rendered (post [`palette::MAX_ENTRIES_PER_GROUP`] capping
    /// in `crate::palette::state::build_groups`), so it never overstates what's on screen.
    pub(in crate::palette) fn render_palette_footer(&self, total: usize) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        let hints: &[(&str, &str)] = match self.palette_step {
            palette::PaletteStep::Root => &[
                ("\u{2191}\u{2193}", "move"),
                ("enter", "run"),
                ("tab", "next scope"),
                ("esc", "close"),
            ],
            palette::PaletteStep::PickLanguageServer => &[
                ("\u{2191}\u{2193}", "move"),
                ("enter", "restart"),
                ("esc", "back"),
            ],
        };
        let hints = hints.iter().copied().map(|(spec, label)| {
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
                    .child(plural::count(total, "result", None)),
            )
    }
}

/// The palette row's 15×15 command chip - every command result gets the same generic `›` chip,
/// since (unlike agents/files) a command has no per-instance colour to inherit.
pub(in crate::palette) fn render_palette_command_chip() -> impl IntoElement {
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

/// The palette row's 15×15 agent chip - the same agent badge/tint
/// (`crate::work_surface::state::agent_tint`/`agent_initial`) the rail's agent rows use, reused
/// verbatim rather than a second, independently-drifting colour mapping.
pub(in crate::palette) fn render_palette_agent_chip(kind: ProcessKind) -> impl IntoElement {
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

/// The palette row's 15×15 file chip - the same language chip
/// `crate::sidebar::file_tree::lang_chip_for_name` gives the Files tree, just at 15×15 rather than the
/// tree row's 13×13 (see [`render_lang_chip`]).
pub(in crate::palette) fn render_palette_file_chip(chip: LangChip) -> impl IntoElement {
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

/// A result row's matched-substring label: three adjacent spans (`pre`/`mid`/`post`), the
/// middle one tinted with `theme::term::PROMPT` (reused rather than a separate token, since the
/// hex value is identical - same precedent as `theme::button::GREEN_KEYCAP_FG`'s docs). `mono`
/// selects mono for a file result, sans for a command/agent result.
pub(in crate::palette) fn render_palette_label(
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

/// Real interaction coverage for the caret position fix above: proves the caret's *painted x
/// position* actually differs between the empty-query and typed-query states, using
/// `VisualTestContext::debug_bounds` - the same real-bounds-measurement technique
/// `code_surface::zoom::code_zoom_tests::zoom_scales_text_but_not_the_gutter_width` uses - rather than only reading the
/// render code, since a doc comment claiming the right positioning can't catch a layout mistake
/// (e.g. a `when` branch wired to the wrong condition) the way a real measured assertion can.
#[cfg(test)]
mod palette_caret_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn caret_sits_before_the_placeholder_when_empty_and_after_the_text_once_typed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "sanity check: the palette should actually be open"
        );

        let empty_caret = cx
            .debug_bounds("palette-caret")
            .expect("the caret should have really painted with an empty query");
        let placeholder = cx
            .debug_bounds("palette-query-text")
            .expect("the placeholder text should have really painted");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty query, the real caret must sit before (at or left of) the \
             placeholder's own start x, not after it - got caret {:?} vs placeholder {:?}",
            empty_caret,
            placeholder,
        );

        cx.simulate_input("ab");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.palette_query.as_str(),
                "ab",
                "sanity check: real typed query"
            );
        });

        let short_caret = cx
            .debug_bounds("palette-caret")
            .expect("the caret should have really painted with a short typed query");
        let short_text = cx
            .debug_bounds("palette-query-text")
            .expect("the real typed text should have really painted");
        assert!(
            short_caret.origin.x >= short_text.origin.x + short_text.size.width,
            "with a typed query, the real caret must sit at or after the typed text's own \
             right edge, not before it - got caret {:?} vs text {:?}",
            short_caret,
            short_text,
        );
        assert!(
            short_caret.origin.x > empty_caret.origin.x,
            "the caret's real measured horizontal position must differ between the \
             empty-query state (before the placeholder) and a typed-query state (after the \
             real text) - got {:?} vs {:?}",
            empty_caret.origin.x,
            short_caret.origin.x,
        );

        cx.simulate_input("cdefgh");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.palette_query.as_str(),
                "abcdefgh",
                "sanity check: real longer typed query"
            );
        });

        let long_caret = cx
            .debug_bounds("palette-caret")
            .expect("the caret should have really painted with a longer typed query");
        assert!(
            long_caret.origin.x > short_caret.origin.x,
            "the real caret must keep tracking the typed text's length - typing more real \
             characters should move it further right, got {:?} then {:?}",
            short_caret.origin.x,
            long_caret.origin.x,
        );
    }
}

/// Real end-to-end coverage for the palette's one drill-down step (`Restart Language Server…`):
/// entering it, filtering/leaving it with the same keys the rest of the palette uses, and picking
/// a row actually restarting that one server and nothing else.
#[cfg(test)]
mod palette_language_server_step_tests {
    use super::*;
    use crate::lsp::client::lsp_connection_facade_tests::spawn_fake_server;
    use crate::lsp::client::LspClientState;
    use gpui::{Entity, TestAppContext};

    const TS_FAILURE: &str = "typescript-language-server's connection was lost";

    /// Two real live clients under the active root: a genuinely spawned `rust-analyzer` process
    /// and a `typescript-language-server` in the same real `Failed` state
    /// `AdeApp::reap_dead_lsp_clients` produces when a server dies.
    fn app_with_two_servers<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (
        Entity<AdeApp>,
        std::sync::Arc<lsp_core::LspClient>,
        &'a mut gpui::VisualTestContext,
    ) {
        let (app, cx) =
            crate::root::focus::palette_focus_tests::open_test_app(cx, root.to_path_buf());
        let server = spawn_fake_server(root, "rust-analyzer", "normal");
        app.update(cx, |app, _cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server.clone()),
            );
            app.lsp_clients.insert(
                (root, "typescript-language-server"),
                LspClientState::Failed(TS_FAILURE.to_string()),
            );
        });
        (app, server, cx)
    }

    /// The flat row index of the first entry with this target, in the same order
    /// `run_selected_palette_entry` flattens - i.e. exactly what `palette_selected` means.
    fn row_index(app: &AdeApp, cx: &App, target: &palette::EntryTarget) -> Option<usize> {
        let groups = app.build_palette_groups(cx);
        palette::flatten(&groups)
            .iter()
            .position(|entry| &entry.target == target)
    }

    /// Selects the row with this target and runs it with a real `⏎` through the real key handler.
    fn press_enter_on(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        target: palette::EntryTarget,
    ) {
        let index = app
            .read_with(cx, |app, cx| row_index(app, cx, &target))
            .unwrap_or_else(|| panic!("{target:?} should be a real, listed palette row"));
        app.update(cx, |app, _cx| app.palette_selected = index);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    #[gpui::test]
    fn restarting_one_server_asks_which_one_inside_the_same_palette(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, _server, cx) = app_with_two_servers(cx, repo.path());

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        press_enter_on(
            &app,
            cx,
            palette::EntryTarget::Command(palette::PaletteCommand::RestartLanguageServer),
        );

        app.read_with(cx, |app, cx| {
            assert!(
                app.palette_open,
                "the command asked a question - closing the palette would throw the answer away \
                 before it could be given"
            );
            assert_eq!(app.palette_step, palette::PaletteStep::PickLanguageServer);
            assert_eq!(
                app.lsp_clients.len(),
                2,
                "nothing may be restarted merely by *asking* which server to restart"
            );

            let groups = app.build_palette_groups(cx);
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].label, "Language Servers");
            let rows: Vec<(&palette::EntryTarget, &str)> = groups[0]
                .entries
                .iter()
                .map(|entry| (&entry.target, entry.secondary.as_str()))
                .collect();
            assert_eq!(
                rows,
                vec![
                    (
                        &palette::EntryTarget::LanguageServer("rust-analyzer"),
                        "Rust \u{b7} ready"
                    ),
                    (
                        &palette::EntryTarget::LanguageServer("typescript-language-server"),
                        &format!("TypeScript \u{b7} {TS_FAILURE}")[..]
                    ),
                ],
                "the step lists the real live clients, each with its registry language name and \
                 its own real state - the broken one has to be identifiable to be pickable"
            );
        });
    }

    #[gpui::test]
    fn typing_filters_the_step_and_escape_goes_back_before_it_closes(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, _server, cx) = app_with_two_servers(cx, repo.path());

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        cx.simulate_input("restart language");
        app.read_with(cx, |app, _| {
            assert_eq!(app.palette_query.as_str(), "restart language");
        });
        press_enter_on(
            &app,
            cx,
            palette::EntryTarget::Command(palette::PaletteCommand::RestartLanguageServer),
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.palette_query.is_empty(),
                "the query that found the command would otherwise filter the server list it just \
                 opened, which is a list of entirely different words"
            );
        });

        cx.simulate_input("types");
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            let groups = app.build_palette_groups(cx);
            assert_eq!(groups.len(), 1);
            assert_eq!(
                groups[0]
                    .entries
                    .iter()
                    .map(|entry| entry.target.clone())
                    .collect::<Vec<_>>(),
                vec![palette::EntryTarget::LanguageServer(
                    "typescript-language-server"
                )],
                "typing narrows the step's own rows, exactly like every other palette list"
            );
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app.palette_open,
                "Esc inside a step goes back to the command list, the way a real nested menu does"
            );
            assert_eq!(app.palette_step, palette::PaletteStep::Root);
            assert!(app.palette_query.is_empty());
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                !app.palette_open,
                "Esc on the root list still closes the whole palette"
            );
        });
    }

    #[gpui::test]
    fn picking_a_row_restarts_exactly_that_server_and_closes_the_palette(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, _server, cx) = app_with_two_servers(cx, repo.path());
        // The key `app_with_two_servers` really inserted under - `AdeApp` canonicalizes the root it
        // is opened with, which on macOS resolves the `/var -> /private/var` symlink `TempDir`
        // hands out, so `repo.path()` would key an entry that does not exist.
        let root = app.read_with(cx, |app, _| app.file_tree_root.clone());

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        press_enter_on(
            &app,
            cx,
            palette::EntryTarget::Command(palette::PaletteCommand::RestartLanguageServer),
        );
        press_enter_on(
            &app,
            cx,
            palette::EntryTarget::LanguageServer("rust-analyzer"),
        );

        app.read_with(cx, |app, _| {
            assert!(
                !app.lsp_clients
                    .contains_key(&(root.clone(), "rust-analyzer")),
                "picking a row must run the real restart for that key - a freed key is what lets \
                 the next render genuinely spawn a fresh process"
            );
            assert!(
                matches!(
                    app.lsp_clients
                        .get(&(root.clone(), "typescript-language-server")),
                    Some(LspClientState::Failed(reason)) if reason == TS_FAILURE
                ),
                "the server the user did not pick is left exactly as it was"
            );
            assert!(
                !app.palette_open,
                "the question was answered, so the palette closes"
            );
            assert_eq!(app.palette_step, palette::PaletteStep::Root);
        });
    }

    #[gpui::test]
    fn a_single_running_server_is_restarted_without_a_pointless_second_step(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let (app, cx) = crate::root::focus::palette_focus_tests::open_test_app(cx, root.clone());
        app.update(cx, |app, _cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root, "rust-analyzer"),
                LspClientState::Failed("rust-analyzer's connection was lost".to_string()),
            );
        });

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            let groups = app.build_palette_groups(cx);
            let entry = palette::flatten(&groups)
                .into_iter()
                .find(|entry| {
                    entry.target
                        == palette::EntryTarget::Command(
                            palette::PaletteCommand::RestartLanguageServer,
                        )
                })
                .expect("the command is listed while a real server is running")
                .clone();
            assert_eq!(
                entry.secondary, "restart rust-analyzer",
                "with nothing to choose between, the row says exactly what running it will do"
            );
        });

        press_enter_on(
            &app,
            cx,
            palette::EntryTarget::Command(palette::PaletteCommand::RestartLanguageServer),
        );

        app.read_with(cx, |app, _| {
            assert!(
                !app.lsp_clients
                    .contains_key(&(root.clone(), "rust-analyzer")),
                "the one server really restarted"
            );
            assert_eq!(
                app.palette_step,
                palette::PaletteStep::Root,
                "no step was entered for a choice that isn't one"
            );
            assert!(!app.palette_open);
        });
    }

    #[gpui::test]
    fn the_command_is_not_listed_when_no_server_is_running_at_all(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) =
            crate::root::focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        // Typed, so the absence below is a real filter miss rather than the group's own
        // eight-row cap hiding a row that is in fact listed.
        cx.simulate_input("restart");
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            assert!(
                app.lsp_clients.is_empty(),
                "sanity check: this window really has no live client"
            );
            assert_eq!(
                row_index(
                    app,
                    cx,
                    &palette::EntryTarget::Command(palette::PaletteCommand::RestartLanguageServer)
                ),
                None,
                "a command whose whole job is picking among running servers must not be offered \
                 when there are none - it would open an empty step and do nothing"
            );
        });
    }
}

/// The results list is a real scrolling viewport, so moving the highlight with `↑`/`↓` has to
/// bring the highlighted row with it (GitHub issue #413). Asserted against the row's and the
/// viewport's genuinely painted boxes, since an index that moves while the viewport stays put is
/// exactly the bug and reads as correct from the render code alone.
#[cfg(test)]
mod palette_scroll_tests {
    use crate::palette::state as palette;
    use crate::root::focus::palette_focus_tests;
    use crate::root::scrollbar::ScrollableHandle;
    use crate::root::{AdeApp, TogglePalette};
    use gpui::{Bounds, Entity, Pixels, TestAppContext, VisualTestContext};

    /// `VisualTestContext::debug_bounds` takes a `&'static str`, so a row's selector cannot be
    /// interpolated from its index and has to be looked up here instead. Long enough to cover
    /// every row the fixture below produces, which [`row_selector`] asserts rather than assumes.
    const ROW_SELECTORS: [&str; 24] = [
        "palette-row-0",
        "palette-row-1",
        "palette-row-2",
        "palette-row-3",
        "palette-row-4",
        "palette-row-5",
        "palette-row-6",
        "palette-row-7",
        "palette-row-8",
        "palette-row-9",
        "palette-row-10",
        "palette-row-11",
        "palette-row-12",
        "palette-row-13",
        "palette-row-14",
        "palette-row-15",
        "palette-row-16",
        "palette-row-17",
        "palette-row-18",
        "palette-row-19",
        "palette-row-20",
        "palette-row-21",
        "palette-row-22",
        "palette-row-23",
    ];

    fn row_selector(index: usize) -> &'static str {
        ROW_SELECTORS
            .get(index)
            .copied()
            .expect("ROW_SELECTORS must cover every row this fixture builds")
    }

    /// An open palette whose results genuinely overflow, which is the only state issue #413 is
    /// about: the commands the palette always offers fill ten rows, which fit, so a real "Recent
    /// Files" group is added on top - `changed` is what puts a file in that group with an empty
    /// query, the same way a repository with uncommitted additions does.
    fn open_overflowing_palette<'a>(
        cx: &'a mut TestAppContext,
        root: &std::path::Path,
    ) -> (Entity<AdeApp>, &'a mut VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, root.to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, _| {
            app.palette_file_candidates = (0..8)
                .map(|i| palette::FileCandidate {
                    path: root.join(format!("src/fixture{i}.rs")),
                    name: format!("fixture{i}.rs"),
                    dir: "src".to_string(),
                    add: 1,
                    del: 0,
                    changed: Some(palette::FileChangeKind::Added),
                })
                .collect();
        });

        cx.dispatch_action(TogglePalette);
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "sanity check: the palette should actually be open"
        );
        assert!(
            max_scroll_offset(&app, cx) > gpui::px(0.0),
            "premise: the seeded results must genuinely overflow the viewport - a zero \
             `max_offset` would mean everything fits and there is nothing to scroll"
        );
        (app, cx)
    }

    fn row_count(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> usize {
        app.read_with(cx, |app, cx| {
            palette::flatten(&app.build_palette_groups(cx)).len()
        })
    }

    fn selected(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> usize {
        app.read_with(cx, |app, _| app.palette_selected)
    }

    fn scroll_offset(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> Pixels {
        app.read_with(cx, |app, _| {
            app.palette_results_scroll_handle.scroll_offset().y
        })
    }

    fn max_scroll_offset(app: &Entity<AdeApp>, cx: &mut VisualTestContext) -> Pixels {
        app.read_with(cx, |app, _| {
            app.palette_results_scroll_handle.max_scroll_offset().y
        })
    }

    /// Whether a row's real painted box sits entirely inside the results viewport's real painted
    /// box - "visible to the user", not "exists in the element tree".
    fn fully_inside(row: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
        row.top() >= viewport.top() && row.bottom() <= viewport.bottom()
    }

    /// Asserts the invariant issue #413 is about, against the real painted frame: whichever row is
    /// highlighted right now is entirely visible.
    fn assert_selection_visible(app: &Entity<AdeApp>, cx: &mut VisualTestContext, step: &str) {
        let index = selected(app, cx);
        let viewport = cx
            .debug_bounds("palette-results")
            .expect("the results viewport should have really painted");
        let row = cx
            .debug_bounds(row_selector(index))
            .unwrap_or_else(|| panic!("row {index} should have really painted after {step}"));
        assert!(
            fully_inside(row, viewport),
            "after {step} the highlighted row {index} must sit entirely inside the results \
             viewport - got row {row:?} against viewport {viewport:?}"
        );
    }

    /// A scrolling list must scroll, not squash. Rows and headers are flex children of a column
    /// container, so without `flex_none` they shrink below their themed height as soon as the
    /// content overflows - the list quietly compacts instead of overflowing, which is both wrong
    /// on its own and hides the very overflow the scrolling exists to handle.
    #[gpui::test]
    fn an_overflowing_list_keeps_every_row_at_its_full_themed_height(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_overflowing_palette(cx, repo.path());

        let total = row_count(&app, cx);
        for index in 0..total {
            let row = cx
                .debug_bounds(row_selector(index))
                .unwrap_or_else(|| panic!("row {index} should have really painted"));
            assert_eq!(
                row.size.height,
                crate::theme::band::PALETTE_ROW,
                "row {index} of {total} must paint at its full themed height even though the list \
                 overflows - a shorter row means flex shrank it, compacting the list instead of \
                 scrolling it"
            );
        }
    }

    #[gpui::test]
    fn arrowing_down_keeps_the_highlighted_row_inside_the_viewport(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_overflowing_palette(cx, repo.path());

        let total = row_count(&app, cx);
        assert_eq!(
            scroll_offset(&app, cx),
            gpui::px(0.0),
            "a freshly opened palette must start at the top"
        );

        // The premise the whole issue rests on: the bottom of the list really is out of sight
        // before any key is pressed, so walking down to it has to move the viewport.
        let viewport = cx
            .debug_bounds("palette-results")
            .expect("the results viewport should have really painted");
        let last = cx
            .debug_bounds(row_selector(total - 1))
            .expect("the last row should have really painted - the list is not virtualized");
        assert!(
            !fully_inside(last, viewport),
            "premise: the last of {total} rows must start out past the viewport's bottom edge, \
             otherwise arrowing to it would never need to scroll - got row {last:?} inside \
             viewport {viewport:?}"
        );

        for step in 1..total {
            cx.simulate_keystrokes("down");
            cx.run_until_parked();
            assert_eq!(
                selected(&app, cx),
                step,
                "sanity check: {step} presses of `down` must land on row {step}"
            );
            assert_selection_visible(&app, cx, "pressing `down`");
        }

        assert!(
            scroll_offset(&app, cx) < gpui::px(0.0),
            "walking to the last row must have really scrolled the viewport (GPUI's own scroll \
             offset goes negative as you scroll down), not merely moved an index - this is \
             GitHub issue #413"
        );
    }

    #[gpui::test]
    fn arrowing_back_up_brings_the_viewport_back_with_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_overflowing_palette(cx, repo.path());

        let total = row_count(&app, cx);
        for _ in 1..total {
            cx.simulate_keystrokes("down");
        }
        cx.run_until_parked();

        let scrolled_down = scroll_offset(&app, cx);
        assert!(
            scrolled_down < gpui::px(0.0),
            "premise: the list must really be scrolled down before `up` can be shown to undo it"
        );

        for step in (0..total - 1).rev() {
            cx.simulate_keystrokes("up");
            cx.run_until_parked();
            assert_eq!(
                selected(&app, cx),
                step,
                "sanity check: walking back up must pass through row {step}"
            );
            assert_selection_visible(&app, cx, "pressing `up`");
        }

        assert!(
            scroll_offset(&app, cx) > scrolled_down,
            "walking back up to the first row must have really scrolled the viewport back toward \
             the top (the offset rises toward zero), not left the highlight stranded above the \
             fold - still at {scrolled_down:?}"
        );
    }
}
