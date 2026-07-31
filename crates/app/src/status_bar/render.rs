use super::*;
use crate::root::widgets::{render_env_chip, render_keycap_row, text_tooltip, KeycapSize};

/// The 28px status bar (`CHANGELOG.md`'s change 7 - height 26 -> 28, gap 12 -> 9, every value
/// 10px mono), rebuilt from the old single `8 agents · 2 waiting · …` summary string into a
/// left/right cluster layout. Every field here reads a real, already-live data source - see each
/// render helper's own docs for exactly which one, and [`AdeApp::status_bar_active_parsed_file`]
/// for the one shared gate that keeps the file-scoped fields (`ln N`, indent, line-ending,
/// `UTF-8`, the diagnostics error count) from ever showing stale data left over from a
/// previously-viewed file.
///
/// The mockup's second `⌘⇧K agents` palette hint is still omitted for the same reason the old
/// bar omitted it: that binding is never wired up anywhere in this codebase, so a keycap for it
/// would advertise a shortcut that silently does nothing. The real `secondary-1`..`secondary-8`
/// agent-jump keycaps (reused from `Self::render_tab_strip`'s own right-aligned cluster) are
/// shown instead, since those genuinely are bound.
impl AdeApp {
    pub(crate) fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-bar")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(12.0))
            .w_full()
            .h(theme::band::STATUS_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_t_1()
            .border_color(theme::border::ZONE)
            .child(self.render_status_bar_left(cx))
            .child(self.render_status_bar_right(cx))
    }

    /// Branch/ahead-behind, real urgency counters, `N agents · cpu · mem`, `N wt · disk` - each
    /// divided by a real vertical rule, per `CHANGELOG.md`'s "Left: ... · divider · ... ·
    /// divider · ...".
    fn render_status_bar_left(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut segments: Vec<gpui::AnyElement> = Vec::new();
        // Shown first, and only while genuinely set - the real, transient feedback from
        // "keep all changes"/"discard worktree"/`Undo`/`Redo` (Revision R10). See this method's
        // own docs for why this lives here, in the status bar, rather than the rail footer's own
        // status slot.
        if let Some(notice) = self.render_status_worktree_history_notice() {
            segments.push(notice);
        }
        if let Some(branch_cluster) = self.render_status_branch_cluster(cx) {
            segments.push(branch_cluster);
        }
        segments.push(self.render_status_urgency_counters(cx).into_any_element());
        segments.push(self.render_status_agents_cluster(cx).into_any_element());
        segments.push(self.render_status_worktrees_cluster().into_any_element());
        render_status_segment_row(segments).into_any_element()
    }

    /// The transient "keeping all changes…"/"discarded …"/"undo failed: …" feedback from
    /// `AdeApp::keep_all_changes`/`AdeApp::execute_discard_worktree`/`AdeApp::perform_undo`/
    /// `AdeApp::perform_redo` (`worktree_history::flow`, Revision R10) - `None` (nothing rendered
    /// at all) whenever [`AdeApp::worktree_history_status`] is `None`.
    ///
    /// Deliberately shown here, in the status bar, rather than the rail footer's own status slot
    /// (`Self::render_rail_footer`) - an audit found two real problems with that shared slot:
    /// [`AdeApp::prune_status`] took priority there and is never cleared once set (see that
    /// field's own docs), so a single prune click permanently hid every future worktree-history
    /// status for the rest of the agent; and the rail footer disappears entirely while Settings
    /// is open (`AdeApp::render_workspace_body` isn't called - `root/mod.rs`'s `Render` impl
    /// swaps it out for `Self::render_settings`), leaving *no* real status surface at all for a
    /// keybinding-triggered `Undo`/`Redo`. The status bar is rendered as an unconditional sibling
    /// of that swap, so it stays visible either way.
    ///
    /// Long, load-bearing text (`Error::DiscardRemovalFailedAfterStash`'s real stash id,
    /// `Error::HeadMovedSinceRecorded`'s two full 40-character commit shas, ...) is truncated
    /// with an ellipsis and carries a real tooltip ([`text_tooltip`]) with the untruncated text -
    /// an audit found this exact text rendered with no truncation or tooltip at all before.
    fn render_status_worktree_history_notice(&self) -> Option<gpui::AnyElement> {
        let status = self.worktree_history_status.clone()?;
        Some(
            div()
                .id("status-bar-worktree-history-notice")
                .min_w_0()
                .max_w(px(320.0))
                .truncate()
                .tooltip(text_tooltip(status.clone()))
                .child(self.render_status_text(status))
                .into_any_element(),
        )
    }

