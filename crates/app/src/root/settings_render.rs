use super::*;
use crate::root::settings_widgets::ChoiceOption;
use crate::root::widgets::{render_env_chip, render_keycap_row, KeycapSize};

impl AdeApp {
    pub(super) fn handle_toggle_settings_action(
        &mut self,
        _action: &ToggleSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open {
            self.close_settings(window, cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    /// The Settings surface's own key handler - just `Esc`-to-close
    /// (`design_handoff_jerry_ade/README.md`: "esc ... returns to the workspace"). Nav is
    /// click-only, so unlike [`Self::handle_palette_key_down`] this needs no arrow-key/tab
    /// handling.
    pub(super) fn handle_settings_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key.as_str() == "escape" {
            self.close_settings(window, cx);
            cx.stop_propagation();
        }
    }

    /// Selects a Settings nav page - the nav row click handler. Cancels any in-progress
    /// keybinding recording first - see [`Self::close_settings`]'s identical reasoning for why a
    /// live `App::intercept_keystrokes` subscription must never outlive the page it started on.
    pub(super) fn select_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.cancel_keybinding_recording(cx);
        self.settings_page = page;
        cx.notify();
    }

    /// Recomputes [`Self::agent_rows`] via `crate::settings::detect_agent_rows`, offloaded to
    /// the background executor and cached, mirroring [`Self::load_disk_usage`]'s shape: a
    /// not-found `resolve_on_path` call walks every `$PATH` entry with no early exit, so running
    /// it inline in `render()` would block the foreground/GPUI thread on every frame the Agents
    /// page is open. Run once when Settings opens ([`Self::open_settings`]), not on every render
    /// or on the 3s status-poll cadence - the set of binaries on `$PATH` essentially never
    /// changes while the app is running.
    pub(super) fn load_agent_rows(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move { settings::detect_agent_rows(pty_core::resolve_on_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.agent_rows = rows;
                cx.notify();
            });
        });
        self._agent_rows_task = Some(task);
    }

    /// Recomputes [`Self::lsp_rows`] via `crate::settings::detect_lsp_rows`, mirroring
    /// [`Self::load_agent_rows`]'s shape and reasoning exactly.
    pub(super) fn load_lsp_rows(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move { settings::detect_lsp_rows(pty_core::resolve_on_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.lsp_rows = rows;
                cx.notify();
            });
        });
        self._lsp_rows_task = Some(task);
    }

    /// The Settings surface (`design_handoff_jerry_ade/README.md`'s "Settings" section): a
    /// 212px nav plus a content column. `track_focus`/`on_key_down` here are what make `Esc`
    /// actually reach [`Self::handle_settings_key_down`] - the same pattern `Self::render_palette`
    /// uses for its own panel (`vendor/zed/crates/gpui/src/elements/div.rs`'s `Div::track_focus`/
    /// `Interactivity::on_key_down`).
    pub(super) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-surface")
            .track_focus(&self.settings_focus_handle)
            .on_key_down(cx.listener(Self::handle_settings_key_down))
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_settings_nav(cx))
            .child(self.render_settings_content(cx))
    }

    /// The 212px nav column - `design_handoff_jerry_ade/revision/README.md`: "Nav 212 wide ...
    /// Groups (Workspace, Interface, Editor, Other) with the same 9.5px uppercase header as the
    /// rail." All eleven pages are clickable navigation (`crate::settings::nav_groups`); seven
    /// render real content past this point - see `crate::settings`'s module docs.
    pub(super) fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = settings::nav_groups();
        // Real counts, not the mockup's fabricated badges.
        let agent_count = settings::AGENT_KINDS.len();
        let worktree_count = self.worktrees.len();

        div()
            .id("settings-nav")
            .flex_none()
            .w(theme::zone::SETTINGS_NAV_WIDTH)
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::surface::RAIL)
            .border_r_1()
            .border_color(theme::border::ZONE)
            .child(
                div()
                    .flex_none()
                    .h(theme::band::RAIL_HEADER)
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(12.0))
                    .pr(px(10.0))
                    .border_b_1()
                    .border_color(theme::border::RAIL_INNER)
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINT)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                                this.close_settings(window, cx);
                            }))
                            .child(render_keycap_row(
                                &keymap::resolve_combo(
                                    "esc",
                                    self.window_controls_style().is_macos(),
                                ),
                                KeycapSize::Standard,
                            )),
                    ),
            )
            .child(
                div()
                    .id("settings-nav-groups")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(px(6.0))
                    .flex()
                    .flex_col()
                    .children(groups.into_iter().map(|group| {
                        self.render_settings_nav_group(group, agent_count, worktree_count, cx)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .h(theme::band::SURFACE_FOOTER)
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .border_t_1()
                    .border_color(theme::border::RAIL_INNER)
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::HINT)
                            // Real crate name/version (`env!` reads this crate's own
                            // `Cargo.toml` at compile time), not Jerry.dc.html's fabricated
                            // "jerry 0.4.2".
                            .child(format!(
                                "{} {} \u{b7} settings.toml",
                                env!("CARGO_PKG_NAME"),
                                env!("CARGO_PKG_VERSION"),
                            )),
                    ),
            )
    }

    pub(super) fn render_settings_nav_group(
        &self,
        group: settings::NavGroup,
        agent_count: usize,
        worktree_count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut el = div()
            .id(format!("settings-nav-group-{}", group.label))
            .flex()
            .flex_col()
            .pb(px(4.0))
            .child(
                div()
                    .px(px(12.0))
                    .pt(px(7.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child(group.label.to_uppercase()),
            );

        for page in group.pages {
            let badge = match page {
                SettingsPage::Agents => Some(agent_count.to_string()),
                SettingsPage::Worktrees => Some(worktree_count.to_string()),
                // Live counts, not Jerry.dc.html's fabricated sample badges (Keybindings' mockup
                // `48` doesn't match this app's real, smaller count - see
                // `crate::settings::keybinding_rows`'s own docs).
                SettingsPage::Theme => Some(settings::THEME_DEFS.len().to_string()),
                SettingsPage::Keymap => Some(
                    settings::keybinding_rows(
                        &crate::default_key_bindings(),
                        &self.settings.keymap.overrides,
                    )
                    .len()
                    .to_string(),
                ),
                SettingsPage::LanguageServers => Some(settings::lsp_languages().len().to_string()),
                // Every other page has nothing real to count - omitted rather than invented.
                _ => None,
            };
            el = el.child(self.render_settings_nav_row(page, badge, cx));
        }
        el
    }

    pub(super) fn render_settings_nav_row(
        &self,
        page: SettingsPage,
        badge: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.settings_page == page;

        div()
            .id(format!("settings-nav-row-{}", page.id()))
            .cursor_pointer()
            .h(px(25.0))
            .pl(px(10.0))
            .pr(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_l(px(2.0))
            .border_color(if active {
                theme::border::SELECTED_EDGE.into()
            } else {
                work_surface::TRANSPARENT
            })
            .when(active, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!active, |el| {
                el.hover(|el| el.bg(theme::settings::NAV_ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_settings_page(page, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::SANS))
                    // `Self::ui_text_size`, not a literal `px(11.5)` - see that method's docs.
                    .text_size(self.ui_text_size(11.5))
                    .text_color(if active {
                        theme::text::SELECTED
                    } else {
                        theme::text::DIM
                    })
                    .child(page.label()),
            )
            .when_some(badge, |el, badge| {
                el.child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(px(9.5))
                        .text_color(theme::text::GHOSTER)
                        .child(badge),
                )
            })
    }

    /// The content column: header block (title + subtitle) plus whichever page's real (or
    /// honestly placeholder) body - `design_handoff_jerry_ade/revision/README.md`'s "Content
    /// column" section. Header and scrollable body are both capped at
    /// `theme::zone::SETTINGS_CONTENT_MAX_WIDTH` (700px), left-aligned inside the 26px padding -
    /// matching `Jerry.dc.html`'s own `max-width:700px` wrapper.
    pub(super) fn render_settings_content(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.settings_page;

        div()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme::surface::CENTER)
            .child(
                div()
                    .flex_none()
                    .px(px(26.0))
                    .pt(px(18.0))
                    .pb(px(14.0))
                    .border_b_1()
                    .border_color(theme::border::INNER)
                    .child(
                        div()
                            .w_full()
                            .max_w(theme::zone::SETTINGS_CONTENT_MAX_WIDTH)
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font(font(theme::font::SANS))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_size(self.ui_text_size(15.0))
                                    .text_color(theme::text::SELECTED)
                                    .child(page.label()),
                            )
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .font(font(theme::font::SANS))
                                    .text_size(self.ui_text_size(11.5))
                                    .text_color(theme::settings::SUBTITLE)
                                    .child(page.subtitle()),
                            ),
                    ),
            )
            .child(
                div()
                    .id("settings-content-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(26.0))
                    .pb(px(20.0))
                    .child(
                        div()
                            .w_full()
                            .max_w(theme::zone::SETTINGS_CONTENT_MAX_WIDTH)
                            .child(match page {
                                SettingsPage::General => {
                                    self.render_settings_general_page(cx).into_any_element()
                                }
                                SettingsPage::Agents => {
                                    self.render_settings_agents_page(cx).into_any_element()
                                }
                                SettingsPage::Worktrees => {
                                    self.render_settings_worktrees_page(cx).into_any_element()
                                }
                                SettingsPage::Appearance => {
                                    self.render_settings_appearance_page(cx).into_any_element()
                                }
                                SettingsPage::Theme => {
                                    self.render_settings_theme_page(cx).into_any_element()
                                }
                                SettingsPage::Keymap => {
                                    self.render_settings_keymap_page(cx).into_any_element()
                                }
                                SettingsPage::LanguageServers => {
                                    self.render_settings_lsp_page(cx).into_any_element()
                                }
                                _ => render_settings_placeholder_page().into_any_element(),
                            }),
                    ),
            )
    }

    /// *Agents › Installed* - `design_handoff_jerry_ade/README.md`: "bordered card ... of four
    /// rows ... agent badge ... name ... binary path ... model ... a `default` pill ... green
    /// dot + 'ready' ... Edit." This app drops the `model`/`default`/`Edit` pieces (see
    /// `crate::settings`'s module docs for why) and shows [`settings::AGENT_KINDS`]'s two real
    /// rows instead of the mockup's four fabricated ones, each with a live PATH-derived status.
    pub(super) fn render_settings_agents_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = &self.agent_rows;
        let last_index = rows.len().saturating_sub(1);

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Installed"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(theme::radius::CARD)
                    .border_1()
                    .border_color(theme::border::CARD)
                    .overflow_hidden()
                    .children(rows.iter().enumerate().map(|(index, row)| {
                        self.render_settings_agent_row(row, index == last_index, cx)
                    }))
                    .child(self.render_settings_agents_footer()),
            )
    }

    pub(super) fn render_settings_agent_row(
        &self,
        row: &settings::AgentRow,
        is_last: bool,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (badge_fg, badge_bg) = work_surface::agent_tint(row.kind);
        let path_text = match &row.resolved_path {
            Some(path) => path.display().to_string(),
            // The exact reason a "ready" dot isn't shown, not just "unknown"/blank.
            None => format!("{} not found on PATH", row.binary_name),
        };
        let dot_color = if row.is_ready() {
            theme::settings::AGENT_READY
        } else {
            theme::settings::AGENT_NOT_FOUND
        };

        div()
            .id(format!("settings-agent-row-{}", row.binary_name))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(9.0))
            .bg(theme::surface::CARD)
            .when(!is_last, |el| {
                el.border_b_1().border_color(theme::settings::CARD_ROW_SEP)
            })
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .h(px(18.0))
                    .rounded(theme::radius::BUTTON)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(badge_bg)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(9.5))
                    .text_color(badge_fg)
                    .child(work_surface::agent_initial(row.kind)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(104.0))
                    .font(font(theme::font::SANS))
                    .text_size(px(12.0))
                    .text_color(theme::text::HEADING)
                    .child(row.kind.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(if row.is_ready() {
                        theme::text::FAINT
                    } else {
                        theme::button::DANGER_FG
                    })
                    .child(path_text),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(div().w(px(5.0)).h(px(5.0)).rounded(px(2.5)).bg(dot_color))
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(row.status_label()),
                    ),
            )
    }

    /// The Installed card's footer - `design_handoff_jerry_ade/README.md`: "Card footer ...
    /// '+ Add an agent — any binary that speaks a resumable session on stdin'." Rendered dimmed
    /// and inert (no `on_click`) - `crate::sessions::SessionKind` is a fixed Rust enum, so there
    /// is no runtime "register a new agent binary" flow to wire this to yet.
    pub(super) fn render_settings_agents_footer(&self) -> impl IntoElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .py(px(8.0))
            .bg(theme::surface::CARD_SUNK)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(12.0))
                    .text_color(theme::text::DISABLED)
                    .child("+"),
            )
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(theme::text::DISABLED)
                    .child("Add an agent"),
            )
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child("\u{2014} any binary that speaks a resumable session on stdin"),
            )
    }

    /// *Worktrees › Disk* - `design_handoff_jerry_ade/README.md`: "same card shape: status dot
    /// ... worktree path ... branch ... size ... a right-aligned Open ... or Prune ...
    /// action. Footer totals ... and a Prune 1 merged action." Every row and total here reads
    /// existing state (`Self::worktrees`, `Self::worktree_notes`, `Self::worktree_disk_usage`/
    /// `Self::disk_usage`), and Prune - both the row action and the footer action - dispatches
    /// through the same `Self::request_prune`/`Self::execute_prune` two-click-confirmation path
    /// the rail footer and command palette use (see [`Self::render_settings_worktree_row`]'s
    /// docs for why a row's Prune click isn't scoped to only that one row).
    pub(super) fn render_settings_worktrees_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let last_index = self.worktrees.len().saturating_sub(1);
        let prunable_count = self.prunable_worktree_paths().len();
        let disk_label = match self.disk_usage {
            Some((bytes, truncated)) => {
                let label = rail::format_bytes(bytes);
                if truncated {
                    format!("{label}+")
                } else {
                    label
                }
            }
            None => "...".to_string(),
        };
        let worktree_count = self.worktrees.len();
        let prune_label = if self.prune_confirm_armed {
            format!("confirm prune ({prunable_count})?")
        } else {
            format!("Prune {prunable_count} merged")
        };

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Disk"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(theme::radius::CARD)
                    .border_1()
                    .border_color(theme::border::CARD)
                    .overflow_hidden()
                    .children(self.worktrees.iter().enumerate().map(|(index, item)| {
                        self.render_settings_worktree_row(item, index == last_index, cx)
                    }))
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .px(px(12.0))
                            .py(px(8.0))
                            .bg(theme::surface::CARD_SUNK)
                            .child(
                                div()
                                    .flex_1()
                                    .font(font(theme::font::MONO))
                                    .text_size(px(10.5))
                                    .text_color(theme::text::FAINTER)
                                    .child(format!(
                                        "{worktree_count} worktrees \u{b7} {disk_label}"
                                    )),
                            )
                            .child(
                                div()
                                    .id("settings-prune-all-merged")
                                    .cursor_pointer()
                                    .font(font(theme::font::SANS))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_size(px(10.5))
                                    .text_color(if prunable_count > 0 {
                                        theme::button::DANGER_FG
                                    } else {
                                        theme::text::DISABLED
                                    })
                                    .hover(|el| el.text_color(theme::button::DANGER_FG_HOVER))
                                    .child(prune_label)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, _window, cx| {
                                            this.request_prune(cx);
                                        },
                                    )),
                            ),
                    ),
            )
    }

    /// One Worktrees-page row. `Open` selects that worktree in the workspace and switches back
    /// to it (`Self::select_worktree_by_path` + `Self::close_settings`). `Prune` deliberately
    /// calls the same [`Self::request_prune`] the footer's `Prune N merged` button and the
    /// command palette's `Prune Worktrees` command call: there is no "prune only this one
    /// worktree" code path, since the one safety-checked removal primitive
    /// (`Self::prunable_worktree_paths` + `Self::execute_prune`) always operates on every
    /// currently-prunable worktree at once, live-session-excluded. A row's `Prune` button only
    /// shows when that row's own worktree is one of those candidates
    /// (`settings::worktree_row_action`), so clicking it always includes this worktree - it just
    /// isn't scoped to *only* this worktree if others are also prunable at the same moment.
    pub(super) fn render_settings_worktree_row(
        &self,
        item: &WorktreeItem,
        is_last: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row = div()
            .id(format!("settings-worktree-row-{}", item.path.display()))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(8.0))
            .bg(theme::surface::CARD)
            .when(!is_last, |el| {
                el.border_b_1().border_color(theme::settings::CARD_ROW_SEP)
            });

        if let Some(error) = &item.error {
            return row
                .child(
                    div()
                        .flex_none()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded(px(2.5))
                        .bg(theme::status::FAIL),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::status::FAIL)
                        .child(error.clone()),
                );
        }

        let note = self.worktree_notes.get(&item.path);
        let dot_color = match note.map(|note| settings::worktree_dot_status(item.is_main, note)) {
            Some(settings::WorktreeDotStatus::Main) => theme::status::IDLE,
            Some(settings::WorktreeDotStatus::Clean) => theme::status::REVIEW,
            Some(settings::WorktreeDotStatus::Dirty) => theme::status::ASK,
            Some(settings::WorktreeDotStatus::Prunable) => theme::settings::WORKTREE_PRUNABLE_DOT,
            Some(settings::WorktreeDotStatus::Unknown) | None => theme::text::DISABLED,
        };
        let branch_label = item
            .branch
            .clone()
            .unwrap_or_else(|| "(detached)".to_string());
        let size_label = match self.worktree_disk_usage.get(&item.path) {
            Some((bytes, truncated)) => {
                let label = rail::format_bytes(*bytes);
                if *truncated {
                    format!("{label}+")
                } else {
                    label
                }
            }
            None => "...".to_string(),
        };
        let action = note.map(|note| settings::worktree_row_action(item.is_main, note));
        let path = item.path.clone();

        let row = row
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded(px(2.5))
                    .bg(dot_color),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(196.0))
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::STRONG)
                    .child(item.path.display().to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::FAINT)
                    .child(branch_label),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    .child(size_label),
            );

        match action {
            Some(settings::WorktreeRowAction::Open) => row.child(
                div()
                    .id(format!("settings-worktree-open-{}", path.display()))
                    .cursor_pointer()
                    .flex_none()
                    .w(px(74.0))
                    .text_right()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.5))
                    .text_color(theme::text::FAINT)
                    .hover(|el| el.text_color(theme::text::SECONDARY))
                    .child("Open")
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.select_worktree_by_path(&path, window, cx);
                        this.close_settings(window, cx);
                    })),
            ),
            Some(settings::WorktreeRowAction::Prune) => row.child(
                div()
                    .id(format!("settings-worktree-prune-{}", path.display()))
                    .cursor_pointer()
                    .flex_none()
                    .w(px(74.0))
                    .text_right()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.5))
                    .text_color(theme::button::DANGER_FG)
                    .hover(|el| el.text_color(theme::button::DANGER_FG_HOVER))
                    .child("Prune")
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.request_prune(cx);
                    })),
            ),
            Some(settings::WorktreeRowAction::None) | None => {
                row.child(div().flex_none().w(px(74.0)))
            }
        }
    }

    /// *General* - `Window controls` as a segmented `System | macOS | Windows/Linux` choice,
    /// wired live (`CHANGELOG.md`'s change 3) - see `Self::window_controls_style`'s own docs for
    /// how both this row and the command palette's three `Window controls: …` entries read/write
    /// the same persisted field. `Default environment` shows the real, live-detected
    /// `crate::env_info`/`crate::root::widgets::render_env_chip` environment chip (real WSL
    /// detection - Revision R6's job, per the doc comment this replaced) - the same chip the
    /// status bar and terminal footer render, not a fourth copy.
    ///
    /// `Restore sessions on launch` and `Confirm before discarding a worktree` - two more rows
    /// `Jerry.dc.html`'s own `settingsRows.general` fixture shows - stay left out for the same
    /// reason as the Agents/Worktrees toggle sections (see `crate::settings`'s module docs):
    /// session-restore-on-launch and a discard-confirmation flow are app behaviour this build
    /// doesn't have, not settings plumbing around behaviour that already exists.
    pub(super) fn render_settings_general_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.window_controls_style().label().to_string();
        let choice = self.render_choice_control(
            "settings-window-controls",
            &[
                ChoiceOption::new("System"),
                ChoiceOption::new("macOS"),
                ChoiceOption::new("Windows/Linux"),
            ],
            selected,
            cx,
            |this, index, cx| {
                // Index into the `options` array above, not a label re-match - see
                // `Self::render_choice_control`'s docs for why.
                let style = match index {
                    1 => WindowControlsStyle::MacosStyle,
                    2 => WindowControlsStyle::WindowsLinuxStyle,
                    _ => WindowControlsStyle::System,
                };
                this.set_window_controls_style(style, cx);
            },
        );
        let window_controls_row = self.render_settings_row(
            "Window controls",
            "Traffic lights on macOS, caption buttons on Windows and Linux. Follows the \
             platform unless you pin it - this switches live.",
            choice,
        );
        let environment_row = self.render_settings_row(
            "Default environment",
            "Where new sessions run - real WSL detection on Windows, real CPU architecture \
             elsewhere. The same chip shown in the status bar and terminal footer.",
            render_env_chip(),
        );

        div()
            .flex()
            .flex_col()
            .child(self.render_config_banner(settings_store::ConfigPage::General, cx))
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Window & launch"),
            )
            .child(window_controls_row)
            .child(environment_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::General))
    }

    /// *Appearance & scaling* - every row here is persisted and round-trips through
    /// [`Self::settings`] (`CHANGELOG.md`'s change 3). This page is itself a live consumer of
    /// `interface_scale_percent` (`Self::ui_text_size`, applied to its own labels, hints, *and*
    /// every row's control - stepper value, choice-segment labels, config banner/snippet block -
    /// see `crate::root::settings_widgets`'s module docs), so editing the choice control below
    /// visibly rescales this page's own text, not just its four preview cards.
    ///
    /// Only *text* sizes respond, by deliberate scope - `theme::ui_scale`'s module docs carry
    /// the current list of which surfaces read this setting and which don't (kept there, not
    /// duplicated here). `editor_font_size`/`terminal_font_size` are separately-applied
    /// baselines for Surface C's zoom (`Self::effective_code_rem_px`) and `crate::terminal_pane`
    /// respectively, distinct from the interface-scale multiplier above them.
    pub(super) fn render_settings_appearance_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_percent = self.settings.appearance.interface_scale_percent;

        let preview = div().flex().gap(px(8.0)).children(
            [90u16, 100, 110, 125]
                .into_iter()
                .map(|percent| self.render_appearance_preview_card(percent, selected_percent, cx)),
        );

        let scale_choice = self.render_choice_control(
            "settings-interface-scale",
            &[
                ChoiceOption::new("90%"),
                ChoiceOption::new("100%"),
                ChoiceOption::new("110%"),
                ChoiceOption::new("125%"),
            ],
            format!("{selected_percent}%"),
            cx,
            |this, index, cx| {
                // Index into the `options` array above, not a label re-match/parse.
                const PERCENTS: [u16; 4] = [90, 100, 110, 125];
                if let Some(percent) = PERCENTS.get(index).copied() {
                    this.set_interface_scale_percent(percent, cx);
                }
            },
        );
        let editor_font_row = self.render_settings_row(
            "Editor font size",
            "The code surface's toolbar zoom multiplies this baseline; both are saved globally.",
            self.render_stepper_control(
                "settings-editor-font",
                format!("{:.0} px", self.settings.appearance.editor_font_size),
                cx,
                |this, cx| this.adjust_editor_font_size(-1.0, cx),
                |this, cx| this.adjust_editor_font_size(1.0, cx),
            ),
        );
        let terminal_font_row = self.render_settings_row(
            "Terminal font size",
            "",
            self.render_stepper_control(
                "settings-terminal-font",
                format!("{:.1} px", self.settings.appearance.terminal_font_size),
                cx,
                |this, cx| this.adjust_terminal_font_size(-0.5, cx),
                |this, cx| this.adjust_terminal_font_size(0.5, cx),
            ),
        );
        let follow_system_row = self.render_settings_row(
            "Follow system text size",
            "Takes the OS accessibility setting instead of the scale above.",
            self.render_toggle_control(
                "settings-follow-system-text-size",
                self.settings.appearance.follow_system_text_size,
                cx,
                |this, cx| this.toggle_follow_system_text_size(cx),
            ),
        );
        div()
            .flex()
            .flex_col()
            .child(self.render_config_banner(settings_store::ConfigPage::Appearance, cx))
            .child(
                div()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Preview"),
            )
            .child(preview)
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Sizing"),
            )
            .child(self.render_settings_row(
                "Interface scale",
                "Rail, tabs, panels and every keycap. Preview above updates as you pick.",
                scale_choice,
            ))
            .child(editor_font_row)
            .child(terminal_font_row)
            .child(follow_system_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::Appearance))
    }

    /// One Appearance page preview card - a static approximation of `percent`'s scale on a fixed
    /// sample row (font sizes scaled by `percent / 100`, not a live re-render of any real pane).
    /// Selection state (border/background) is live, tied to
    /// `Self::settings.appearance.interface_scale_percent`.
    fn render_appearance_preview_card(
        &self,
        percent: u16,
        selected_percent: u16,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = percent == selected_percent;
        let macos = self.window_controls_style().is_macos();

        div()
            .id(format!("settings-scale-preview-{percent}"))
            .cursor_pointer()
            .flex_1()
            .min_w_0()
            .rounded(theme::radius::CARD)
            .border_1()
            .border_color(if is_selected {
                theme::border::SELECTED_EDGE
            } else {
                theme::border::CARD
            })
            .bg(if is_selected {
                theme::settings::CARD_SELECTED_BG
            } else {
                theme::settings::CARD_UNSELECTED_BG
            })
            .px(px(10.0))
            .py(px(9.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(if is_selected {
                        theme::text::SELECTED
                    } else {
                        theme::text::DIM
                    })
                    .child(format!("{percent}%")),
            )
            .child(
                div()
                    .mt(px(8.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(theme::ui_scale::scaled_px(11.5, percent))
                            .text_color(theme::text::HEADING)
                            .child("Needs input"),
                    )
                    .child(
                        div()
                            .mt(px(2.0))
                            .font(font(theme::font::MONO))
                            .text_size(theme::ui_scale::scaled_px(10.5, percent))
                            .text_color(theme::text::FAINT)
                            .child("fix/auth-token-race"),
                    )
                    .child(div().mt(px(7.0)).child(render_keycap_row(
                        &keymap::resolve_combo("mod+K", macos),
                        KeycapSize::Hint,
                    ))),
            )
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.set_interface_scale_percent(percent, cx);
            }))
    }

    /// *Themes* - the six cards from `crate::settings::THEME_DEFS`, with persisted selection.
    /// Selecting a card persists (`Self::settings.theme.name` round-trips through
    /// `settings.toml`) **and** really re-skins the running app: `crate::theme`'s ~200 colour
    /// tokens are each a `crate::theme::ColorToken`, resolved against a real, live-selected
    /// index (`crate::theme::current_theme_index`) rather than a plain compile-time constant -
    /// see that module's own docs for the runtime mechanism and how the five non-Jerry-Dark
    /// palettes are derived. `Self::set_theme_name` is the one real place a selection is applied:
    /// it updates the shared index and forces a real full repaint
    /// (`App::refresh_windows`) so every already-rendered surface picks up the new colours on the
    /// very next frame, not just newly-mounted ones.
    pub(super) fn render_settings_theme_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = div().flex().flex_wrap().gap(px(8.0)).children(
            settings::THEME_DEFS
                .iter()
                .map(|def| self.render_theme_card(def, cx)),
        );

        let follow_system_row = self.render_settings_row(
            "Follow system appearance",
            "Switch to the light theme when the OS does.",
            self.render_toggle_control(
                "settings-theme-follow-system",
                self.settings.theme.follow_system,
                cx,
                |this, cx| this.toggle_theme_follow_system(cx),
            ),
        );
        let high_contrast_row = self.render_settings_row(
            "High-contrast diff colours",
            "Stronger add and delete backgrounds for bright rooms.",
            self.render_toggle_control(
                "settings-theme-high-contrast-diff",
                self.settings.theme.high_contrast_diff,
                cx,
                |this, cx| this.toggle_high_contrast_diff(cx),
            ),
        );

        div()
            .flex()
            .flex_col()
            .child(self.render_config_banner(settings_store::ConfigPage::Theme, cx))
            .child(
                div()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Installed themes"),
            )
            .child(cards)
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Colour"),
            )
            .child(follow_system_row)
            .child(high_contrast_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::Theme))
    }

    fn render_theme_card(
        &self,
        def: &settings::ThemeDef,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = def.name == self.settings.theme.name;
        let name = def.name;

        div()
            .id(format!("settings-theme-card-{name}"))
            .cursor_pointer()
            .w(px(212.0))
            .rounded(theme::radius::CARD)
            .border_1()
            .border_color(if is_selected {
                theme::border::SELECTED_EDGE
            } else {
                theme::border::CARD
            })
            .bg(if is_selected {
                theme::settings::CARD_SELECTED_BG
            } else {
                theme::settings::CARD_UNSELECTED_BG
            })
            .overflow_hidden()
            .when(!is_selected, |el| {
                el.hover(|el| el.border_color(theme::settings::THEME_CARD_HOVER_BORDER))
            })
            .child(
                div().h(px(34.0)).flex().children(
                    def.swatches
                        .iter()
                        .map(|hex| div().flex_1().bg(gpui::rgb(*hex))),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(9.0))
                    .py(px(7.0))
                    .border_t_1()
                    .border_color(theme::border::CARD)
                    .child(
                        div()
                            .flex_none()
                            .font(font(theme::font::SANS))
                            .text_size(px(11.5))
                            .text_color(if is_selected {
                                theme::text::SELECTED
                            } else {
                                theme::text::BODY
                            })
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(def.subtitle),
                    )
                    .when(is_selected, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .font(font(theme::font::MONO))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(9.0))
                                .text_color(theme::status::REVIEW)
                                .child("in use"),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.set_theme_name(name.to_string(), cx);
            }))
    }

    /// *Keybindings* - every row is derived at render time from
    /// `crate::default_key_bindings()`'s live-registered `gpui::KeyBinding`s plus any real,
    /// persisted `Settings.keymap.overrides` (`crate::settings::keybinding_rows` - see that
    /// function's docs for why this replaced a hand-maintained parallel list). No config
    /// banner/snippet here - these rows aren't a single flat `settings.toml` table the way
    /// `crate::settings_store::ConfigPage`'s other pages are, since each one carries its own
    /// per-row identity (see `crate::keymap_overrides::BindingIdentity`'s own docs).
    ///
    /// Real rebind UI: clicking a row's keycap starts recording (`Self::start_recording_
    /// keybinding`), the next real physical key chord replaces it (or reports a real collision -
    /// `Self::keymap_rebind_error`), and an overridden row gets a "Reset" affordance
    /// (`Self::reset_one_keybinding`). "Reset all" (`Self::reset_all_keybindings`) clears every
    /// override at once, shown only when at least one exists.
    pub(super) fn render_settings_keymap_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        let bindings = crate::default_key_bindings();
        let rows = settings::keybinding_rows(&bindings, &self.settings.keymap.overrides);
        let filtered = settings::filter_keybinding_rows(&rows, &self.settings_keymap_filter);
        let last_index = filtered.len().saturating_sub(1);
        let has_overrides = !self.settings.keymap.overrides.is_empty();

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(9.5))
                            .text_color(theme::palette::GROUP_HEADER)
                            .child("Bindings"),
                    )
                    .when(has_overrides, |el| {
                        el.child(
                            div()
                                .id("settings-keymap-reset-all")
                                .cursor_pointer()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(10.0))
                                .text_color(theme::button::DANGER_FG)
                                .hover(|el| el.text_color(theme::button::DANGER_FG_HOVER))
                                .child("Reset all")
                                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.reset_all_keybindings(cx);
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .rounded(theme::radius::CARD)
                    .border_1()
                    .border_color(theme::border::CARD)
                    .overflow_hidden()
                    .child(self.render_settings_keymap_filter_row(filtered.len(), rows.len(), cx))
                    .children(filtered.iter().enumerate().flat_map(|(index, row)| {
                        let mut elements = vec![self
                            .render_settings_keybinding_row(row, index == last_index, macos, cx)
                            .into_any_element()];
                        if let Some((_, message)) = self
                            .keymap_rebind_error
                            .as_ref()
                            .filter(|(identity, _)| identity == &row.identity)
                        {
                            elements.push(
                                self.render_settings_keybinding_error_row(
                                    message,
                                    index == last_index,
                                )
                                .into_any_element(),
                            );
                        }
                        elements
                    })),
            )
    }

    /// The inline collision-error banner directly under whichever row
    /// [`Self::keymap_rebind_error`] is for - see [`Self::render_settings_keymap_page`]'s own
    /// docs.
    fn render_settings_keybinding_error_row(
        &self,
        message: &str,
        is_last: bool,
    ) -> impl IntoElement {
        div()
            .flex()
            .px(px(11.0))
            .py(px(6.0))
            .bg(theme::status::FAIL_BG)
            .when(!is_last, |el| {
                el.border_b_1().border_color(theme::settings::CARD_ROW_SEP)
            })
            .font(font(theme::font::SANS))
            .text_size(px(10.0))
            .text_color(theme::status::FAIL)
            .child(message.to_string())
    }

    fn render_settings_keymap_filter_row(
        &self,
        shown: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_query = !self.settings_keymap_filter.is_empty();

        div()
            .id("settings-keymap-filter")
            .track_focus(&self.settings_keymap_filter_focus_handle)
            .on_key_down(cx.listener(Self::handle_settings_keymap_filter_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.settings_keymap_filter_focus_handle, cx);
            }))
            .flex()
            .items_center()
            .gap(px(7.0))
            .px(px(11.0))
            .py(px(7.0))
            .bg(theme::surface::CARD_SUNK)
            .border_b_1()
            .border_color(theme::border::CARD)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::GHOSTER)
                    .child("/"),
            )
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(if has_query {
                        theme::text::DIM
                    } else {
                        theme::text::GHOST
                    })
                    .child(if has_query {
                        self.settings_keymap_filter.clone()
                    } else {
                        format!("filter {total} bindings")
                    }),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(format!("{shown} shown")),
            )
    }

    /// Same minimal append/backspace/escape-clears shape as [`Self::handle_filter_key_down`] -
    /// see that method's docs for the deliberate scope cut (no cursor positioning, no selection,
    /// no IME).
    pub(super) fn handle_settings_keymap_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        let changed = match keystroke.key.as_str() {
            "backspace" => self.settings_keymap_filter.pop().is_some(),
            "escape" => {
                let had_text = !self.settings_keymap_filter.is_empty();
                self.settings_keymap_filter.clear();
                had_text
            }
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => {
                    self.settings_keymap_filter.push_str(text);
                    true
                }
                _ => false,
            },
        };
        if changed {
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn render_settings_keybinding_row(
        &self,
        row: &settings::KeybindingRow,
        is_last: bool,
        macos: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_recording = self.keymap_recording.as_ref() == Some(&row.identity);
        let glyphs: Vec<String> = row
            .keystrokes
            .iter()
            .flat_map(|keystroke| keymap::resolve_keystroke(keystroke, macos))
            .collect();
        let identity_for_record = row.identity.clone();
        let identity_for_reset = row.identity.clone();
        // The row's own full identity, not just `identity.action` - two rows can share an action
        // (`CompletionsAccept` is bound twice, to `tab` and to `enter`), so `action` alone isn't
        // a unique element id.
        let row_key = format!(
            "{}-{}-{}",
            row.identity.action, row.identity.context, row.identity.default_keystrokes
        );
        div()
            .id(format!("settings-keybinding-row-{}", row.command))
            // Test-only, no-op in release builds - lets `VisualTestContext::debug_bounds`
            // confirm which rows the render call actually produced for a filter query - see
            // `settings_keymap_filter_tests` below.
            .debug_selector(move || format!("keybinding-row-{}", row.command))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(11.0))
            .py(px(7.0))
            .bg(theme::surface::CARD)
            .when(!is_last, |el| {
                el.border_b_1().border_color(theme::settings::CARD_ROW_SEP)
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::STRONG)
                    .child(row.command),
            )
            .child(
                div()
                    .flex_none()
                    // Widened from an earlier px(64.0), plus a real whitespace/overflow backstop
                    // - `KeybindingRow::context` now shows the real predicate string (see that
                    // field's own docs for the real row-collision bug fixed by showing it), which
                    // can be considerably longer than the old constant `"scoped"` (e.g.
                    // `"file-editor && completions"`) - the same real gutter-overflow fix class
                    // `code_surface::render_diff_gutter_number`'s own docs describe for a
                    // different column.
                    .w(px(190.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(row.context.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(96.0))
                    .flex()
                    .justify_end()
                    .when(is_recording, |el| {
                        el.child(
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(px(9.5))
                                .text_color(theme::status::ASK)
                                .child("press a key\u{2026}"),
                        )
                    })
                    .when(!is_recording, |el| {
                        el.child(render_keycap_row(&glyphs, KeycapSize::Standard))
                    }),
            )
            .child(
                div()
                    .id(format!("settings-keybinding-record-{row_key}"))
                    .flex_none()
                    .w(px(46.0))
                    .text_right()
                    .cursor_pointer()
                    .font(font(theme::font::MONO))
                    .text_size(px(9.5))
                    .text_color(if is_recording {
                        theme::status::ASK
                    } else {
                        theme::text::FAINTER
                    })
                    .hover(|el| el.text_color(theme::text::SELECTED))
                    .child(if is_recording { "esc" } else { "rebind" })
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        if this.keymap_recording.as_ref() == Some(&identity_for_record) {
                            this.cancel_keybinding_recording(cx);
                        } else {
                            this.start_recording_keybinding(identity_for_record.clone(), cx);
                        }
                    })),
            )
            .when(row.is_overridden, |el| {
                el.child(
                    div()
                        .id(format!("settings-keybinding-reset-{row_key}"))
                        .flex_none()
                        .w(px(40.0))
                        .text_right()
                        .cursor_pointer()
                        .font(font(theme::font::MONO))
                        .text_size(px(9.5))
                        .text_color(theme::button::DANGER_FG)
                        .hover(|el| el.text_color(theme::button::DANGER_FG_HOVER))
                        .child("reset")
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            this.reset_one_keybinding(identity_for_reset.clone(), cx);
                        })),
                )
            })
    }

    /// Starts capturing a new chord for `identity` (the row's own "rebind" click) - see
    /// [`Self::keymap_recording`]/[`Self::_keymap_intercept`]'s own docs for the real capture
    /// mechanism (`App::intercept_keystrokes`, the same real API `vendor/zed/crates/
    /// keymap_editor/src/ui_components/keystroke_input.rs`'s own keybinding-editor UI uses).
    /// Clears any stale collision error first - a fresh recording attempt starts clean.
    pub(super) fn start_recording_keybinding(
        &mut self,
        identity: keymap_overrides::BindingIdentity,
        cx: &mut Context<Self>,
    ) {
        self.keymap_rebind_error = None;
        self.keymap_recording = Some(identity);
        let listener = cx.listener(|this, event: &gpui::KeystrokeEvent, _window, cx| {
            this.handle_keymap_recording_keystroke(&event.keystroke, cx);
        });
        self._keymap_intercept = Some(cx.intercept_keystrokes(listener));
        cx.notify();
    }

    /// Cancels an in-progress recording (clicking the row's own "esc" affordance, a real Esc
    /// keystroke while recording, or leaving the Keybindings page/Settings entirely) without
    /// changing anything - drops [`Self::_keymap_intercept`], the real, global subscription that
    /// must never outlive the recording it belongs to.
    pub(super) fn cancel_keybinding_recording(&mut self, cx: &mut Context<Self>) {
        if self.keymap_recording.is_none() && self._keymap_intercept.is_none() {
            return;
        }
        self.keymap_recording = None;
        self._keymap_intercept = None;
        cx.notify();
    }

    /// The real `App::intercept_keystrokes` callback while a row is recording -
    /// `KeystrokeEvent::keystroke` is the real, physical chord (`vendor/zed/crates/gpui/src/
    /// app.rs`'s own `KeystrokeEvent` struct). `cx.stop_propagation()` unconditionally swallows
    /// every intercepted keystroke while recording, matching the task's own "without it being
    /// consumed as a normal keystroke or dispatched to an existing action" requirement - a
    /// modifier-only press (an empty `key`) keeps recording rather than being treated as the
    /// captured chord, and a bare `Escape` (no modifiers) cancels instead of binding `Escape`
    /// itself, matching this project's other Esc-cancels-an-overlay conventions.
    fn handle_keymap_recording_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        let Some(identity) = self.keymap_recording.clone() else {
            self._keymap_intercept = None;
            return;
        };
        cx.stop_propagation();
        if keystroke.key.is_empty() {
            // A modifier-only press (e.g. just holding Shift) - keep listening for the real key.
            return;
        }
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.cancel_keybinding_recording(cx);
            return;
        }
        self.finish_recording_keybinding(identity, keystroke.clone(), cx);
    }

    /// Real collision check (`keymap_overrides::find_colliding_binding`) plus, if the candidate
    /// is safe, a real, persisted rebind - see `keymap_overrides`'s own module docs for both
    /// mechanisms. Always ends the recording (successful or not); a genuine collision leaves
    /// [`Self::keymap_rebind_error`] set for [`Self::render_settings_keymap_page`] to show inline
    /// on the row that was being recorded, without touching `Settings.keymap.overrides` at all.
    fn finish_recording_keybinding(
        &mut self,
        identity: keymap_overrides::BindingIdentity,
        keystroke: gpui::Keystroke,
        cx: &mut Context<Self>,
    ) {
        self.keymap_recording = None;
        self._keymap_intercept = None;
        let candidate = keystroke.unparse();
        let defaults = crate::default_key_bindings();
        let Some(for_binding) = defaults
            .iter()
            .find(|binding| keymap_overrides::BindingIdentity::of(binding) == identity)
        else {
            // Every real identity offered to `Self::start_recording_keybinding` is read straight
            // off `crate::default_key_bindings()` at render time, so this can't happen from the
            // UI - degrading honestly (no-op) rather than panicking if it somehow ever does.
            cx.notify();
            return;
        };
        let effective = keymap_overrides::effective_key_bindings(&self.settings.keymap.overrides);
        if let Some(colliding) =
            keymap_overrides::find_colliding_binding(&defaults, &effective, for_binding, &candidate)
        {
            let label = settings::action_label(colliding.action()).unwrap_or("another binding");
            self.keymap_rebind_error = Some((
                identity,
                format!("{candidate} is already used by \u{201c}{label}\u{201d} in an overlapping scope"),
            ));
            cx.notify();
            return;
        }
        self.settings
            .keymap
            .overrides
            .retain(|entry| !identity.matches_override(entry));
        self.settings
            .keymap
            .overrides
            .push(settings_store::KeybindingOverride {
                action: identity.action,
                context: identity.context,
                default_keystrokes: identity.default_keystrokes,
                keystrokes: candidate,
            });
        self.apply_effective_key_bindings(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Removes `identity`'s real override, if one exists, falling back to the compiled-in
    /// default (the row's own "reset" click).
    pub(super) fn reset_one_keybinding(
        &mut self,
        identity: keymap_overrides::BindingIdentity,
        cx: &mut Context<Self>,
    ) {
        let before = self.settings.keymap.overrides.len();
        self.settings
            .keymap
            .overrides
            .retain(|entry| !identity.matches_override(entry));
        if self.settings.keymap.overrides.len() == before {
            return;
        }
        self.apply_effective_key_bindings(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Clears every real, persisted override at once (the page header's "Reset all").
    pub(super) fn reset_all_keybindings(&mut self, cx: &mut Context<Self>) {
        if self.settings.keymap.overrides.is_empty() {
            return;
        }
        self.settings.keymap.overrides.clear();
        self.apply_effective_key_bindings(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Re-registers the app's real, effective keybinding set - `crate::default_key_bindings()`
    /// with every persisted `Settings.keymap.overrides` entry applied on top
    /// (`keymap_overrides::effective_key_bindings`) - via the real, verified runtime API
    /// (`App::clear_key_bindings` + `App::bind_keys`, `vendor/zed/crates/gpui/src/app.rs`), so a
    /// rebind takes effect live, in this same running process, with no restart. Called once at
    /// startup (`Self::new_with_settings`) and again every time `Settings.keymap.overrides`
    /// changes. Clears first rather than a second `bind_keys` merge on top of whatever was
    /// already registered - `App::bind_keys`'s own docs say it *merges*, so without the clear, a
    /// rebind would leave the old default binding still registered alongside the new override,
    /// both matching the same action and genuinely colliding under GPUI's own real dispatch, not
    /// just this app's own UI-level collision guard.
    pub(super) fn apply_effective_key_bindings(&self, cx: &mut Context<Self>) {
        cx.clear_key_bindings();
        cx.bind_keys(keymap_overrides::effective_key_bindings(
            &self.settings.keymap.overrides,
        ));
    }

    /// *Language servers* - PATH-detection rows, following the same pattern as
    /// [`Self::render_settings_agents_page`] (`crate::settings::detect_lsp_rows`, cached in
    /// [`Self::lsp_rows`]). `format on save`/`inlay hints`/`diagnostics in the rail` toggles from
    /// `Jerry.dc.html`'s own `settingsRows.lsp` fixture are left out for the same reason as the
    /// Agents/Worktrees toggle sections (see `crate::settings`'s module docs). No config
    /// banner/snippet either: these rows are live-detected `$PATH` state, not `settings.toml`
    /// keys.
    pub(super) fn render_settings_lsp_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = &self.lsp_rows;
        let last_index = rows.len().saturating_sub(1);

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .pt(px(16.0))
                    .pb(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Servers"),
            )
            .child(
                div()
                    .rounded(theme::radius::CARD)
                    .border_1()
                    .border_color(theme::border::CARD)
                    .overflow_hidden()
                    .children(rows.iter().enumerate().map(|(index, row)| {
                        self.render_settings_lsp_row(row, index == last_index, cx)
                    })),
            )
    }

    /// `install_url`'s `Install` action (see this method's own docs for the fuller shape) only
    /// ever appears for a genuinely `not installed` row (`!row.is_ready()`) - a `ready` row has
    /// live-found the binary already, so there is nothing real for the action to do. Styled to
    /// match this same page's own established clickable-row-action pattern
    /// ([`Self::render_settings_worktree_row`]'s `Open`/`Prune` links: a right-aligned,
    /// `cursor_pointer`, medium-weight sans link that only darkens on hover), not a new visual
    /// pattern invented for this one row.
    fn render_settings_lsp_row(
        &self,
        row: &settings::LspRow,
        is_last: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chip = file_tree::lang_chip_for_name(&format!("x.{}", row.ext));
        let path_text = match &row.resolved_path {
            Some(path) => path.display().to_string(),
            None => format!("{} not found on PATH", row.binary),
        };
        let dot_color = if row.is_ready() {
            theme::settings::AGENT_READY
        } else {
            theme::status::IDLE
        };

        div()
            .id(format!("settings-lsp-row-{}", row.binary))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(8.0))
            .bg(theme::surface::CARD)
            .when(!is_last, |el| {
                el.border_b_1().border_color(theme::settings::CARD_ROW_SEP)
            })
            .child(
                div()
                    .flex_none()
                    .w(px(17.0))
                    .h(px(17.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(chip.bg)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(7.5))
                    .text_color(chip.fg)
                    .child(chip.label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(78.0))
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::HEADING)
                    .child(row.language),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(196.0))
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(if row.is_ready() {
                        theme::text::DIM
                    } else {
                        theme::button::DANGER_FG
                    })
                    .child(path_text),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(row.note),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(div().w(px(5.0)).h(px(5.0)).rounded(px(2.5)).bg(dot_color))
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(row.status_label()),
                    ),
            )
            .when(!row.is_ready(), |el| {
                let install_url = row.install_url;
                el.child(
                    div()
                        .id(format!("settings-lsp-install-{}", row.binary))
                        // Test-only, no-op in release builds - lets `VisualTestContext::
                        // debug_bounds` (keyed by this, not `.id`) confirm the Install action
                        // only actually renders for a genuinely not-installed row.
                        .debug_selector(move || format!("settings-lsp-install-{}", row.binary))
                        .cursor_pointer()
                        .flex_none()
                        .ml(px(10.0))
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(px(10.5))
                        .text_color(theme::text::FAINT)
                        .hover(|el| el.text_color(theme::text::SECONDARY))
                        .child("Install")
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            this.open_install_url(install_url, cx);
                        })),
                )
            })
    }

    fn set_interface_scale_percent(&mut self, percent: u16, cx: &mut Context<Self>) {
        self.settings.appearance.interface_scale_percent = percent;
        self.persist_settings(cx);
        cx.notify();
    }

    fn adjust_editor_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.appearance.editor_font_size = (self.settings.appearance.editor_font_size
            + delta)
            .clamp(settings_store::FONT_SIZE_MIN, settings_store::FONT_SIZE_MAX);
        self.persist_settings(cx);
        cx.notify();
    }

    /// `pub(super)`, not private: `terminal_font_size_tests` below drives this directly, the
    /// same edit path the Appearance page's stepper click invokes, rather than a second,
    /// test-only setter.
    ///
    /// The new value isn't only persisted - it's also pushed into every currently open session's
    /// [`crate::terminal_pane::TerminalPane`] via
    /// [`crate::sessions::Sessions::set_terminal_font_size`], so already-open panes pick it up
    /// too, not just newly spawned ones.
    pub(super) fn adjust_terminal_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.appearance.terminal_font_size = (self.settings.appearance.terminal_font_size
            + delta)
            .clamp(settings_store::FONT_SIZE_MIN, settings_store::FONT_SIZE_MAX);
        self.sessions
            .set_terminal_font_size(self.settings.appearance.terminal_font_size, cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Persisted-only, not yet applied: this vendored GPUI checkout has no Linux API for a
    /// system text-scale accessibility preference to follow.
    /// `vendor/zed/crates/gpui/src/platform.rs`'s `PlatformWindow::appearance`/
    /// `on_appearance_changed` only carry light/dark mode. The XDG Desktop Portal integration
    /// (`vendor/zed/crates/gpui_linux/src/linux/xdg_desktop_portal.rs`) reads
    /// `org.gnome.desktop.interface`'s `cursor-theme`/`cursor-size`/`color-scheme` only, never
    /// `text-scaling-factor`; the client's `Xft.dpi` handling (`.../x11/client.rs`) is
    /// pixel-density DPI, a different concept. Treating DPI as if it were text scale would be
    /// fabricated functionality, so this stays real, persisted, not yet applied.
    fn toggle_follow_system_text_size(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance.follow_system_text_size =
            !self.settings.appearance.follow_system_text_size;
        self.persist_settings(cx);
        cx.notify();
    }

    /// A real theme card click - persists the name *and* actually re-skins the running app (see
    /// [`Self::render_settings_theme_page`]'s own docs). Also updates
    /// `Settings.theme.last_dark_theme` whenever `name` isn't `"Paper"` (the one real light
    /// theme) - see that field's own docs for the real data-loss bug this fixes for
    /// `follow_system`.
    fn set_theme_name(&mut self, name: String, cx: &mut Context<Self>) {
        if name != "Paper" {
            self.settings.theme.last_dark_theme = name.clone();
        }
        self.settings.theme.name = name;
        self.apply_theme_selection(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Turning this on immediately syncs to the real, current OS appearance
    /// (`App::window_appearance`) rather than waiting for the next real OS change to fire the
    /// live subscription (`Self::sync_theme_to_system_appearance`, subscribed once at startup in
    /// `Self::new_with_settings`) - a user who turns this on while already in light mode expects
    /// an immediate effect, not silence until they also happen to toggle their OS theme.
    fn toggle_theme_follow_system(&mut self, cx: &mut Context<Self>) {
        self.settings.theme.follow_system = !self.settings.theme.follow_system;
        if self.settings.theme.follow_system {
            let appearance = cx.window_appearance();
            self.apply_follow_system_appearance(appearance, cx);
        }
        self.persist_settings(cx);
        cx.notify();
    }

    fn toggle_high_contrast_diff(&mut self, cx: &mut Context<Self>) {
        self.settings.theme.high_contrast_diff = !self.settings.theme.high_contrast_diff;
        self.persist_settings(cx);
        cx.notify();
    }

    /// Applies `self.settings.theme.name` as the real, live-selected theme
    /// (`crate::theme::set_current_theme_index`) and forces a real full repaint
    /// (`App::refresh_windows`, `vendor/zed/crates/gpui/src/app.rs:1025`) so every
    /// already-rendered surface - not just ones that happen to re-render for some other reason -
    /// picks up the new colours on the very next frame. An unrecognized `theme.name` (only
    /// reachable via a hand-edited `settings.toml`) falls back to index `0` (Jerry Dark) rather
    /// than leaving the previous theme's index in place unnoticed.
    pub(super) fn apply_theme_selection(&self, cx: &mut Context<Self>) {
        let index = settings::THEME_DEFS
            .iter()
            .position(|def| def.name == self.settings.theme.name)
            .unwrap_or(0);
        theme::set_current_theme_index(index);
        cx.refresh_windows();
    }

    /// The real, shared "follow system" logic both [`Self::toggle_theme_follow_system`] (a
    /// one-shot sync using `App::window_appearance`, no live `Window` needed) and
    /// [`Self::sync_theme_to_system_appearance`] (the live `observe_window_appearance`
    /// subscription, which does have one) apply `appearance` through - see this app's own
    /// "Themes" settings row copy, "Switch to the light theme when the OS does": a real OS-light
    /// signal selects `"Paper"` (the only real light theme in `crate::settings::THEME_DEFS`); a
    /// real OS-dark signal selects `Settings.theme.last_dark_theme` - the user's own most
    /// recently chosen dark theme (defaults to `"Jerry Dark"` - see that field's own docs), not a
    /// hardcoded default, so a user on e.g. "Slate" who round-trips through light and back to
    /// dark lands back on "Slate", not silently loses their choice. A no-op if the resolved name
    /// is already current, so this can be called freely without spuriously re-persisting/
    /// repainting.
    pub(super) fn apply_follow_system_appearance(
        &mut self,
        appearance: gpui::WindowAppearance,
        cx: &mut Context<Self>,
    ) {
        let target = match appearance {
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => {
                "Paper".to_string()
            }
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => {
                self.settings.theme.last_dark_theme.clone()
            }
        };
        if self.settings.theme.name == target {
            return;
        }
        self.settings.theme.name = target;
        self.apply_theme_selection(cx);
        self.persist_settings(cx);
    }

    /// The live `Window::observe_window_appearance`/`Context::observe_window_appearance`
    /// subscription callback (`vendor/zed/crates/gpui/src/window.rs:1946`,
    /// `vendor/zed/crates/gpui/src/app/context.rs:462` - real, verified APIs, the same real
    /// mechanism `vendor/zed/crates/workspace/src/workspace.rs:1802` uses for its own theme
    /// following), subscribed once in `Self::new_with_settings` regardless of whether
    /// `follow_system` starts on - it checks the flag itself on every real fire, so turning the
    /// setting on later doesn't require re-subscribing. A no-op whenever `follow_system` is off.
    pub(super) fn sync_theme_to_system_appearance(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.theme.follow_system {
            return;
        }
        let appearance = window.appearance();
        self.apply_follow_system_appearance(appearance, cx);
        cx.notify();
    }
}

/// A nav-only Settings page's placeholder body - `Jerry.dc.html`'s own `setStub` copy, "not
/// designed in this mockup". Used for every page [`SettingsPage::is_implemented`] reports
/// `false` for - see `crate::settings`'s module docs for why.
pub(super) fn render_settings_placeholder_page() -> impl IntoElement {
    div()
        .py(px(26.0))
        .font(font(theme::font::MONO))
        .text_size(px(11.0))
        .text_color(theme::text::DISABLED)
        .child("not designed in this mockup")
}

/// Interactive regression coverage for the Keybindings page's filter row - unlike
/// `crate::settings`'s own `filter_keybinding_rows_*` tests (which call the pure logic function
/// directly), this drives the actual rendered UI: a focused GPUI element receiving simulated
/// keystrokes, checked via `VisualTestContext::debug_bounds` (keyed by each row's
/// `debug_selector`) against which rows the render call actually painted.
#[cfg(test)]
mod settings_keymap_filter_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn typing_into_the_keybindings_filter_changes_which_rows_are_actually_rendered(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        app.update(cx, |app, cx| {
            app.select_settings_page(SettingsPage::Keymap, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("keybinding-row-Command palette").is_some(),
            "every real row should be rendered before any filter is typed"
        );
        assert!(
            cx.debug_bounds("keybinding-row-Go to definition").is_some(),
            "every real row should be rendered before any filter is typed"
        );

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.settings_keymap_filter_focus_handle, cx);
        });
        cx.simulate_input("definition");
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("keybinding-row-Go to definition").is_some(),
            "the row matching the real, just-typed filter query should still be rendered"
        );
        assert!(
            cx.debug_bounds("keybinding-row-Command palette").is_none(),
            "typing a real keystroke into the real, focused filter field should have actually \
             changed which rows the real render call produces - not just updated the pure \
             filter_keybinding_rows logic function's own separately-tested output"
        );

        app.update(cx, |app, cx| {
            app.settings_keymap_filter.clear();
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("keybinding-row-Command palette").is_some(),
            "clearing the real filter should render every row again"
        );
    }
}

