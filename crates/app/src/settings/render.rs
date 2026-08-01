use super::*;
use crate::root::widgets::{render_env_chip, render_keycap_row, KeycapSize};
use crate::settings::widgets::ChoiceOption;

impl AdeApp {
    pub(crate) fn handle_toggle_settings_action(
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
    pub(in crate::settings) fn handle_settings_key_down(
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
    ///
    /// Also moves real keyboard focus off the Keybindings page's own filter field when leaving it.
    /// That field is `track_focus`'d and stops being rendered the instant the page changes, so
    /// without this the focused `FocusId` is no longer in the rendered frame at all and GPUI falls
    /// back to the dispatch root with an **empty** context stack
    /// (`Window::focus_node_id_in_rendered_frame`). Every scoped binding is dead against an empty
    /// stack - `KeyBindingContextPredicate::eval_inner` short-circuits to `false` when there is no
    /// context to evaluate against - so `secondary-z` reached neither undo system and vanished
    /// with no feedback at all.
    ///
    /// The dangling-focus mechanism itself long predates GitHub issue #17 (it is the same class
    /// `OverlayFocus`/`restore_focus` exists for, and which `close_agent`/`select_worktree`/
    /// `cancel_new_file` already handle). What that issue changed is that this specific site
    /// became *silent*: before the filter field carried a `"text-input"` context there was nothing
    /// here for a stale focus to be pointing at in the first place. Found by an independent
    /// adversarial audit - the fourth site of this shape, after the three already fixed on this
    /// branch.
    pub(crate) fn select_settings_page(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_keybinding_recording(cx);
        if self.settings_page == SettingsPage::Keymap && page != SettingsPage::Keymap {
            window.focus(&self.settings_focus_handle, cx);
        }
        if page != SettingsPage::Theme {
            // See `Self::custom_theme_remove_armed`'s own docs - the confirm-arm is scoped to
            // this one page, so leaving it (even to come straight back) must not leave a stale
            // arm ready to fire on whatever card happens to render in the same position.
            self.custom_theme_remove_armed = None;
        }
        self.settings_page = page;
        cx.notify();
    }

    /// Recomputes [`Self::agent_rows`] via `crate::settings::state::detect_agent_rows`, offloaded to
    /// the background executor and cached, mirroring [`Self::load_disk_usage`]'s shape: a
    /// not-found `resolve_on_path` call walks every `$PATH` entry with no early exit, so running
    /// it inline in `render()` would block the foreground/GPUI thread on every frame the Agents
    /// page is open. Run once when Settings opens ([`Self::open_settings`]), not on every render
    /// or on the 3s status-poll cadence - the set of binaries on `$PATH` essentially never
    /// changes while the app is running.
    pub(crate) fn load_agent_rows(&mut self, cx: &mut Context<Self>) {
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

    /// Recomputes [`Self::lsp_rows`] via `crate::settings::state::detect_lsp_rows`, mirroring
    /// [`Self::load_agent_rows`]'s shape and reasoning exactly.
    pub(crate) fn load_lsp_rows(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
    /// rail." All eleven pages are clickable navigation (`crate::settings::state::nav_groups`); seven
    /// render real content past this point - see `crate::settings::state`'s module docs.
    pub(in crate::settings) fn render_settings_nav(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-nav-groups")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.settings_nav_scroll_handle)
                            .py(px(6.0))
                            .flex()
                            .flex_col()
                            .children(groups.into_iter().map(|group| {
                                self.render_settings_nav_group(
                                    group,
                                    agent_count,
                                    worktree_count,
                                    cx,
                                )
                            })),
                    )
                    .children(self.render_vertical_scrollbar(
                        "settings-nav-scrollbar",
                        &self.settings_nav_scroll_handle,
                        &[],
                        cx,
                    )),
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

    pub(in crate::settings) fn render_settings_nav_group(
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
                // `crate::settings::state::keybinding_rows`'s own docs).
                // Built-in + real, disk-loaded custom themes (GitHub issue #5) - one combined
                // count, matching `Self::render_settings_theme_page`'s own combined card list.
                SettingsPage::Theme => {
                    Some((settings::THEME_DEFS.len() + self.custom_themes.len()).to_string())
                }
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

    pub(in crate::settings) fn render_settings_nav_row(
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
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_settings_page(page, window, cx);
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
    pub(in crate::settings) fn render_settings_content(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-content-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.settings_content_scroll_handle)
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
                                        SettingsPage::Worktrees => self
                                            .render_settings_worktrees_page(cx)
                                            .into_any_element(),
                                        SettingsPage::Appearance => self
                                            .render_settings_appearance_page(cx)
                                            .into_any_element(),
                                        SettingsPage::Theme => {
                                            self.render_settings_theme_page(cx).into_any_element()
                                        }
                                        SettingsPage::Keymap => {
                                            self.render_settings_keymap_page(cx).into_any_element()
                                        }
                                        SettingsPage::LanguageServers => {
                                            self.render_settings_lsp_page(cx).into_any_element()
                                        }
                                        SettingsPage::Editor => {
                                            self.render_settings_editor_page(cx).into_any_element()
                                        }
                                        _ => render_settings_placeholder_page().into_any_element(),
                                    }),
                            ),
                    )
                    .children(self.render_vertical_scrollbar(
                        "settings-content-scrollbar",
                        &self.settings_content_scroll_handle,
                        &[],
                        cx,
                    )),
            )
    }

    /// *Agents › Installed* - `design_handoff_jerry_ade/README.md`: "bordered card ... of four
    /// rows ... agent badge ... name ... binary path ... model ... a `default` pill ... green
    /// dot + 'ready' ... Edit." This app drops the `model`/`default`/`Edit` pieces (see
    /// `crate::settings::state`'s module docs for why) and shows [`settings::AGENT_KINDS`]'s two real
    /// rows instead of the mockup's four fabricated ones, each with a live PATH-derived status.
    pub(in crate::settings) fn render_settings_agents_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