    /// Environment chip + real LSP server/error counts, the real file/editor cluster, then the
    /// palette/agent keycap hints - divided the same way as the left side.
    fn render_status_bar_right(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let env_and_servers = div()
            .flex()
            .items_center()
            .gap(px(9.0))
            .child(render_env_chip())
            .child(self.render_status_servers_errors());

        let hints = div()
            .flex()
            .items_center()
            .gap(px(16.0))
            .child(self.render_status_palette_hint(cx))
            .child(self.render_status_agent_hint());

        render_status_segment_row(vec![
            env_and_servers.into_any_element(),
            self.render_status_file_and_editor_cluster(cx)
                .into_any_element(),
            hints.into_any_element(),
        ])
        .into_any_element()
    }

    /// The active agent's real branch name (`self.worktrees`, the same lookup
    /// `Self::render_agent_context_bar` already does - not a second copy) plus its real
    /// `↑ahead ↓behind` from [`Self::ahead_behind_cache`] (populated by the same periodic
    /// `wt_core::diff::ahead_behind_against_base` refresh as [`Self::diff_cache`]). `None` (the
    /// whole cluster hidden) only when there is no active agent at all - a detached `HEAD`
    /// still shows real text (`"(detached)"`), matching the agent context bar's own
    /// convention, and a not-yet-computed ahead/behind is honestly omitted rather than shown as
    /// a fabricated `↑0 ↓0`.
    /// The branch text (design spec change 6: "the branch text became a clickable cluster (fork
    /// glyph + `main` + `↑2 ↓0`, hover `#1b1f22`) that opens the graph tab") - a real click target
    /// via `crate::graph_view::render::AdeApp::open_git_graph`, the same entry point the `+` menu
    /// and the palette's "Open git graph" use.
    fn render_status_branch_cluster(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let agent = self.agents.active()?;
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == agent.cwd)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| "(detached)".to_string());

        let mut row = div()
            .id("status-bar-branch-cluster")
            .debug_selector(|| "status-bar-branch-cluster".to_string())
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .rounded(theme::radius::CHIP)
            .px(px(4.0))
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.open_git_graph(window, cx);
            }))
            .child(crate::graph_view::render::render_graph_tab_chip())
            .child(self.render_status_text(branch));

        if let Some(ahead_behind) = self.ahead_behind_cache.get(&agent.cwd) {
            row = row.child(self.render_status_text(format!(
                "\u{2191}{} \u{2193}{}",
                ahead_behind.ahead, ahead_behind.behind
            )));
        }

        Some(row.into_any_element())
    }

    /// Five real 5×5 radius-1 urgency-counter squares, one per [`Status`] in
    /// [`Status::ORDER`] (amber/red/green/blue/grey), each with a real count aggregated across
    /// every currently open agent - [`rail::urgency_counts`] over [`Self::build_agent_rows`],
    /// the exact same per-agent status classification the rail's own urgency grouping uses
    /// (`Self::agent_status` under the hood), not a second, independent classification.
    ///
    /// Known, low-priority redundancy: [`Self::render_rail_list`] already calls
    /// `build_agent_rows` once earlier in the very same frame, so this is a second, real (if
    /// cheap - no I/O, per that method's own docs) computation of the identical rows, including,
    /// for any [`Status::Ask`] agent, a fresh `Vec<String>` allocation of that agent's whole
    /// visible terminal grid. Not fixed here: both `Self::render_rail`/`Self::render_rail_list`
    /// and this status bar are `&self` methods (not `&mut self`), so caching this frame's rows
    /// on `self` for reuse here would need either widening several rail-render signatures to
    /// `&mut self` or a `RefCell`-style interior-mutability cache - both a more invasive
    /// restructure than this redundant-but-cheap computation is worth fixing in this pass.
    fn render_status_urgency_counters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.build_agent_rows(cx);
        div().flex().items_center().gap(px(8.0)).children(
            rail::urgency_counts(&rows)
                .into_iter()
                .map(|(status, count)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(3.0))
                        .child(
                            div()
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded(px(1.0))
                                .bg(status.color()),
                        )
                        .child(self.render_status_text(count.to_string()))
                }),
        )
    }

    /// `N agents · X% cpu · Y GB` - agent count is a real count of currently open
    /// [`AgentKind::Claude`]/[`AgentKind::Codex`] agents; cpu/mem are the real,
    /// periodically-sampled [`Self::process_stats`] aggregated across those agents' real pids
    /// via [`process_stats::aggregate_process_stats`]. A field with no real sample known yet for
    /// *any* agent shows `...` for that one piece rather than a fabricated `0%`/`0 B` - see that
    /// function's own docs for exactly when that is: a single un-sampleable pid (e.g. a real
    /// zombie mid-EOF-poll) no longer blanks every other agent's genuinely known value.
    ///
    /// `agent_pids` isn't additionally filtered on `TerminalPane::is_running()`: a pid mid-EOF-
    /// poll (the routine "just exited, not yet reaped" case the aggregation fix above targets)
    /// still reports `is_running() == true` for up to `MAX_EOF_POLL_TICKS` by design, so that
    /// filter wouldn't actually exclude the zombie pid this fix cares about. A pid whose agent
    /// has genuinely gone (`is_running() == false`) already has no pid at all
    /// (`TerminalPane::pid` returns `None` once its `agent` is cleared), so it's already
    /// excluded by the `filter_map` below without a separate `is_running` check.
    ///
    /// CPU% is normalized to total real system capacity (0-100%) by dividing the raw,
    /// per-process-ticks-summed aggregate by `std::thread::available_parallelism()`'s real core
    /// count - `aggregate_process_stats` sums each process's own ticks-derived percentage
    /// (itself able to exceed 100% for a single busy multi-threaded process), so without this,
    /// two genuinely busy real agent processes could report a combined `196% cpu`, inconsistent
    /// with a single 0-100%-of-system-capacity scale. `available_parallelism` is a real,
    /// already-available `std` API - no new dependency for this.
    ///
    /// Linux-only (`#[cfg(target_os = "linux")]`, matching `crate::status_bar::process_stats`'s own scope) -
    /// see the `#[cfg(not(target_os = "linux"))]` twin below for the non-Linux build, which omits
    /// the cpu/mem fields entirely rather than showing a `...% cpu` that can never resolve.
    #[cfg(target_os = "linux")]
    fn render_status_agents_cluster(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let agents: Vec<&Agent> = self
            .agents
            .iter()
            .filter(|agent| matches!(agent.kind, AgentKind::Claude | AgentKind::Codex))
            .collect();
        let agent_count = agents.len();
        let agent_pids: Vec<u32> = agents
            .iter()
            .filter_map(|agent| agent.pane.read(cx).pid())
            .collect();

        let (cpu_total, rss_total) =
            process_stats::aggregate_process_stats(&agent_pids, &self.process_stats);
        let cpu_label = match cpu_total {
            Some(percent) => {
                let cores = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1);
                let normalized = process_stats::normalize_cpu_percent(percent, cores);
                format!("{}% cpu", normalized.round() as i64)
            }
            None => "...% cpu".to_string(),
        };
        let mem_label = match rss_total {
            Some(bytes) => rail::format_bytes(bytes),
            None => "...".to_string(),
        };

        self.render_status_text(format!(
            "{agent_count} agents \u{b7} {cpu_label} \u{b7} {mem_label}"
        ))
    }

    /// Non-Linux build: `crate::status_bar::process_stats` has no real sampling to offer on this platform
    /// (cross-platform support is Revision R11, a separate, already-tracked phase) - shows just
    /// the real agent count, omitting the cpu/mem fields entirely rather than a `...% cpu`
    /// placeholder that would sit on screen forever, looking broken, and never resolve.
    #[cfg(not(target_os = "linux"))]
    fn render_status_agents_cluster(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let agent_count = self
            .agents
            .iter()
            .filter(|agent| matches!(agent.kind, AgentKind::Claude | AgentKind::Codex))
            .count();
        self.render_status_text(format!("{agent_count} agents"))
    }

    /// `N wt · Y GB` - both real, both already computed elsewhere: worktree count from
    /// [`Self::worktrees`], disk usage from [`Self::disk_usage_label`] (the same formatted label
    /// `Self::render_rail_footer` shows, via `crate::rail::state::disk_usage_bytes`/
    /// `Self::load_disk_usage` - not a second computation).
    fn render_status_worktrees_cluster(&self) -> impl IntoElement {
        let worktree_count = self.worktrees.len();
        let disk_label = self.disk_usage_label();
        self.render_status_text(format!("{worktree_count} wt \u{b7} {disk_label}"))
    }

    /// `N servers · M errors` - server count is the real number of [`LspClientState::Ready`]
    /// entries in [`Self::lsp_clients`] (per that field's own docs, eviction keeps this to the
    /// active root's own clients only). That is genuinely more than one for a language whose
    /// primary needs a coordinated companion process: a `.vue` file really does have two live
    /// language servers working on it, so `2 servers` there is the honest count, not a
    /// double-count of one. The error count is only ever shown while
    /// [`Self::status_bar_active_parsed_file`] confirms a real file's File view is genuinely on
    /// screen right now (see that method's docs for why) - otherwise just `N servers` is shown,
    /// rather than a stale count left over from a file that isn't even open anymore.
    fn render_status_servers_errors(&self) -> impl IntoElement {
        let server_count = self
            .lsp_clients
            .values()
            .filter(|state| matches!(state, LspClientState::Ready(_)))
            .count();

        let label = match self.status_bar_error_count() {
            Some(errors) => format!("{server_count} servers \u{b7} {errors} errors"),
            None => format!("{server_count} servers"),
        };
        self.render_status_text(label)
    }

    /// Whichever [`code_view::ParsedFile`] is genuinely, currently on screen in Surface C's File
    /// view - `None` whenever that's not true (Settings is open and covering the whole
    /// workspace, an agent tab is active, the Diff view is showing, or
    /// [`Self::file_view_cache`] hasn't caught up with a just-opened file yet). The shared gate
    /// for every status-bar field that only makes sense for a real, currently-viewed file
    /// (`ln N`, indent width, line-ending, `UTF-8`, the diagnostics error count) - without it,
    /// switching away from a file view would leave those fields silently describing a file
    /// that's no longer open, since [`Self::file_view_cache`]/[`Self::file_view_diagnostics`]
    /// are only ever refreshed by `Self::render_file_view` itself and nothing clears them on
    /// tab switch (mirrors how `Self::render_file_view`'s own status bar simply isn't called at
    /// all outside the File view - the same "don't render an inapplicable field" convention,
    /// applied here per-field instead of for a whole surface).
    ///
    /// The `settings_open` check comes first, and deliberately can't be skipped by any of the
    /// checks below it: [`Self::render_status_bar`] is rendered as an unconditional sibling of
    /// whichever body is currently swapped in (`root/mod.rs`'s `Render` impl) - the title bar
    /// and status bar stay on screen while Settings replaces the three-zone workspace body, so
    /// without this check every file-specific field here would keep showing a frozen snapshot of
    /// whatever file was open right before Settings opened, for a file the user can no longer
    /// even see.
    fn status_bar_active_parsed_file(&self) -> Option<&code_view::ParsedFile> {
        if self.settings_open {
            return None;
        }
        if self.code_view != code_view::CodeView::File {
            return None;
        }
        let open_path = self.open_change.as_ref()?;
        let absolute = self.file_tree_root.join(open_path);
        self.file_view_cache
            .as_ref()
            .filter(|cached| cached.path == absolute)
    }

    /// Real error-severity diagnostic count for the file [`Self::status_bar_active_parsed_file`]
    /// confirms is genuinely on screen - `None` (not `Some(0)`) when that isn't true, so
    /// [`Self::render_status_servers_errors`] can tell "genuinely zero errors in this open file"
    /// apart from "not applicable right now".
    ///
    /// Reads [`Self::file_view_error_count`] directly rather than re-deriving a count from
    /// [`Self::file_view_diagnostics`]: that map is `diagnostics_view::index_diagnostics_by_line`'s
    /// per-*line* index, which pushes one entry for every line a diagnostic's range touches, not
    /// one entry per diagnostic - counting it directly would over-count any diagnostic spanning
    /// more than one line (and under-count one past a truncated file's parsed line range).
    /// [`Self::file_view_error_count`] is instead the exact same real count
    /// `code_surface::file_view::render_file_status_bar` already computed and displayed for this same file
    /// in this same frame, so the two can never disagree.
    fn status_bar_error_count(&self) -> Option<usize> {
        self.status_bar_active_parsed_file()?;
        self.file_view_error_count
    }

    /// `ln N` · `N spaces` · `LF`/`CRLF` · `UTF-8` (each shown only while
    /// [`Self::status_bar_active_parsed_file`] confirms a real file is on screen - `ln N` has
    /// the further real precondition that [`Self::code_cursor`] has ever been set, per
    /// `Self::render_file_status_bar`'s own established "no column tracking, so no fabricated
    /// `col 1`" convention, extended here the same way to "no click yet, so no fabricated `ln
    /// 1`") · the real, clickable editor-zoom value (reuses [`Self::reset_zoom`], not a second
    /// zoom mechanism) · the real `UI N%` interface-scale readout. Zoom and UI-scale are always
    /// shown, independent of whether a file is open - both are real, currently-effective values
    /// regardless of what's on screen. The `UTF-8` label reads
    /// [`code_view::ParsedFile::is_valid_utf8`] - a genuinely lossily-decoded file (real bytes
    /// that weren't valid UTF-8) shows `UTF-8 (lossy)` instead, never a hardcoded claim about
    /// bytes that were never actually checked.
    fn render_status_file_and_editor_cluster(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let parsed = self.status_bar_active_parsed_file();

        let mut row = div().flex().items_center().gap(px(9.0));

        if let Some(parsed) = parsed {
            if let Some(line) = self.code_cursor {
                row = row.child(self.render_status_text(format!("ln {line}")));
            }
            if let Some(width) = code_view::detect_indent_width(&parsed.lines) {
                row = row.child(self.render_status_text(format!("{width} spaces")));
            }
            row = row.child(self.render_status_text(parsed.line_ending.label().to_string()));
            // A real, checked value (`code_view::ParsedFile::is_valid_utf8`), not a hardcoded
            // literal - a file whose real bytes weren't valid UTF-8 (and so were lossily
            // decoded, real content silently replaced with `U+FFFD`) says so honestly rather
            // than falsely claiming `UTF-8`.
            let encoding_label = if parsed.is_valid_utf8 {
                "UTF-8".to_string()
            } else {
                "UTF-8 (lossy)".to_string()
            };
            row = row.child(self.render_status_text(encoding_label));
        }

        row.child(self.render_status_zoom_value(cx))
            .child(self.render_status_text(format!(
                "UI {}%",
                self.settings.appearance.interface_scale_percent
            )))
    }

    /// The real editor-zoom value, clickable to reset to 100% - reuses [`Self::reset_zoom`]
    /// (the exact mechanism the code toolbar's own `Self::render_zoom_control` uses), so this is
    /// a second *view* of the same real state, never a second zoom implementation. A distinct
    /// element id (`"status-bar-zoom-value"`) from the toolbar's own `"code-zoom-value"`, since
    /// both can be on screen in the same frame.
    fn render_status_zoom_value(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-bar-zoom-value")
            // Test-only (no-op outside `cfg(test)`/`test-support`) - lets a real interaction
            // test find this element's painted bounds via `VisualTestContext::debug_bounds` and
            // drive a genuine `simulate_click` against it, the same idiom
            // `code_surface`'s own click-driven tests use.
            .debug_selector(|| "status-bar-zoom-value".to_string())
            .cursor_pointer()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(self.ui_text_size(10.0))
            .text_color(theme::text::DIM)
            .hover(|el| el.text_color(theme::text::SELECTED))
            .child(format!("{}%", self.settings.appearance.editor_zoom_percent))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.reset_zoom(cx);
            }))
    }

    /// The `⌘P commands` hint: clicking it (or pressing the bound `secondary-p` - see
    /// [`TogglePalette`]) opens the command palette.
    fn render_status_palette_hint(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("status-bar-open-palette")
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(render_keycap_row(
                &keymap::resolve_combo("mod+P", self.window_controls_style().is_macos()),
                KeycapSize::Standard,
            ))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::FAINT)
                    .child("commands"),
            )
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.open_palette(window, cx);
            }))
    }

    /// The real agent-jump keycap hint (`secondary-1`..`secondary-8`) - reuses
    /// [`Self::agent_jump_keys`], the exact same computation `Self::render_tab_strip`'s own
    /// right-aligned cluster uses, not a second copy that could drift from what's really bound.
    fn render_status_agent_hint(&self) -> impl IntoElement {
        let jump_keys = self.agent_jump_keys();
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(render_keycap_row(&jump_keys, KeycapSize::Standard))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::FAINT)
                    .child("agent"),
            )
    }

    /// The plain 10px mono `theme::text::DIM` (`#8b9197`) style shared by every text-only
    /// status-bar value (`CHANGELOG.md`: "all values 10px mono") - matches the zoom value two
    /// elements over ([`Self::render_status_zoom_value`]), which already correctly uses the same
    /// token.
    fn render_status_text(&self, label: String) -> impl IntoElement {
        div()
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(10.0))
            .text_color(theme::text::DIM)
            .child(label)
    }
}

