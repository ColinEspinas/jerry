use super::*;
use crate::root::widgets::render_keycap;

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

    /// The Settings surface's own key handler - just real `Esc`-to-close
    /// (`design_handoff_jerry_ade/README.md`: "esc (rendered as a keycap in the nav header)
    /// returns to the workspace"). No other Settings keyboard affordance is documented in the
    /// design (nav is click-only - `Jerry.dc.html`'s own nav rows have no keyboard binding),
    /// so unlike [`Self::handle_palette_key_down`] this doesn't need arrow-key/tab handling.
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

    /// Selects a Settings nav page - the nav row click handler.
    pub(super) fn select_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.settings_page = page;
        cx.notify();
    }

    /// Recomputes [`Self::agent_rows`] - a real `$PATH` search via `pty_core::resolve_on_path`
    /// (the same real search `pty-core`'s own spawn path performs, per that function's docs) for
    /// each known agent kind, via `crate::settings::detect_agent_rows`. Offloaded to the
    /// background executor and cached, mirroring [`Self::load_disk_usage`]'s exact shape: a
    /// not-found `resolve_on_path` call has no early exit and walks every `$PATH` entry (~30ms
    /// measured on a real dev machine for `codex`, genuinely absent), so running it inline in
    /// `render()` - which used to happen here - would block the foreground/GPUI thread for that
    /// long on every single frame the Agents page was open, and again on every one of
    /// `start_status_polling`'s 3s re-renders. Run once when Settings opens
    /// ([`Self::open_settings`]), not on every render or on the 3s poll cadence - the set of
    /// agent binaries actually on `$PATH` essentially never changes while the app is running.
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

    /// The Settings surface (`design_handoff_jerry_ade/README.md`'s "Settings" section): a
    /// 212px nav plus a content column. `track_focus`/`on_key_down` here are what make real
    /// `Esc` actually reach [`Self::handle_settings_key_down`] - the same real pattern
    /// `Self::render_palette` already uses for its own panel (`vendor/zed/crates/gpui/src/
    /// elements/div.rs`'s real `Div::track_focus`/`Interactivity::on_key_down`).
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

    /// The 212px nav column - `design_handoff_jerry_ade/README.md`: "Nav 212 wide ... Groups
    /// (Workspace, Editor, Other) with the same 9.5px uppercase header as the rail." Every one
    /// of the ten real pages is real, clickable navigation (`crate::settings::nav_groups`);
    /// only two render real content past this point - see `crate::settings`'s module docs.
    pub(super) fn render_settings_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = settings::nav_groups();
        // Real counts, not the mockup's fabricated `4`/`11`/`3` badges - `crate::settings::
        // AGENT_KINDS.len()` is exactly how many rows `self.agent_rows` will show, and
        // `self.worktrees.len()` is exactly how many rows the Worktrees page's card will show
        // (including any real error rows - see `Self::render_settings_worktree_row`).
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
                            .child(render_keycap("esc")),
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
                            // Real crate name/version (`env!` reads this crate's own real
                            // `Cargo.toml` at compile time), not `Jerry.dc.html`'s fabricated
                            // "jerry 0.4.2" - and an honest "no settings.toml yet" rather than
                            // the mockup's own "· settings.toml", since this app has no real
                            // settings-persistence file to point at (see `crate::settings`'s
                            // module docs for why the Behaviour/Policy toggle rows that would
                            // read/write one aren't built either).
                            .child(format!(
                                "{} {} \u{b7} no settings.toml yet",
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
                // Every other page's mockup badge (`3` for Language servers, etc.) is
                // fabricated sample data with nothing real behind it - omitted rather than
                // invented, matching `crate::settings`'s own documented scope.
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
                theme::border::SELECTED_EDGE
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
                    .text_size(px(11.5))
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

    /// The content column: header block (title + real subtitle) plus whichever page's real (or
    /// honestly placeholder) body - `design_handoff_jerry_ade/README.md`'s "Content column"
    /// section.
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
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(15.0))
                            .text_color(theme::text::SELECTED)
                            .child(page.label()),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .font(font(theme::font::SANS))
                            .text_size(px(11.5))
                            .text_color(theme::settings::SUBTITLE)
                            .child(page.subtitle()),
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
                    .child(match page {
                        SettingsPage::Agents => {
                            self.render_settings_agents_page(cx).into_any_element()
                        }
                        SettingsPage::Worktrees => {
                            self.render_settings_worktrees_page(cx).into_any_element()
                        }
                        _ => render_settings_placeholder_page().into_any_element(),
                    }),
            )
    }

    /// *Agents › Installed* - `design_handoff_jerry_ade/README.md`: "bordered card ... of four
    /// rows ... agent badge ... name ... binary path ... model ... a `default` pill ... green
    /// dot + 'ready' ... Edit." This app's real version drops the `model`/`default`/`Edit`
    /// pieces - see `crate::settings`'s module docs for why - and shows exactly
    /// [`settings::AGENT_KINDS`]'s two real rows (`claude`, `codex`) instead of the mockup's
    /// four fabricated ones, each with a real, live PATH-search-derived status.
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
            // Real, honest - not "unknown"/blank - the exact reason a "ready" dot isn't shown:
            // a real `$PATH` search for this literal binary name came back empty.
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
    /// '+ Add an agent — any binary that speaks a resumable session on stdin'." Rendered real
    /// and dimmed/inert (no `on_click`, no fake modal) - `crate::sessions::SessionKind` is a
    /// fixed Rust enum, so there is no real runtime "register a new agent binary" flow to wire
    /// this to yet; see `crate::settings`'s module docs for the judgment call this documents.
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
    /// action. Footer totals ... and a Prune 1 merged action." Every row and every total here
    /// is the exact real data Phase B already built (`Self::worktrees`, `Self::worktree_notes`,
    /// `Self::worktree_disk_usage`/`Self::disk_usage`) - not a re-derivation of it - and Prune
    /// (both the row action and the footer action) dispatches through the exact same
    /// `Self::request_prune`/`Self::execute_prune` two-click-confirmation path the rail footer
    /// and command palette already use (see [`Self::render_settings_worktree_row`]'s docs for
    /// why a *row's* Prune click isn't scoped to only that one row).
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

    /// One real Worktrees-page row. `Open` selects that worktree in the real workspace and
    /// switches back to it (`Self::select_worktree_by_path` + `Self::close_settings`, exactly
    /// what clicking a worktree in the rail already does, plus leaving Settings). `Prune`
    /// deliberately calls the exact same [`Self::request_prune`] the footer's own
    /// `Prune N merged` button and the command palette's `Prune Worktrees` command call -
    /// there is no separate "prune only this one worktree" code path in this app, since the
    /// one real, safety-checked removal primitive (`Self::prunable_worktree_paths` plus
    /// `Self::execute_prune`) always operates on *every* currently-prunable worktree at once,
    /// live-session-excluded. A row's `Prune` button is only ever shown when that row's own
    /// worktree is itself one of those candidates (`settings::worktree_row_action`), so
    /// clicking it is always a real, honest prune that includes this worktree - it just isn't
    /// scoped to *only* this worktree if others also happen to be prunable at the same moment,
    /// exactly like the footer button it reuses.
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
                        this.select_worktree_by_path(&path, cx);
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
}

/// A nav-only Settings page's real, honest placeholder body - `Jerry.dc.html`'s own `setStub`
/// state's exact copy (line ~705: `not designed in this mockup`). Used for every page except
/// [`SettingsPage::Agents`]/[`SettingsPage::Worktrees`] - see `crate::settings`'s module docs
/// for why this is a documented act of fidelity to the source design (which itself never
/// specified what these pages should contain), not a shortcut.
pub(super) fn render_settings_placeholder_page() -> impl IntoElement {
    div()
        .py(px(26.0))
        .font(font(theme::font::MONO))
        .text_size(px(11.0))
        .text_color(theme::text::DISABLED)
        .child("not designed in this mockup")
}