    pub(in crate::settings) fn render_settings_agent_row(
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
    /// '+ Add an agent — any binary that speaks a resumable agent on stdin'." Rendered dimmed
    /// and inert (no `on_click`) - `crate::work_surface::agents::AgentKind` is a fixed Rust enum, so there
    /// is no runtime "register a new agent binary" flow to wire this to yet.
    pub(in crate::settings) fn render_settings_agents_footer(&self) -> impl IntoElement {
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
                    .child("\u{2014} any binary that speaks a resumable agent on stdin"),
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
    pub(in crate::settings) fn render_settings_worktrees_page(
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
    /// currently-prunable worktree at once, live-agent-excluded. A row's `Prune` button only
    /// shows when that row's own worktree is one of those candidates
    /// (`settings::worktree_row_action`), so clicking it always includes this worktree - it just
    /// isn't scoped to *only* this worktree if others are also prunable at the same moment.
    pub(in crate::settings) fn render_settings_worktree_row(
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
    /// `Restore agents on launch` and `Confirm before discarding a worktree` - two more rows
    /// `Jerry.dc.html`'s own `settingsRows.general` fixture shows - stay left out for the same
    /// reason as the Agents/Worktrees toggle sections (see `crate::settings::state`'s module docs):
    /// agent-restore-on-launch and a discard-confirmation flow are app behaviour this build
    /// doesn't have, not settings plumbing around behaviour that already exists.
    pub(in crate::settings) fn render_settings_general_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            |this, index, _window, cx| {
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
            "Where new agents run - real WSL detection on Windows, real CPU architecture \
             elsewhere. The same chip shown in the status bar and terminal footer.",
            render_env_chip(),
        );
        let inline_blame_row = self.render_settings_row(
            "Inline git blame",
            "Show who last changed the current line, and when, dimmed at the end of it. Off \
             stops the background lookup entirely, not just the display.",
            self.render_toggle_control(
                "settings-inline-blame",
                self.settings.blame.show_inline,
                cx,
                |this, cx| this.set_show_inline_blame(!this.settings.blame.show_inline, cx),
            ),
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
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Editor"),
            )
            .child(inline_blame_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::General))
    }

    /// *Appearance & scaling* - every row here is persisted and round-trips through
    /// [`Self::settings`] (`CHANGELOG.md`'s change 3). This page is itself a live consumer of
    /// `interface_scale_percent` (`Self::ui_text_size`, applied to its own labels, hints, *and*
    /// every row's control - stepper value, choice-segment labels, config banner/snippet block -
    /// see `crate::settings::widgets`'s module docs), so editing the choice control below
    /// visibly rescales this page's own text, not just its four preview cards.
    ///
    /// Only *text* sizes respond, by deliberate scope - `theme::ui_scale`'s module docs carry
    /// the current list of which surfaces read this setting and which don't (kept there, not
    /// duplicated here). `editor_font_size`/`terminal_font_size` are separately-applied
    /// baselines for Surface C's zoom (`Self::effective_code_rem_px`) and `crate::terminal::pane`
    /// respectively, distinct from the interface-scale multiplier above them.
    pub(in crate::settings) fn render_settings_appearance_page(
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
            |this, index, _window, cx| {
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
        let caret_style_choice = self.render_choice_control(
            "settings-caret-style",
            &[
                ChoiceOption::new("Line"),
                ChoiceOption::new("Block"),
                ChoiceOption::new("Underline"),
            ],
            self.settings.appearance.caret_style.label().to_string(),
            cx,
            |this, index, _window, cx| {
                // Index into the `options` array above, not a label re-match - same discipline
                // `Self::render_choice_control`'s own docs establish.
                use settings_store::CaretStyle;
                let style = match index {
                    1 => CaretStyle::Block,
                    2 => CaretStyle::Underline,
                    _ => CaretStyle::Line,
                };
                this.set_caret_style(style, cx);
            },
        );
        let caret_style_row = self.render_settings_row(
            "Caret style",
            "How the code editor's real insertion point is drawn.",
            caret_style_choice,
        );
        let caret_blink_row = self.render_settings_row(
            "Blink caret",
            "Turn off for a permanently solid caret (also honors reduced-motion).",
            self.render_toggle_control(
                "settings-caret-blink",
                self.settings.appearance.caret_blink,
                cx,
                |this, cx| this.toggle_caret_blink(cx),
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
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Editing"),
            )
            .child(caret_style_row)
            .child(caret_blink_row)
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

    /// *Themes* - the six cards from `crate::settings::state::THEME_DEFS`, with persisted selection.
    /// Selecting a card persists (`Self::settings.theme.name` round-trips through
    /// `settings.toml`) **and** really re-skins the running app: `crate::theme`'s ~200 colour
    /// tokens are each a `crate::theme::ColorToken`, resolved against a real, live-selected
    /// index (`crate::theme::current_theme_index`) rather than a plain compile-time constant -
    /// see that module's own docs for the runtime mechanism and how the five non-Jerry-Dark
    /// palettes are derived. `Self::set_theme_name` is the one real place a selection is applied:
    /// it updates the shared index and forces a real full repaint
    /// (`App::refresh_windows`) so every already-rendered surface picks up the new colours on the
    /// very next frame, not just newly-mounted ones.
    pub(in crate::settings) fn render_settings_theme_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let builtin_cards =
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(settings::THEME_DEFS.iter().map(|def| {
                    self.render_theme_card(def.name, def.subtitle, def.swatches, false, cx)
                }));

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
            .child(builtin_cards)
            .child(self.render_custom_themes_section(cx))
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

    /// GitHub issue #5: the "Custom themes" block - every real, disk-loaded
    /// [`custom_theme::CustomTheme`] as a card (same [`Self::render_theme_card`] used for the six
    /// built-ins, so a custom theme is visually a first-class citizen, not a second-tier list),
    /// the real `New from template…`/`Import theme…`/`Export current theme…` actions
    /// ([`Self::start_create_theme_from_template`]/[`Self::start_import_custom_theme`]/
    /// [`Self::start_export_custom_theme`]), any real load errors from the last time
    /// `Self::custom_themes` was populated, and the most recent action's own real result.
    fn render_custom_themes_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_custom = !self.custom_themes.is_empty();

        let header_row = div()
            .flex()
            .items_center()
            .justify_between()
            .pt(px(20.0))
            .pb(px(6.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Custom themes"),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(self.render_theme_action_button(
                        "settings-theme-new-from-template",
                        "New from template\u{2026}",
                        cx,
                        |this, cx| this.start_create_theme_from_template(cx),
                    ))
                    .child(self.render_theme_action_button(
                        "settings-theme-import",
                        "Import theme\u{2026}",
                        cx,
                        |this, cx| this.start_import_custom_theme(cx),
                    ))
                    .child(self.render_theme_action_button(
                        "settings-theme-export",
                        "Export current theme\u{2026}",
                        cx,
                        |this, cx| this.start_export_custom_theme(cx),
                    ))
                    .child(self.render_theme_action_button(
                        "settings-theme-open-folder",
                        "Open theme folder",
                        cx,
                        |this, cx| this.start_open_custom_themes_folder(cx),
                    )),
            );

        let cards = div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .children(self.custom_themes.iter().map(|theme| {
                self.render_theme_card(&theme.name, &theme.subtitle, theme.swatches, true, cx)
            }));

        let empty_state = (!has_custom).then(|| {
            // The real directory this instance actually loads from
            // (`crate::settings::custom_theme::custom_themes_dir_for`, derived from
            // `Self::settings_path`) - not a hardcoded `~/.config/jerry/themes/` string, which
            // would have been wrong for anything other than the one real production settings
            // path.
            let themes_dir = self
                .settings_path
                .as_deref()
                .map(|path| {
                    custom_theme::custom_themes_dir_for(path)
                        .display()
                        .to_string()
                })
                .unwrap_or_else(|| "~/.config/jerry/themes".to_string());
            div()
                .py(px(10.0))
                .font(font(theme::font::MONO))
                .text_size(px(10.5))
                .text_color(theme::text::DISABLED)
                .child(format!(
                    "No custom themes yet - click \u{201c}New from template\u{2026}\u{201d} for a \
                     real, well-commented starting point, Import a `.toml` theme file, or drop \
                     one into {themes_dir}/."
                ))
        });

        let load_errors = (!self.custom_theme_load_errors.is_empty()).then(|| {
            div().flex().flex_col().gap(px(2.0)).pt(px(6.0)).children(
                self.custom_theme_load_errors.iter().map(|message| {
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::status::ASK)
                        .child(format!("skipped: {message}"))
                }),
            )
        });

        let status = self.custom_theme_status.as_ref().map(|result| {
            let (text, color) = match result {
                Ok(message) => (message.clone(), theme::status::REVIEW),
                Err(message) => (message.clone(), theme::status::FAIL),
            };
            div()
                .pt(px(6.0))
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(color)
                .child(text)
        });

        div()
            .flex()
            .flex_col()
            .child(header_row)
            .children(empty_state)
            .child(cards)
            .children(load_errors)
            .children(status)
    }

    /// A small bordered text button, matching [`Self::render_config_banner`]'s own `Open file`
    /// button shape - the real trigger for [`Self::start_import_custom_theme`]/
    /// [`Self::start_export_custom_theme`].
    fn render_theme_action_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .cursor_pointer()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .border_color(theme::border::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .text_color(theme::text::MUTED)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child(label)
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                on_click(this, cx);
            }))
    }

    /// Renders one Themes-page card - shared by built-in (`settings::THEME_DEFS`) and real,
    /// disk-loaded custom (`Self::custom_themes`) themes alike, so the two read as one combined
    /// list rather than a first-class set and a second-tier one (GitHub issue #5). `is_custom`
    /// only controls whether a `Remove` affordance is shown - `crate::settings::custom_theme`'s
    /// own validation already guarantees a custom theme's `name` can never collide with a
    /// built-in's, so no other branch needs it.
    fn render_theme_card(
        &self,
        name: &str,
        subtitle: &str,
        swatches: [u32; 5],
        is_custom: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = name == self.settings.theme.name;
        let is_remove_armed = self.custom_theme_remove_armed.as_deref() == Some(name);
        let name = name.to_string();
        let name_for_click = name.clone();
        let name_for_remove = name.clone();
        let name_for_selector = name.clone();

        div()
            .id(format!("settings-theme-card-{name}"))
            // Test-only, no-op in release builds - lets `VisualTestContext::debug_bounds` (keyed
            // by this, not `.id`) confirm a real theme (built-in or custom) actually renders as
            // its own card, matching `Self::render_settings_lsp_row`'s identical convention.
            .debug_selector(move || format!("settings-theme-card-{name_for_selector}"))
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
                    swatches
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
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(subtitle.to_string()),
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
                    })
                    .when(is_custom, |el| {
                        // Not `move` - see `crate::settings::render::AdeApp::
                        // render_settings_lsp_row`'s identical `let install_url = row.install_url;`
                        // pattern this mirrors: a fresh owned clone taken *inside* the closure
                        // body, so only the innermost `cx.listener` closure (which really is
                        // `move`, since a `gpui::Context::listener` callback must own what it
                        // captures) takes `name_for_remove` by value - the outer `.when` closure
                        // stays a plain borrow, leaving `cx` itself free for the row's own
                        // `on_click` below.
                        //
                        // Shown even for the currently-*selected* custom theme (an audit caught
                        // an earlier version that hid this whenever `is_selected`, making the
                        // active theme's own file permanently undeletable from the UI) - a real
                        // two-click confirm (`Self::request_remove_custom_theme`) is the guard
                        // against an accidental single click, not "only show it somewhere less
                        // reachable".
                        let name_for_remove = name_for_remove.clone();
                        let name_for_remove_selector = name_for_remove.clone();
                        let label = if is_remove_armed {
                            "Confirm?"
                        } else {
                            "Remove"
                        };
                        el.child(
                            div()
                                .id(format!("settings-theme-card-remove-{name_for_remove}"))
                                // Test-only, no-op in release builds - lets a real, click-driven
                                // interaction test (`VisualTestContext::debug_bounds` +
                                // `simulate_click`) find and click this exact button, proving
                                // `cx.stop_propagation()` above genuinely beats the card's own
                                // `on_click` rather than that only being verified by reading
                                // GPUI's dispatch source.
                                .debug_selector(move || {
                                    format!("settings-theme-card-remove-{name_for_remove_selector}")
                                })
                                .cursor_pointer()
                                .flex_none()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(9.0))
                                .text_color(theme::button::DANGER_FG)
                                .hover(|el| el.text_color(theme::button::DANGER_FG_HOVER))
                                .child(label)
                                .on_click(cx.listener(
                                    move |this, _event: &ClickEvent, _window, cx| {
                                        cx.stop_propagation();
                                        this.request_remove_custom_theme(
                                            name_for_remove.clone(),
                                            cx,
                                        );
                                    },
                                )),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.set_theme_name(name_for_click.clone(), cx);
            }))
    }

    /// *Keybindings* - every row is derived at render time from
    /// `crate::default_key_bindings()`'s live-registered `gpui::KeyBinding`s plus any real,
    /// persisted `Settings.keymap.overrides` (`crate::settings::state::keybinding_rows` - see that
    /// function's docs for why this replaced a hand-maintained parallel list). No config
    /// banner/snippet here - these rows aren't a single flat `settings.toml` table the way
    /// `crate::settings::store::ConfigPage`'s other pages are, since each one carries its own
    /// per-row identity (see `crate::keymap_overrides::BindingIdentity`'s own docs).
    ///
    /// Real rebind UI: clicking a row's keycap starts recording (`Self::start_recording_
    /// keybinding`), the next real physical key chord replaces it (or reports a real collision -
    /// `Self::keymap_rebind_error`), and an overridden row gets a "Reset" affordance
    /// (`Self::reset_one_keybinding`). "Reset all" (`Self::reset_all_keybindings`) clears every
    /// override at once, shown only when at least one exists.
    pub(in crate::settings) fn render_settings_keymap_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        let bindings = crate::default_key_bindings();
        let rows = settings::keybinding_rows(&bindings, &self.settings.keymap.overrides);
        let filtered =
            settings::filter_keybinding_rows(&rows, self.settings_keymap_filter.as_str());
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
            // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the tag and
            // the listener both live on this exact node.
            .key_context("text-input")
            .on_action(cx.listener(Self::handle_settings_keymap_filter_text_undo))
            .on_action(cx.listener(Self::handle_settings_keymap_filter_text_redo))
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
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(11.0))
                            .text_color(if has_query {
                                theme::text::DIM
                            } else {
                                theme::text::GHOST
                            })
                            .child(if has_query {
                                self.settings_keymap_filter.as_str().to_string()
                            } else {
                                format!("filter {total} bindings")
                            }),
                    )
                    .child(self.render_simple_input_caret()),
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
    pub(in crate::settings) fn handle_settings_keymap_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        // GitHub issue #27's "solid mid-keystroke" - see `crate::palette::render::AdeApp::
        // handle_palette_key_down`'s identical reasoning.
        self.reset_caret_blink(cx);
        let changed = match keystroke.key.as_str() {
            "backspace" => self.settings_keymap_filter.pop(Instant::now()),
            // A real, undoable step - see `crate::rail::AdeApp::handle_filter_key_down`'s own
            // identical `Esc` handling.
            "escape" => self.settings_keymap_filter.clear(Instant::now()),
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => {
                    self.settings_keymap_filter.push_str(text, Instant::now())
                }
                _ => false,
            },
        };
        if changed {
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// `TextUndo`/`TextRedo` for the Keybindings page's filter field (GitHub issue #17) - see
    /// `crate::default_key_bindings`' own docs for the scoping.
    pub(in crate::settings) fn handle_settings_keymap_filter_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_keymap_filter.undo() {
            cx.notify();
        }
    }

    pub(in crate::settings) fn handle_settings_keymap_filter_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_keymap_filter.redo() {
            cx.notify();
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
                    // `code_surface::diff_view::render_diff_gutter_number`'s own docs describe for a
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
    pub(in crate::settings) fn start_recording_keybinding(
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
    pub(crate) fn cancel_keybinding_recording(&mut self, cx: &mut Context<Self>) {
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
    pub(in crate::settings) fn reset_one_keybinding(
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
    pub(in crate::settings) fn reset_all_keybindings(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn apply_effective_key_bindings(&self, cx: &mut Context<Self>) {
        cx.clear_key_bindings();
        cx.bind_keys(keymap_overrides::effective_key_bindings(
            &self.settings.keymap.overrides,
        ));
    }

    /// *Language servers* - PATH-detection rows, following the same pattern as
    /// [`Self::render_settings_agents_page`] (`crate::settings::state::detect_lsp_rows`, cached in
    /// [`Self::lsp_rows`]). `format on save`/`inlay hints`/`diagnostics in the rail` toggles from
    /// `Jerry.dc.html`'s own `settingsRows.lsp` fixture are left out for the same reason as the
    /// Agents/Worktrees toggle sections (see `crate::settings::state`'s module docs). No config
    /// banner/snippet either: these rows are live-detected `$PATH` state, not `settings.toml`
    /// keys.
    pub(in crate::settings) fn render_settings_lsp_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

    /// *Editor* - the one real row on this page is the minimap (GitHub issue #30,
    /// `crate::code_surface::minimap`) - see `crate::settings::state`'s own module docs on why
    /// the rest of this page (indentation/soft-wrap/whitespace-display) still has no real
    /// backing and stays left off entirely, rather than shown as an inert control.
    pub(in crate::settings) fn render_settings_editor_page(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let minimap_enabled = self.settings.editor.minimap_enabled;
        let minimap_row = self.render_settings_row(
            "Minimap",
            "A reduced-scale, syntax-colored overview of the file to the right of the code \
             column - drag its slider or click it to jump around. Hidden automatically for very \
             large files regardless of this toggle.",
            self.render_toggle_control(
                "settings-minimap-enabled",
                minimap_enabled,
                cx,
                |this, cx| this.toggle_minimap_enabled(cx),
            ),
        );
        let minimap_scale_row = self.render_settings_row(
            "Minimap scale",
            "Panel width and per-line height together.",
            self.render_stepper_control(
                "settings-minimap-scale",
                format!("{}%", self.settings.editor.minimap_scale_percent),
                cx,
                |this, cx| {
                    this.adjust_minimap_scale_percent(
                        -(settings_store::MINIMAP_SCALE_PERCENT_STEP as i32),
                        cx,
                    )
                },
                |this, cx| {
                    this.adjust_minimap_scale_percent(
                        settings_store::MINIMAP_SCALE_PERCENT_STEP as i32,
                        cx,
                    )
                },
            ),
        );

        div()
            .flex()
            .flex_col()
            .child(self.render_config_banner(settings_store::ConfigPage::Editor, cx))
            .child(
                div()
                    .pt(px(20.0))
                    .pb(px(4.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.5))
                    .text_color(theme::palette::GROUP_HEADER)
                    .child("Minimap"),
            )
            .child(minimap_row)
            .child(minimap_scale_row)
            .child(self.render_snippet_block(settings_store::ConfigPage::Editor))
    }

    fn set_interface_scale_percent(&mut self, percent: u16, cx: &mut Context<Self>) {
        self.settings.appearance.interface_scale_percent = percent;
        self.persist_settings(cx);
        cx.notify();
    }

    /// `pub(crate)`, not private: `caret_settings_tests` (`crate::code_surface::editing`) drives
    /// this directly for its own real-render coverage of [`settings_store::CaretStyle`]'s three
    /// painted shapes, the same edit path the Appearance page's choice control invokes.
    pub(crate) fn set_caret_style(
        &mut self,
        style: settings_store::CaretStyle,
        cx: &mut Context<Self>,
    ) {
        self.settings.appearance.caret_style = style;
        self.persist_settings(cx);
        cx.notify();
    }

    /// `pub(crate)`, not private - see [`Self::set_caret_style`]'s own docs for why
    /// (`caret_blink_tests`' own real coverage of the shared blink loop this gates).
    pub(crate) fn toggle_caret_blink(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance.caret_blink = !self.settings.appearance.caret_blink;
        // A live toggle takes effect immediately, not just on the next focus/reset - if blink
        // was just turned off mid-blink-cycle the caret must snap back to solid right away
        // rather than staying stuck on whichever phase it happened to be in; if it was just
        // turned on, the idle loop should start fresh rather than waiting for some unrelated
        // future action to kick it off.
        self.reset_caret_blink(cx);
        cx.notify();
    }

    fn adjust_editor_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.appearance.editor_font_size = (self.settings.appearance.editor_font_size
            + delta)
            .clamp(settings_store::FONT_SIZE_MIN, settings_store::FONT_SIZE_MAX);
        self.persist_settings(cx);
        cx.notify();
    }

    /// `pub(crate)`, not private: `terminal_font_size_tests` below drives this directly, the
    /// same edit path the Appearance page's stepper click invokes, rather than a second,
    /// test-only setter.
    ///
    /// The new value isn't only persisted - it's also pushed into every currently open agent's
    /// [`crate::terminal::pane::TerminalPane`] via
    /// [`crate::work_surface::agents::Agents::set_terminal_font_size`], so already-open panes pick it up
    /// too, not just newly spawned ones.
    pub(in crate::settings) fn adjust_terminal_font_size(
        &mut self,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        self.settings.appearance.terminal_font_size = (self.settings.appearance.terminal_font_size
            + delta)
            .clamp(settings_store::FONT_SIZE_MIN, settings_store::FONT_SIZE_MAX);
        self.agents
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

    /// The Editor page's minimap toggle - `crate::code_surface::minimap::AdeApp::render_minimap`
    /// reads this directly every render.
    fn toggle_minimap_enabled(&mut self, cx: &mut Context<Self>) {
        self.settings.editor.minimap_enabled = !self.settings.editor.minimap_enabled;
        self.persist_settings(cx);
        cx.notify();
    }

    /// The Editor page's minimap scale stepper - clamped the same
    /// [`settings_store::AppearanceSettings::sanitize`]-style way a hand-edited
    /// `settings.toml` value already is (`settings_store::EditorSettings::sanitize`), so a UI
    /// edit and a hand-edited file can never disagree about the real bounds.
    fn adjust_minimap_scale_percent(&mut self, delta: i32, cx: &mut Context<Self>) {
        let current = self.settings.editor.minimap_scale_percent as i32;
        let updated = (current + delta).clamp(
            settings_store::MINIMAP_SCALE_PERCENT_MIN as i32,
            settings_store::MINIMAP_SCALE_PERCENT_MAX as i32,
        );
        self.settings.editor.minimap_scale_percent = updated as u16;
        self.persist_settings(cx);
        cx.notify();
    }

    /// A real theme card click (built-in or custom - see
    /// [`Self::render_settings_theme_page`]'s own docs) - persists the name *and* actually
    /// re-skins the running app. Also updates `Settings.theme.last_dark_theme` whenever the
    /// newly selected theme isn't a light one (`crate::theme::theme_is_light`, generalized from
    /// the old hardcoded `name == "Paper"` check so a custom light theme is remembered correctly
    /// too) - see that field's own docs for the real data-loss bug this fixes for
    /// `follow_system`.
    fn set_theme_name(&mut self, name: String, cx: &mut Context<Self>) {
        let is_light = self
            .theme_swatches_for(&name)
            .map(theme::theme_is_light)
            .unwrap_or(false);
        if !is_light {
            self.settings.theme.last_dark_theme = name.clone();
        }
        self.settings.theme.name = name;
        self.apply_theme_selection(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Real, single lookup used by both [`Self::apply_theme_selection`] (which colour palette is
    /// live) and [`Self::set_theme_name`] (is the newly selected theme light, for
    /// `last_dark_theme` bookkeeping) - looks up `name` first against the six built-in
    /// `settings::THEME_DEFS`, then against [`Self::custom_themes`], so the two callers can never
    /// resolve a name differently.
    fn theme_swatches_for(&self, name: &str) -> Option<[u32; 5]> {
        settings::THEME_DEFS
            .iter()
            .find(|def| def.name == name)
            .map(|def| def.swatches)
            .or_else(|| {
                self.custom_themes
                    .iter()
                    .find(|theme| theme.name == name)
                    .map(|theme| theme.swatches)
            })
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

    /// Applies `self.settings.theme.name` as the real, live-selected theme and forces a real full
    /// repaint (`App::refresh_windows`, `vendor/zed/crates/gpui/src/app.rs:1025`) so every
    /// already-rendered surface - not just ones that happen to re-render for some other reason -
    /// picks up the new colours on the very next frame.
    ///
    /// Checks the six built-in `settings::THEME_DEFS` first, then [`Self::custom_themes`]
    /// (GitHub issue #5) - a built-in name always wins if both somehow matched, though
    /// `crate::settings::custom_theme::CustomThemeFile::validate` already rejects a custom theme
    /// whose name collides with a built-in one, so that's not actually reachable outside a
    /// theoretical race with a hand-edited file. A name matching neither (only reachable via a
    /// hand-edited `settings.toml`, or a custom theme file that's since been deleted) falls back
    /// to index `0` (Jerry Dark) rather than leaving the previous theme's index in place
    /// unnoticed. [`crate::theme::set_current_theme_index`]/[`crate::theme::
    /// set_current_custom_theme`] are always written together here - see the latter's own docs
    /// for why `crate::theme::ColorToken::resolve` depends on that.
    pub(crate) fn apply_theme_selection(&self, cx: &mut Context<Self>) {
        let name = self.settings.theme.name.as_str();
        if let Some(index) = settings::THEME_DEFS.iter().position(|def| def.name == name) {
            theme::set_current_theme_index(index);
            theme::set_current_custom_theme(None);
        } else if let Some(swatches) = self
            .custom_themes
            .iter()
            .find(|theme| theme.name == name)
            .map(|theme| theme.swatches)
        {
            theme::set_current_theme_index(0);
            theme::set_current_custom_theme(Some(swatches));
        } else {
            theme::set_current_theme_index(0);
            theme::set_current_custom_theme(None);
        }
        cx.refresh_windows();
    }

    /// GitHub issue #5's real "Import theme…" action - a genuine native file-open dialog
    /// (`gpui::App::prompt_for_paths`, `vendor/zed/crates/gpui/src/app.rs:1490` - the same real
    /// API `vendor/zed/crates/agent_ui/src/threads_archive_view.rs`'s own `open_local_folder`
    /// uses, verified there before writing this), not a synthetic path. The user picks any real
    /// `.toml` file on disk; the actual import+reload runs on the background executor (matching
    /// [`Self::start_export_custom_theme`]'s own convention - an adversarial audit caught the
    /// first version of this doing synchronous foreground-thread I/O instead), and
    /// [`Self::apply_custom_theme_import_result`] applies the outcome once it resolves. A
    /// cancelled dialog (`Ok(Ok(None))`) or a platform error (`Err`/`Ok(Err(_))`) is a real,
    /// silent no-op - there is nothing wrong to report, the user simply didn't pick a file.
    /// "Open theme folder" - hands the real custom-themes directory
    /// ([`custom_theme::custom_themes_dir_for`]) to [`Self::open_path_with_os_handler`], the same
    /// real per-platform default-open handler the file tree's "Reveal in file manager" already
    /// uses. Creates the directory first if it doesn't exist yet, on the background executor
    /// (never the GPUI foreground thread, matching every other real I/O in this module) - a user
    /// who has never imported or created a custom theme has no real directory there otherwise,
    /// and handing a nonexistent path to `xdg-open`/`open`/`cmd /c start` would just fail.
    pub(in crate::settings) fn start_open_custom_themes_folder(&mut self, cx: &mut Context<Self>) {
        let Some(settings_path) = self.settings_path.clone() else {
            self.custom_theme_status = Some(Err(
                "can't open the themes folder: no settings file location is known".to_string(),
            ));
            cx.notify();
            return;
        };
        let dest_dir = custom_theme::custom_themes_dir_for(&settings_path);
        cx.spawn(async move |this, cx| {
            let mkdir_dir = dest_dir.clone();
            let mkdir_result = cx
                .background_executor()
                .spawn(async move { std::fs::create_dir_all(&mkdir_dir) })
                .await;
            if let Err(err) = mkdir_result {
                log::warn!(
                    "failed to create the custom themes directory {}: {err}",
                    dest_dir.display()
                );
            }
            let _ = this.update(cx, |this, cx| {
                this.open_path_with_os_handler(&dest_dir, cx);
            });
        })
        .detach();
    }

    pub(in crate::settings) fn start_import_custom_theme(&mut self, cx: &mut Context<Self>) {
        let paths_receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        // Same seam `Self::custom_themes` was populated through at construction time
        // (`crate::root::AdeApp::new_with_settings`) - a test instance (`settings_path == None`)
        // has nowhere real to import into, matching its own real "no persistence" contract.
        let settings_path = self.settings_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = paths_receiver.await else {
                return;
            };
            let Some(source_path) = paths.pop() else {
                return;
            };
            let Some(settings_path) = settings_path else {
                let _ = this.update(cx, |this, cx| {
                    this.custom_theme_status = Some(Err(
                        "can't import a theme: no settings file location is known".to_string(),
                    ));
                    cx.notify();
                });
                return;
            };
            let dest_dir = custom_theme::custom_themes_dir_for(&settings_path);
            let result = cx
                .background_executor()
                .spawn(async move {
                    let imported = custom_theme::import_theme_file(&source_path, &dest_dir)?;
                    // Reload from disk rather than manually splicing the in-memory list - an
                    // adversarial audit caught the previous `retain` + `push` version drifting
                    // from what's actually on disk whenever `import_theme_file` itself resolved
                    // a slug collision to a *different* path than the naive one (see that
                    // function's own docs), and it also left stale `custom_theme_load_errors`
                    // (e.g. a since-fixed duplicate-name warning) on screen forever.
                    let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                    Ok((imported, themes, errors))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_custom_theme_import_result(result, cx);
            });
        });
        self._custom_theme_import_task = Some(task);
    }

    /// Applies [`Self::start_import_custom_theme`]'s real background-executor result - shared
    /// with nothing else (there is exactly one real caller). On success, re-loads
    /// [`Self::custom_themes`]/[`Self::custom_theme_load_errors`] wholesale from what's actually
    /// on disk (see [`Self::start_import_custom_theme`]'s own docs for why), and - if the
    /// imported theme is the one currently selected (a real "re-import to update my colours"
    /// flow) - immediately re-applies it via [`Self::apply_theme_selection`] rather than leaving
    /// the app rendering the *old* palette until the next restart (a real bug an adversarial
    /// audit caught: the card would redraw with the new swatches and an "in use" badge while
    /// every other surface stayed on the stale colours).
    #[allow(clippy::type_complexity)]
    fn apply_custom_theme_import_result(
        &mut self,
        result: Result<
            (
                custom_theme::CustomTheme,
                Vec<custom_theme::CustomTheme>,
                Vec<String>,
            ),
            custom_theme::ThemeFileError,
        >,
        cx: &mut Context<Self>,
    ) {
        self.apply_custom_theme_load_result(result, |name| format!("Imported \"{name}\"."), cx);
    }

    /// The real, shared "a background load-or-write-then-reload-from-disk action finished"
    /// applier both [`Self::apply_custom_theme_import_result`] and
    /// [`Self::apply_custom_theme_create_from_template_result`] go through - not two
    /// independently-maintained copies of the same success/failure bookkeeping (registry
    /// replacement, load-error replacement, status line, and - if the affected theme is the one
    /// currently selected - an immediate re-skin via [`Self::apply_theme_selection`], matching
    /// [`Self::apply_custom_theme_import_result`]'s own original "re-import the active theme"
    /// fix). `success_message` is the one real difference between the two real callers: what the
    /// status line should say for *this* action having succeeded.
    #[allow(clippy::type_complexity)]
    fn apply_custom_theme_load_result(
        &mut self,
        result: Result<
            (
                custom_theme::CustomTheme,
                Vec<custom_theme::CustomTheme>,
                Vec<String>,
            ),
            custom_theme::ThemeFileError,
        >,
        success_message: impl FnOnce(&str) -> String,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((theme, themes, errors)) => {
                let name = theme.name;
                self.custom_themes = themes;
                self.custom_theme_load_errors = errors;
                self.custom_theme_status = Some(Ok(success_message(&name)));
                if self.settings.theme.name == name {
                    self.apply_theme_selection(cx);
                }
            }
            Err(err) => {
                self.custom_theme_status = Some(Err(err.to_string()));
            }
        }
        cx.notify();
    }

    /// GitHub issue #5 follow-up's real "New from template" action - writes the same real,
    /// well-commented starting-point file the repository itself ships at
    /// `assets/themes/template.toml` (`custom_theme::write_template_theme`, which embeds and
    /// writes `custom_theme::CUSTOM_THEME_TEMPLATE_TOML` verbatim) straight into this instance's
    /// own real custom-themes directory, then reloads the registry from disk - the same real
    /// background-executor write-then-reload-from-disk shape
    /// [`Self::start_import_custom_theme`] already uses, reusing the exact same
    /// [`custom_theme::load_custom_themes_from_dir`] reload and
    /// [`Self::apply_custom_theme_load_result`] applier, not a second, parallel path. Unlike
    /// Import, there's no file-picker dialog to await first - the "file" being written is a
    /// fixed, embedded constant, not something the user picks - so this goes straight to the
    /// background executor.
    pub(in crate::settings) fn start_create_theme_from_template(&mut self, cx: &mut Context<Self>) {
        let Some(settings_path) = self.settings_path.clone() else {
            self.custom_theme_status = Some(Err(
                "can't create a theme: no settings file location is known".to_string(),
            ));
            cx.notify();
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let dest_dir = custom_theme::custom_themes_dir_for(&settings_path);
            let result = cx
                .background_executor()
                .spawn(async move {
                    let created = custom_theme::write_template_theme(&dest_dir)?;
                    let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
                    Ok((created, themes, errors))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_custom_theme_create_from_template_result(result, cx);
            });
        });
        self._custom_theme_create_task = Some(task);
    }

    /// Applies [`Self::start_create_theme_from_template`]'s real result - shared with nothing
    /// else but [`Self::apply_custom_theme_load_result`]'s own common bookkeeping.
    #[allow(clippy::type_complexity)]
    fn apply_custom_theme_create_from_template_result(
        &mut self,
        result: Result<
            (
                custom_theme::CustomTheme,
                Vec<custom_theme::CustomTheme>,
                Vec<String>,
            ),
            custom_theme::ThemeFileError,
        >,
        cx: &mut Context<Self>,
    ) {
        self.apply_custom_theme_load_result(
            result,
            |name| format!("Created \"{name}\" from the template."),
            cx,
        );
    }

    /// GitHub issue #5's real "Export current theme…" action - serializes whichever theme
    /// (built-in or custom) is currently active to a real file at a location the user picks via a
    /// genuine native save dialog (`gpui::App::prompt_for_new_path`,
    /// `vendor/zed/crates/gpui/src/app.rs:1503` - verified against
    /// `vendor/zed/crates/miniprofiler_ui/src/miniprofiler_ui.rs`'s own real caller before writing
    /// this). The real file write itself runs on the background executor, matching every other
    /// disk write in this codebase (`crate::settings::store::Settings::save_at`'s own callers).
    ///
    /// A built-in theme is exported under `"<name> (copy)"`, never its own bare built-in name -
    /// `crate::settings::custom_theme::CustomThemeFile::validate` unconditionally rejects any
    /// file whose `name` collides with a `settings::THEME_DEFS` entry, so exporting e.g. "Slate"
    /// verbatim would produce a file this app (the exporter's own, or anyone it's shared with)
    /// can never actually import back - a real "looks like it worked, quietly can't be used"
    /// bug an adversarial audit caught. A custom theme keeps its own name, so re-importing an
    /// unmodified export is a real no-op "update" rather than spawning a `(copy)` duplicate.
    pub(in crate::settings) fn start_export_custom_theme(&mut self, cx: &mut Context<Self>) {
        let active_name = self.settings.theme.name.clone();
        let Some(swatches) = self.theme_swatches_for(&active_name) else {
            self.custom_theme_status = Some(Err(format!(
                "can't export: no theme named \"{active_name}\" is currently loaded"
            )));
            cx.notify();
            return;
        };
        let export_name = export_theme_name_for(active_name.as_str());
        let subtitle = self
            .custom_themes
            .iter()
            .find(|theme| theme.name == active_name)
            .map(|theme| theme.subtitle.clone())
            .or_else(|| {
                settings::THEME_DEFS
                    .iter()
                    .find(|def| def.name == active_name)
                    .map(|def| def.subtitle.to_string())
            })
            .unwrap_or_default();
        let export_theme = custom_theme::CustomTheme {
            name: export_name.clone(),
            subtitle,
            swatches,
            source_path: None,
        };
        let default_dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        // Reuses `crate::settings::custom_theme::slugify` rather than a second, hand-rolled
        // implementation - an adversarial audit caught the original inline `.replace(...)` here
        // disagreeing with the real one on e.g. runs of punctuation, so the suggested filename
        // and the filename `import_theme_file` would actually pick on re-import could differ.
        let suggested_name = format!("{}.toml", custom_theme::slugify(&export_name));
        let path_receiver = cx.prompt_for_new_path(&default_dir, Some(suggested_name.as_str()));
        let task = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dest_path))) = path_receiver.await else {
                return;
            };
            let write_result = cx
                .background_executor()
                .spawn(async move {
                    custom_theme::export_theme_to_path(&export_theme, &dest_path).map(|_| dest_path)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match write_result {
                    Ok(path) => {
                        this.custom_theme_status =
                            Some(Ok(format!("Exported to {}.", path.display())));
                    }
                    Err(err) => {
                        this.custom_theme_status =
                            Some(Err(format!("couldn't export the theme: {err}")));
                    }
                }
                cx.notify();
            });
        });
        self._custom_theme_export_task = Some(task);
    }

    /// The Themes page's "Remove" action on a custom theme card - real two-click confirmation via
    /// [`Self::custom_theme_remove_armed`] (see that field's own docs for why: an adversarial
    /// audit caught the first version of this deleting the user's file on a single click, unlike
    /// every other destructive action in this app). The first click on a given `name` only arms
    /// it; a second click on the *same* name actually deletes
    /// ([`Self::execute_remove_custom_theme`]) - mirroring
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree`'s identical shape.
    fn request_remove_custom_theme(&mut self, name: String, cx: &mut Context<Self>) {
        if self.custom_theme_remove_armed.as_deref() != Some(name.as_str()) {
            self.custom_theme_remove_armed = Some(name);
            cx.notify();
            return;
        }
        self.custom_theme_remove_armed = None;
        self.execute_remove_custom_theme(name, cx);
    }

    /// Runs the real, already-confirmed removal - only ever reached through
    /// [`Self::request_remove_custom_theme`]'s second click. Deletes the theme's real backing
    /// file (`crate::settings::custom_theme::remove_custom_theme_file`) on the background
    /// executor, then reloads [`Self::custom_themes`]/[`Self::custom_theme_load_errors`] fresh
    /// from disk (same "never manually splice, always re-read" discipline as
    /// [`Self::apply_custom_theme_import_result`]). If the removed theme was the active
    /// selection, falls back to `"Jerry Dark"`; if it was also the remembered
    /// `Settings.theme.last_dark_theme`, that is reset too - an adversarial audit caught the
    /// first version leaving a dangling `last_dark_theme` that a later real OS-dark
    /// `follow_system` signal (`Self::apply_follow_system_appearance`) would have written straight
    /// back into `Settings.theme.name`, resolving to nothing and silently landing back on Jerry
    /// Dark with no visible error - exactly the "dangling selection" this method's remaining
    /// fallback logic is meant to prevent.
    fn execute_remove_custom_theme(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(theme) = self
            .custom_themes
            .iter()
            .find(|theme| theme.name == name)
            .cloned()
        else {
            return;
        };
        let Some(settings_path) = self.settings_path.clone() else {
            self.custom_theme_status = Some(Err(
                "can't remove a theme: no settings file location is known".to_string(),
            ));
            cx.notify();
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let dest_dir = custom_theme::custom_themes_dir_for(&settings_path);
            let result = cx
                .background_executor()
                .spawn(async move {
                    custom_theme::remove_custom_theme_file(&theme)?;
                    Ok(custom_theme::load_custom_themes_from_dir(&dest_dir))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_custom_theme_remove_result(name, result, cx);
            });
        });
        self._custom_theme_remove_task = Some(task);
    }

    /// Applies [`Self::execute_remove_custom_theme`]'s real result - shared with nothing else.
    #[allow(clippy::type_complexity)]
    fn apply_custom_theme_remove_result(
        &mut self,
        name: String,
        result: std::io::Result<(Vec<custom_theme::CustomTheme>, Vec<String>)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((themes, errors)) => {
                self.custom_themes = themes;
                self.custom_theme_load_errors = errors;
                if self.settings.theme.name == name {
                    self.settings.theme.name = "Jerry Dark".to_string();
                    self.apply_theme_selection(cx);
                }
                if self.settings.theme.last_dark_theme == name {
                    self.settings.theme.last_dark_theme = "Jerry Dark".to_string();
                }
                self.custom_theme_status = Some(Ok(format!("Removed \"{name}\".")));
                self.persist_settings(cx);
            }
            Err(err) => {
                self.custom_theme_status = Some(Err(format!("couldn't remove \"{name}\": {err}")));
            }
        }
        cx.notify();
    }

    /// The real, shared "follow system" logic both [`Self::toggle_theme_follow_system`] (a
    /// one-shot sync using `App::window_appearance`, no live `Window` needed) and
    /// [`Self::sync_theme_to_system_appearance`] (the live `observe_window_appearance`
    /// subscription, which does have one) apply `appearance` through - see this app's own
    /// "Themes" settings row copy, "Switch to the light theme when the OS does": a real OS-light
    /// signal selects `"Paper"` (the only real light theme in `crate::settings::state::THEME_DEFS`); a
    /// real OS-dark signal selects `Settings.theme.last_dark_theme` - the user's own most
    /// recently chosen dark theme (defaults to `"Jerry Dark"` - see that field's own docs), not a
    /// hardcoded default, so a user on e.g. "Slate" who round-trips through light and back to
    /// dark lands back on "Slate", not silently loses their choice. A no-op if the resolved name
    /// is already current, so this can be called freely without spuriously re-persisting/
    /// repainting.
    pub(crate) fn apply_follow_system_appearance(
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
    pub(crate) fn sync_theme_to_system_appearance(
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

/// The real name [`AdeApp::start_export_custom_theme`] exports `active_name` under - a pure,
/// directly-`#[test]`-able decision (not a `gpui`-window-touching method) so this can be checked
/// without a real save dialog. A built-in theme (`settings::THEME_DEFS`) is renamed to `"<name>
/// (copy)"`; a custom one keeps its own name unchanged. See `AdeApp::start_export_custom_theme`'s
/// own docs for why this matters: `crate::settings::custom_theme::CustomThemeFile::validate`
/// unconditionally rejects any file whose `name` collides with a built-in, so exporting a
/// built-in theme under its own bare name would produce a file nobody - not even this same app -
/// could ever import back in.
fn export_theme_name_for(active_name: &str) -> String {
    if settings::THEME_DEFS
        .iter()
        .any(|def| def.name == active_name)
    {
        format!("{active_name} (copy)")
    } else {
        active_name.to_string()
    }
}