/// Lays out already-built segments with a real 1px vertical divider between each consecutive
/// pair - segments that don't apply right now are simply never pushed into the `Vec` by the
/// caller, so no divider ever appears next to a missing field (`CHANGELOG.md`'s explicit
/// `divider` markers only ever separate segments that are always real and present).
fn render_status_segment_row(segments: Vec<gpui::AnyElement>) -> impl IntoElement {
    let mut row = div().flex().items_center().gap(px(9.0));
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            row = row.child(render_status_divider());
        }
        row = row.child(segment);
    }
    row
}

fn render_status_divider() -> impl IntoElement {
    div()
        .flex_none()
        .w(px(1.0))
        .h(px(14.0))
        .bg(theme::border::DIVIDER)
}

/// A real interaction test proving the status bar's editor-zoom value is genuinely clickable and
/// resets zoom through [`AdeApp::reset_zoom`] - the exact same real mechanism the code toolbar's
/// own zoom control (`code_surface::zoom::render_zoom_control`) uses, never a second, copied
/// implementation.
#[cfg(test)]
mod status_bar_zoom_click_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn clicking_the_status_bar_zoom_value_resets_a_real_non_default_zoom_to_one_hundred(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // Move zoom away from the default via the real toolbar action, so this test can prove
        // a genuine reset rather than a value that already happened to be 100%.
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            120,
            "sanity check: two real zoom-in steps from 100% land on 120%"
        );

        let bounds = cx
            .debug_bounds("status-bar-zoom-value")
            .expect("the status bar's zoom value must have painted at least once");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "a real click on the status bar's zoom value must reset zoom to 100%, through the \
             same AdeApp::reset_zoom the toolbar's own zoom control calls"
        );
    }
}