/// End-to-end coverage: `appearance.terminal_font_size` isn't just a persisted number, it
/// changes how `crate::terminal_pane::TerminalPane` measures cells and, through that, what
/// `(rows, cols)` its grid *and* its live child pty are resized to.
#[cfg(test)]
mod terminal_font_size_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn changing_terminal_font_size_recomputes_grid_dimensions_and_resizes_the_real_pty(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let pane = app
            .read_with(cx, |app, _| app.sessions.active().map(|s| s.pane.clone()))
            .expect("a fresh test window has one real, active shell session");

        // `grid_dimensions` reports `(cols, rows)` but `resize_sync_state_for_test` reports
        // `(rows, cols)` - hence the swap below.
        let before_dims = pane.read_with(cx, |pane, _| pane.grid_dimensions());
        let before_dims_rows_cols = (before_dims.1, before_dims.0);
        let (_, before_session_sync) =
            pane.read_with(cx, |pane, _| pane.resize_sync_state_for_test());
        assert_eq!(
            before_session_sync,
            Some(before_dims_rows_cols),
            "the initial spawn's own resize must have already reached the real live pty"
        );

        // A large jump so the resulting grid dimensions can't coincidentally match the old ones.
        app.update(cx, |app, cx| {
            app.adjust_terminal_font_size(18.0 - app.settings.appearance.terminal_font_size, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            pane.read_with(cx, |pane, _| pane.font_size_px_for_test()),
            18.0,
            "the real pane must actually receive the new setting value, not just settings.toml"
        );

        let after_dims = pane.read_with(cx, |pane, _| pane.grid_dimensions());
        let after_dims_rows_cols = (after_dims.1, after_dims.0);
        assert_ne!(
            before_dims, after_dims,
            "a real font-size change must actually recompute the grid's real (cols, rows)"
        );

        let (after_grid_sync, after_session_sync) =
            pane.read_with(cx, |pane, _| pane.resize_sync_state_for_test());
        assert_eq!(
            after_grid_sync,
            Some(after_dims_rows_cols),
            "the grid itself must be resized to the new dimensions"
        );
        assert_eq!(
            after_session_sync,
            Some(after_dims_rows_cols),
            "the real, live child pty must also have been informed of the new size - not just \
             the local grid repainting at a size the process underneath it doesn't know about"
        );
    }

    #[gpui::test]
    fn a_terminal_font_size_edit_reaches_every_open_session_not_just_new_ones(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // A second real session, spawned at whatever the default font size already was.
        app.update_in(cx, |app, window, cx| {
            app.new_session(SessionKind::Shell, window, cx);
        });
        cx.run_until_parked();

        let panes: Vec<_> = app.read_with(cx, |app, _| {
            app.sessions.iter().map(|s| s.pane.clone()).collect()
        });
        assert_eq!(panes.len(), 2, "expected two real open sessions");

        app.update(cx, |app, cx| {
            app.adjust_terminal_font_size(20.0 - app.settings.appearance.terminal_font_size, cx);
        });
        cx.run_until_parked();

        for pane in panes {
            assert_eq!(
                pane.read_with(cx, |pane, _| pane.font_size_px_for_test()),
                20.0,
                "every already-open session's pane must pick up the new font size, not just \
                 whichever one happens to be active"
            );
        }
    }
}

/// Coverage for the Language servers page's `Install` action (task #33): it must only ever
/// render for a genuinely `not installed` row, never a `ready` one. Uses synthetic
/// [`settings::LspRow`] values (not real live `$PATH` state, which would make the ready/
/// not-ready split nondeterministic across machines) written straight into
/// [`AdeApp::lsp_rows`] - the same cache `Self::load_lsp_rows` populates in production, just
/// seeded directly here for a deterministic test.
#[cfg(test)]
mod settings_lsp_install_action_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn synthetic_row(binary: &'static str, ready: bool) -> settings::LspRow {
        settings::LspRow {
            language: "Synthetic",
            ext: "sy",
            binary,
            note: "synthetic row for a deterministic test",
            install_url: "https://example.invalid/synthetic",
            resolved_path: ready.then(|| PathBuf::from(format!("/usr/bin/{binary}"))),
        }
    }

    #[gpui::test]
    fn install_action_renders_only_for_a_genuinely_not_installed_row(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        cx.dispatch_action(ToggleSettings);
        app.update(cx, |app, cx| {
            app.lsp_rows = vec![
                synthetic_row("ready-binary", true),
                synthetic_row("missing-binary", false),
            ];
            app.select_settings_page(SettingsPage::LanguageServers, cx);
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("settings-lsp-install-missing-binary")
                .is_some(),
            "a genuinely not-installed row should show a real Install action"
        );
        assert!(
            cx.debug_bounds("settings-lsp-install-ready-binary")
                .is_none(),
            "a ready row has already live-found its binary and should show no Install action"
        );
    }
}

/// End-to-end regression coverage for the real keybinding rebind mechanism - driven through
/// GPUI's real dispatch (`VisualTestContext::simulate_keystrokes`), a real `App::
/// intercept_keystrokes` capture, and a real `settings.toml` file, mirroring
/// `root::focus::palette_focus_tests::secondary_keystroke_opens_the_palette_through_the_real_key_
/// bindings`'s own "test through real dispatch, not a direct method call" discipline - a plain
/// unit test of `keymap_overrides::effective_key_bindings` alone couldn't catch a wiring bug in
/// `Self::apply_effective_key_bindings`'s own call sites (e.g. forgetting to call it after a
/// rebind, or at startup).
#[cfg(test)]
mod keybinding_rebind_tests {
    use super::*;
    use crate::keymap_overrides::BindingIdentity;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings: settings_store::Settings,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(repo_path, settings, Some(settings_path), window, cx)
        })
    }

    /// The real, live-dispatched proof this whole mechanism actually works: recording a new
    /// chord for `TogglePalette` through the same `App::intercept_keystrokes` path a real click
    /// on the Keybindings page's "rebind" affordance uses, confirming the *old* keystroke stops
    /// opening the palette and the *new* one does, that the override round-trips through a real
    /// `settings.toml` write, and that a freshly constructed `AdeApp` loading that same file -
    /// standing in for a real app restart, since nothing in this codebase can restart the actual
    /// process mid-test - applies the override again at startup with no further action needed.
    #[gpui::test]
    fn a_real_rebind_persists_across_a_simulated_reload_and_the_old_chord_stops_working(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path.clone(),
        );

        let old_chord = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        let new_chord = "ctrl-shift-p";

        // Sanity check: the real, unmodified default binding opens the palette before any
        // rebind - this test's whole premise depends on this being true first.
        cx.simulate_keystrokes(old_chord);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "the real default {old_chord} chord must open the palette before any rebind"
        );
        cx.dispatch_action(TogglePalette);

        let toggle_palette_identity = app.read_with(cx, |_, _| {
            let defaults = crate::default_key_bindings();
            let binding = defaults
                .iter()
                .find(|binding| binding.action().name() == "app::TogglePalette")
                .expect("TogglePalette should be a real default binding")
                .clone();
            BindingIdentity::of(&binding)
        });

        app.update(cx, |app, cx| {
            app.start_recording_keybinding(toggle_palette_identity, cx);
        });
        cx.simulate_keystrokes(new_chord);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.keymap_recording.is_none()),
            "capturing a real keystroke must end the recording"
        );
        assert!(
            app.read_with(cx, |app, _| app.keymap_rebind_error.is_none()),
            "rebinding TogglePalette onto an unused chord must not report a real collision"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.keymap.overrides.len()),
            1
        );

        // The *old* chord must no longer do anything - `apply_effective_key_bindings` really
        // cleared and rebuilt the live keymap, not just recorded the override in `Settings`.
        cx.simulate_keystrokes(old_chord);
        assert!(
            !app.read_with(cx, |app, _| app.palette_open),
            "the old default chord must genuinely stop opening the palette after a real rebind"
        );

        // The *new* chord must now open it.
        cx.simulate_keystrokes(new_chord);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "the newly recorded chord must genuinely open the palette"
        );
        cx.dispatch_action(TogglePalette);

        // Let the real serial settings-save writer loop actually reach disk before reading it
        // back - the same real, already-tested mechanism `root::settings_persist_tests` covers.
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.settings_save_pending),
            "the override must have actually been queued and written, not left pending"
        );

        let reloaded_settings = settings_store::Settings::load_or_init_at(&settings_path);
        assert_eq!(
            reloaded_settings.keymap.overrides.len(),
            1,
            "the real override must round-trip through the real settings.toml file"
        );

        // "Simulated reload": a second, independent `AdeApp` loading the same real file, standing
        // in for an actual app restart - see this test's own docs.
        let (reloaded_app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            reloaded_settings,
            settings_path,
        );

        cx.simulate_keystrokes(old_chord);
        assert!(
            !reloaded_app.read_with(cx, |app, _| app.palette_open),
            "a fresh app instance must apply the persisted override at startup - the old chord \
             must not work here either, with no further action taken"
        );
        cx.simulate_keystrokes(new_chord);
        assert!(
            reloaded_app.read_with(cx, |app, _| app.palette_open),
            "a fresh app instance must apply the persisted override at startup - the new chord \
             must already work"
        );
    }

    /// A real, live-dispatched collision: recording `EditorLeft` (scoped `"file-editor"`) onto
    /// `secondary-k` - the real, currently-global `TogglePalette` chord - must be rejected with a
    /// real, visible error, and must leave both bindings' real dispatch behavior completely
    /// unchanged.
    #[gpui::test]
    fn recording_a_chord_that_collides_with_a_real_global_binding_is_rejected(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("main.rs");
        std::fs::write(&file_path, "fn main() {}\n").expect("write main.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });
        cx.run_until_parked();

        let editor_left_identity = app.read_with(cx, |_, _| {
            let defaults = crate::default_key_bindings();
            let binding = defaults
                .iter()
                .find(|binding| {
                    binding.action().name() == "app::EditorLeft"
                        && BindingIdentity::of(binding).context == "file-editor"
                })
                .expect("a file-editor-scoped EditorLeft binding should exist")
                .clone();
            BindingIdentity::of(&binding)
        });

        app.update(cx, |app, cx| {
            app.start_recording_keybinding(editor_left_identity.clone(), cx);
        });
        let secondary_k = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        cx.simulate_keystrokes(secondary_k);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.keymap_recording.is_none()),
            "recording must end even when the candidate is rejected"
        );
        assert!(
            app.read_with(cx, |app, _| app.settings.keymap.overrides.is_empty()),
            "a genuine collision must never be persisted as a real override"
        );
        let error = app.read_with(cx, |app, _| app.keymap_rebind_error.clone());
        let (error_identity, message) = error.expect("a real collision must set a visible error");
        assert_eq!(error_identity, editor_left_identity);
        assert!(
            message.contains("Command palette"),
            "the error should name the real command it collides with, got: {message:?}"
        );

        // The real, original TogglePalette binding must still work, completely undisturbed.
        cx.simulate_keystrokes(secondary_k);
        assert!(app.read_with(cx, |app, _| app.palette_open));
    }
}