#[cfg(test)]
mod export_theme_name_tests {
    use super::*;

    #[test]
    fn a_builtin_theme_is_renamed_to_a_real_importable_copy() {
        assert_eq!(export_theme_name_for("Slate"), "Slate (copy)");
        assert_eq!(export_theme_name_for("Jerry Dark"), "Jerry Dark (copy)");
        assert_eq!(export_theme_name_for("Paper"), "Paper (copy)");
    }

    #[test]
    fn a_custom_theme_keeps_its_own_name() {
        assert_eq!(export_theme_name_for("Midnight Coral"), "Midnight Coral");
    }

    /// The real, end-to-end proof this fixes the bug it exists for: exporting a built-in under
    /// its bare name would produce a file `CustomThemeFile::validate` rejects (a real, previously
    /// shipped "looks like it worked, quietly can't be imported back" bug an adversarial audit
    /// caught) - the renamed form must actually validate successfully.
    #[test]
    fn the_renamed_export_of_a_builtin_actually_validates_as_importable() {
        let bare = custom_theme::CustomThemeFile {
            name: "Slate".to_string(),
            subtitle: String::new(),
            background: "#0d1117".to_string(),
            panel: "#161b22".to_string(),
            accent_green: "#57a773".to_string(),
            accent_amber: "#c9a227".to_string(),
            accent_blue: "#6b9bd1".to_string(),
        };
        assert!(
            bare.validate().is_err(),
            "exporting a builtin under its own bare name must be rejected on import - this is \
             the real bug being guarded against"
        );

        let mut renamed = bare;
        renamed.name = export_theme_name_for("Slate");
        assert!(
            renamed.validate().is_ok(),
            "the renamed export must actually be importable"
        );
    }
}