/// Regression coverage for the `N servers · M errors` over-counting bug: a real single
/// diagnostic whose range spans three lines must count as one error in the status bar, matching
/// the File view's own footer (`code_surface::file_view::render_file_status_bar`) - not once per line the
/// per-line diagnostic index (`diagnostics_view::index_diagnostics_by_line`) happens to record
/// it on.
#[cfg(test)]
mod status_bar_error_count_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use lsp_core::lsp_types;

    /// A real diagnostic spanning three lines (start line 0 through end line 2) - the exact
    /// shape a real multi-line rustc error produces.
    fn three_line_error() -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 2,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("rustc".to_string()),
            message: "mismatched types".to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[gpui::test]
    fn a_real_multiline_diagnostic_counts_once_not_once_per_touched_line(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("broken.rs");
        std::fs::write(
            &file_path,
            "fn main() {\n    let x: i32 =\n        \"nope\";\n}\n",
        )
        .expect("write broken.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let parsed = code_view::load_file(&file_path).expect("load_file");
        let diagnostics = vec![three_line_error()];
        let by_line = diagnostics_view::index_diagnostics_by_line(&diagnostics, &parsed.lines);
        // Sanity check on the real per-line index this bug came from: a single diagnostic
        // touching three lines really does produce three separate per-line entries - proving
        // that summing it directly (the old, buggy approach) really would over-count.
        assert_eq!(
            by_line.values().flatten().count(),
            3,
            "sanity check: the per-line index really does record one entry per touched line, \
             for a single real diagnostic"
        );

        app.update(cx, |app, _cx| {
            app.code_view = code_view::CodeView::File;
            app.open_change = Some(PathBuf::from("broken.rs"));
            app.file_view_cache = Some(parsed);
            // What `render_file_view` genuinely writes alongside a real
            // `LspFileStatus::Analyzed { errors: 1, .. }` for this exact diagnostic list: one
            // real error, not three - the same real count the File view's own footer displays.
            app.file_view_diagnostics = by_line;
            app.file_view_error_count = Some(1);
        });
        cx.run_until_parked();

        let error_count = app.read_with(cx, |app, _| app.status_bar_error_count());
        assert_eq!(
            error_count,
            Some(1),
            "a single diagnostic spanning three lines must count once in the status bar, \
             matching the File view's own footer - not once per line it touches"
        );
    }

    /// The status bar's file-specific fields (including the error count) must disappear while
    /// Settings covers the whole workspace, rather than keep showing a frozen snapshot of
    /// whatever file was open right before Settings opened.
    #[gpui::test]
    fn file_specific_fields_disappear_while_settings_is_open(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("broken.rs");
        std::fs::write(&file_path, "fn main() {\n    let x = 1;\n}\n").expect("write broken.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let parsed = code_view::load_file(&file_path).expect("load_file");
        app.update(cx, |app, _cx| {
            app.code_view = code_view::CodeView::File;
            app.open_change = Some(PathBuf::from("broken.rs"));
            app.file_view_cache = Some(parsed);
            app.file_view_error_count = Some(2);
            app.code_cursor = Some(1);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.status_bar_active_parsed_file().is_some()),
            "sanity check: with Settings closed, a real, matching file view is genuinely active"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.status_bar_error_count()),
            Some(2),
            "sanity check: the error count is genuinely shown before Settings opens"
        );

        cx.dispatch_action(ToggleSettings);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.settings_open),
            "sanity check: Settings genuinely opened"
        );
        assert!(
            app.read_with(cx, |app, _| app.status_bar_active_parsed_file().is_none()),
            "the file-specific gate must return None while Settings covers the whole workspace"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.status_bar_error_count()),
            None,
            "the error count must disappear while Settings is open, not keep showing a frozen \
             snapshot of the file that was open before Settings covered the workspace"
        );
    }
}