/// End-to-end regression coverage for the real theme-swap mechanism, driven through the same
/// `AdeApp` methods a real theme-card click invokes (`Self::set_theme_name`), not a direct call
/// into `crate::theme`'s own already-tested pure mechanism (`theme::theme_runtime_tests`) - this
/// module proves the *wiring* (persistence, the live `crate::theme::current_theme_index` write,
/// and that a representative real render call genuinely reads the new value), which a pure unit
/// test of `crate::theme` alone can't catch (e.g. forgetting to call `apply_theme_selection` from
/// `Self::set_theme_name` would still pass every `crate::theme` test).
#[cfg(test)]
mod theme_swap_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    /// `crate::theme::CURRENT_THEME_INDEX` is real, process-global, mutable state - reset it
    /// after this test regardless of outcome, matching `crate::theme::theme_runtime_tests`'s own
    /// discipline (see that module's docs for why a leaked non-default index would corrupt other
    /// tests in this binary). In practice any *other* test that goes on to construct a fresh
    /// `AdeApp` already self-heals this via `Self::apply_theme_selection` running in `Self::
    /// new_with_settings`, but this test doesn't rely on that - it cleans up its own real global
    /// write directly.
    struct ResetThemeIndexOnDrop;
    impl Drop for ResetThemeIndexOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme_index(0);
        }
    }

    #[gpui::test]
    fn selecting_a_real_theme_card_changes_the_live_selected_index_and_a_representative_color(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeIndexOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert_eq!(
            theme::current_theme_index(),
            0,
            "a fresh app defaults to Jerry Dark (index 0)"
        );
        let jerry_dark_window_bg = theme::surface::WINDOW.resolve();

        let slate_index = settings::THEME_DEFS
            .iter()
            .position(|def| def.name == "Slate")
            .expect("Slate should be a real theme");

        app.update(cx, |app, cx| {
            app.set_theme_name("Slate".to_string(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Slate",
            "the selection must really persist in Settings"
        );
        assert_eq!(
            theme::current_theme_index(),
            slate_index,
            "selecting a theme card must really update the live-selected index, not just the \
             persisted setting"
        );
        assert!(
            theme::surface::WINDOW.resolve() != jerry_dark_window_bg,
            "a representative real colour token must actually resolve differently once Slate is \
             selected - this is the real proof a theme swap changes what gets rendered, not just \
             what's saved"
        );

        // Selecting back to Jerry Dark must restore the exact original value.
        app.update(cx, |app, cx| {
            app.set_theme_name("Jerry Dark".to_string(), cx);
        });
        cx.run_until_parked();
        assert_eq!(theme::current_theme_index(), 0);
        assert_eq!(theme::surface::WINDOW.resolve(), jerry_dark_window_bg);
    }

    /// Real `follow_system` behavior - `Self::apply_follow_system_appearance` is the shared real
    /// logic both the live OS-appearance subscription and turning the toggle on both go through
    /// (see that method's own docs); this drives it directly with real `gpui::WindowAppearance`
    /// values, the same real enum `Window::appearance()`/`App::window_appearance()` return.
    #[gpui::test]
    fn follow_system_selects_paper_on_light_and_jerry_dark_on_dark(cx: &mut TestAppContext) {
        let _guard = ResetThemeIndexOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.apply_follow_system_appearance(gpui::WindowAppearance::Light, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Paper",
            "a real OS-light signal must select the one real light theme"
        );
        let paper_index = settings::THEME_DEFS
            .iter()
            .position(|def| def.name == "Paper")
            .expect("Paper should be a real theme");
        assert_eq!(theme::current_theme_index(), paper_index);

        app.update(cx, |app, cx| {
            app.apply_follow_system_appearance(gpui::WindowAppearance::Dark, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Jerry Dark",
            "a real OS-dark signal must switch back to the real last-chosen dark theme, which \
             for a fresh install is the documented default"
        );
        assert_eq!(theme::current_theme_index(), 0);
    }

    /// Regression for a real data-loss bug an audit caught: before `Settings.theme.
    /// last_dark_theme` existed, an OS-dark signal always hardcoded `"Jerry Dark"`, silently
    /// discarding whichever dark theme a user had actually chosen (e.g. "Slate") the moment their
    /// OS round-tripped through light and back to dark.
    #[gpui::test]
    fn follow_system_restores_the_users_own_last_chosen_dark_theme_not_a_hardcoded_default(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeIndexOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            app.set_theme_name("Slate".to_string(), cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.last_dark_theme.clone()),
            "Slate"
        );

        app.update(cx, |app, cx| {
            app.apply_follow_system_appearance(gpui::WindowAppearance::Light, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Paper"
        );
        // Selecting "Paper" itself must not overwrite the real remembered dark theme.
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.last_dark_theme.clone()),
            "Slate"
        );

        app.update(cx, |app, cx| {
            app.apply_follow_system_appearance(gpui::WindowAppearance::Dark, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Slate",
            "a real OS-dark signal must restore the user's own last-chosen dark theme, not \
             silently reset to Jerry Dark"
        );
    }

    /// Turning `follow_system` on while the real (test-environment) OS appearance is already
    /// known must apply it immediately, not wait for a later change - see `Self::
    /// toggle_theme_follow_system`'s own docs for why an immediate sync matters.
    #[gpui::test]
    fn turning_follow_system_on_immediately_syncs_to_the_real_current_appearance(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeIndexOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        assert!(!app.read_with(cx, |app, _| app.settings.theme.follow_system));

        let appearance_before = app.read_with(cx, |_, cx| cx.window_appearance());

        app.update(cx, |app, cx| {
            app.toggle_theme_follow_system(cx);
        });
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.settings.theme.follow_system));
        let expected_name = match appearance_before {
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => "Paper",
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => "Jerry Dark",
        };
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            expected_name,
            "turning follow_system on must immediately apply the real current OS appearance"
        );
    }
}