/// A nav-only Settings page's placeholder body - `Jerry.dc.html`'s own `setStub` copy, "not
/// designed in this mockup". Used for every page [`SettingsPage::is_implemented`] reports
/// `false` for - see `crate::settings::state`'s module docs for why.
pub(in crate::settings) fn render_settings_placeholder_page() -> impl IntoElement {
    div()
        .py(px(26.0))
        .font(font(theme::font::MONO))
        .text_size(px(11.0))
        .text_color(theme::text::DISABLED)
        .child("not designed in this mockup")
}

/// Interactive regression coverage for the Keybindings page's filter row - unlike
/// `crate::settings::state`'s own `filter_keybinding_rows_*` tests (which call the pure logic function
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
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Keymap, window, cx);
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
            app.settings_keymap_filter.clear(Instant::now());
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
/// changes how `crate::terminal::pane::TerminalPane` measures cells and, through that, what
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
            .read_with(cx, |app, _| app.agents.active().map(|s| s.pane.clone()))
            .expect("a fresh test window has one real, active shell agent");

        // `grid_dimensions` reports `(cols, rows)` but `resize_sync_state_for_test` reports
        // `(rows, cols)` - hence the swap below.
        let before_dims = pane.read_with(cx, |pane, _| pane.grid_dimensions());
        let before_dims_rows_cols = (before_dims.1, before_dims.0);
        let (_, before_agent_sync) =
            pane.read_with(cx, |pane, _| pane.resize_sync_state_for_test());
        assert_eq!(
            before_agent_sync,
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

        let (after_grid_sync, after_agent_sync) =
            pane.read_with(cx, |pane, _| pane.resize_sync_state_for_test());
        assert_eq!(
            after_grid_sync,
            Some(after_dims_rows_cols),
            "the grid itself must be resized to the new dimensions"
        );
        assert_eq!(
            after_agent_sync,
            Some(after_dims_rows_cols),
            "the real, live child pty must also have been informed of the new size - not just \
             the local grid repainting at a size the process underneath it doesn't know about"
        );
    }

    #[gpui::test]
    fn a_terminal_font_size_edit_reaches_every_open_agent_not_just_new_ones(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // A second real agent, spawned at whatever the default font size already was.
        app.update_in(cx, |app, window, cx| {
            app.new_agent(AgentKind::Shell, window, cx);
        });
        cx.run_until_parked();

        let panes: Vec<_> = app.read_with(cx, |app, _| {
            app.agents.iter().map(|s| s.pane.clone()).collect()
        });
        assert_eq!(panes.len(), 2, "expected two real open agents");

        app.update(cx, |app, cx| {
            app.adjust_terminal_font_size(20.0 - app.settings.appearance.terminal_font_size, cx);
        });
        cx.run_until_parked();

        for pane in panes {
            assert_eq!(
                pane.read_with(cx, |pane, _| pane.font_size_px_for_test()),
                20.0,
                "every already-open agent's pane must pick up the new font size, not just \
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
        app.update_in(cx, |app, window, cx| {
            app.lsp_rows = vec![
                synthetic_row("ready-binary", true),
                synthetic_row("missing-binary", false),
            ];
            app.select_settings_page(SettingsPage::LanguageServers, window, cx);
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

/// GitHub issue #27's "caret width and style configurable ... in user settings" / "no blink
/// setting" - real coverage for [`AdeApp::set_caret_style`]/[`AdeApp::toggle_caret_blink`],
/// mirroring `terminal_font_size_tests`' own established "through the real mutator, assert it
/// actually persisted and applied" discipline.
#[cfg(test)]
mod caret_settings_tests {
    use crate::root::focus::palette_focus_tests;
    use crate::settings::store::CaretStyle;
    use gpui::TestAppContext;

    #[gpui::test]
    fn set_caret_style_changes_the_real_persisted_setting(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.caret_style),
            CaretStyle::Line,
            "sanity check: the real default is Line"
        );

        app.update(cx, |app, cx| {
            app.set_caret_style(CaretStyle::Block, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.caret_style),
            CaretStyle::Block,
            "the real Settings field, not a second independent copy, must have changed"
        );

        app.update(cx, |app, cx| {
            app.set_caret_style(CaretStyle::Underline, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.caret_style),
            CaretStyle::Underline
        );
    }

    /// [`AdeApp::toggle_caret_blink`] must both flip the real persisted setting *and* take
    /// effect immediately on the live blink loop - a toggle that only updated `settings.toml`
    /// and left a currently-blinking caret blinking (or a currently-solid one solid, if it had
    /// just been turned back on) until some unrelated future action reset it would be a real,
    /// user-visible "the toggle didn't do anything until I clicked elsewhere" bug.
    #[gpui::test]
    fn toggle_caret_blink_flips_the_real_setting_and_takes_effect_immediately(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.caret_blink),
            "sanity check: the real default is blinking on"
        );

        app.update(cx, |app, cx| {
            app.toggle_caret_blink(cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.settings.appearance.caret_blink),
            "the real Settings field must have flipped off"
        );
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "turning blink off must immediately snap the caret to solid/visible, not leave it \
             wherever the blink phase happened to be"
        );

        app.update(cx, |app, cx| {
            app.toggle_caret_blink(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.caret_blink),
            "the real Settings field must have flipped back on"
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
            "cmd-p"
        } else {
            "ctrl-p"
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
    /// `secondary-p` - the real, currently-global `TogglePalette` chord - must be rejected with a
    /// real, visible error, and must leave both bindings' real dispatch behavior completely
    /// unchanged. `find_colliding_binding` only ever flags a single-keystroke candidate against
    /// another single-keystroke binding (`crate::keymap_overrides`'s own docs), so `"ctrl-k"`
    /// alone can't be used here any more either: it's now only a *prefix* of the real, two-
    /// keystroke `"ctrl-k ctrl-d"` chord (`EditorSkipOccurrence`), which this checker doesn't
    /// examine at all - `secondary-p` is the one real, single-keystroke global binding left to
    /// prove a genuine rejection with.
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
        let secondary_p = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(secondary_p);
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
        cx.simulate_keystrokes(secondary_p);
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

/// GitHub issue #5's real end-to-end wiring coverage: a theme file really written to disk really
/// gets loaded into a fresh `AdeApp`, really renders as a selectable card, really re-skins the
/// app when picked, and a real removal really deletes its backing file - each proven the same
/// way `theme_swap_tests` proves the built-in mechanism, by driving the actual `AdeApp` methods a
/// real user action invokes, not `crate::settings::custom_theme`'s own already-covered pure
/// functions directly.
#[cfg(test)]
mod custom_theme_settings_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Same real-global-leak discipline `theme_swap_tests::ResetThemeIndexOnDrop` documents, for
    /// both `CURRENT_THEME_INDEX` and `CURRENT_CUSTOM_SHIFT` together - a custom-theme test can
    /// leave either non-default.
    struct ResetThemeStateOnDrop;
    impl Drop for ResetThemeStateOnDrop {
        fn drop(&mut self) {
            theme::set_current_theme_index(0);
            theme::set_current_custom_theme(None);
        }
    }

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

    fn write_custom_theme_file(themes_dir: &std::path::Path, file_name: &str, contents: &str) {
        std::fs::create_dir_all(themes_dir).expect("create themes dir");
        std::fs::write(themes_dir.join(file_name), contents).expect("write theme file");
    }

    const MIDNIGHT_CORAL_TOML: &str = "name = \"Midnight Coral\"\n\
         subtitle = \"warm accent\"\n\
         background = \"#0c0d10\"\n\
         panel = \"#181a1e\"\n\
         accent_green = \"#5cb87f\"\n\
         accent_amber = \"#e2a336\"\n\
         accent_blue = \"#e07a5f\"\n";

    /// A real theme file, sitting in the settings-sibling `themes/` directory before the app is
    /// ever constructed, is loaded at startup (`crate::root::AdeApp::new_with_settings`) and
    /// really renders as a selectable card on the Themes page - not just present in
    /// `Self::custom_themes` as data nothing draws.
    #[gpui::test]
    fn a_custom_theme_file_on_disk_loads_at_startup_and_renders_as_a_real_card(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        write_custom_theme_file(
            &settings_dir.path().join("themes"),
            "midnight-coral.toml",
            MIDNIGHT_CORAL_TOML,
        );

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );

        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes.len()),
            1,
            "the real on-disk theme file must be loaded into the registry at construction"
        );
        assert!(app.read_with(cx, |app, _| app.custom_theme_load_errors.is_empty()));

        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("settings-theme-card-Midnight Coral")
                .is_some(),
            "the custom theme must actually render as a card on the Themes page, not just exist \
             as unrendered data"
        );
    }

    /// Selecting a real custom theme (`Self::set_theme_name`, the same method a card click
    /// invokes) must really re-skin the app - a representative colour token resolves to
    /// something other than Jerry Dark's own value - and persist the selection, exactly like
    /// `theme_swap_tests::selecting_a_real_theme_card_changes_the_live_selected_index_and_a_representative_color`
    /// proves for a built-in theme.
    #[gpui::test]
    fn selecting_a_custom_theme_really_reskins_the_app_and_persists(cx: &mut TestAppContext) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        write_custom_theme_file(
            &settings_dir.path().join("themes"),
            "midnight-coral.toml",
            MIDNIGHT_CORAL_TOML,
        );
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path.clone(),
        );

        let jerry_dark_window_bg = theme::surface::WINDOW.resolve();

        app.update(cx, |app, cx| {
            app.set_theme_name("Midnight Coral".to_string(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Midnight Coral"
        );
        assert!(
            theme::surface::WINDOW.resolve() != jerry_dark_window_bg,
            "selecting a real custom theme must actually change what a representative colour \
             token resolves to"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.last_dark_theme.clone()),
            "Midnight Coral",
            "Midnight Coral's background swatch is dark, so it should be remembered as the last \
             dark theme"
        );

        // Persisted for real - reloading the same settings file picks the same theme back up.
        cx.run_until_parked();
        let reloaded = settings_store::Settings::load_or_init_at(&settings_path);
        assert_eq!(reloaded.theme.name, "Midnight Coral");
    }

    /// The real, synchronous validate-then-apply step (`Self::apply_custom_theme_import_result`)
    /// behind `Self::start_import_custom_theme`'s async file-picker plumbing - driven directly
    /// with a real `custom_theme::import_theme_file`/`load_custom_themes_from_dir` result here
    /// since there is no real headless file dialog to simulate, matching how
    /// `Self::open_install_url`/`Self::open_settings_file`'s own OS-handoff calls are only ever
    /// unit-tested at the pure-decision-function layer (`crate::settings::widgets::
    /// open_command_for`), never through an actual OS dialog. The picker plumbing itself
    /// (`cx.prompt_for_paths`, `cx.background_executor()`) is real, verified GPUI API usage, not
    /// re-tested here.
    #[gpui::test]
    fn importing_a_real_theme_file_adds_it_to_the_registry_and_writes_a_canonical_copy(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path.clone(),
        );

        let source_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(&source_path, MIDNIGHT_CORAL_TOML).expect("write source file");
        let dest_dir = settings_dir.path().join("themes");

        let result = custom_theme::import_theme_file(&source_path, &dest_dir).map(|imported| {
            let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
            (imported, themes, errors)
        });
        app.update(cx, |app, cx| {
            app.apply_custom_theme_import_result(result, cx);
        });

        assert_eq!(app.read_with(cx, |app, _| app.custom_themes.len()), 1);
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes[0].name.clone()),
            "Midnight Coral"
        );
        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Ok(_))),
            "a real successful import should report a real success status, got {status:?}"
        );
        let expected_file = dest_dir.join("midnight-coral.toml");
        assert!(
            expected_file.exists(),
            "import must write a real, canonical copy into the settings-sibling themes directory"
        );

        // A malformed source is rejected with a real error and does not touch the registry.
        let bad_source = source_dir.path().join("bad.toml");
        std::fs::write(&bad_source, "name = \"\"\n").expect("write bad source");
        let bad_result = custom_theme::import_theme_file(&bad_source, &dest_dir).map(|imported| {
            let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
            (imported, themes, errors)
        });
        app.update(cx, |app, cx| {
            app.apply_custom_theme_import_result(bad_result, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes.len()),
            1,
            "a malformed import must not add a bogus entry"
        );
        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Err(_))),
            "a malformed import should report a real, honest error, got {status:?}"
        );
    }

    /// Re-importing the theme currently in use must immediately re-skin the app with its updated
    /// swatches, not leave it rendering the stale palette until a restart - a real bug an
    /// adversarial audit caught in the first version of `Self::apply_custom_theme_import_result`.
    #[gpui::test]
    fn reimporting_the_currently_active_theme_immediately_reskins_the_app(cx: &mut TestAppContext) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let dest_dir = settings_dir.path().join("themes");
        write_custom_theme_file(&dest_dir, "midnight-coral.toml", MIDNIGHT_CORAL_TOML);
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        app.update(cx, |app, cx| {
            app.set_theme_name("Midnight Coral".to_string(), cx);
        });
        let original = theme::surface::WINDOW.resolve();

        // Re-import the same theme under a different background swatch - deliberately near-black
        // (`#050505`), not an arbitrary colour: it must still clear the real panel/background
        // readability floor `CustomThemeFile::validate_with_builtin_check` enforces (see
        // `custom_theme::readability_floor_per_mille`'s own docs), or this re-import would be a
        // real, correct rejection rather than the re-skin this test means to exercise.
        let source_dir = tempfile::tempdir().expect("tempdir");
        let source_path = source_dir.path().join("picked.toml");
        std::fs::write(
            &source_path,
            MIDNIGHT_CORAL_TOML.replace("#0c0d10", "#050505"),
        )
        .expect("write updated source");
        let result = custom_theme::import_theme_file(&source_path, &dest_dir).map(|imported| {
            let (themes, errors) = custom_theme::load_custom_themes_from_dir(&dest_dir);
            (imported, themes, errors)
        });
        app.update(cx, |app, cx| {
            app.apply_custom_theme_import_result(result, cx);
        });

        assert!(
            theme::surface::WINDOW.resolve() != original,
            "re-importing the active custom theme with new swatches must re-skin the app \
             immediately, not require a restart"
        );
    }

    /// The Themes page's "Remove" action - real two-click confirmation
    /// (`Self::request_remove_custom_theme`/`Self::custom_theme_remove_armed`): the first click
    /// only arms it and touches nothing on disk; the second click actually deletes the real
    /// backing file and, when the removed theme was the active selection or the remembered
    /// `last_dark_theme`, falls back to Jerry Dark rather than leaving a dangling name neither
    /// can resolve.
    #[gpui::test]
    fn removing_the_active_custom_theme_requires_two_clicks_then_falls_back_to_jerry_dark(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let themes_dir = settings_dir.path().join("themes");
        write_custom_theme_file(&themes_dir, "midnight-coral.toml", MIDNIGHT_CORAL_TOML);
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );

        app.update(cx, |app, cx| {
            app.set_theme_name("Midnight Coral".to_string(), cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Midnight Coral"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.last_dark_theme.clone()),
            "Midnight Coral"
        );

        // First click only arms the confirmation - nothing is deleted yet.
        app.update(cx, |app, cx| {
            app.request_remove_custom_theme("Midnight Coral".to_string(), cx);
        });
        cx.run_until_parked();
        assert!(
            themes_dir.join("midnight-coral.toml").exists(),
            "a single click must not delete anything"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            Some("Midnight Coral".to_string())
        );
        assert_eq!(app.read_with(cx, |app, _| app.custom_themes.len()), 1);

        // Second click on the same name actually removes it.
        app.update(cx, |app, cx| {
            app.request_remove_custom_theme("Midnight Coral".to_string(), cx);
        });
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.custom_themes.is_empty()));
        assert!(!themes_dir.join("midnight-coral.toml").exists());
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            None
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Jerry Dark",
            "removing the active theme must fall back to Jerry Dark, not leave a dangling \
             selection"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.last_dark_theme.clone()),
            "Jerry Dark",
            "the dangling last_dark_theme must be reset too, or a later real OS-dark \
             follow_system signal would resolve to nothing"
        );
    }

    /// Leaving the Themes page must disarm a pending "Remove" confirmation - otherwise a stray
    /// click landing back on the same card position later could delete a theme the user never
    /// actually confirmed removing this time.
    #[gpui::test]
    fn leaving_the_themes_page_disarms_a_pending_remove_confirmation(cx: &mut TestAppContext) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        write_custom_theme_file(
            &settings_dir.path().join("themes"),
            "midnight-coral.toml",
            MIDNIGHT_CORAL_TOML,
        );
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
            app.request_remove_custom_theme("Midnight Coral".to_string(), cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            Some("Midnight Coral".to_string())
        );

        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::General, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            None
        );
    }

    /// The real, click-driven proof behind [`Self::request_remove_custom_theme`]'s
    /// `cx.stop_propagation()` (an adversarial audit verified this by reading GPUI's own event
    /// dispatch source rather than a live test - this closes that gap for real): two genuine
    /// simulated clicks on the Remove button's own real painted bounds delete the theme, and
    /// neither click also fires the card's own `on_click` underneath it - if it did, the first
    /// click would have switched the active theme to "Midnight Coral" instead of just arming the
    /// confirmation.
    #[gpui::test]
    fn clicking_the_real_remove_button_deletes_the_theme_without_selecting_its_card(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let themes_dir = settings_dir.path().join("themes");
        write_custom_theme_file(&themes_dir, "midnight-coral.toml", MIDNIGHT_CORAL_TOML);
        let mut settings = settings_store::Settings::default();
        settings.theme.name = "Slate".to_string();
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings,
            settings_path,
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        let remove_bounds = cx
            .debug_bounds("settings-theme-card-remove-Midnight Coral")
            .expect("the Remove affordance must have painted on the custom theme's card");

        // First real click: arms the confirmation.
        cx.simulate_click(remove_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Slate",
            "clicking Remove must not also select the card underneath it via the same click"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            Some("Midnight Coral".to_string())
        );
        assert!(themes_dir.join("midnight-coral.toml").exists());

        // Second real click on the same button (now re-rendered reading "Confirm?", same real
        // id/`debug_selector`) actually deletes it.
        let confirm_bounds = cx
            .debug_bounds("settings-theme-card-remove-Midnight Coral")
            .expect("the button must still be painted, now reading Confirm?");
        cx.simulate_click(confirm_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(!themes_dir.join("midnight-coral.toml").exists());
        assert!(app.read_with(cx, |app, _| app.custom_themes.is_empty()));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Slate",
            "Slate wasn't the removed theme, so it must stay selected throughout"
        );
    }

    /// The real, click-driven proof the Import/Export action buttons themselves are wired to a
    /// real handler, not just present in the tree - clicking "Import theme…" with no real file
    /// dialog available in this headless test environment still reaches
    /// `Self::start_import_custom_theme` (proven by the in-flight picker task actually being
    /// set), rather than silently doing nothing because the button's `on_click` was never wired.
    #[gpui::test]
    fn clicking_the_real_import_button_reaches_the_real_handler(cx: &mut TestAppContext) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        let import_bounds = cx
            .debug_bounds("settings-theme-import")
            .expect("the Import theme… button must have painted");
        cx.simulate_click(import_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app._custom_theme_import_task.is_some()),
            "a real click on the button must actually invoke Self::start_import_custom_theme, \
             not silently do nothing"
        );
    }

    /// Same real-click proof as
    /// [`clicking_the_real_import_button_reaches_the_real_handler`], for "Export current theme…".
    #[gpui::test]
    fn clicking_the_real_export_button_reaches_the_real_handler(cx: &mut TestAppContext) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        let export_bounds = cx
            .debug_bounds("settings-theme-export")
            .expect("the Export current theme… button must have painted");
        cx.simulate_click(export_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app._custom_theme_export_task.is_some()),
            "a real click on the button must actually invoke Self::start_export_custom_theme, \
             not silently do nothing"
        );
    }

    /// The full real, click-driven "New from template" round trip - not just "the handler was
    /// reached" (unlike the Import/Export click tests above, this action has no file-picker
    /// dialog to get stuck awaiting, so a real click here really can run to completion under
    /// `cx.run_until_parked()`): a genuine click on the button writes the real template file to
    /// disk, the resulting theme actually renders as a selectable Themes-page card (not just data
    /// sitting unrendered in `Self::custom_themes`), selecting that card really re-skins the app,
    /// and the two-click Remove affordance on it really deletes the file again - proving this
    /// isn't a decorative button bound to nothing.
    #[gpui::test]
    fn clicking_new_from_template_creates_a_real_selectable_removable_theme_end_to_end(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let themes_dir = settings_dir.path().join("themes");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path.clone(),
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.custom_themes.is_empty()),
            "sanity check: no custom theme should exist before the click"
        );

        // The real click.
        let create_bounds = cx
            .debug_bounds("settings-theme-new-from-template")
            .expect("the New from template… button must have painted");
        cx.simulate_click(create_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        // The real file landed on disk, at the real settings-sibling themes directory.
        let written_path = themes_dir.join("my-custom-theme.toml");
        assert!(
            written_path.exists(),
            "a real click must actually write the template file to the real custom-themes \
             directory, not just update in-memory state"
        );
        assert_eq!(
            std::fs::read_to_string(&written_path).expect("read written template"),
            custom_theme::CUSTOM_THEME_TEMPLATE_TOML,
            "the written file must be the real template's own bytes, comments included"
        );
        assert_eq!(app.read_with(cx, |app, _| app.custom_themes.len()), 1);
        let status = app.read_with(cx, |app, _| app.custom_theme_status.clone());
        assert!(
            matches!(status, Some(Ok(_))),
            "a real successful create-from-template should report a real success status, got \
             {status:?}"
        );

        // It really renders as a selectable card, not just data.
        assert!(
            cx.debug_bounds("settings-theme-card-My Custom Theme")
                .is_some(),
            "the created theme must actually render as a card on the Themes page"
        );

        // Selecting it really re-skins the app and persists.
        let jerry_dark_window_bg = theme::surface::WINDOW.resolve();
        let card_bounds = cx
            .debug_bounds("settings-theme-card-My Custom Theme")
            .expect("card must still be painted");
        cx.simulate_click(card_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "My Custom Theme"
        );
        assert!(
            theme::surface::WINDOW.resolve() != jerry_dark_window_bg,
            "selecting the template-created theme must really change what a representative \
             colour token resolves to"
        );

        // And it's really removable: first click arms, second click deletes.
        let remove_bounds = cx
            .debug_bounds("settings-theme-card-remove-My Custom Theme")
            .expect("the Remove affordance must have painted");
        cx.simulate_click(remove_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.custom_theme_remove_armed.clone()),
            Some("My Custom Theme".to_string())
        );
        let confirm_bounds = cx
            .debug_bounds("settings-theme-card-remove-My Custom Theme")
            .expect("the button must still be painted, now reading Confirm?");
        cx.simulate_click(confirm_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !written_path.exists(),
            "confirming Remove must really delete the real backing file"
        );
        assert!(app.read_with(cx, |app, _| app.custom_themes.is_empty()));
        assert_eq!(
            app.read_with(cx, |app, _| app.settings.theme.name.clone()),
            "Jerry Dark",
            "removing the currently-selected theme must fall back to Jerry Dark"
        );
    }

    /// A second real click on "New from template" refreshes the same on-disk file rather than
    /// creating a second, differently-suffixed one - matching
    /// `custom_theme::write_template_theme_a_second_time_refreshes_the_same_file_not_a_new_one`'s
    /// pure-function proof, exercised here through the real button instead.
    #[gpui::test]
    fn clicking_new_from_template_twice_refreshes_the_same_file_not_a_duplicate(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );
        cx.dispatch_action(ToggleSettings);
        app.update_in(cx, |app, window, cx| {
            app.select_settings_page(SettingsPage::Theme, window, cx);
        });
        cx.run_until_parked();

        for _ in 0..2 {
            let create_bounds = cx
                .debug_bounds("settings-theme-new-from-template")
                .expect("the New from template… button must have painted");
            cx.simulate_click(create_bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        }

        assert_eq!(
            app.read_with(cx, |app, _| app.custom_themes.len()),
            1,
            "clicking New from template twice must not leave two entries behind"
        );
    }

    /// A malformed theme file on disk at startup is skipped with a real, honest error - not
    /// silently dropped, and not a startup crash.
    #[gpui::test]
    fn a_malformed_theme_file_on_disk_is_skipped_with_a_real_recorded_error(
        cx: &mut TestAppContext,
    ) {
        let _guard = ResetThemeStateOnDrop;
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");
        write_custom_theme_file(
            &settings_dir.path().join("themes"),
            "broken.toml",
            "this is not { valid toml",
        );

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_store::Settings::default(),
            settings_path,
        );

        assert!(app.read_with(cx, |app, _| app.custom_themes.is_empty()));
        let errors = app.read_with(cx, |app, _| app.custom_theme_load_errors.clone());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("broken.toml:"),
            "the real error should name the offending file, got: {errors:?}"
        );
    }
}
